//! A master found through Sentinel, followed across failovers.
//!
//! The master's address comes from `SENTINEL GET-MASTER-ADDR-BY-NAME`, asking
//! the sentinels in order until one answers. Requests go to it over a
//! [`ConnectionManager`]. When a request shows that the master moved (the
//! server answers `READONLY` or `MASTERDOWN`, or the connection breaks), the
//! address is asked for again, a connection to the new master replaces the
//! old one, and the request is sent again: a bounded number of times, with a
//! growing pause, never in a busy loop.
//!
//! A write is sent again only when it certainly did not run: after
//! `READONLY` / `MASTERDOWN` (the server refused it) or a refused
//! connection. A write whose connection broke while it was on the way may
//! have run, so its error is returned instead. To keep that rare, a write
//! on a connection idle for a while is preceded by a PING: Sentinel closes
//! client connections during a failover, and the PING finds that out (and
//! reconnects) before the write is sent.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use futures::future::BoxFuture;
use redis::{
    AsyncConnectionConfig, Cmd, ErrorKind, Pipeline, RedisError, RedisFuture, RedisResult, Value,
    aio::{ConnectionLike, ConnectionManager, ConnectionManagerConfig},
    cmd,
};

use super::connect_error;
use crate::{
    commands::{self, CommandClass},
    config::{ConnectionConfig, Target, Topology, format_address, parse_address},
};

/// How long a sentinel may take to connect and answer.
const SENTINEL_TIMEOUT: Duration = Duration::from_secs(3);
/// Re-sends after a failover sign, and the pause before the first; each
/// further pause doubles (0.2 + 0.4 + 0.8 + 1.6 + 3.2 s ≈ 6 s in all, which
/// covers a Sentinel failover).
const MAX_RETRIES: u32 = 5;
const FIRST_PAUSE: Duration = Duration::from_millis(200);
/// A write after this long without traffic first checks the connection.
const IDLE_CHECK: Duration = Duration::from_secs(1);

/// The connection to the current master. Clones share it.
#[derive(Clone)]
pub(crate) struct Sentinel {
    inner: Arc<Inner>,
}

struct Inner {
    /// Settings for the master: database, user, password, TLS.
    config: Arc<ConnectionConfig>,
    master: String,
    sentinels: Vec<(String, u16)>,
    /// The settings sentinels are reached with.
    sentinel_config: ConnectionConfig,
    current: Mutex<Current>,
    /// One re-resolution at a time. Holds when the last one started: a
    /// request that failed before that uses its outcome instead of asking
    /// the sentinels again. Never held across a pause.
    switching: tokio::sync::Mutex<Option<Instant>>,
}

struct Current {
    con: ConnectionManager,
    address: (String, u16),
    /// Bumped on every switch, so requests that failed on the old master
    /// switch once between them.
    generation: u64,
    last_used: Instant,
}

/// Why a request should be sent again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sign {
    /// The server refused it, or it never reached a server.
    Refused,
    /// The connection broke; it may have run.
    Dropped,
}

impl Sentinel {
    /// Asks the sentinels for the master and connects to it. Retries a few
    /// times when the address it gets is not a master (yet): a failover may
    /// be under way.
    pub(crate) async fn connect(config: Arc<ConnectionConfig>) -> Result<Self> {
        let Topology::Sentinel {
            master, sentinels, ..
        } = &config.topology
        else {
            bail!("not a Sentinel connection");
        };
        let sentinels = sentinels
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| parse_address(s).map_err(|why| anyhow!("sentinel {s:?}: {why}")))
            .collect::<Result<Vec<_>>>()?;
        let sentinel_config = sentinel_settings(&config);
        let master = master.trim().to_string();
        let mut attempt = 0;
        let (con, address) = loop {
            let address = resolve(&master, &sentinels, &sentinel_config).await?;
            match open_master(&config, &master, &address).await {
                Ok(con) => break (con, address),
                Err(err) if attempt < 3 && err.downcast_ref::<NotMaster>().is_some() => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(err) => return Err(err),
            }
        };
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                master,
                sentinels,
                sentinel_config,
                current: Mutex::new(Current {
                    con,
                    address,
                    generation: 0,
                    last_used: Instant::now(),
                }),
                switching: tokio::sync::Mutex::new(None),
            }),
        })
    }

    pub(crate) fn master_name(&self) -> &str {
        &self.inner.master
    }

    /// `host:port` of the master as last resolved.
    pub(crate) fn address(&self) -> String {
        let current = self.inner.current.lock().unwrap_or_else(|e| e.into_inner());
        format_address(&current.address.0, current.address.1)
    }

    /// Bumped each time requests move to another master. A SCAN cursor
    /// only means something on the server that handed it out, so a walk
    /// that sees this change starts over (see
    /// [`NodeCursor`](super::conn::NodeCursor)).
    pub(crate) fn generation(&self) -> u64 {
        self.inner
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }

    /// The master's address as the sentinels say it now, for connections of
    /// their own (the console's).
    pub(crate) async fn resolve(&self) -> Result<(String, u16)> {
        let inner = &self.inner;
        resolve(&inner.master, &inner.sentinels, &inner.sentinel_config).await
    }

    fn current(&self) -> (ConnectionManager, u64) {
        let mut current = self.inner.current.lock().unwrap_or_else(|e| e.into_inner());
        current.last_used = Instant::now();
        (current.con.clone(), current.generation)
    }

    /// Before a write: a connection that has been idle for a while is
    /// checked with a PING (which may be sent again), so one that Sentinel
    /// closed during a failover is replaced before the write goes out.
    async fn check_if_idle(&self) {
        let idle = {
            let current = self.inner.current.lock().unwrap_or_else(|e| e.into_inner());
            current.last_used.elapsed()
        };
        if idle < IDLE_CHECK {
            return;
        }
        let ping = cmd("PING");
        let ping = &ping;
        let checked = self
            .request(
                false,
                |mut con| Box::pin(async move { con.req_packed_command(ping).await }),
                command_sign,
            )
            .await;
        if let Err(err) = checked {
            tracing::debug!("sentinel: the connection check failed: {err}");
        }
    }

    /// After a request on `generation` failed at `failed_at`: switches to
    /// the master if it moved, else waits before returning, so the
    /// caller's retry is spaced out. Concurrent requests wait side by side,
    /// not one after another, and ask the sentinels once between them.
    async fn switch(&self, generation: u64, attempt: u32, failed_at: Instant) {
        if !self.follow(generation, failed_at).await {
            tokio::time::sleep(FIRST_PAUSE * 2u32.pow(attempt)).await;
        }
    }

    /// Re-resolves the master, one request at a time, and switches to it if
    /// it moved. Returns whether the request should be sent again at once:
    /// the master moved, here or in a request this one waited for.
    async fn follow(&self, generation: u64, failed_at: Instant) -> bool {
        let mut last_asked = self.inner.switching.lock().await;
        let (seen, address) = {
            let current = self.inner.current.lock().unwrap_or_else(|e| e.into_inner());
            (current.generation, current.address.clone())
        };
        if seen != generation {
            // Another request switched while this one waited.
            return true;
        }
        if last_asked.is_some_and(|asked| asked >= failed_at) {
            // Another request asked after this one failed, and the master
            // had not moved: asking again would tell nothing new.
            return false;
        }
        *last_asked = Some(Instant::now());
        let resolved = match self.resolve().await {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!("sentinel: {err:#}");
                return false;
            }
        };
        if resolved == address {
            // Sentinel has not switched yet (or the old master is back).
            return false;
        }
        match open_master(&self.inner.config, &self.inner.master, &resolved).await {
            Ok(con) => {
                tracing::info!(
                    "sentinel: master {} moved from {} to {}",
                    self.inner.master,
                    format_address(&address.0, address.1),
                    format_address(&resolved.0, resolved.1)
                );
                let mut current = self.inner.current.lock().unwrap_or_else(|e| e.into_inner());
                *current = Current {
                    con,
                    address: resolved,
                    generation: generation + 1,
                    last_used: Instant::now(),
                };
                true
            }
            Err(err) => {
                tracing::warn!("sentinel: {err:#}");
                false
            }
        }
    }

    /// Sends a request, following the master if it moved (see the module
    /// documentation).
    async fn request<'a, T>(
        &self,
        write: bool,
        mut send: impl FnMut(ConnectionManager) -> BoxFuture<'a, RedisResult<T>>,
        sign: fn(&RedisResult<T>) -> Option<Sign>,
    ) -> RedisResult<T> {
        let mut attempt = 0;
        loop {
            let (con, generation) = self.current();
            let result = send(con).await;
            let failed_at = Instant::now();
            let retry = match sign(&result) {
                None => return result,
                Some(Sign::Refused) => true,
                Some(Sign::Dropped) => !write,
            };
            if attempt == MAX_RETRIES {
                return result;
            }
            self.switch(generation, attempt, failed_at).await;
            if !retry {
                return result.map_err(|err| {
                    RedisError::from((
                        ErrorKind::Io,
                        "the connection to the master broke while a write was on its way, so it may or may not have run; check, then try again",
                        err.to_string(),
                    ))
                });
            }
            attempt += 1;
        }
    }
}

impl ConnectionLike for Sentinel {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        let write = is_write(cmd);
        Box::pin(async move {
            if write {
                self.check_if_idle().await;
            }
            self.request(
                write,
                |mut con| Box::pin(async move { con.req_packed_command(cmd).await }),
                command_sign,
            )
            .await
        })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        let write = pipeline.cmd_iter().any(is_write);
        Box::pin(async move {
            if write {
                self.check_if_idle().await;
            }
            self.request(
                write,
                |mut con| {
                    Box::pin(async move { con.req_packed_commands(pipeline, offset, count).await })
                },
                pipeline_sign,
            )
            .await
        })
    }

    fn get_db(&self) -> i64 {
        i64::from(self.inner.config.db)
    }
}

/// Whether a command may change data: anything the command table does not
/// know to be a read counts.
fn is_write(cmd: &Cmd) -> bool {
    let argv: Vec<String> = cmd
        .args_iter()
        .take(2)
        .filter_map(|arg| match arg {
            redis::Arg::Simple(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        })
        .collect();
    commands::classify(&argv) != CommandClass::Read
}

fn error_sign(err: &RedisError) -> Option<Sign> {
    if matches!(err.code(), Some("READONLY" | "MASTERDOWN")) || err.is_connection_refusal() {
        Some(Sign::Refused)
    } else if err.is_io_error()
        || err.is_connection_dropped()
        || err.is_unrecoverable_error()
        || err.is_timeout()
    {
        Some(Sign::Dropped)
    } else {
        None
    }
}

fn reply_sign(value: &Value) -> Option<Sign> {
    match value {
        Value::ServerError(err) if matches!(err.code(), "READONLY" | "MASTERDOWN") => {
            Some(Sign::Refused)
        }
        _ => None,
    }
}

fn command_sign(result: &RedisResult<Value>) -> Option<Sign> {
    match result {
        Ok(value) => reply_sign(value),
        Err(err) => error_sign(err),
    }
}

fn pipeline_sign(result: &RedisResult<Vec<Value>>) -> Option<Sign> {
    match result {
        Ok(values) => values.iter().find_map(reply_sign),
        Err(err) => error_sign(err),
    }
}

/// The settings the sentinels of `config` are asked with: their own
/// username and password, never the master's (those go to the master, see
/// [`open_master`]); no database, since sentinels have none; and TLS only
/// with `sentinel_tls`, with the connection's TLS files (or the system's
/// trusted certificates without any).
fn sentinel_settings(config: &ConnectionConfig) -> ConnectionConfig {
    let (username, tls) = match &config.topology {
        Topology::Sentinel {
            sentinel_username,
            sentinel_tls,
            ..
        } => (sentinel_username.clone(), *sentinel_tls),
        _ => (None, false),
    };
    ConnectionConfig {
        username,
        password: config.sentinel_password.clone(),
        db: 0,
        tls: tls.then(|| config.tls.clone().unwrap_or_default()),
        ..ConnectionConfig::default()
    }
}

/// The address the sentinels give for `master`, asking each in turn.
async fn resolve(
    master: &str,
    sentinels: &[(String, u16)],
    settings: &ConnectionConfig,
) -> Result<(String, u16)> {
    let mut problems = Vec::new();
    for (host, port) in sentinels {
        let at = format_address(host, *port);
        match tokio::time::timeout(SENTINEL_TIMEOUT, ask(master, host, *port, settings)).await {
            Ok(Ok(Some(address))) => return Ok(address),
            Ok(Ok(None)) => problems.push(format!("{at} knows no master named {master:?}")),
            Ok(Err(err)) => problems.push(format!("{at}: {err:#}")),
            Err(_) => problems.push(format!(
                "{at}: no answer within {} s",
                SENTINEL_TIMEOUT.as_secs()
            )),
        }
    }
    bail!(
        "no sentinel could say where the master {master:?} is: {}",
        problems.join("; ")
    )
}

async fn ask(
    master: &str,
    host: &str,
    port: u16,
    settings: &ConnectionConfig,
) -> Result<Option<(String, u16)>> {
    let client = settings.client_for(Target::Tcp(host, port))?;
    let mut con = client
        .get_multiplexed_async_connection_with_config(
            &AsyncConnectionConfig::new()
                .set_connection_timeout(Some(SENTINEL_TIMEOUT))
                .set_response_timeout(Some(SENTINEL_TIMEOUT)),
        )
        .await?;
    let address: Option<(String, u16)> = cmd("SENTINEL")
        .arg("GET-MASTER-ADDR-BY-NAME")
        .arg(master)
        .query_async(&mut con)
        .await?;
    Ok(address)
}

/// The resolved address is not (or no longer) a master.
#[derive(Debug)]
struct NotMaster(String);

impl std::fmt::Display for NotMaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Sentinel named {} as the master, but it is a replica; a failover may be under way",
            self.0
        )
    }
}

impl std::error::Error for NotMaster {}

/// Connects to `address` and makes sure it is the master.
async fn open_master(
    config: &ConnectionConfig,
    master: &str,
    address: &(String, u16),
) -> Result<ConnectionManager> {
    let at = format_address(&address.0, address.1);
    let target = format!("the master {master} at {at}");
    let client = config.client_for(Target::Tcp(&address.0, address.1))?;
    let settings = ConnectionManagerConfig::new()
        .set_connection_timeout(Some(Duration::from_secs(5)))
        .set_response_timeout(Some(Duration::from_secs(30)))
        .set_number_of_retries(1);
    let mut con = ConnectionManager::new_with_config(client, settings)
        .await
        .map_err(|err| connect_error(err, config, &target))?;
    // Also asks for the password the way PING does in `Client::connect`.
    match cmd("ROLE").query_async::<Value>(&mut con).await {
        Ok(Value::Array(role)) => {
            let is_master = matches!(role.first(), Some(Value::BulkString(r)) if r == b"master");
            if !is_master {
                return Err(NotMaster(at).into());
            }
        }
        Ok(_) => {}
        // A user that may not run ROLE is still logged in.
        Err(err) if err.code() == Some("NOPERM") => {}
        Err(err) => return Err(connect_error(err, config, &target)),
    }
    Ok(con)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(line: &str) -> Cmd {
        let mut words = line.split_whitespace();
        let mut command = cmd(words.next().unwrap());
        for word in words {
            command.arg(word);
        }
        command
    }

    #[test]
    fn only_reads_count_as_safe_to_repeat() {
        for read in [
            "GET k",
            "PING",
            "SCAN 0",
            "TYPE k",
            "INFO",
            "MEMORY USAGE k",
            "XRANGE s - +",
        ] {
            assert!(!is_write(&command(read)), "{read}");
        }
        for write in [
            "SET k v",
            "INCR k",
            "EVAL x 0",
            "UNLINK k",
            "FROBNICATE",
            "MULTI",
            "CONFIG SET a b",
        ] {
            assert!(is_write(&command(write)), "{write}");
        }
    }

    #[test]
    fn sentinels_get_their_own_credentials() {
        use crate::config::TlsConfig;

        let ca = TlsConfig {
            ca_file: Some("/ca.pem".into()),
            ..TlsConfig::default()
        };
        let topology = |sentinel_username: Option<&str>, sentinel_tls| Topology::Sentinel {
            master: "m".into(),
            sentinels: vec!["s:26379".into()],
            sentinel_username: sentinel_username.map(str::to_string),
            sentinel_tls,
        };
        let config = ConnectionConfig {
            username: Some("app".into()),
            password: Some("redis-secret".into()),
            db: 4,
            tls: Some(ca.clone()),
            sentinel_password: Some("sentinel-secret".into()),
            topology: topology(Some("watcher"), false),
            ..ConnectionConfig::default()
        };
        let settings = sentinel_settings(&config);
        assert_eq!(settings.username.as_deref(), Some("watcher"));
        assert_eq!(settings.password.as_deref(), Some("sentinel-secret"));
        assert_eq!((settings.db, settings.tls.clone()), (0, None));

        // TLS for the sentinels only when asked, with the master's files.
        let settings = sentinel_settings(&ConnectionConfig {
            topology: topology(Some("watcher"), true),
            ..config.clone()
        });
        assert_eq!(settings.tls, Some(ca));
        let settings = sentinel_settings(&ConnectionConfig {
            tls: None,
            topology: topology(None, true),
            ..config.clone()
        });
        assert_eq!(settings.tls, Some(TlsConfig::default()));

        // Without credentials of their own, the sentinels get none, never
        // the master's.
        let settings = sentinel_settings(&ConnectionConfig {
            sentinel_password: None,
            topology: topology(None, false),
            ..config
        });
        assert_eq!((settings.username, settings.password), (None, None));
    }

    #[test]
    fn failover_signs() {
        let server = |line: &[u8]| match redis::parse_redis_value(line).unwrap() {
            Value::ServerError(err) => err,
            other => panic!("{other:?}"),
        };
        let readonly = server(b"-READONLY You can't write against a read only replica.\r\n");
        let masterdown = server(b"-MASTERDOWN Link with MASTER is down\r\n");
        let other = server(b"-ERR wrong number of arguments\r\n");
        assert_eq!(
            reply_sign(&Value::ServerError(readonly.clone())),
            Some(Sign::Refused)
        );
        assert_eq!(
            reply_sign(&Value::ServerError(masterdown.clone())),
            Some(Sign::Refused)
        );
        assert_eq!(reply_sign(&Value::ServerError(other.clone())), None);
        assert_eq!(command_sign(&Ok(Value::Okay)), None);
        assert_eq!(
            command_sign(&Err(RedisError::from(readonly.clone()))),
            Some(Sign::Refused)
        );
        assert_eq!(command_sign(&Err(RedisError::from(other.clone()))), None);
        let refused = RedisError::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert_eq!(error_sign(&refused), Some(Sign::Refused));
        let broken = RedisError::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert_eq!(error_sign(&broken), Some(Sign::Dropped));
        assert_eq!(
            pipeline_sign(&Ok(vec![Value::Okay, Value::ServerError(readonly)])),
            Some(Sign::Refused)
        );
        assert_eq!(pipeline_sign(&Ok(vec![Value::ServerError(other)])), None);
    }
}
