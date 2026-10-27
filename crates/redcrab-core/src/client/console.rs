//! The console's own session: a dedicated connection used one command at a
//! time, like `redis-cli`, so that SELECT, MULTI/EXEC or WATCH typed into
//! the console stay in the console.

use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{
            AtomicBool, AtomicI64, AtomicU64,
            Ordering::{Relaxed, SeqCst},
        },
    },
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use redis::{
    AsyncConnectionConfig, Cmd, ErrorKind, Value,
    aio::{ConnectionLike as _, MultiplexedConnection},
    cmd,
};
use tokio::sync::{Notify, mpsc, oneshot};

use super::{ReadOnlyError, connect_error};
use crate::{
    commands::{self, CommandClass, CommandSpec, Flags},
    config::ConnectionConfig,
    runtime::runtime,
};

/// How long the console waits for a reply. Blocking commands (BLPOP, XREAD
/// BLOCK, WAIT, ...) carry their own timeout and are not limited.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connection-state commands a read-only console still allows: they cannot
/// write by themselves, and every command queued inside MULTI is checked on
/// its own when it is typed.
const READ_ONLY_STATE: &[&str] = &["SELECT", "WATCH", "UNWATCH", "MULTI", "EXEC", "DISCARD"];

/// A console attached to one server. Commands run in the order they are
/// submitted, on a connection of the session's own that is opened on the
/// first command and reopened after it breaks. Dropping the session closes
/// the connection.
pub struct ConsoleSession {
    requests: mpsc::UnboundedSender<Request>,
    shared: Arc<Shared>,
}

/// State the caller can observe while the session task owns the connection.
struct Shared {
    /// Wakes the command that is running when [`ConsoleSession::cancel`] is
    /// called.
    cancel: Notify,
    /// How many cancels there have been. A request records the count it was
    /// submitted under, so a cancel also reaches the commands submitted
    /// before it that have not started yet, however soon after submitting
    /// it comes.
    cancels: AtomicU64,
    closed: AtomicBool,
    db: AtomicI64,
    in_transaction: AtomicBool,
}

struct Request {
    argv: Vec<Vec<u8>>,
    cancels: u64,
    reply: oneshot::Sender<Result<Value>>,
}

impl ConsoleSession {
    pub(crate) fn new(redis: Arc<redis::Client>, config: Arc<ConnectionConfig>) -> Self {
        let db = i64::from(config.db);
        let shared = Arc::new(Shared {
            cancel: Notify::new(),
            cancels: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            db: AtomicI64::new(db),
            in_transaction: AtomicBool::new(false),
        });
        let (requests, queue) = mpsc::unbounded_channel();
        let session = Session {
            redis,
            config,
            shared: shared.clone(),
            con: None,
            db,
            transaction: None,
        };
        runtime().spawn(session.run(queue));
        Self { requests, shared }
    }

    /// Runs one command line (name first). Arguments are bytes, so binary
    /// keys can be addressed (see [`crate::format::split_args`]); strings
    /// work as they are. Commands run strictly in the order of the calls,
    /// whether or not earlier futures were awaited.
    ///
    /// Refused without reaching the server: on a read-only connection,
    /// anything but reads and the transaction commands (with a
    /// [`ReadOnlyError`]); on any connection, commands the console cannot
    /// follow (SUBSCRIBE, MONITOR, RESET, HELLO, ...). A command refused
    /// inside MULTI makes EXEC fail, as a command the server rejects would.
    pub fn exec<A: Into<Vec<u8>>>(
        &self,
        argv: impl IntoIterator<Item = A>,
    ) -> impl Future<Output = Result<Value>> + Send + 'static {
        let argv = argv.into_iter().map(Into::into).collect();
        let (reply, response) = oneshot::channel();
        let cancels = self.shared.cancels.load(SeqCst);
        let sent = self
            .requests
            .send(Request {
                argv,
                cancels,
                reply,
            })
            .is_ok();
        async move {
            anyhow::ensure!(sent, "the console session has ended");
            response
                .await
                .unwrap_or_else(|_| Err(anyhow!("the console session has ended")))
        }
    }

    /// Abandons every command submitted before this call: the one that is
    /// running, whose connection is dropped so that the server stops
    /// working on it (a `BLPOP key 0` would otherwise block the console for
    /// good), and the ones waiting behind it, which never run. Each answers
    /// with an error starting with "cancelled". Commands submitted later run
    /// as usual, on a new connection on the same database; a transaction
    /// that was open is lost.
    pub fn cancel(&self) {
        self.shared.cancels.fetch_add(1, SeqCst);
        self.shared.cancel.notify_waiters();
    }

    /// The database the console is on, after its own SELECTs.
    pub fn db(&self) -> i64 {
        self.shared.db.load(Relaxed)
    }

    /// Whether a MULTI is open, so a prompt can show it.
    pub fn in_transaction(&self) -> bool {
        self.shared.in_transaction.load(Relaxed)
    }
}

impl Drop for ConsoleSession {
    fn drop(&mut self) {
        // Commands still queued fail instead of running after the console is gone.
        self.shared.closed.store(true, Relaxed);
        self.shared.cancel.notify_waiters();
    }
}

/// An argument as text, for looking commands up and naming them; command
/// names are ASCII, so a lossy conversion loses nothing that matters.
fn text(arg: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(arg)
}

/// Decides whether the console may send `argv`. Returns the command's table
/// entry (`None` for commands the table does not know), or why it is refused.
fn check(argv: &[Vec<u8>], read_only: bool) -> Result<Option<&'static CommandSpec>> {
    let Some(name) = argv.first() else {
        bail!("empty command");
    };
    let sub = argv.get(1).map(|sub| text(sub));
    let spec = commands::lookup(&text(name), sub.as_deref());
    if let Some(spec) = spec
        && let Some(reason) = unsupported(spec.name)
    {
        bail!("{} is not available in the console: {reason}", spec.name);
    }
    let class = spec.map_or(CommandClass::Unknown, CommandSpec::class);
    let allowed = match class {
        CommandClass::Read => true,
        CommandClass::ConnectionState => spec.is_some_and(|s| READ_ONLY_STATE.contains(&s.name)),
        CommandClass::Write | CommandClass::Admin | CommandClass::Unknown => false,
    };
    if read_only && !allowed {
        let shown = spec.map_or_else(|| display_name(argv), |s| s.name.to_string());
        return Err(ReadOnlyError::refused(shown, class).into());
    }
    Ok(spec)
}

/// Why the console cannot run a command on any connection.
fn unsupported(name: &str) -> Option<&'static str> {
    Some(match name {
        "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" => {
            "subscribing streams messages, which needs a pub/sub viewer; the console shows one reply per command"
        }
        "UNSUBSCRIBE" | "PUNSUBSCRIBE" | "SUNSUBSCRIBE" => {
            "the console never subscribes, so there is nothing to unsubscribe from"
        }
        "MONITOR" => {
            "it streams every command the server runs, which needs a streaming viewer; the console shows one reply per command"
        }
        "SYNC" | "PSYNC" | "REPLCONF" => {
            "it belongs to the replication protocol and would turn the console's connection into a replica link"
        }
        "RESET" => {
            "it would reset the console's connection behind its back; use DISCARD, UNWATCH or SELECT instead"
        }
        "QUIT" => "it closes the console's connection; close the console instead",
        "HELLO" => "it switches the connection's protocol, which the console cannot follow",
        "CLIENT REPLY" => {
            "with OFF or SKIP the server stops answering, and the console waits for every reply"
        }
        "SCRIPT DEBUG" => "debugging scripts needs an interactive debugger (redis-cli --ldb)",
        _ => return None,
    })
}

/// Upper-cased name of a command the table does not know, with its
/// subcommand when it was typed under a known container (`CONFIG BOGUS`).
fn display_name(argv: &[Vec<u8>]) -> String {
    let name = text(&argv[0]).to_uppercase();
    match (commands::lookup(&text(&argv[0]), None), argv.get(1)) {
        (Some(container), Some(sub)) if !commands::subcommands(container).is_empty() => {
            format!("{name} {}", text(sub).to_uppercase())
        }
        _ => name,
    }
}

/// A server error in redis-cli's words (`ERR unknown command ...`) instead
/// of the library's (`ResponseError: unknown command ...`).
fn reply_error(err: redis::RedisError) -> anyhow::Error {
    let server = matches!(err.kind(), ErrorKind::Server(_) | ErrorKind::Extension);
    match (err.code(), err.detail()) {
        (Some(code), Some(detail)) if server => anyhow!("{code} {detail}"),
        (Some(code), None) if server => anyhow!("{code}"),
        _ => err.into(),
    }
}

/// The task that owns the console's connection.
struct Session {
    redis: Arc<redis::Client>,
    config: Arc<ConnectionConfig>,
    shared: Arc<Shared>,
    con: Option<MultiplexedConnection>,
    /// The selected database, restored when the connection is reopened.
    db: i64,
    transaction: Option<Transaction>,
}

/// An open MULTI, as far as the session can tell.
#[derive(Default)]
struct Transaction {
    /// Commands the server has queued.
    queued: usize,
    /// Queued SELECTs by queue position; they take effect if EXEC runs them.
    selects: Vec<(usize, i64)>,
    /// A command was refused here, so EXEC must not run the others.
    refused: bool,
}

impl Session {
    async fn run(mut self, mut queue: mpsc::UnboundedReceiver<Request>) {
        while let Some(request) = queue.recv().await {
            self.handle(request).await;
        }
    }

    /// Runs one request, unless it was cancelled, and answers it.
    async fn handle(&mut self, request: Request) {
        let Request {
            argv,
            cancels,
            reply,
        } = request;
        let result = if self.shared.closed.load(Relaxed) {
            Err(anyhow!("the console was closed"))
        } else {
            let shared = self.shared.clone();
            let cancelled = shared.cancel.notified();
            tokio::pin!(cancelled);
            // Listening before looking at the count, so that a cancel between
            // the two is not missed either.
            cancelled.as_mut().enable();
            if shared.cancels.load(SeqCst) != cancels {
                // Cancelled while it waited its turn: nothing was sent. A
                // transaction it belonged in must not run without it.
                if let Some(transaction) = &mut self.transaction {
                    transaction.refused = true;
                }
                Err(anyhow!("cancelled before it ran"))
            } else {
                let outcome = tokio::select! {
                    result = self.exec(&argv) => Some(result),
                    () = &mut cancelled => None,
                };
                outcome.unwrap_or_else(|| {
                    self.disconnect();
                    Err(anyhow!(
                        "cancelled; the console reconnects for the next command"
                    ))
                })
            }
        };
        let _ = reply.send(result);
    }

    async fn exec(&mut self, argv: &[Vec<u8>]) -> Result<Value> {
        let spec = match check(argv, self.config.read_only) {
            Ok(spec) => spec,
            Err(err) => {
                if let Some(transaction) = &mut self.transaction {
                    transaction.refused = true;
                }
                return Err(err);
            }
        };
        let name = spec.map_or("", |spec| spec.name);
        if name == "EXEC" && self.transaction.as_ref().is_some_and(|t| t.refused) {
            // Like the server after a queuing error: nothing runs.
            let discarded = self.send(&[b"DISCARD".to_vec()], Some(REPLY_TIMEOUT)).await;
            self.end_transaction();
            discarded?;
            bail!("EXECABORT Transaction discarded because a command was refused");
        }
        let blocking = spec.is_some_and(|spec| spec.flags.contains(Flags::BLOCKING));
        let reply = self.send(argv, (!blocking).then_some(REPLY_TIMEOUT)).await;
        self.track(name, argv, &reply);
        reply
    }

    /// Sends one command on the session's connection, opening it first if
    /// needed. A timeout or a broken connection drops it, so the next command
    /// starts over on a fresh one instead of reading a late reply.
    async fn send(&mut self, argv: &[Vec<u8>], timeout: Option<Duration>) -> Result<Value> {
        let mut con = self.connection().await?;
        let mut command = Cmd::new();
        for arg in argv {
            command.arg(&arg[..]);
        }
        // The raw reply: `query_async` would turn an error anywhere inside
        // it into an error for all of it, so an EXEC with one failed command
        // would lose the replies of the others (and the SELECTs among them).
        let request = con.req_packed_command(&command);
        let result = match timeout {
            Some(limit) => match tokio::time::timeout(limit, request).await {
                Ok(result) => result,
                Err(_) => {
                    self.disconnect();
                    bail!(
                        "no reply within {} s; the console reconnects for the next command",
                        limit.as_secs()
                    );
                }
            },
            None => request.await,
        };
        match result {
            Ok(Value::ServerError(err)) => Err(reply_error(err.into())),
            Ok(value) => Ok(value),
            Err(err) => {
                if err.is_io_error() || err.is_connection_dropped() || err.is_unrecoverable_error()
                {
                    self.disconnect();
                }
                Err(reply_error(err))
            }
        }
    }

    async fn connection(&mut self) -> Result<MultiplexedConnection> {
        if let Some(con) = &self.con {
            return Ok(con.clone());
        }
        let settings = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(CONNECT_TIMEOUT))
            // `send` applies its own, per-command timeout.
            .set_response_timeout(None);
        let mut con = self
            .redis
            .get_multiplexed_async_connection_with_config(&settings)
            .await
            .map_err(|err| connect_error(err, &self.config))?;
        // A new connection starts on the configured database; go back to the
        // one the console had selected.
        let configured = i64::from(self.config.db);
        if self.db != configured {
            let mut select = cmd("SELECT");
            select.arg(self.db);
            let selected = tokio::time::timeout(REPLY_TIMEOUT, select.query_async::<()>(&mut con));
            if !matches!(selected.await, Ok(Ok(()))) {
                let lost = self.db;
                self.set_db(configured);
                bail!(
                    "reconnected, but could not select database {lost} again; the console is back on database {configured}"
                );
            }
        }
        self.con = Some(con.clone());
        Ok(con)
    }

    /// Follows SELECT and MULTI/EXEC/DISCARD so that `db()` and
    /// `in_transaction()` stay true and a reconnect can restore the database.
    fn track(&mut self, name: &str, argv: &[Vec<u8>], reply: &Result<Value>) {
        let ok = matches!(reply, Ok(Value::Okay));
        let db_arg = || argv.get(1).and_then(|db| text(db).parse::<i64>().ok());
        match (name, &mut self.transaction) {
            ("MULTI", None) if ok => {
                self.transaction = Some(Transaction::default());
                self.shared.in_transaction.store(true, Relaxed);
            }
            ("EXEC", Some(transaction)) => {
                let selects = std::mem::take(&mut transaction.selects);
                self.end_transaction();
                if let Ok(Value::Array(replies)) = reply {
                    for (ix, db) in selects {
                        if matches!(replies.get(ix), Some(Value::Okay)) {
                            self.set_db(db);
                        }
                    }
                }
            }
            ("DISCARD", Some(_)) => self.end_transaction(),
            (_, Some(transaction)) => {
                if matches!(reply, Ok(Value::SimpleString(s)) if s == "QUEUED") {
                    if name == "SELECT"
                        && let Some(db) = db_arg()
                    {
                        transaction.selects.push((transaction.queued, db));
                    }
                    transaction.queued += 1;
                }
            }
            ("SELECT", None) if ok => {
                if let Some(db) = db_arg() {
                    self.set_db(db);
                }
            }
            _ => {}
        }
    }

    /// Forgets the connection; the server drops a transaction or WATCH
    /// along with it.
    fn disconnect(&mut self) {
        self.con = None;
        self.end_transaction();
    }

    fn end_transaction(&mut self) {
        self.transaction = None;
        self.shared.in_transaction.store(false, Relaxed);
    }

    fn set_db(&mut self, db: i64) {
        self.db = db;
        self.shared.db.store(db, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<Vec<u8>> {
        line.split_whitespace()
            .map(|arg| arg.as_bytes().to_vec())
            .collect()
    }

    /// `Ok(name)` with the table name, or `Err(message)`.
    fn gate(line: &str, read_only: bool) -> Result<&'static str, String> {
        check(&argv(line), read_only)
            .map(|spec| spec.map_or("?", |s| s.name))
            .map_err(|err| err.to_string())
    }

    #[test]
    fn read_only_gate() {
        // (command line, allowed on a read-only connection)
        let cases = [
            ("GET k", true),
            ("get k", true),
            ("HGETALL h", true),
            ("SCAN 0 MATCH * COUNT 10", true),
            ("INFO memory", true),
            ("config get maxmemory", true),
            ("CLIENT LIST", true),
            ("SLOWLOG GET 10", true),
            ("MEMORY USAGE k", true),
            ("KEYS *", true),
            ("DBSIZE", true),
            ("COMMAND DOCS GET", true),
            ("FT.CURSOR READ idx 1", true),
            ("JSON.GET doc", true),
            ("FT.SEARCH idx *", true),
            ("SELECT 2", true),
            ("MULTI", true),
            ("EXEC", true),
            ("DISCARD", true),
            ("WATCH k", true),
            ("UNWATCH", true),
            ("SET k v", false),
            ("set k v", false),
            ("DEL k", false),
            ("UNLINK k", false),
            ("EXPIRE k 10", false),
            ("FLUSHALL", false),
            ("FLUSHDB", false),
            ("CONFIG SET maxmemory 1", false),
            ("config rewrite", false),
            ("CLIENT KILL ID 1", false),
            ("EVAL return 1 0", false),
            ("EVALSHA abc 0", false),
            ("FCALL f 0", false),
            // Read-only scripts can still run administrative commands.
            ("EVAL_RO return 1 0", false),
            ("evalsha_ro abc 0", false),
            ("FCALL_RO f 0", false),
            // Declared `readonly`, but they change the value or other clients' state.
            ("PFCOUNT h", false),
            ("CF.COMPACT cf", false),
            ("FT.CURSOR DEL idx 1", false),
            ("FT.CURSOR GC idx", false),
            ("PUBLISH ch hi", false),
            ("SCRIPT FLUSH", false),
            ("SCRIPT LOAD return", false),
            ("JSON.SET doc $ 1", false),
            ("FT.CREATE idx SCHEMA f TEXT", false),
            ("DEBUG SLEEP 0", false),
            ("SHUTDOWN", false),
            ("AUTH user pass", false),
            ("CLIENT SETNAME x", false),
            ("NOSUCHCOMMAND", false),
            ("CONFIG BOGUS", false),
        ];
        for (line, allowed) in cases {
            assert!(
                gate(line, false).is_ok(),
                "{line} refused on a writable connection"
            );
            let result = gate(line, true);
            assert_eq!(
                result.is_ok(),
                allowed,
                "{line} on a read-only connection: {result:?}"
            );
            if let Err(message) = result {
                assert!(
                    message.ends_with("this connection is read-only"),
                    "{line}: {message}"
                );
            }
        }
    }

    #[test]
    fn read_only_refusals_are_typed_and_name_the_command() {
        let err = check(&argv("config set maxmemory 1"), true).unwrap_err();
        let read_only = err.downcast_ref::<ReadOnlyError>().unwrap();
        assert_eq!(read_only.command(), Some("CONFIG SET"));
        assert_eq!(
            err.to_string(),
            "CONFIG SET is an administrative command, and this connection is read-only"
        );
        let err = check(&argv("set k v"), true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "SET can modify data, and this connection is read-only"
        );
        let err = check(&argv("config bogus x"), true).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ReadOnlyError>().unwrap().command(),
            Some("CONFIG BOGUS")
        );
        let err = check(&argv("frobnicate x"), true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "FROBNICATE is not a command known to be safe, and this connection is read-only"
        );
        let err = check(&argv("eval_ro x 0"), true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "EVAL_RO runs a script, which can still run administrative commands, and this connection is read-only"
        );
        // Arguments that are not UTF-8 are fine; names are looked up as text.
        assert_eq!(
            check(&[b"GET".to_vec(), b"\xff\x00".to_vec()], true)
                .unwrap()
                .map(|s| s.name),
            Some("GET")
        );
        let err = check(&[b"SET".to_vec(), b"\xff".to_vec()], true).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ReadOnlyError>().unwrap().command(),
            Some("SET")
        );
        let err = check(&[b"\xffBAD".to_vec()], true).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ReadOnlyError>().unwrap().command(),
            Some("\u{fffd}BAD")
        );
    }

    #[test]
    fn refuses_what_would_break_the_session_everywhere() {
        for line in [
            "SUBSCRIBE ch",
            "psubscribe *",
            "SSUBSCRIBE ch",
            "UNSUBSCRIBE",
            "MONITOR",
            "SYNC",
            "PSYNC ? -1",
            "REPLCONF listening-port 1",
            "RESET",
            "QUIT",
            "HELLO 3",
            "client reply off",
            "SCRIPT DEBUG YES",
        ] {
            for read_only in [false, true] {
                let err = check(&argv(line), read_only).unwrap_err();
                assert!(
                    err.downcast_ref::<ReadOnlyError>().is_none(),
                    "{line}: {err}"
                );
                assert!(
                    err.to_string()
                        .contains("is not available in the console: "),
                    "{line}: {err}"
                );
            }
        }
        assert!(gate("CLIENT REPLY ON", false).is_err());
        assert!(gate("CLIENT INFO", false).is_ok());
        assert!(gate("", false).unwrap_err().contains("empty command"));
    }

    #[test]
    fn unknown_commands_pass_on_writable_connections() {
        // The server answers for them; only read-only connections need to know.
        assert_eq!(gate("FROBNICATE", false), Ok("?"));
        assert_eq!(gate("CONFIG BOGUS", false), Ok("?"));
    }

    /// A session against a port nothing listens on: whatever reaches the
    /// connection step fails to connect, so refusals are told apart from
    /// commands that were let through.
    fn offline_session(read_only: bool) -> ConsoleSession {
        let config = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: 1,
            read_only,
            ..ConnectionConfig::default()
        };
        let redis = config.client().unwrap();
        ConsoleSession::new(Arc::new(redis), Arc::new(config))
    }

    #[test]
    fn refuses_before_connecting() {
        let session = offline_session(true);
        let run = |line: &str| futures::executor::block_on(session.exec(argv(line))).unwrap_err();
        assert!(run("SET k v").downcast_ref::<ReadOnlyError>().is_some());
        assert!(run("SUBSCRIBE ch").to_string().contains("not available"));
        let err = run("GET k");
        assert!(
            format!("{err:#}").contains("could not connect to 127.0.0.1:1"),
            "{err:#}"
        );
        assert_eq!(session.db(), 0);
        assert!(!session.in_transaction());
    }

    #[test]
    fn runs_commands_in_submission_order() {
        let session = offline_session(true);
        // Submitted before any is awaited; each must still be answered, in order.
        let pending: Vec<_> = ["SET a 1", "GET a", "DEL a"]
            .into_iter()
            .map(|line| session.exec(argv(line)))
            .collect();
        let results: Vec<_> = pending
            .into_iter()
            .map(futures::executor::block_on)
            .collect();
        assert!(
            results[0]
                .as_ref()
                .unwrap_err()
                .downcast_ref::<ReadOnlyError>()
                .is_some()
        );
        assert!(
            results[1]
                .as_ref()
                .unwrap_err()
                .downcast_ref::<ReadOnlyError>()
                .is_none()
        );
        assert!(
            results[2]
                .as_ref()
                .unwrap_err()
                .downcast_ref::<ReadOnlyError>()
                .is_some()
        );
    }

    /// A session against a server that accepts the connection and never
    /// answers, so a command that reaches it stays running.
    fn silent_session() -> (ConsoleSession, std::net::TcpListener) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let config = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            ..ConnectionConfig::default()
        };
        let redis = config.client().unwrap();
        (
            ConsoleSession::new(Arc::new(redis), Arc::new(config)),
            listener,
        )
    }

    #[test]
    fn cancel_reaches_commands_submitted_right_before_it() {
        let (session, listener) = silent_session();
        // However far the session got with them, all three are abandoned:
        // the first may be running, the others are still waiting.
        let pending: Vec<_> = ["GET a", "GET b", "GET c"]
            .into_iter()
            .map(|line| session.exec(argv(line)))
            .collect();
        session.cancel();
        for result in pending.into_iter().map(futures::executor::block_on) {
            let err = result.unwrap_err().to_string();
            assert!(err.starts_with("cancelled"), "{err}");
        }
        // A command submitted after the cancel runs: with the server gone it
        // fails to connect, rather than being cancelled.
        drop(listener);
        let err = futures::executor::block_on(session.exec(argv("GET d"))).unwrap_err();
        assert!(format!("{err:#}").contains("could not connect"), "{err:#}");
    }

    #[test]
    fn a_cancelled_command_inside_multi_aborts_the_transaction() {
        let session = offline_session(false);
        let mut runner = Session {
            redis: Arc::new(ConnectionConfig::default().client().unwrap()),
            config: Arc::new(ConnectionConfig::default()),
            shared: session.shared.clone(),
            con: None,
            db: 0,
            transaction: Some(Transaction::default()),
        };
        let (reply, response) = oneshot::channel();
        let request = Request {
            argv: argv("SET k v"),
            cancels: 0,
            reply,
        };
        session.cancel();
        let transaction = runtime().block_on(async move {
            runner.handle(request).await;
            runner.transaction
        });
        let err = futures::executor::block_on(response).unwrap().unwrap_err();
        assert_eq!(err.to_string(), "cancelled before it ran");
        assert!(
            transaction.is_some_and(|t| t.refused),
            "EXEC must not run without it"
        );
    }

    #[test]
    fn dropping_the_session_fails_queued_commands() {
        let session = offline_session(false);
        let pending = session.exec(argv("GET k"));
        drop(session);
        assert!(futures::executor::block_on(pending).is_err());
    }
}
