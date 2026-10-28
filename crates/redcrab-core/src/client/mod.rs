//! The connection to one server and every request the app makes over it.
//!
//! Requests are split by topic: `keys` (scan and key-level operations),
//! `values` (reading and writing values), `admin` (server-level requests) and
//! `console` (the console's own session). They all talk to Redis through
//! [`Conn`], which hides the topology: one server (`Single`, also over a
//! Unix socket or an SSH tunnel), a `cluster`, or a master found through
//! `sentinel`.

mod admin;
mod cluster;
mod conn;
mod connector;
mod console;
mod keys;
mod sentinel;
mod values;

use std::{
    fmt,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use redis::{
    ErrorKind,
    aio::{ConnectionManager, ConnectionManagerConfig},
    cmd,
};

pub use self::cluster::key_slot;
pub(crate) use self::conn::Conn;
pub use self::console::ConsoleSession;
use self::{cluster::Cluster, connector::Connector, sentinel::Sentinel};
use crate::{
    commands::{CommandClass, READ_ONLY_SCRIPTS},
    config::{ConnectionConfig, Target, Topology},
    runtime::spawn,
    ssh::{KnownHosts, Tunnel, TunnelOptions},
};

/// Keys are binary-safe and shared between the tree, caches and requests.
pub type Key = Arc<[u8]>;

/// A cheap-to-clone handle to one Redis server, cluster or Sentinel-managed
/// master. Every clone shares the same connections, which reconnect on
/// their own, and the SSH tunnel, if any, which closes when the last clone
/// (and the last console session) is dropped.
#[derive(Clone)]
pub struct Client {
    conn: Conn,
    /// Opens connections of their own (the console's) to the same server.
    connector: Connector,
    config: Arc<ConnectionConfig>,
}

/// Settings for [`Client::connect_with`] that are about this computer
/// rather than the connection.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// Where SSH host keys are checked, and new ones recorded.
    pub known_hosts: KnownHosts,
    /// The ssh-agent socket; `None` uses `SSH_AUTH_SOCK`.
    pub ssh_agent: Option<std::path::PathBuf>,
}

impl Default for ConnectOptions {
    /// `~/.ssh/known_hosts` plus redcrab's own known_hosts file, and the
    /// agent from `SSH_AUTH_SOCK`.
    fn default() -> Self {
        Self {
            known_hosts: KnownHosts::system(),
            ssh_agent: None,
        }
    }
}

/// What the status bar shows about a server. Each fact is `None` when the
/// server did not tell: a user without permission for INFO or DBSIZE
/// connects fine but learns none of it, and "0 keys" would be wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerInfo {
    pub version: Option<String>,
    pub mode: Option<String>,
    pub used_memory_human: Option<String>,
    pub connected_clients: Option<u64>,
    pub uptime_secs: Option<u64>,
    /// Keys in the selected database (on a cluster: in all primaries).
    pub db_size: Option<u64>,
    /// On a cluster, the primaries the other facts were summed over.
    pub nodes: Option<usize>,
}

impl ServerInfo {
    /// "Redis 8.0.2 · standalone", or as much of it as is known.
    pub fn server_label(&self) -> Option<String> {
        match (&self.version, &self.mode) {
            (Some(version), Some(mode)) => Some(format!("Redis {version} · {mode}")),
            (Some(version), None) => Some(format!("Redis {version}")),
            (None, Some(mode)) => Some(format!("Redis · {mode}")),
            (None, None) => None,
        }
    }

    /// The known facts as short phrases, for a status line: the server,
    /// "1,234 keys", "1.20M used", "3 clients".
    pub fn summary(&self) -> Vec<String> {
        [
            self.server_label(),
            self.db_size
                .map(|n| format!("{} keys", crate::format::count(n))),
            self.used_memory_human.as_ref().map(|m| format!("{m} used")),
            self.connected_clients.map(|n| format!("{n} clients")),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScanEvent {
    Progress {
        keys: usize,
    },
    Done {
        keys: usize,
        elapsed: Duration,
        /// The scan stopped at [`ScanOptions::limit`] with keys left unseen.
        truncated: bool,
    },
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub pattern: String,
    /// The COUNT hint per SCAN round trip.
    pub count: u32,
    /// Restricts SCAN to one type (`TYPE` argument, Redis 6+).
    pub type_filter: Option<String>,
    /// Stop after this many keys. [`Client::scan_into`] counts distinct
    /// keys and reports whether any were left out; [`Client::scan`] counts
    /// the keys it sends, duplicates included.
    pub limit: Option<usize>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            pattern: "*".into(),
            count: 2000,
            type_filter: None,
            limit: None,
        }
    }
}

/// What a read-only connection answers to anything that could change the
/// server. UIs can tell it apart from other failures with
/// `err.downcast_ref::<ReadOnlyError>()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadOnlyError {
    /// The console command that was refused, with its class.
    command: Option<(String, CommandClass)>,
}

impl ReadOnlyError {
    pub(crate) fn refused(command: String, class: CommandClass) -> Self {
        Self {
            command: Some((command, class)),
        }
    }

    /// The refused console command (`SET`, `CONFIG SET`), if the error came
    /// from the console rather than from a method of [`Client`].
    pub fn command(&self) -> Option<&str> {
        self.command.as_ref().map(|(name, _)| name.as_str())
    }
}

impl fmt::Display for ReadOnlyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some((name, class)) = &self.command else {
            return f.write_str("this connection is read-only");
        };
        let what = match class {
            CommandClass::Admin if READ_ONLY_SCRIPTS.contains(&name.as_str()) => {
                "runs a script, which can still run administrative commands"
            }
            CommandClass::Write => "can modify data",
            CommandClass::Admin => "is an administrative command",
            CommandClass::ConnectionState => "changes the state of the connection",
            CommandClass::Unknown => "is not a command known to be safe",
            CommandClass::Read => "was refused",
        };
        write!(f, "{name} {what}, and this connection is read-only")
    }
}

impl std::error::Error for ReadOnlyError {}

/// Turns a failed connection attempt into an error that says what to do.
/// `target` names what was being connected to ("10.0.0.5:6379", "the
/// master mymaster at ..."). The library's own message stays at the end of
/// the chain.
fn connect_error(err: redis::RedisError, config: &ConnectionConfig, target: &str) -> anyhow::Error {
    let message = err.to_string();
    let has = |needle: &str| message.contains(needle);
    let tls = config.tls.is_some();
    // redis-rs reports a refused AUTH as "Password authentication failed",
    // without the server's WRONGPASS.
    let auth_failed = err.kind() == ErrorKind::AuthenticationFailed;
    let hint = if has("UnknownIssuer") {
        Some("the server's certificate is not signed by a trusted CA; add its CA file, or allow untrusted certificates for a self-signed development server".to_string())
    } else if has("NotValidForName") || has("not valid for name") {
        Some(format!(
            "the server's certificate is not valid for {}",
            config.host
        ))
    } else if has("Expired") {
        Some("the server's certificate has expired".into())
    } else if has("CertificateRequired") {
        Some("the server requires a client certificate (mutual TLS)".into())
    } else if has("NOAUTH") {
        Some("the server requires a password".into())
    } else if has("SELECT is not allowed in cluster mode") {
        Some(
            "the server is a node of a cluster, which has only database 0; choose database 0 to connect to the cluster"
                .into(),
        )
    } else if auth_failed || has("WRONGPASS") || has("invalid username-password") {
        Some("wrong username or password".into())
    } else if config.unix_socket.is_some() && err.is_connection_refusal() {
        Some("no server is listening on that socket (is the path right, and is Redis running with unixsocket set?)".into())
    } else if has("Permission denied") || has("permission denied") {
        Some("this user may not open the socket; check its permissions (unixsocketperm)".into())
    } else if has("Connection refused") && config.ssh.is_none() {
        Some(format!("nothing is listening on {target}"))
    } else if tls && has("timed out") {
        Some("no answer to the TLS handshake; if the server does not use TLS, turn TLS off".into())
    } else if !tls && (has("broken pipe") || has("reset by peer") || has("end of file")) {
        Some("the server closed the connection; if it requires TLS, turn TLS on (rediss://)".into())
    } else {
        None
    };
    let err = anyhow::Error::new(err);
    let err = match hint {
        Some(hint) => err.context(hint),
        None => err,
    };
    err.context(format!("could not connect to {target}"))
}

impl Client {
    /// Connects as `config` says: to one server (over TCP, a Unix socket or
    /// an SSH tunnel), to a cluster, or to the master Sentinel names. Checks
    /// the settings first (see [`ConnectionConfig::validate`]).
    pub async fn connect(config: ConnectionConfig) -> Result<Self> {
        Self::connect_with(config, ConnectOptions::default()).await
    }

    /// [`Self::connect`] with SSH known_hosts files and agent of the
    /// caller's choosing, e.g. for tests.
    pub async fn connect_with(config: ConnectionConfig, options: ConnectOptions) -> Result<Self> {
        spawn(async move {
            config.validate()?;
            let config = Arc::new(config);
            let client = match &config.topology {
                Topology::Standalone => {
                    let (client, cluster_node) =
                        Self::connect_single(config.clone(), options).await?;
                    if cluster_node {
                        drop(client);
                        Self::connect_to_cluster_of_node(config).await?
                    } else {
                        client
                    }
                }
                Topology::Cluster { .. } => Self::connect_cluster(config).await?,
                Topology::Sentinel { .. } => {
                    let sentinel = Sentinel::connect(config.clone()).await?;
                    Self {
                        conn: Conn::Sentinel(sentinel.clone()),
                        connector: Connector::Sentinel(sentinel),
                        config,
                    }
                }
            };
            tracing::info!(
                "connected to {} ({})",
                client.config.name,
                std::iter::once(client.config.endpoint())
                    .chain(client.topology_summary())
                    .collect::<Vec<_>>()
                    .join(" · ")
            );
            Ok(client)
        })
        .await
    }

    async fn connect_cluster(config: Arc<ConnectionConfig>) -> Result<Self> {
        let (cluster, console) = Cluster::connect(&config).await?;
        Ok(Self {
            conn: Conn::Cluster(cluster.clone()),
            connector: Connector::Cluster {
                client: Arc::new(console),
                cluster,
            },
            config,
        })
    }

    /// A server set up as one turned out to be a node of a cluster, which
    /// holds only some of the keys and has no databases to switch between.
    /// Over TCP that is how a URL start (`redcrab redis://node:7000`) or a
    /// connection saved as Standalone reaches a cluster, so the cluster is
    /// connected to as a whole, with the node as its seed. A tunnel or a
    /// socket cannot reach the other nodes, so that is refused rather than
    /// showing one node's keys as if they were all of them.
    async fn connect_to_cluster_of_node(config: Arc<ConnectionConfig>) -> Result<Self> {
        let target = config.route_label();
        if config.ssh.is_some() || config.unix_socket.is_some() {
            bail!(
                "{target} is a node of a Redis Cluster, which redcrab cannot reach through an SSH tunnel or a Unix socket yet; connect to it over the network and choose Cluster"
            );
        }
        tracing::info!("{target} is a node of a cluster; connecting to the whole cluster");
        let config = ConnectionConfig {
            topology: Topology::Cluster { seeds: Vec::new() },
            ..(*config).clone()
        };
        Self::connect_cluster(Arc::new(config)).await
    }

    /// One server: over TCP, a Unix socket, or an SSH tunnel opened first.
    /// Also says whether the server is a node of a cluster.
    async fn connect_single(
        config: Arc<ConnectionConfig>,
        options: ConnectOptions,
    ) -> Result<(Self, bool)> {
        let tunnel = match &config.ssh {
            Some(ssh) => {
                let options = TunnelOptions {
                    known_hosts: options.known_hosts,
                    agent: options.ssh_agent,
                };
                Some(Arc::new(
                    Tunnel::open(ssh, config.host.trim(), config.port, &options).await?,
                ))
            }
            None => None,
        };
        // Reads certificate files, so it runs here rather than on the caller's thread.
        let client = match &tunnel {
            Some(tunnel) => config.client_for(Target::Tunnel(tunnel.local_addr()))?,
            None => config.client()?,
        };
        let target = config.route_label();
        // Through a tunnel, why the SSH server could not reach the target
        // while connecting says more than the closed connection redis-rs
        // sees.
        let started = Instant::now();
        let failed = |err: redis::RedisError| {
            let err = connect_error(err, &config, &target);
            match tunnel.as_ref().and_then(|t| t.problem_since(started)) {
                Some(problem) => err.context(problem),
                None => err,
            }
        };
        let manager_config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(Duration::from_secs(5)))
            .set_response_timeout(Some(Duration::from_secs(30)))
            .set_number_of_retries(1);
        let mut con = ConnectionManager::new_with_config(client.clone(), manager_config)
            .await
            .map_err(failed)?;
        // Without credentials the handshake succeeds even where the
        // server requires them; only the first command gets NOAUTH. Ask
        // now, so that a connection nothing can be done with fails here,
        // with a hint, and not on every request after.
        match cmd("PING").query_async::<()>(&mut con).await {
            // A user that may not PING is still logged in.
            Err(err) if err.code() != Some("NOPERM") => return Err(failed(err)),
            _ => {}
        }
        // A user that may not run INFO cannot tell; such a node is then
        // shown as it is.
        let info: Option<String> = cmd("INFO").arg("cluster").query_async(&mut con).await.ok();
        let cluster_node =
            info.is_some_and(|info| info.lines().any(|line| line.trim() == "cluster_enabled:1"));
        let client = Self {
            conn: Conn::Single(con),
            connector: Connector::Direct {
                redis: Arc::new(client),
                tunnel,
            },
            config,
        };
        Ok((client, cluster_node))
    }

    /// How the servers are arranged: as configured, except that a server
    /// configured as Standalone that is a node of a cluster is connected to
    /// as that cluster. A front end hides its database switcher for a
    /// cluster (see [`Topology::supports_databases`]).
    pub fn topology(&self) -> &Topology {
        &self.config.topology
    }

    /// Short phrases about how the client reaches the server, for a status
    /// line: "cluster · 3 primaries", "sentinel · mymaster @ 10.0.0.5:6379",
    /// "unix socket /tmp/redis.sock", "via SSH ops@bastion". Live: a
    /// cluster's primaries and a Sentinel master's address follow the
    /// server. Empty for a single server over plain TCP.
    pub fn topology_summary(&self) -> Vec<String> {
        let mut parts = Vec::new();
        match &self.conn {
            Conn::Cluster(cluster) => {
                let n = cluster.primary_count();
                parts.push(format!(
                    "cluster · {n} {}",
                    if n == 1 { "primary" } else { "primaries" }
                ));
            }
            Conn::Sentinel(sentinel) => parts.push(format!(
                "sentinel · {} @ {}",
                sentinel.master_name(),
                sentinel.address()
            )),
            Conn::Single(_) | Conn::ClusterNode(_) => {
                if let Some(path) = &self.config.unix_socket {
                    parts.push(format!("unix socket {}", path.display()));
                }
            }
        }
        if let Some(tunnel) = self.connector.tunnel() {
            parts.push(format!("via SSH {}", tunnel.label()));
        }
        parts
    }

    /// The local end of the SSH tunnel, if the client uses one.
    pub fn ssh_tunnel(&self) -> Option<SocketAddr> {
        self.connector.tunnel().map(Tunnel::local_addr)
    }

    pub fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    /// Fails with a [`ReadOnlyError`] on a read-only connection. Every method
    /// that can change the server calls this first, so the guarantee holds
    /// for every front end, not just for the buttons a UI hides.
    pub fn ensure_writable(&self) -> Result<(), ReadOnlyError> {
        if self.config.read_only {
            Err(ReadOnlyError::default())
        } else {
            Ok(())
        }
    }
}

/// Through the test environment's SSH tunnel (`REDCRAB_IT=1`: 2222 to
/// `ssh-redis:6379`). Here rather than in `tests/` because they reach into
/// the tunnel.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use futures::executor::block_on;
    use redis::Value;

    use super::*;
    use crate::{
        config::{HostKeyPolicy, SshAuth, SshConfig},
        runtime::runtime,
    };

    /// A file `scripts/test-env.sh up` generated.
    fn secret(name: &str) -> PathBuf {
        std::env::var_os("REDCRAB_IT_DIR")
            .map_or_else(
                || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/docker/.generated"),
                PathBuf::from,
            )
            .join(name)
    }

    /// A client through the tunnel, or `None` unless `REDCRAB_IT=1`.
    fn tunneled(test: &str) -> Option<Client> {
        if std::env::var("REDCRAB_IT").as_deref() != Ok("1") {
            eprintln!("skipped {test}: REDCRAB_IT is not 1");
            return None;
        }
        let config = ConnectionConfig {
            name: "it-ssh-in-crate".into(),
            host: "ssh-redis".into(),
            port: 6379,
            ssh: Some(SshConfig {
                host: "127.0.0.1".into(),
                port: 2222,
                user: "tunnel".into(),
                auth: SshAuth::PrivateKey {
                    path: secret("id_ed25519"),
                },
                host_key: HostKeyPolicy::Strict,
                ..SshConfig::default()
            }),
            ..ConnectionConfig::default()
        };
        // Pins the server's key; nothing is learned, so no file of its own.
        let options = ConnectOptions {
            known_hosts: KnownHosts {
                openssh: Some(secret("known_hosts")),
                own: None,
            },
            ssh_agent: None,
        };
        let client = block_on(Client::connect_with(config, options));
        Some(client.expect("is scripts/test-env.sh up?"))
    }

    #[test]
    fn an_old_tunnel_problem_does_not_explain_a_new_failure() {
        let Some(client) = tunneled("an_old_tunnel_problem_does_not_explain_a_new_failure") else {
            return;
        };
        let Connector::Direct {
            tunnel: Some(tunnel),
            ..
        } = client.connector.clone()
        else {
            panic!("no tunnel");
        };
        // A forward that failed a while ago, while Redis was restarting.
        tunnel.note_problem(
            "the SSH server could not open a connection to ssh-redis:6379: it could not connect",
        );
        std::thread::sleep(Duration::from_millis(5));
        // Now a connection fails for another reason: a password the server
        // does not have. The channel itself opens fine.
        let config = ConnectionConfig {
            password: Some("not-the-password".into()),
            ..client.config().clone()
        };
        let redis = config
            .client_for(Target::Tunnel(tunnel.local_addr()))
            .unwrap();
        let connector = Connector::Direct {
            redis: Arc::new(redis),
            tunnel: Some(tunnel.clone()),
        };
        let err = match runtime().block_on(connector.connect(&config, Duration::from_secs(5))) {
            Ok(_) => panic!("the connection was expected to fail"),
            Err(err) => format!("{err:#}"),
        };
        assert!(
            err.starts_with("could not connect to ssh-redis:6379 via SSH tunnel@127.0.0.1:2222"),
            "{err}"
        );
        assert!(!err.contains("could not open a connection"), "{err}");
    }

    #[test]
    fn the_tunnel_logs_in_again_when_the_session_ends() {
        let Some(client) = tunneled("the_tunnel_logs_in_again_when_the_session_ends") else {
            return;
        };
        block_on(client.ping()).unwrap();
        let tunnel = client.connector.tunnel().expect("a tunnel");
        let before = tunnel.session();
        let started = Instant::now();
        // As when sshd restarts, or the laptop slept past the keepalives.
        runtime().block_on(tunnel.end_session());

        // The connection the old session carried is gone; the same client
        // gets through again over a new one, without a reconnect by the
        // caller (the first request may still see the broken connection).
        let answered = (0..20).any(|_| {
            let ok = block_on(client.ping()).is_ok();
            if !ok {
                std::thread::sleep(Duration::from_millis(100));
            }
            ok
        });
        assert!(answered, "the client did not get through again");
        let after = tunnel.session();
        assert!(!Arc::ptr_eq(&before, &after), "no new login");
        assert!(!after.is_closed());
        assert_eq!(tunnel.problem_since(started), None);
        // A console connects through the new session too.
        let console = client.console();
        assert_eq!(
            block_on(console.exec(["PING"])).unwrap(),
            Value::SimpleString("PONG".into())
        );
    }
}
