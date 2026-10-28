//! Sentinel (26410 watching `mymaster` on 6410 / 6411): finding the
//! master, following a failover with the same client, and what goes wrong
//! with wrong names or unreachable sentinels.
//!
//! Only lane A uses this service, and the failover test switches the
//! master, so the tests that use it run one at a time, also across test
//! processes (see [`serial`]), and tolerate either node being the master
//! when they start. The failover test switches back to 6410 at the end,
//! leaving the service as it started (its health check accepts either
//! node, but containers started from older checkouts expect 6410). The
//! tests with a sentinel of their own do not touch the service.

#[macro_use]
mod common;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};

use common::*;
use redcrab_core::{
    Client, ConnectionConfig, Cursor, DeleteProgress, Key, KeyType, RawValue, ScanEvent,
    ScanOptions, ScanProgress, Topology, ValueData, viewer,
};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{TcpListener, TcpStream, tcp::OwnedReadHalf},
    runtime::Runtime,
};

/// The tests here share one master; the failover test moves it.
static SERIAL: Mutex<()> = Mutex::new(());

/// Held while a test uses the shared Sentinel service.
struct Serial {
    _file: std::fs::File,
    _guard: std::sync::MutexGuard<'static, ()>,
}

/// Waits until no other test uses the shared Sentinel service: in this
/// process, and in every other one on this machine (a second `cargo test`,
/// another checkout), whose failover would otherwise move the master
/// between a test's look at it and its assertion. Taken before the test's
/// `It`, so its keys are cleaned up before the next test starts. The
/// operating system releases the file lock if a test process dies.
fn serial() -> Serial {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let path = std::env::temp_dir().join("redcrab-it-sentinel.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    file.lock()
        .unwrap_or_else(|err| panic!("locking {}: {err}", path.display()));
    Serial {
        _file: file,
        _guard: guard,
    }
}

fn with_sentinels(master: &str, sentinels: &[&str]) -> ConnectionConfig {
    ConnectionConfig {
        topology: Topology::Sentinel {
            master: master.into(),
            sentinels: sentinels.iter().map(|s| s.to_string()).collect(),
            sentinel_username: None,
            sentinel_tls: false,
        },
        ..sentinel()
    }
}

fn get(client: &Client, key: &Key) -> Option<Vec<u8>> {
    match block_on(client.load(key.clone(), KeyType::String, Cursor::Start, 1))
        .unwrap()
        .data
    {
        ValueData::String(value) => Some(value),
        _ => None,
    }
}

fn sentinel_node() -> redis::Connection {
    node(SENTINEL_PORT)
}

/// Asks Sentinel to fail over, retrying while it is not ready for one (a
/// replica that is still syncing after an earlier failover).
fn fail_over() {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let reply: redis::RedisResult<()> = redis::cmd("SENTINEL")
            .arg("FAILOVER")
            .arg(SENTINEL_MASTER)
            .query(&mut sentinel_node());
        match reply {
            Ok(()) => return,
            Err(err) if Instant::now() < deadline => {
                eprintln!("SENTINEL FAILOVER: {err}; trying again");
                sleep(Duration::from_millis(500));
            }
            Err(err) => panic!("SENTINEL FAILOVER kept failing: {err}"),
        }
    }
}

/// Waits until Sentinel names `port` as the master, the node there says it
/// is the master, the other one has become its replica, and the replica's
/// link is up.
fn wait_for_master(port: u16) {
    let other = SENTINEL_NODES.into_iter().find(|&p| p != port).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let named: Option<(String, u16)> = redis::cmd("SENTINEL")
            .arg("GET-MASTER-ADDR-BY-NAME")
            .arg(SENTINEL_MASTER)
            .query(&mut sentinel_node())
            .unwrap();
        let link_up = || {
            let info: String = redis::cmd("INFO")
                .arg("replication")
                .query(&mut node(other))
                .unwrap();
            info.lines()
                .any(|line| line.trim() == "master_link_status:up")
        };
        if named.is_some_and(|(_, p)| p == port)
            && role(&mut node(port)) == "master"
            && role(&mut node(other)) == "slave"
            && link_up()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Sentinel did not switch to {port} in time"
        );
        sleep(Duration::from_millis(250));
    }
}

#[test]
fn connects_to_the_master_sentinel_names() {
    let _serial = serial();
    let Some(t) = it!() else { return };
    let master = sentinel_master();
    let client = t.connect(sentinel());
    assert_eq!(
        client.topology_summary(),
        [format!("sentinel · mymaster @ 127.0.0.1:{master}")]
    );
    assert!(client.topology().supports_databases());
    // Where a delete would go: the master, not the unused host and port.
    assert!(
        client.location().ends_with(&format!(
            " · sentinel mymaster @ 127.0.0.1:{master} · db{}",
            client.config().db
        )),
        "{}",
        client.location()
    );

    let key = t.bkey("hello");
    put_string(&client, &key, b"world \xff");
    let mut seed = t.seed_sentinel();
    let stored: Vec<u8> = redis::cmd("GET").arg(&*key).query(&mut seed).unwrap();
    assert_eq!(stored, b"world \xff");
    let metas = block_on(client.key_meta(vec![key.clone()])).unwrap();
    assert_eq!(metas[0].ty, KeyType::String);

    let info = block_on(client.server_info()).unwrap();
    assert!(info.version.is_some());
    block_on(client.ping()).unwrap();

    // The console talks to the master too.
    let console = client.console();
    assert_eq!(
        ok(&console, &format!("GET {}", t.key("hello"))),
        bulk(b"world \xff")
    );
    assert_eq!(
        ok(&console, &format!("SET {} 2", t.key("c"))),
        RawValue::Okay
    );
    assert_eq!(
        ok(&console, "ROLE").as_sequence().map(|r| r[0].clone()),
        Some(bulk("master"))
    );
}

#[test]
fn the_configured_database_is_used() {
    let _serial = serial();
    let Some(t) = it!() else { return };
    t.uses(Server::Sentinel, 7);
    let client = t.connect(ConnectionConfig {
        db: 7,
        ..sentinel()
    });
    let key = t.bkey("in-db-7");
    put_string(&client, &key, b"x");
    let mut db7 = node(sentinel_master());
    redis::cmd("SELECT").arg(7).query::<()>(&mut db7).unwrap();
    let exists: bool = redis::cmd("EXISTS").arg(&*key).query(&mut db7).unwrap();
    assert!(exists);
    let console = client.console();
    assert_eq!(console.db(), 7);
    let mut db0 = node(sentinel_master());
    let exists: bool = redis::cmd("EXISTS").arg(&*key).query(&mut db0).unwrap();
    assert!(!exists);
    redis::cmd("UNLINK")
        .arg(&*key)
        .query::<()>(&mut db7)
        .unwrap();
}

#[test]
fn the_next_sentinel_is_asked_when_one_is_down() {
    let _serial = serial();
    let Some(t) = it!() else { return };
    let client = t.connect(with_sentinels(
        SENTINEL_MASTER,
        &["127.0.0.1:1", &format!("127.0.0.1:{SENTINEL_PORT}")],
    ));
    block_on(client.ping()).unwrap();
}

#[test]
fn wrong_names_and_unreachable_sentinels_are_explained() {
    let _serial = serial();
    let Some(_t) = it!() else { return };
    let err = connect_error(with_sentinels(
        "nosuchmaster",
        &[&format!("127.0.0.1:{SENTINEL_PORT}")],
    ));
    assert!(
        err.contains(&format!(
            "127.0.0.1:{SENTINEL_PORT} knows no master named \"nosuchmaster\""
        )),
        "{err}"
    );
    let err = connect_error(with_sentinels(
        SENTINEL_MASTER,
        &["127.0.0.1:1", "127.0.0.1:2"],
    ));
    assert!(
        err.starts_with("no sentinel could say where the master \"mymaster\" is: 127.0.0.1:1: "),
        "{err}"
    );
    assert!(err.contains("; 127.0.0.1:2: "), "{err}");
    // A data node is no sentinel.
    let err = connect_error(with_sentinels(SENTINEL_MASTER, &["127.0.0.1:6400"]));
    assert!(err.contains("127.0.0.1:6400: "), "{err}");
    let err = connect_error(with_sentinels(SENTINEL_MASTER, &[]));
    assert!(err.starts_with("add at least one sentinel"), "{err}");
}

#[test]
fn read_only_masters_refuse_changes() {
    let _serial = serial();
    let Some(t) = it!() else { return };
    let client = t.connect(ConnectionConfig {
        read_only: true,
        ..sentinel()
    });
    let key = t.bkey("ro");
    assert!(is_read_only(
        &block_on(client.set_string(key.clone(), b"x".to_vec())).unwrap_err()
    ));
    assert!(is_read_only(
        &block_on(client.delete(vec![key.clone()])).unwrap_err()
    ));
    let console = client.console();
    assert!(is_read_only(
        &run(&console, &format!("SET {} x", t.key("ro"))).unwrap_err()
    ));
    assert_eq!(
        ok(&console, &format!("EXISTS {}", t.key("ro"))),
        RawValue::Int(0)
    );
}

#[test]
fn the_same_client_follows_a_failover() {
    let _serial = serial();
    let Some(t) = it!() else { return };
    let before = sentinel_master();
    let after = SENTINEL_NODES.into_iter().find(|&p| p != before).unwrap();
    // A replica that is still syncing from an earlier run cannot take over.
    wait_for_master(before);

    let client = t.connect(sentinel());
    let console = client.console();
    let first = t.bkey("before-failover");
    put_string(&client, &first, b"1");
    assert_eq!(get(&client, &first).as_deref(), Some(&b"1"[..]));
    assert_eq!(
        ok(&console, &format!("SET {} c1", t.key("console"))),
        RawValue::Okay
    );

    let started = Instant::now();
    fail_over();
    wait_for_master(after);
    eprintln!("failover took {:.1} s", started.elapsed().as_secs_f64());

    // The same client, with no reconnect by the caller.
    let second = t.bkey("after-failover");
    put_string(&client, &second, b"2");
    assert_eq!(get(&client, &second).as_deref(), Some(&b"2"[..]));
    assert_eq!(
        get(&client, &first).as_deref(),
        Some(&b"1"[..]),
        "replicated before the failover"
    );
    assert_eq!(
        client.topology_summary(),
        [format!("sentinel · mymaster @ 127.0.0.1:{after}")]
    );
    let mut on_new_master = node(after);
    let stored: Vec<u8> = redis::cmd("GET")
        .arg(&*second)
        .query(&mut on_new_master)
        .unwrap();
    assert_eq!(stored, b"2");

    // The console's connection broke or now reaches a replica: that
    // command fails with a hint, and the next one goes to the new master.
    let line = format!("SET {} c2", t.key("console"));
    if let Err(err) = run(&console, &line) {
        eprintln!("the console's first command after the failover: {err:#}");
        assert_eq!(ok(&console, &line), RawValue::Okay);
    }
    assert_eq!(
        ok(&console, &format!("GET {}", t.key("console"))),
        bulk("c2")
    );
    // A console opened now starts on the new master.
    let fresh = client.console();
    assert_eq!(
        ok(&fresh, "ROLE").as_sequence().map(|r| r[0].clone()),
        Some(bulk("master"))
    );

    // Leave 6410 as the master, as the service started.
    if after != 6410 {
        fail_over();
        wait_for_master(6410);
        put_string(&client, &second, b"3");
        assert_eq!(get(&client, &second).as_deref(), Some(&b"3"[..]));
    }
}

/// A runtime of the test's own for the servers it plays. Stopping it
/// closes their listeners and connections.
fn server_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// A sentinel of the test's own, so that what it is sent can be checked
/// without touching the shared one: it names 127.0.0.1:`master` for any
/// name (until [`Self::master`] is changed), wants `AUTH user password`
/// first when `auth` says so, and records every AUTH it gets.
struct FakeSentinel {
    port: u16,
    auths: Arc<Mutex<Vec<Vec<String>>>>,
    /// The port it names as the master's.
    master: Arc<AtomicU16>,
    runtime: Option<Runtime>,
}

impl FakeSentinel {
    fn start(master: u16, auth: Option<(&str, &str)>) -> Self {
        let runtime = server_runtime();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        let auths = Arc::new(Mutex::new(Vec::new()));
        let master = Arc::new(AtomicU16::new(master));
        let required: Option<Vec<String>> =
            auth.map(|(user, password)| vec![user.into(), password.into()]);
        runtime.spawn({
            let auths = auths.clone();
            let master = master.clone();
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let named = master.load(Ordering::SeqCst);
                    tokio::spawn(answer(socket, named, required.clone(), auths.clone()));
                }
            }
        });
        Self {
            port,
            auths,
            master,
            runtime: Some(runtime),
        }
    }

    /// `base` with this sentinel as its only one.
    fn config(&self, base: ConnectionConfig, username: Option<&str>) -> ConnectionConfig {
        ConnectionConfig {
            topology: Topology::Sentinel {
                master: SENTINEL_MASTER.into(),
                sentinels: vec![format!("127.0.0.1:{}", self.port)],
                sentinel_username: username.map(str::to_string),
                sentinel_tls: false,
            },
            ..base
        }
    }

    fn auths(&self) -> Vec<Vec<String>> {
        self.auths.lock().unwrap().clone()
    }
}

impl Drop for FakeSentinel {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

/// One RESP command, or `None` at the end of the stream.
async fn read_command(read: &mut BufReader<OwnedReadHalf>) -> Option<Vec<String>> {
    let mut line = String::new();
    read.read_line(&mut line).await.ok()?;
    let count: usize = line.trim().strip_prefix('*')?.parse().ok()?;
    let mut argv = Vec::with_capacity(count);
    for _ in 0..count {
        line.clear();
        read.read_line(&mut line).await.ok()?;
        let len: usize = line.trim().strip_prefix('$')?.parse().ok()?;
        let mut arg = vec![0; len + 2];
        read.read_exact(&mut arg).await.ok()?;
        arg.truncate(len);
        argv.push(String::from_utf8_lossy(&arg).into_owned());
    }
    Some(argv)
}

/// What [`FakeSentinel`] answers on one connection.
async fn answer(
    socket: TcpStream,
    master: u16,
    required: Option<Vec<String>>,
    auths: Arc<Mutex<Vec<Vec<String>>>>,
) {
    let (read, mut write) = socket.into_split();
    let mut read = BufReader::new(read);
    let mut logged_in = required.is_none();
    while let Some(argv) = read_command(&mut read).await {
        let reply = match argv[0].to_ascii_uppercase().as_str() {
            "AUTH" => {
                let given = argv[1..].to_vec();
                auths.lock().unwrap().push(given.clone());
                let given = match &given[..] {
                    [password] => vec!["default".into(), password.clone()],
                    _ => given,
                };
                if required.as_ref().is_none_or(|required| *required == given) {
                    logged_in = true;
                    "+OK\r\n".to_string()
                } else {
                    "-WRONGPASS invalid username-password pair or user is disabled.\r\n".into()
                }
            }
            "SENTINEL" if !logged_in => "-NOAUTH Authentication required.\r\n".into(),
            "SENTINEL" => {
                let port = master.to_string();
                format!("*2\r\n$9\r\n127.0.0.1\r\n${}\r\n{port}\r\n", port.len())
            }
            "PING" => "+PONG\r\n".into(),
            _ => "+OK\r\n".into(),
        };
        if write.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Forwards a port of its own to 127.0.0.1:`target` until dropped, which
/// closes the listener and every forwarded connection: a master that goes
/// down while Sentinel still names it.
struct Forward {
    port: u16,
    runtime: Option<Runtime>,
}

impl Forward {
    fn start(target: u16) -> Self {
        let runtime = server_runtime();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        runtime.spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(("127.0.0.1", target)).await {
                        let _ = tokio::io::copy_bidirectional(&mut socket, &mut server).await;
                    }
                });
            }
        });
        Self {
            port,
            runtime: Some(runtime),
        }
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

#[test]
fn sentinels_and_master_each_get_their_own_credentials() {
    let Some(_t) = it!() else { return };
    // The master is `auth` (requirepass rootpass); the sentinel has a user
    // of its own.
    let fake = FakeSentinel::start(Server::Auth.port(), Some(("watcher", "sentinel-secret")));
    let config = fake.config(
        ConnectionConfig {
            password: Some("rootpass".into()),
            sentinel_password: Some("sentinel-secret".into()),
            ..sentinel()
        },
        Some("watcher"),
    );
    let client = block_on(Client::connect(config.clone())).unwrap();
    block_on(client.ping()).unwrap();
    // The console finds the master through the sentinel too.
    assert_eq!(
        ok(&client.console(), "PING"),
        RawValue::SimpleString("PONG".into())
    );
    let auths = fake.auths();
    assert!(auths.len() >= 2, "{auths:?}");
    assert!(
        auths
            .iter()
            .all(|auth| auth == &["watcher", "sentinel-secret"]),
        "only the sentinel's own credentials reach it: {auths:?}"
    );

    // Swapped, each side refuses the other's password.
    let err = connect_error(ConnectionConfig {
        sentinel_password: Some("rootpass".into()),
        ..config.clone()
    });
    assert!(
        err.starts_with("no sentinel could say where the master \"mymaster\" is"),
        "{err}"
    );
    let err = connect_error(ConnectionConfig {
        password: Some("sentinel-secret".into()),
        ..config.clone()
    });
    assert!(err.contains("wrong username or password"), "{err}");
    // TLS for the sentinels is used when asked for: this one has none.
    let Topology::Sentinel {
        master,
        sentinels,
        sentinel_username,
        ..
    } = config.topology.clone()
    else {
        unreachable!()
    };
    let err = connect_error(ConnectionConfig {
        topology: Topology::Sentinel {
            master,
            sentinels,
            sentinel_username,
            sentinel_tls: true,
        },
        ..config
    });
    assert!(err.starts_with("no sentinel could say"), "{err}");
}

#[test]
fn requests_to_a_master_that_is_down_back_off_side_by_side() {
    let Some(_t) = it!() else { return };
    let master = Forward::start(Server::Standalone.port());
    let fake = FakeSentinel::start(master.port, None);
    let client = block_on(Client::connect(fake.config(standalone(), None))).unwrap();
    block_on(client.ping()).unwrap();

    // The master goes down and no failover follows: each request backs
    // off (about 6 s in all) and fails. Requests at the same time wait
    // side by side, not one after another.
    drop(master);
    let started = Instant::now();
    let results = block_on(futures::future::join_all((0..4).map(|_| client.ping())));
    let elapsed = started.elapsed();
    assert!(results.iter().all(Result::is_err), "{results:?}");
    assert!(
        elapsed < Duration::from_secs(12),
        "4 requests took {elapsed:?}; one alone takes about 6 s"
    );
}

/// A master of the test's own in front of the standalone server, the way
/// a Sentinel-managed master is seen by a client: it passes each command
/// on and its reply back, one at a time, and records the commands. Once
/// demoted (after answering `scans` SCAN commands, see [`Self::demote`])
/// it answers everything with READONLY, as an old master that has just
/// become a replica does, and its sentinel names the next master.
/// Where a demoted [`Relay`] points its sentinel: the sentinel's master
/// port, and the next master's.
type Handoff = Arc<Mutex<Option<(Arc<AtomicU16>, u16)>>>;

struct Relay {
    port: u16,
    log: Arc<Mutex<Vec<Vec<String>>>>,
    /// SCANs left to answer before demoting itself; 0: never.
    scans: Arc<AtomicUsize>,
    demoted: Arc<AtomicBool>,
    /// On demotion: the sentinel's master port, and the next master.
    next: Handoff,
    runtime: Option<Runtime>,
}

impl Relay {
    fn start() -> Self {
        let runtime = server_runtime();
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let (scans, demoted, next): (Arc<AtomicUsize>, Arc<AtomicBool>, Handoff) =
            Default::default();
        let shared = (log.clone(), scans.clone(), demoted.clone(), next.clone());
        runtime.spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (log, scans, demoted, next) = shared.clone();
                tokio::spawn(async move {
                    let Ok(server) =
                        TcpStream::connect(("127.0.0.1", Server::Standalone.port())).await
                    else {
                        return;
                    };
                    let (client_read, mut client_write) = socket.into_split();
                    let (server_read, mut server_write) = server.into_split();
                    let (mut client_read, mut server_read) =
                        (BufReader::new(client_read), BufReader::new(server_read));
                    while let Some(argv) = read_command(&mut client_read).await {
                        log.lock().unwrap().push(argv.clone());
                        let reply = if demoted.load(Ordering::SeqCst) {
                            b"-READONLY You can't write against a read only replica.\r\n".to_vec()
                        } else {
                            if server_write.write_all(&encode(&argv)).await.is_err() {
                                return;
                            }
                            let Some(reply) = read_reply(&mut server_read).await else {
                                return;
                            };
                            if argv[0].eq_ignore_ascii_case("SCAN")
                                && scans.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                                    n.checked_sub(1)
                                }) == Ok(1)
                            {
                                demoted.store(true, Ordering::SeqCst);
                                if let Some((sentinel, port)) = next.lock().unwrap().take() {
                                    sentinel.store(port, Ordering::SeqCst);
                                }
                            }
                            reply
                        };
                        if client_write.write_all(&reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            port,
            log,
            scans,
            demoted,
            next,
            runtime: Some(runtime),
        }
    }

    /// After answering `scans` more SCANs, refuses everything and has
    /// `sentinel` name `next` as the master: a failover in the middle of
    /// a walk.
    fn demote(&self, scans: usize, sentinel: &FakeSentinel, next: &Relay) {
        *self.next.lock().unwrap() = Some((sentinel.master.clone(), next.port));
        self.scans.store(scans, Ordering::SeqCst);
    }

    /// Right away, as [`Self::demote`].
    fn demote_now(&self, sentinel: &FakeSentinel, next: &Relay) {
        sentinel.master.store(next.port, Ordering::SeqCst);
        self.demoted.store(true, Ordering::SeqCst);
    }

    /// The cursors of the SCAN-family commands (`SCAN`, `HSCAN`, ...) it
    /// was sent, in order.
    fn cursors(&self, command: &str) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|argv| argv[0].eq_ignore_ascii_case(command))
            .map(|argv| match command {
                "SCAN" => argv[1].clone(),
                _ => argv[2].clone(),
            })
            .collect()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

fn encode(argv: &[String]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", argv.len()).into_bytes();
    for arg in argv {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// One whole RESP reply, as it came.
async fn read_reply(read: &mut BufReader<OwnedReadHalf>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pending = 1usize;
    while pending > 0 {
        pending -= 1;
        let mut line = String::new();
        if read.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        out.extend_from_slice(line.as_bytes());
        let (kind, rest) = line.split_at(1);
        let n: i64 = rest.trim().parse().unwrap_or(0);
        match kind {
            "$" | "=" | "!" if n >= 0 => {
                let mut data = vec![0; n as usize + 2];
                read.read_exact(&mut data).await.ok()?;
                out.extend_from_slice(&data);
            }
            "*" | "~" | ">" if n > 0 => pending += n as usize,
            "%" | "|" if n > 0 => pending += 2 * n as usize,
            _ => {}
        }
    }
    Some(out)
}

/// Seeds `n` small strings under the test's prefix on standalone db 0.
fn seed_many(t: &It, n: usize) {
    let mut con = t.seed0();
    let mut p = redis::pipe();
    for i in 0..n {
        p.cmd("SET").arg(t.key(&format!("k{i}"))).arg(i).ignore();
    }
    p.query::<()>(&mut con).unwrap();
}

fn matching_keys(t: &It) -> usize {
    let mut con = raw(Server::Standalone, 0);
    let mut cursor = 0u64;
    let mut found = 0;
    loop {
        let (next, keys): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(t.pattern())
            .arg("COUNT")
            .arg(1000)
            .query(&mut con)
            .unwrap();
        found += keys.len();
        if next == 0 {
            return found;
        }
        cursor = next;
    }
}

/// A walk that a failover moves to another master starts over there,
/// rather than sending the old master's cursor on: a cursor only means
/// something on the server that handed it out, and followed elsewhere it
/// skips keys and still ends "Done". The two masters here are the same
/// server behind two relays, so the cursors they were sent tell.
#[test]
fn walks_start_over_on_the_new_master_after_a_failover() {
    let Some(t) = it!() else { return };
    t.uses(Server::Standalone, 0);
    const KEYS: usize = 6000;
    seed_many(&t, KEYS);

    // Counting, with the old master refusing the third SCAN.
    let (old, new) = (Relay::start(), Relay::start());
    let fake = FakeSentinel::start(old.port, None);
    let client = block_on(Client::connect(fake.config(standalone(), None))).unwrap();
    old.demote(2, &fake, &new);
    let events = collect(client.count_matching(t.pattern()));
    match events.last() {
        Some(ScanProgress::Done { keys, .. }) => assert_eq!(*keys, KEYS as u64),
        other => panic!("{other:?}"),
    }
    let cursors = new.cursors("SCAN");
    assert!(
        cursors.iter().filter(|c| *c == "0").count() == 1 && cursors[1] == "0",
        "the count went on with the old master's cursor: {cursors:?}"
    );

    // Scanning the key list, the same way.
    let (old, new) = (Relay::start(), Relay::start());
    let fake = FakeSentinel::start(old.port, None);
    let client = block_on(Client::connect(fake.config(standalone(), None))).unwrap();
    old.demote(2, &fake, &new);
    let tree = Arc::new(Mutex::new(redcrab_core::KeyTree::new(Some(b':'))));
    let events = collect(client.scan_into(
        tree.clone(),
        ScanOptions {
            pattern: t.pattern(),
            ..ScanOptions::default()
        },
    ));
    assert_eq!(done(&events), (KEYS, false), "{:?}", events.last());
    assert!(!matches!(events.last(), Some(ScanEvent::Failed(_))));
    assert_eq!(new.cursors("SCAN")[1], "0", "{:?}", new.cursors("SCAN"));

    // Deleting: the old master refuses the UNLINKs and the SCAN of the
    // third round trip; both are sent to the new one, and the walk starts
    // over there.
    let (old, new) = (Relay::start(), Relay::start());
    let fake = FakeSentinel::start(old.port, None);
    let client = block_on(Client::connect(fake.config(standalone(), None))).unwrap();
    old.demote(2, &fake, &new);
    let events = collect(client.delete_matching(t.pattern()).unwrap());
    match events.last() {
        Some(DeleteProgress::Done { stats, .. }) => {
            assert_eq!(stats.deleted, KEYS as u64, "{stats:?}");
            assert_eq!(stats.failed, 0);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(matching_keys(&t), 0);
    assert_eq!(new.cursors("SCAN")[1], "0", "{:?}", new.cursors("SCAN"));
}

/// A hash pages with HSCAN cursors, which mean nothing on another master:
/// after a failover the next page asks for a reload instead.
#[test]
fn value_pages_do_not_follow_a_cursor_to_another_master() {
    let Some(t) = it!() else { return };
    let key = t.bkey("hash");
    let mut con = t.seed0();
    let mut p = redis::pipe();
    for i in 0..2000 {
        p.cmd("HSET")
            .arg(&*key)
            .arg(format!("f{i}"))
            .arg(i)
            .ignore();
    }
    p.query::<()>(&mut con).unwrap();

    let (old, new) = (Relay::start(), Relay::start());
    let fake = FakeSentinel::start(old.port, None);
    let client = block_on(Client::connect(fake.config(standalone(), None))).unwrap();
    let mut loaded = block_on(viewer::load(&client, key.clone())).unwrap();
    assert!(matches!(loaded.next, Some(Cursor::Scan(_))));
    // A page on the same master is fine.
    let more = block_on(loaded.load_more(&client).unwrap()).unwrap();
    assert!(loaded.apply_page(more));
    assert!(matches!(loaded.next, Some(Cursor::Scan(_))));

    // The master moves; any request on the client follows it.
    old.demote_now(&fake, &new);
    block_on(client.ping()).unwrap();
    let Err(err) = block_on(loaded.load_more(&client).unwrap()) else {
        panic!("a page from the new master");
    };
    assert!(err.to_string().contains("reload the value"), "{err:#}");
    assert!(
        new.cursors("HSCAN").is_empty(),
        "{:?}",
        new.cursors("HSCAN")
    );
    // Loaded again, it pages on the new master.
    let mut loaded = block_on(viewer::load(&client, key)).unwrap();
    while loaded.has_more() {
        let more = block_on(loaded.load_more(&client).unwrap()).unwrap();
        assert!(loaded.apply_page(more));
    }
    assert_eq!(loaded.data.len(), 2000);
}
