//! What the integration tests share: the test environment's endpoints
//! (`scripts/test-env.sh up`), the `REDCRAB_IT=1` gate, and a key prefix of
//! each test's own that is removed when the test ends.
//!
//! Several test binaries run at once against the same containers, so a test
//! only touches keys under its own prefix. The few tests that need a whole
//! database own one of the indexes below on `standalone` and say so.
//!
//! Seeding and cleaning up go through plain redis-rs connections rather than
//! [`Client`], so that neither depends on the code under test.

// Every test file is a crate of its own and uses only some of these.
#![allow(dead_code)]

use std::{
    collections::HashSet,
    io::Write as _,
    path::PathBuf,
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
pub use futures::executor::block_on;
use futures::{Stream, StreamExt as _};
use redcrab_core::{
    Client, ConnectionConfig, ConsoleSession, EditError, Key, KeyTree, NewValue, RawValue,
    ReadOnlyError, ScanEvent, ScanOptions, TlsConfig, Topology, browser,
};
use redis::{ConnectionAddr, IntoConnectionInfo as _, RedisConnectionInfo};

/// The database console tests SELECT away to. Shared: only prefixed keys.
pub const CONSOLE_DB: u32 = 5;
/// Owned by `connect::server_info_counts_the_configured_database`, which
/// flushes it.
pub const SERVER_INFO_DB: u32 = 13;
/// Owned by `read_only::nothing_reaches_the_server`, which finds the
/// read-only client's connection by it.
pub const READ_ONLY_DB: u32 = 14;
/// Owned by `read_only::console_refuses_admin_commands`: an empty database
/// for commands that would act on a whole one if the gate let them through.
pub const SCRATCH_DB: u32 = 12;
/// Owned by `analysis::a_database_is_measured_key_by_key`, which flushes it
/// and analyzes the whole of it.
pub const ANALYSIS_DB: u32 = 11;

/// Starts an integration test, or skips it unless `REDCRAB_IT=1`:
/// `let Some(t) = it!() else { return };`. The test's name comes from the
/// function it is called in.
macro_rules! it {
    () => {{
        fn here() {}
        let path = std::any::type_name_of_val(&here);
        $crate::common::It::start(path.strip_suffix("::here").unwrap_or(path))
    }};
}

/// One server of the test environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Server {
    /// 6400, no auth.
    Standalone,
    /// 6401: `requirepass rootpass`, plus the ACL user `app` / `apppass`
    /// limited to `app:*` without `@dangerous`.
    Auth,
    /// 6402, TLS only, client certificate optional.
    Tls,
    /// 6403, TLS only, client certificate required.
    Mtls,
    /// 7000-7005: three primaries with a replica each.
    Cluster,
    /// Sentinel on 26410 watching `mymaster` (6410 / 6411, either may be
    /// the master: the failover test switches them). Only lane A's tests
    /// use it.
    Sentinel,
}

impl Server {
    pub const ALL: [Self; 6] = [
        Self::Standalone,
        Self::Auth,
        Self::Tls,
        Self::Mtls,
        Self::Cluster,
        Self::Sentinel,
    ];

    pub fn port(self) -> u16 {
        match self {
            Self::Standalone => 6400,
            Self::Auth => 6401,
            Self::Tls => 6402,
            Self::Mtls => 6403,
            Self::Cluster => 7000,
            Self::Sentinel => SENTINEL_PORT,
        }
    }

    /// Settings with every permission: the root password, the CA and the
    /// client certificate, as the server needs.
    pub fn admin(self) -> ConnectionConfig {
        let config = ConnectionConfig {
            name: format!("it-{self:?}"),
            host: "127.0.0.1".into(),
            port: self.port(),
            ..ConnectionConfig::default()
        };
        match self {
            Self::Standalone => config,
            Self::Auth => ConnectionConfig {
                password: Some("rootpass".into()),
                ..config
            },
            Self::Tls => ConnectionConfig {
                tls: Some(ca()),
                ..config
            },
            Self::Mtls => ConnectionConfig {
                tls: Some(client_certificate()),
                ..config
            },
            Self::Cluster => ConnectionConfig {
                topology: Topology::Cluster {
                    seeds: vec!["127.0.0.1:7001".into(), "127.0.0.1:7002".into()],
                },
                ..config
            },
            Self::Sentinel => ConnectionConfig {
                topology: Topology::Sentinel {
                    master: SENTINEL_MASTER.into(),
                    sentinels: vec![format!("127.0.0.1:{SENTINEL_PORT}")],
                    sentinel_username: None,
                    sentinel_tls: false,
                },
                ..config
            },
        }
    }

    /// The server `config` points at, if it is one of the environment's.
    fn of(config: &ConnectionConfig) -> Option<Self> {
        if config.ssh.is_some() || config.unix_socket.is_some() {
            return None;
        }
        match &config.topology {
            Topology::Sentinel { .. } => return Some(Self::Sentinel),
            Topology::Cluster { .. } => return Some(Self::Cluster),
            Topology::Standalone => {}
        }
        let local = matches!(config.host.as_str(), "127.0.0.1" | "localhost");
        Self::ALL
            .into_iter()
            .find(|server| local && server.port() == config.port)
    }
}

/// The cluster's nodes.
pub const CLUSTER_PORTS: [u16; 6] = [7000, 7001, 7002, 7003, 7004, 7005];
/// The Sentinel test service: one sentinel watching a master and a replica.
pub const SENTINEL_PORT: u16 = 26410;
pub const SENTINEL_MASTER: &str = "mymaster";
pub const SENTINEL_NODES: [u16; 2] = [6410, 6411];

pub fn cluster() -> ConnectionConfig {
    Server::Cluster.admin()
}

pub fn sentinel() -> ConnectionConfig {
    Server::Sentinel.admin()
}

/// A plain connection to one node of the cluster or the Sentinel service.
pub fn node(port: u16) -> redis::Connection {
    redis::Client::open(format!("redis://127.0.0.1:{port}"))
        .and_then(|client| client.get_connection_with_timeout(Duration::from_secs(5)))
        .unwrap_or_else(|err| {
            panic!("could not reach 127.0.0.1:{port} ({err:#}); is scripts/test-env.sh up?")
        })
}

/// A cluster connection with every permission, routing by key.
pub fn cluster_connection() -> redis::cluster::ClusterConnection {
    redis::cluster::ClusterClient::new(vec!["redis://127.0.0.1:7000", "redis://127.0.0.1:7001"])
        .and_then(|client| client.get_connection())
        .unwrap_or_else(|err| {
            panic!("could not reach the cluster ({err:#}); is scripts/test-env.sh up?")
        })
}

/// The first word of ROLE: `master`, `slave` or `sentinel`.
pub fn role(con: &mut redis::Connection) -> String {
    let role: Vec<redis::Value> = redis::cmd("ROLE").query(con).expect("ROLE");
    match role.first() {
        Some(redis::Value::BulkString(role)) => String::from_utf8_lossy(role).into_owned(),
        other => panic!("ROLE: {other:?}"),
    }
}

/// The ports of the cluster's primaries, as they report themselves.
pub fn cluster_primaries() -> Vec<u16> {
    CLUSTER_PORTS
        .into_iter()
        .filter(|&port| role(&mut node(port)) == "master")
        .collect()
}

/// The port of the Sentinel service's current master.
pub fn sentinel_master() -> u16 {
    SENTINEL_NODES
        .into_iter()
        .find(|&port| role(&mut node(port)) == "master")
        .expect("the sentinel service has no master")
}

pub fn standalone() -> ConnectionConfig {
    Server::Standalone.admin()
}

/// The `auth` server as `username` (the default user when `None`).
pub fn auth(username: Option<&str>, password: Option<&str>) -> ConnectionConfig {
    ConnectionConfig {
        username: username.map(str::to_string),
        password: password.map(str::to_string),
        ..Server::Auth.admin()
    }
}

/// The ACL user that may only touch `app:*` and has no `@dangerous`
/// commands (no INFO, KEYS, CONFIG, CLIENT, FLUSHALL, ...).
pub fn app_user() -> ConnectionConfig {
    auth(Some("app"), Some("apppass"))
}

pub fn tls(tls: TlsConfig) -> ConnectionConfig {
    ConnectionConfig {
        tls: Some(tls),
        ..Server::Tls.admin()
    }
}

pub fn mtls(tls: TlsConfig) -> ConnectionConfig {
    ConnectionConfig {
        tls: Some(tls),
        ..Server::Mtls.admin()
    }
}

/// Trusts the environment's CA.
pub fn ca() -> TlsConfig {
    TlsConfig {
        ca_file: Some(secret("ca.crt")),
        ..TlsConfig::default()
    }
}

/// Trusts the environment's CA and presents its client certificate.
pub fn client_certificate() -> TlsConfig {
    TlsConfig {
        client_cert_file: Some(secret("client.crt")),
        client_key_file: Some(secret("client.key")),
        ..ca()
    }
}

/// A file `scripts/test-env.sh up` generated: in `$REDCRAB_IT_DIR` (set by
/// `eval "$(scripts/test-env.sh env)"`) or else where the script puts them.
pub fn secret(name: &str) -> PathBuf {
    let dir = std::env::var_os("REDCRAB_IT_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/docker/.generated"),
        PathBuf::from,
    );
    dir.join(name)
}

/// Whether integration tests run at all.
pub fn enabled() -> bool {
    std::env::var("REDCRAB_IT").is_ok_and(|v| v == "1")
}

/// A running integration test: its name, a key prefix of its own
/// (`it:{test}:{pid}:{n}:`), and the servers and databases it wrote to.
/// Dropping it removes every key under the prefix from each of them.
pub struct It {
    /// `crate::function`, for messages.
    pub name: String,
    prefix: String,
    used: Mutex<Vec<(Server, u32)>>,
}

impl It {
    /// `None`, after saying why, unless `REDCRAB_IT=1`. Called by `it!()`.
    pub fn start(path: &str) -> Option<Self> {
        if !enabled() {
            // The test harness captures what a passing test prints, so say it
            // once per binary on stderr directly, where it is always seen.
            static SAID: Once = Once::new();
            SAID.call_once(|| {
                let suite = path.split("::").next().unwrap_or(path);
                let _ = writeln!(
                    std::io::stderr(),
                    "skipped the `{suite}` integration tests: run scripts/test-env.sh up, then REDCRAB_IT=1 cargo test"
                );
            });
            eprintln!("skipped {path}: REDCRAB_IT is not 1");
            return None;
        }
        let test = path.rsplit("::").next().unwrap_or(path);
        Some(Self {
            name: path.to_string(),
            prefix: unique_prefix("", test),
            used: Mutex::new(Vec::new()),
        })
    }

    /// Another prefix for the same test that starts with `namespace`, for
    /// an ACL user limited to keys such as `app:*`. It cleans up on its own.
    pub fn under(&self, namespace: &str) -> Self {
        let test = self.name.rsplit("::").next().unwrap_or(&self.name);
        Self {
            name: self.name.clone(),
            prefix: unique_prefix(namespace, test),
            used: Mutex::new(Vec::new()),
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// `name` under the prefix.
    pub fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// `name` (any bytes) under the prefix, as the client takes keys.
    pub fn bkey(&self, name: impl AsRef<[u8]>) -> Key {
        let mut key = self.prefix.as_bytes().to_vec();
        key.extend_from_slice(name.as_ref());
        key.into()
    }

    /// A MATCH pattern for every key under the prefix.
    pub fn pattern(&self) -> String {
        self.pattern_for(b"")
    }

    /// A MATCH pattern for keys under the prefix followed by the literal
    /// `name`, then anything.
    pub fn pattern_for(&self, name: &[u8]) -> String {
        let mut literal = self.prefix.as_bytes().to_vec();
        literal.extend_from_slice(name);
        let mut pattern = browser::escape_glob(&literal);
        pattern.push(b'*');
        String::from_utf8(pattern).expect("prefixes are UTF-8")
    }

    /// Records that keys under the prefix may be written to `db` on
    /// `server`, so that they are removed at the end.
    pub fn uses(&self, server: Server, db: u32) {
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if !used.contains(&(server, db)) {
            used.push((server, db));
        }
    }

    /// Connects the client under test, and cleans up where it points.
    pub fn connect(&self, config: ConnectionConfig) -> Client {
        if let Some(server) = Server::of(&config) {
            self.uses(server, config.db);
        }
        let address = config.address();
        block_on(Client::connect(config)).unwrap_or_else(|err| {
            panic!("could not connect to {address} ({err:#}); is scripts/test-env.sh up?")
        })
    }

    /// A plain connection with every permission, to seed data with.
    pub fn seed(&self, server: Server, db: u32) -> redis::Connection {
        self.uses(server, db);
        raw(server, db)
    }

    /// [`Self::seed`] on `standalone`, database 0.
    pub fn seed0(&self) -> redis::Connection {
        self.seed(Server::Standalone, 0)
    }

    /// A cluster connection with every permission, routing by key.
    pub fn seed_cluster(&self) -> redis::cluster::ClusterConnection {
        self.uses(Server::Cluster, 0);
        cluster_connection()
    }

    /// A plain connection to the Sentinel service's current master.
    pub fn seed_sentinel(&self) -> redis::Connection {
        self.uses(Server::Sentinel, 0);
        node(sentinel_master())
    }
}

impl Drop for It {
    fn drop(&mut self) {
        let used = std::mem::take(self.used.get_mut().unwrap_or_else(|e| e.into_inner()));
        let pattern = self.pattern();
        for (server, db) in used {
            // Never panic here: the test may already be unwinding.
            if let Err(err) = remove_matching(server, db, &pattern) {
                eprintln!(
                    "{}: could not clean up {server:?} db {db}: {err:#}",
                    self.name
                );
            }
        }
    }
}

fn unique_prefix(namespace: &str, test: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{namespace}it:{test}:{}:{n}:", std::process::id())
}

fn remove_matching(server: Server, db: u32, pattern: &str) -> Result<()> {
    let ports = match server {
        // Each primary holds its own keys; one UNLINK per key, since keys
        // in different slots cannot share one.
        Server::Cluster => cluster_primaries(),
        Server::Sentinel => vec![sentinel_master()],
        _ => {
            let mut con =
                raw_client(server, db)?.get_connection_with_timeout(Duration::from_secs(5))?;
            return remove_from(&mut con, pattern, false);
        }
    };
    for port in ports {
        remove_from(&mut node(port), pattern, server == Server::Cluster)?;
    }
    Ok(())
}

fn remove_from(con: &mut redis::Connection, pattern: &str, per_key: bool) -> Result<()> {
    let mut cursor = 0u64;
    loop {
        let (next, keys): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(1000)
            .query(con)?;
        if per_key {
            if !keys.is_empty() {
                let mut p = redis::pipe();
                for key in &keys {
                    p.cmd("UNLINK").arg(key).ignore();
                }
                p.query::<()>(con)?;
            }
        } else {
            for chunk in keys.chunks(500) {
                redis::cmd("UNLINK").arg(chunk).query::<()>(con)?;
            }
        }
        if next == 0 {
            return Ok(());
        }
        cursor = next;
    }
}

/// A plain redis-rs connection to `db` with every permission.
pub fn raw(server: Server, db: u32) -> redis::Connection {
    raw_client(server, db)
        .and_then(|client| Ok(client.get_connection_with_timeout(Duration::from_secs(5))?))
        .unwrap_or_else(|err| {
            panic!("could not reach {server:?} ({err:#}); is scripts/test-env.sh up?")
        })
}

fn raw_client(server: Server, db: u32) -> Result<redis::Client> {
    let config = server.admin();
    let mut settings = RedisConnectionInfo::default().set_db(db.into());
    if let Some(password) = &config.password {
        settings = settings.set_password(password);
    }
    let Some(tls) = &config.tls else {
        let addr = ConnectionAddr::Tcp(config.host.clone(), config.port);
        return Ok(redis::Client::open(
            addr.into_connection_info()?.set_redis_settings(settings),
        )?);
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let read = |path: &Option<PathBuf>| -> Result<Option<Vec<u8>>> {
        path.as_ref()
            .map(|path| std::fs::read(path).with_context(|| format!("{}", path.display())))
            .transpose()
    };
    let client_tls = match (read(&tls.client_cert_file)?, read(&tls.client_key_file)?) {
        (Some(client_cert), Some(client_key)) => Some(redis::ClientTlsConfig {
            client_cert,
            client_key,
        }),
        _ => None,
    };
    let addr = ConnectionAddr::TcpTls {
        host: config.host.clone(),
        port: config.port,
        insecure: false,
        tls_params: None,
    };
    let info = addr.into_connection_info()?.set_redis_settings(settings);
    let certificates = redis::TlsCertificates {
        client_tls,
        root_cert: read(&tls.ca_file)?,
    };
    Ok(redis::Client::build_with_tls(info, certificates)?)
}

/// Writes a string through `client`, creating the key when it is new:
/// `set_string` only changes a string that exists, `create_key` only makes
/// a key that does not.
pub fn put_string(client: &Client, key: &Key, value: impl Into<Vec<u8>>) {
    let value = value.into();
    match block_on(client.create_key(key.clone(), NewValue::String(value.clone()), None)) {
        Ok(()) => {}
        Err(err) if err.downcast_ref::<EditError>() == Some(&EditError::KeyExists) => {
            block_on(client.set_string(key.clone(), value)).unwrap_or_else(|err| {
                panic!("could not write {}: {err:#}", String::from_utf8_lossy(key))
            });
        }
        Err(err) => panic!("could not create {}: {err:#}", String::from_utf8_lossy(key)),
    }
}

/// `Client::connect`'s error as `{:#}` shows it (the whole chain).
pub fn connect_error(config: ConnectionConfig) -> String {
    match block_on(Client::connect(config)) {
        Ok(_) => panic!("the connection was expected to fail"),
        Err(err) => format!("{err:#}"),
    }
}

/// Runs [`Client::scan_into`] on a fresh `:`-separated tree to the end, and
/// returns every event and the tree.
pub fn scan_into(client: &Client, options: ScanOptions) -> (Vec<ScanEvent>, Arc<Mutex<KeyTree>>) {
    let tree = Arc::new(Mutex::new(KeyTree::new(Some(b':'))));
    let events = collect(client.scan_into(tree.clone(), options));
    (events, tree)
}

/// Runs [`Client::scan`] to the end: every key it sent, in order,
/// duplicates included, or the first error.
pub fn scan(client: &Client, options: ScanOptions) -> Result<Vec<Key>> {
    let batches = collect(client.scan(options));
    let mut keys = Vec::new();
    for batch in batches {
        keys.extend(batch?);
    }
    Ok(keys)
}

pub fn collect<T>(stream: impl Stream<Item = T>) -> Vec<T> {
    block_on(stream.collect())
}

/// The keys and `truncated` of a scan's final `Done` event.
pub fn done(events: &[ScanEvent]) -> (usize, bool) {
    match events.last() {
        Some(ScanEvent::Done {
            keys, truncated, ..
        }) => (*keys, *truncated),
        other => panic!("the scan did not end with Done: {other:?}"),
    }
}

/// Keys as a set of byte strings, for comparisons that ignore order.
pub fn key_set<'a>(keys: impl IntoIterator<Item = &'a Key>) -> HashSet<Vec<u8>> {
    keys.into_iter().map(|key| key.to_vec()).collect()
}

pub fn is_read_only(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ReadOnlyError>().is_some()
}

// Console helpers. A line is split on whitespace, so arguments with spaces
// go through `ConsoleSession::exec` directly.

pub fn args(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_string).collect()
}

pub fn run(console: &ConsoleSession, line: &str) -> Result<RawValue> {
    block_on(console.exec(args(line)))
}

pub fn ok(console: &ConsoleSession, line: &str) -> RawValue {
    run(console, line).unwrap_or_else(|err| panic!("{line}: {err:#}"))
}

pub fn bulk(value: impl AsRef<[u8]>) -> RawValue {
    RawValue::BulkString(value.as_ref().to_vec())
}

pub fn queued() -> RawValue {
    RawValue::SimpleString("QUEUED".into())
}

/// The console connection's `CLIENT INFO` line.
pub fn client_info(console: &ConsoleSession) -> String {
    match ok(console, "CLIENT INFO") {
        RawValue::BulkString(info) => String::from_utf8_lossy(&info).into_owned(),
        RawValue::VerbatimString { text, .. } => text,
        other => panic!("CLIENT INFO: {other:?}"),
    }
}

/// One `name=value` field of a `CLIENT INFO` / `CLIENT LIST` line.
pub fn field<'a>(info: &'a str, name: &str) -> Option<&'a str> {
    info.split_whitespace()
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}
