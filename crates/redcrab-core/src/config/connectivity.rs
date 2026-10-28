//! How a connection reaches its servers: the topology (one server, a
//! cluster, or a master found through Sentinel), a local Unix socket, and an
//! SSH tunnel. Also the checks that turn settings that cannot work into a
//! message before anything connects.
//!
//! Every type here loads leniently: a value written by a newer version
//! (another topology kind, auth method or host key policy) falls back to a
//! safe default instead of making the whole connections file unreadable.

use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::ConnectionConfig;

/// How the servers of a connection are arranged. Stored with a `kind` tag:
/// `{"kind":"cluster","seeds":["10.0.0.2:7001"]}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Topology {
    /// One server, at `host`/`port` or at the connection's Unix socket.
    #[default]
    Standalone,
    /// A Redis Cluster. `host`/`port` is the first node to ask for the
    /// cluster's layout; `seeds` are more (`host:port`), tried when it does
    /// not answer.
    Cluster { seeds: Vec<String> },
    /// The master that Sentinel names `master`. The sentinels to ask are
    /// `sentinels` (`host:port`); `host`/`port` are not used.
    Sentinel {
        master: String,
        sentinels: Vec<String>,
        /// For sentinels with ACL users; the password is
        /// [`ConnectionConfig::sentinel_password`].
        sentinel_username: Option<String>,
        /// Talk TLS to the sentinels too, with the connection's TLS
        /// settings (or the system's trusted certificates without them).
        sentinel_tls: bool,
    },
}

impl Topology {
    pub fn is_cluster(&self) -> bool {
        matches!(self, Self::Cluster { .. })
    }

    /// Whether SELECT works: a cluster only has database 0, so a front end
    /// hides its database switcher for one.
    pub fn supports_databases(&self) -> bool {
        !self.is_cluster()
    }
}

/// The shape of every topology in a file, all fields optional, so that
/// missing fields and unknown kinds load instead of failing.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTopology {
    kind: Option<String>,
    seeds: Vec<String>,
    master: String,
    sentinels: Vec<String>,
    sentinel_username: Option<String>,
    sentinel_tls: bool,
}

/// A kind this version does not know (written by a newer one) loads as
/// `Standalone`: host/port is then tried as one server, which fails or works
/// without sending anything anywhere else.
impl<'de> Deserialize<'de> for Topology {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Option::<RawTopology>::deserialize(deserializer)?.unwrap_or_default();
        Ok(
            match raw.kind.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("cluster") => Self::Cluster { seeds: raw.seeds },
                Some("sentinel") => Self::Sentinel {
                    master: raw.master,
                    sentinels: raw.sentinels,
                    sentinel_username: raw.sentinel_username,
                    sentinel_tls: raw.sentinel_tls,
                },
                _ => Self::Standalone,
            },
        )
    }
}

/// An SSH server to tunnel the Redis connection through: redcrab listens on
/// a local port and forwards every connection to `host`/`port` of the Redis
/// settings, as the SSH server sees them (like `ssh -L`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    pub host_key: HostKeyPolicy,
    /// The SSH password ([`SshAuth::Password`]). Never written to disk; it
    /// lives in the credential store (see [`crate::store::Secret`]).
    #[serde(skip)]
    pub password: Option<String>,
    /// The private key's passphrase ([`SshAuth::PrivateKey`]), if the key
    /// file is encrypted. Never written to disk either.
    #[serde(skip)]
    pub passphrase: Option<String>,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuth::default(),
            host_key: HostKeyPolicy::default(),
            password: None,
            passphrase: None,
        }
    }
}

impl SshConfig {
    /// `user@host`, with `:port` unless it is 22.
    pub fn label(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.user, self.host)
        } else {
            format!("{}@{}:{}", self.user, self.host, self.port)
        }
    }
}

/// How redcrab logs in to the SSH server. Stored with a `method` tag.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum SshAuth {
    /// [`SshConfig::password`].
    #[default]
    Password,
    /// An OpenSSH (or PEM) private key file, encrypted or not; a path
    /// starting with `~/` is in the home directory.
    PrivateKey { path: PathBuf },
    /// The keys of the running ssh-agent (`SSH_AUTH_SOCK`).
    Agent,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawSshAuth {
    method: Option<String>,
    path: PathBuf,
}

/// An unknown method loads as `Password`: without a saved password the
/// login fails with a clear message, and the tunnel is never skipped.
impl<'de> Deserialize<'de> for SshAuth {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Option::<RawSshAuth>::deserialize(deserializer)?.unwrap_or_default();
        Ok(match raw.method.as_deref() {
            Some("private_key") => Self::PrivateKey { path: raw.path },
            Some("agent") => Self::Agent,
            _ => Self::Password,
        })
    }
}

/// What to do about the SSH server's host key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HostKeyPolicy {
    /// Connect only to a server whose key is in `~/.ssh/known_hosts` or in
    /// redcrab's own known_hosts file.
    #[default]
    Strict,
    /// Like `StrictHostKeyChecking accept-new`: a server seen for the first
    /// time is trusted and recorded in redcrab's own known_hosts file; a
    /// known server whose key changed is refused, loudly.
    AcceptNew,
}

impl HostKeyPolicy {
    pub const ALL: [Self; 2] = [Self::Strict, Self::AcceptNew];

    pub fn label(self) -> &'static str {
        match self {
            Self::Strict => "Known hosts only",
            Self::AcceptNew => "Trust new hosts",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::AcceptNew => "accept_new",
        }
    }
}

impl Serialize for HostKeyPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.key())
    }
}

/// Anything unknown loads as `Strict`, the safe choice.
impl<'de> Deserialize<'de> for HostKeyPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let key = Option::<String>::deserialize(deserializer)?.unwrap_or_default();
        Ok(Self::ALL
            .into_iter()
            .find(|policy| policy.key() == key)
            .unwrap_or_default())
    }
}

/// The setting a validation problem is about, so a form can show the
/// message next to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConfigField {
    Name,
    Host,
    Port,
    Db,
    UnixSocket,
    Tls,
    Seeds,
    SentinelMaster,
    Sentinels,
    Ssh,
    SshHost,
    SshPort,
    SshUser,
    SshKey,
}

/// Why a [`ConnectionConfig`] cannot work; see
/// [`ConnectionConfig::validate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidConfig {
    pub field: ConfigField,
    pub message: String,
}

impl fmt::Display for InvalidConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InvalidConfig {}

/// The longest Unix socket path the platforms allow, in bytes, without the
/// terminating NUL (`sun_path` is 104 bytes on macOS, 108 on Linux).
const MAX_SOCKET_PATH: usize = 103;

/// Whether this platform can connect to a Unix socket, so a front end can
/// leave the choice out where it cannot.
pub const UNIX_SOCKETS: bool = cfg!(unix);

impl ConnectionConfig {
    /// Refuses settings that cannot work, with a message that says why and
    /// the field it is about. [`crate::Client::connect`] checks this first;
    /// a form can call it to show problems before connecting. Only the first
    /// problem is reported.
    pub fn validate(&self) -> Result<(), InvalidConfig> {
        let fail = |field, message: String| Err(InvalidConfig { field, message });
        if let Some(path) = &self.unix_socket {
            if !UNIX_SOCKETS {
                // redis-rs only connects to Unix sockets on Unix.
                return fail(
                    ConfigField::UnixSocket,
                    "Unix sockets are not available on this platform; connect over TCP".into(),
                );
            }
            if !matches!(self.topology, Topology::Standalone) {
                return fail(
                    ConfigField::UnixSocket,
                    "a Unix socket reaches a single server; cluster and Sentinel connections need network addresses".into(),
                );
            }
            if self.tls.is_some() {
                return fail(
                    ConfigField::Tls,
                    "TLS is not used over a Unix socket; turn TLS off".into(),
                );
            }
            if self.ssh.is_some() {
                return fail(
                    ConfigField::UnixSocket,
                    "a Unix socket is on this computer, so it is not reached through an SSH tunnel; turn one of them off".into(),
                );
            }
            if path.as_os_str().is_empty() {
                return fail(
                    ConfigField::UnixSocket,
                    "choose the server's socket file".into(),
                );
            }
            if !path.is_absolute() {
                return fail(
                    ConfigField::UnixSocket,
                    format!(
                        "give the full path of the socket file (e.g. /var/run/redis/redis.sock), not {}",
                        path.display()
                    ),
                );
            }
            let len = path.as_os_str().len();
            if len > MAX_SOCKET_PATH {
                return fail(
                    ConfigField::UnixSocket,
                    format!(
                        "the socket path is {len} bytes long; Unix sockets allow at most {MAX_SOCKET_PATH}"
                    ),
                );
            }
        } else if !matches!(self.topology, Topology::Sentinel { .. }) {
            if let Err(message) = check_host(&self.host) {
                return fail(ConfigField::Host, message);
            }
            if self.port == 0 {
                return fail(
                    ConfigField::Port,
                    "the port must be between 1 and 65535".into(),
                );
            }
        }
        match &self.topology {
            Topology::Standalone => {}
            Topology::Cluster { seeds } => {
                if self.db != 0 {
                    return fail(ConfigField::Db, "a cluster has only database 0".into());
                }
                if let Some(message) = bad_address(seeds, "seed node") {
                    return fail(ConfigField::Seeds, message);
                }
            }
            Topology::Sentinel {
                master, sentinels, ..
            } => {
                if master.trim().is_empty() {
                    return fail(
                        ConfigField::SentinelMaster,
                        "enter the name Sentinel knows the master by (e.g. mymaster)".into(),
                    );
                }
                if sentinels.iter().all(|s| s.trim().is_empty()) {
                    return fail(
                        ConfigField::Sentinels,
                        "add at least one sentinel as host:port (e.g. 10.0.0.5:26379)".into(),
                    );
                }
                if let Some(message) = bad_address(sentinels, "sentinel") {
                    return fail(ConfigField::Sentinels, message);
                }
            }
        }
        if let Some(tls) = &self.tls
            && tls.client_cert_file.is_some() != tls.client_key_file.is_some()
        {
            return fail(
                ConfigField::Tls,
                "mutual TLS needs both a client certificate and a client key".into(),
            );
        }
        // Through a tunnel the certificate's name is not checked (see
        // `ConnectionConfig::client_for`), so only its chain is. With the
        // system's roots that would accept a certificate any public CA
        // signed for any name; the SSH host key only vouches for the
        // bastion, not for the hop from it to Redis.
        if let Some(tls) = &self.tls
            && self.ssh.is_some()
            && !tls.insecure
            && tls.ca_file.is_none()
        {
            return fail(
                ConfigField::Tls,
                "TLS through an SSH tunnel needs the CA file that signs the server's certificate: the certificate's name cannot be checked through the tunnel, so the system's trusted certificates would accept any certificate a public CA signed".into(),
            );
        }
        if let Some(ssh) = &self.ssh {
            if !matches!(self.topology, Topology::Standalone) {
                return fail(
                    ConfigField::Ssh,
                    "SSH tunnels work only with standalone servers for now: a cluster or Sentinel hands out the addresses of other nodes, which point inside the remote network".into(),
                );
            }
            if let Err(message) = check_host(&ssh.host) {
                return fail(ConfigField::SshHost, format!("SSH server: {message}"));
            }
            if ssh.port == 0 {
                return fail(
                    ConfigField::SshPort,
                    "the SSH port must be between 1 and 65535".into(),
                );
            }
            if ssh.user.trim().is_empty() {
                return fail(
                    ConfigField::SshUser,
                    "enter the user to log in to the SSH server as".into(),
                );
            }
            if let SshAuth::PrivateKey { path } = &ssh.auth {
                if path.as_os_str().is_empty() {
                    return fail(ConfigField::SshKey, "choose the private key file".into());
                }
                // `has_root` rather than `is_absolute`: the same on Unix, and
                // on Windows `\keys\id` names a file as clearly as `C:\keys\id`.
                if !path.has_root() && !path.starts_with("~") {
                    return fail(
                        ConfigField::SshKey,
                        format!(
                            "give the full path of the key file (e.g. ~/.ssh/id_ed25519), not {}",
                            path.display()
                        ),
                    );
                }
            }
        }
        // Last: a form names an unnamed connection after where it goes,
        // which only works once that is right.
        if self.name.trim().is_empty() {
            return fail(ConfigField::Name, "give the connection a name".into());
        }
        Ok(())
    }

    /// Refuses a name a connection is about to be saved under for the first
    /// time (a new connection, or a renamed one) when it contains `#`, which
    /// separates the name from the suffix of the credential store accounts
    /// its other secrets are saved under (`<name>#ssh`, see
    /// [`crate::store::Secret`]): a connection named `prod#ssh` would share
    /// one with the SSH password of `prod`. Older versions allowed any
    /// name, so [`Self::validate`] does not check this, and connections
    /// saved under such a name keep working (see
    /// [`crate::form::ConnectionForm::to_config`]).
    pub fn validate_new_name(&self) -> Result<(), InvalidConfig> {
        if self.name.contains('#') {
            return Err(InvalidConfig {
                field: ConfigField::Name,
                message: "a connection name cannot contain #: it separates the parts of the names its passwords are saved under".into(),
            });
        }
        Ok(())
    }

    /// The same settings without any password or passphrase, e.g. for a
    /// list that is written to disk or a form shown again.
    pub fn without_secrets(&self) -> ConnectionConfig {
        ConnectionConfig {
            password: None,
            sentinel_password: None,
            ssh: self.ssh.clone().map(|ssh| SshConfig {
                password: None,
                passphrase: None,
                ..ssh
            }),
            ..self.clone()
        }
    }

    /// A name for a connection the user left unnamed: the host, the socket
    /// file, or the Sentinel master.
    pub fn suggested_name(&self) -> String {
        if let Some(path) = &self.unix_socket {
            return path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned();
        }
        match &self.topology {
            Topology::Sentinel { master, .. } if !master.trim().is_empty() => {
                master.trim().to_string()
            }
            _ => self.host.clone(),
        }
    }

    /// Where the connection goes, for titles and lists: `10.0.0.5:6379`,
    /// `/tmp/redis.sock`, or the Sentinel master's name.
    pub fn endpoint(&self) -> String {
        if let Some(path) = &self.unix_socket {
            return path.display().to_string();
        }
        match &self.topology {
            Topology::Sentinel { master, .. } => master.clone(),
            _ => self.address(),
        }
    }

    /// [`Self::endpoint`], with the kind of connection for the ones that
    /// are not a single server over TCP: `cluster 10.0.0.5:7000`,
    /// `sentinel mymaster`, and `via SSH user@host` when tunneled.
    pub fn route_label(&self) -> String {
        let mut label = match &self.topology {
            Topology::Standalone => self.endpoint(),
            Topology::Cluster { .. } => format!("cluster {}", self.endpoint()),
            Topology::Sentinel { .. } => format!("sentinel {}", self.endpoint()),
        };
        if let Some(ssh) = &self.ssh {
            label.push_str(&format!(" via SSH {}", ssh.label()));
        }
        label
    }

    /// Where the connection's keys are, for a question about changing
    /// them: the name, [`Self::route_label`], and the database where there
    /// are databases (not on a cluster): `prod · 10.0.0.5:6379 · db2`,
    /// `prod · sentinel mymaster · db0`, `prod · /tmp/redis.sock · db0`,
    /// `prod · localhost:6379 via SSH ops@bastion · db0`,
    /// `prod · cluster 10.0.0.2:7000`. A connected client says more
    /// ([`Client::location`](crate::Client::location)).
    pub fn location(&self) -> String {
        self.location_via(self.route_label())
    }

    /// [`Self::location`] with `route` in place of the route label.
    pub(crate) fn location_via(&self, route: String) -> String {
        let mut parts = Vec::with_capacity(3);
        if !self.name.trim().is_empty() {
            parts.push(self.name.trim().to_string());
        }
        parts.push(route);
        if self.topology.supports_databases() {
            parts.push(format!("db{}", self.db));
        }
        parts.join(" · ")
    }
}

/// Why `host` is not a host name or address, if it is not.
fn check_host(host: &str) -> Result<(), String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("enter the host".into());
    }
    if host.contains("://") {
        return Err(format!(
            "{host} is a URL; enter just the host name or address"
        ));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(format!("{host:?} is not a host name or address"));
    }
    Ok(())
}

/// The first entry of `addresses` that is not `host:port`, as a message.
fn bad_address(addresses: &[String], what: &str) -> Option<String> {
    addresses
        .iter()
        .filter(|address| !address.trim().is_empty())
        .find_map(|address| {
            parse_address(address)
                .err()
                .map(|why| format!("the {what} {:?} is not host:port: {why}", address.trim()))
        })
}

/// Parses `host:port` or `[ipv6]:port`. The port is required, since seed
/// nodes and sentinels rarely listen on the default one. IPv6 addresses
/// come back without brackets.
pub fn parse_address(address: &str) -> Result<(String, u16), String> {
    let address = address.trim();
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or("the IPv6 address has no closing ]")?;
        let port = rest
            .strip_prefix(':')
            .ok_or("the port is missing after the IPv6 address")?;
        (host, port)
    } else {
        let (host, port) = address.rsplit_once(':').ok_or("the port is missing")?;
        if host.contains(':') {
            return Err("write IPv6 addresses in brackets, e.g. [::1]:7000".into());
        }
        (host, port)
    };
    check_host(host).map_err(|_| "the host is missing or not valid".to_string())?;
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!("{port:?} is not a port")),
        Ok(port) => Ok((host.to_string(), port)),
    }
}

/// `host:port`, with brackets around IPv6 addresses; the inverse of
/// [`parse_address`].
pub fn format_address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Splits addresses typed into one field, separated by commas, spaces or
/// new lines. Empty entries are dropped.
pub fn split_addresses(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::config::TlsConfig;

    fn standalone() -> ConnectionConfig {
        ConnectionConfig {
            name: "test".into(),
            ..ConnectionConfig::default()
        }
    }

    fn cluster(seeds: &[&str]) -> ConnectionConfig {
        ConnectionConfig {
            topology: Topology::Cluster {
                seeds: seeds.iter().map(|s| s.to_string()).collect(),
            },
            port: 7000,
            ..standalone()
        }
    }

    fn sentinel(master: &str, sentinels: &[&str]) -> ConnectionConfig {
        ConnectionConfig {
            topology: Topology::Sentinel {
                master: master.into(),
                sentinels: sentinels.iter().map(|s| s.to_string()).collect(),
                sentinel_username: None,
                sentinel_tls: false,
            },
            ..standalone()
        }
    }

    fn ssh(auth: SshAuth) -> SshConfig {
        SshConfig {
            host: "bastion".into(),
            user: "ops".into(),
            auth,
            ..SshConfig::default()
        }
    }

    fn unix(path: &str) -> ConnectionConfig {
        ConnectionConfig {
            unix_socket: Some(path.into()),
            ..standalone()
        }
    }

    /// The field of the first problem, or `None` when valid.
    fn problem(config: &ConnectionConfig) -> Option<ConfigField> {
        config.validate().err().map(|err| {
            assert!(!err.message.is_empty());
            err.field
        })
    }

    #[test]
    fn valid_settings_pass() {
        let sockets = [
            unix("/tmp/redis.sock"),
            ConnectionConfig {
                db: 3,
                ..unix("/var/run/redis/redis.sock")
            },
        ];
        for config in sockets {
            if UNIX_SOCKETS {
                assert_eq!(config.validate(), Ok(()), "{config:?}");
            } else {
                assert_eq!(problem(&config), Some(ConfigField::UnixSocket));
            }
        }
        let valid = [
            standalone(),
            cluster(&[]),
            cluster(&["127.0.0.1:7001", " 10.0.0.2:7002 ", "[::1]:7003", ""]),
            sentinel("mymaster", &["127.0.0.1:26379"]),
            ConnectionConfig {
                db: 4,
                tls: Some(TlsConfig::default()),
                ..sentinel("mymaster", &["a:26379", "b:26379"])
            },
            ConnectionConfig {
                ssh: Some(ssh(SshAuth::Password)),
                ..standalone()
            },
            ConnectionConfig {
                ssh: Some(ssh(SshAuth::Agent)),
                tls: Some(TlsConfig {
                    ca_file: Some("/etc/redis/ca.pem".into()),
                    ..TlsConfig::default()
                }),
                ..standalone()
            },
            ConnectionConfig {
                ssh: Some(ssh(SshAuth::Agent)),
                tls: Some(TlsConfig {
                    insecure: true,
                    ..TlsConfig::default()
                }),
                ..standalone()
            },
            ConnectionConfig {
                ssh: Some(ssh(SshAuth::PrivateKey {
                    path: "~/.ssh/id_ed25519".into(),
                })),
                ..standalone()
            },
            ConnectionConfig {
                ssh: Some(ssh(SshAuth::PrivateKey {
                    path: "/keys/id_rsa".into(),
                })),
                ..standalone()
            },
        ];
        for config in valid {
            assert_eq!(config.validate(), Ok(()), "{config:?}");
        }
    }

    #[test]
    fn combinations_that_cannot_work_are_refused() {
        use ConfigField as F;
        // Where there are no Unix sockets, that comes first.
        let tls_over_socket = if UNIX_SOCKETS { F::Tls } else { F::UnixSocket };
        let cases: Vec<(ConnectionConfig, ConfigField)> = vec![
            (
                ConnectionConfig {
                    name: " ".into(),
                    ..standalone()
                },
                F::Name,
            ),
            (
                ConnectionConfig {
                    host: String::new(),
                    ..standalone()
                },
                F::Host,
            ),
            (
                ConnectionConfig {
                    host: "redis://h:6379".into(),
                    ..standalone()
                },
                F::Host,
            ),
            (
                ConnectionConfig {
                    host: "my host".into(),
                    ..standalone()
                },
                F::Host,
            ),
            (
                ConnectionConfig {
                    port: 0,
                    ..standalone()
                },
                F::Port,
            ),
            // Unix sockets: standalone only, no TLS, no SSH, a full path.
            (
                ConnectionConfig {
                    tls: Some(TlsConfig::default()),
                    ..unix("/tmp/redis.sock")
                },
                tls_over_socket,
            ),
            (
                ConnectionConfig {
                    ssh: Some(ssh(SshAuth::Agent)),
                    ..unix("/tmp/redis.sock")
                },
                F::UnixSocket,
            ),
            (
                ConnectionConfig {
                    unix_socket: Some("/tmp/r.sock".into()),
                    ..cluster(&[])
                },
                F::UnixSocket,
            ),
            (unix(""), F::UnixSocket),
            (unix("redis.sock"), F::UnixSocket),
            (unix(&format!("/tmp/{}", "s".repeat(120))), F::UnixSocket),
            // Clusters: database 0 only, seeds as host:port.
            (
                ConnectionConfig {
                    db: 1,
                    ..cluster(&[])
                },
                F::Db,
            ),
            (cluster(&["127.0.0.1"]), F::Seeds),
            (cluster(&["127.0.0.1:x"]), F::Seeds),
            (cluster(&["::1:7000"]), F::Seeds),
            (
                ConnectionConfig {
                    ssh: Some(ssh(SshAuth::Agent)),
                    ..cluster(&[])
                },
                F::Ssh,
            ),
            // Sentinel: a master name, at least one sentinel, no SSH.
            (sentinel("", &["a:26379"]), F::SentinelMaster),
            (sentinel("mymaster", &[]), F::Sentinels),
            (sentinel("mymaster", &["", " "]), F::Sentinels),
            (sentinel("mymaster", &["a:26379", "b"]), F::Sentinels),
            (
                ConnectionConfig {
                    ssh: Some(ssh(SshAuth::Agent)),
                    ..sentinel("mymaster", &["a:26379"])
                },
                F::Ssh,
            ),
            // SSH: a host, a port, a user, a key file when it uses one.
            (
                ConnectionConfig {
                    ssh: Some(SshConfig {
                        host: String::new(),
                        ..ssh(SshAuth::Agent)
                    }),
                    ..standalone()
                },
                F::SshHost,
            ),
            (
                ConnectionConfig {
                    ssh: Some(SshConfig {
                        port: 0,
                        ..ssh(SshAuth::Agent)
                    }),
                    ..standalone()
                },
                F::SshPort,
            ),
            (
                ConnectionConfig {
                    ssh: Some(SshConfig {
                        user: " ".into(),
                        ..ssh(SshAuth::Agent)
                    }),
                    ..standalone()
                },
                F::SshUser,
            ),
            (
                ConnectionConfig {
                    ssh: Some(ssh(SshAuth::PrivateKey {
                        path: PathBuf::new(),
                    })),
                    ..standalone()
                },
                F::SshKey,
            ),
            (
                ConnectionConfig {
                    ssh: Some(ssh(SshAuth::PrivateKey {
                        path: "id_rsa".into(),
                    })),
                    ..standalone()
                },
                F::SshKey,
            ),
            (
                ConnectionConfig {
                    tls: Some(TlsConfig {
                        client_cert_file: Some("/c.pem".into()),
                        ..TlsConfig::default()
                    }),
                    ..standalone()
                },
                F::Tls,
            ),
            // TLS through a tunnel: the system's roots alone would accept
            // any public certificate, since the name is not checked there.
            (
                ConnectionConfig {
                    ssh: Some(ssh(SshAuth::Agent)),
                    tls: Some(TlsConfig::default()),
                    ..standalone()
                },
                F::Tls,
            ),
            (
                ConnectionConfig {
                    ssh: Some(ssh(SshAuth::Agent)),
                    tls: Some(TlsConfig {
                        client_cert_file: Some("/c.pem".into()),
                        client_key_file: Some("/k.pem".into()),
                        ..TlsConfig::default()
                    }),
                    ..standalone()
                },
                F::Tls,
            ),
        ];
        for (config, field) in cases {
            assert_eq!(problem(&config), Some(field), "{config:?}");
        }
        // Sentinel connections do not use host/port.
        let config = ConnectionConfig {
            host: String::new(),
            port: 0,
            ..sentinel("mymaster", &["a:26379"])
        };
        assert_eq!(problem(&config), None);
    }

    #[test]
    fn old_files_load_as_standalone_without_tunnel() {
        let old: ConnectionConfig =
            serde_json::from_str(r#"{"name":"x","host":"h","port":6380,"db":2}"#).unwrap();
        assert_eq!(old.topology, Topology::Standalone);
        assert_eq!(old.unix_socket, None);
        assert_eq!(old.ssh, None);
        assert_eq!(old.validate(), Ok(()));
    }

    #[test]
    fn names_older_versions_allowed_still_connect() {
        // Saved before `#` was kept out of new names.
        let old: ConnectionConfig =
            serde_json::from_str(r#"{"name":"cache #2","host":"h","port":6380}"#).unwrap();
        assert_eq!(old.validate(), Ok(()));
        // Only a name about to be saved for the first time is refused.
        let err = old.validate_new_name().unwrap_err();
        assert_eq!(err.field, ConfigField::Name);
        assert!(err.message.contains("cannot contain #"), "{err}");
        for name in ["prod#ssh", "#", "a#b"] {
            let config = ConnectionConfig {
                name: name.into(),
                ..standalone()
            };
            assert!(config.validate().is_ok());
            assert!(config.validate_new_name().is_err(), "{name}");
        }
        assert_eq!(standalone().validate_new_name(), Ok(()));
    }

    #[test]
    fn connectivity_settings_round_trip_without_secrets() {
        let configs = [
            ConnectionConfig {
                sentinel_password: Some("sentinel-secret".into()),
                ..sentinel("mymaster", &["a:26379", "[::1]:26380"])
            },
            ConnectionConfig {
                topology: Topology::Sentinel {
                    master: "m".into(),
                    sentinels: vec!["s:1".into()],
                    sentinel_username: Some("watcher".into()),
                    sentinel_tls: true,
                },
                ..standalone()
            },
            cluster(&["127.0.0.1:7001"]),
            unix("/tmp/redis.sock"),
            ConnectionConfig {
                ssh: Some(SshConfig {
                    port: 2222,
                    host_key: HostKeyPolicy::AcceptNew,
                    password: Some("ssh-secret".into()),
                    passphrase: Some("key-secret".into()),
                    ..ssh(SshAuth::PrivateKey {
                        path: "/k/id".into(),
                    })
                }),
                ..standalone()
            },
            ConnectionConfig {
                ssh: Some(ssh(SshAuth::Agent)),
                ..standalone()
            },
        ];
        for config in configs {
            let json = serde_json::to_string(&config).unwrap();
            for secret in ["sentinel-secret", "ssh-secret", "key-secret", "password"] {
                assert!(!json.contains(secret), "{json}");
            }
            let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
            let without_secrets = ConnectionConfig {
                sentinel_password: None,
                ssh: config.ssh.clone().map(|ssh| SshConfig {
                    password: None,
                    passphrase: None,
                    ..ssh
                }),
                ..config.clone()
            };
            assert_eq!(back, without_secrets, "{json}");
        }
        let json = serde_json::to_string(&cluster(&["s:1"])).unwrap();
        assert!(
            json.contains(r#""topology":{"kind":"cluster","seeds":["s:1"]}"#),
            "{json}"
        );
        let json = serde_json::to_string(&ConnectionConfig {
            ssh: Some(ssh(SshAuth::PrivateKey { path: "/k".into() })),
            ..standalone()
        })
        .unwrap();
        assert!(
            json.contains(r#""auth":{"method":"private_key","path":"/k"},"host_key":"strict""#),
            "{json}"
        );
    }

    #[test]
    fn values_from_newer_versions_load_safely() {
        let config: ConnectionConfig = serde_json::from_str(
            r#"{"name":"x","topology":{"kind":"proxy","via":"y"},"read_only":true,
                "ssh":{"host":"b","user":"u","auth":{"method":"fido"},"host_key":"ask"}}"#,
        )
        .unwrap();
        assert_eq!(config.topology, Topology::Standalone);
        assert!(config.read_only, "the rest of the entry survives");
        let ssh = config.ssh.unwrap();
        assert_eq!(ssh.auth, SshAuth::Password);
        assert_eq!(ssh.host_key, HostKeyPolicy::Strict);
        assert_eq!(ssh.port, 22);

        // Missing or null parts load with their defaults.
        let config: ConnectionConfig = serde_json::from_str(
            r#"{"topology":{"kind":"Sentinel","master":"m"},"ssh":{"auth":null,"host_key":null}}"#,
        )
        .unwrap();
        assert_eq!(
            config.topology,
            Topology::Sentinel {
                master: "m".into(),
                sentinels: vec![],
                sentinel_username: None,
                sentinel_tls: false
            }
        );
        let config: ConnectionConfig = serde_json::from_str(r#"{"topology":null}"#).unwrap();
        assert_eq!(config.topology, Topology::Standalone);
    }

    #[test]
    fn url_starts_keep_the_saved_route() {
        // A URL names an address; the tunnel, socket or topology it is
        // reached by comes from the saved connection, so it is never skipped.
        let tunneled = ConnectionConfig {
            name: "10.0.0.5:6379".into(),
            host: "10.0.0.5".into(),
            ssh: Some(ssh(SshAuth::Agent)),
            ..standalone()
        };
        let clustered = ConnectionConfig {
            name: "10.0.0.6:7000".into(),
            ..cluster(&["10.0.0.7:7000"])
        };
        let saved = [tunneled.clone(), clustered.clone()];
        let url = |url| ConnectionConfig::parse_url(url).unwrap().with_saved(&saved);
        let merged = url("redis://:pw@10.0.0.5:6379/2");
        assert_eq!(merged.ssh, tunneled.ssh);
        assert_eq!((merged.db, merged.password.as_deref()), (2, Some("pw")));
        assert_eq!(url("10.0.0.6:7000").topology, clustered.topology);
        assert_eq!(url("10.0.0.8:1").ssh, None);
    }

    #[test]
    fn labels() {
        let config = ConnectionConfig {
            ssh: Some(SshConfig {
                port: 2222,
                ..ssh(SshAuth::Agent)
            }),
            ..standalone()
        };
        assert_eq!(
            config.route_label(),
            "127.0.0.1:6379 via SSH ops@bastion:2222"
        );
        assert_eq!(cluster(&[]).route_label(), "cluster 127.0.0.1:7000");
        let s = sentinel("mymaster", &["a:1"]);
        assert_eq!(s.route_label(), "sentinel mymaster");

        // Where keys would be deleted: never the unused host and port of a
        // Sentinel or socket connection, and no database on a cluster.
        let named = |config: ConnectionConfig| ConnectionConfig {
            name: "prod".into(),
            db: 2,
            ..config
        };
        assert_eq!(
            named(config.clone()).location(),
            "prod · 127.0.0.1:6379 via SSH ops@bastion:2222 · db2"
        );
        assert_eq!(
            named(s.clone()).location(),
            "prod · sentinel mymaster · db2"
        );
        assert_eq!(
            named(unix("/tmp/r.sock")).location(),
            "prod · /tmp/r.sock · db2"
        );
        assert_eq!(
            ConnectionConfig {
                name: "prod".into(),
                ..cluster(&[])
            }
            .location(),
            "prod · cluster 127.0.0.1:7000"
        );
        assert_eq!(standalone().location_via("x".into()), "test · x · db0");
        // A connection without a name (a URL start) is not named.
        let unnamed = ConnectionConfig {
            name: " ".into(),
            ..standalone()
        };
        assert_eq!(unnamed.location(), "127.0.0.1:6379 · db0");
        assert_eq!(s.suggested_name(), "mymaster");
        let u = unix("/var/run/redis.sock");
        assert_eq!(u.endpoint(), "/var/run/redis.sock");
        assert_eq!(u.suggested_name(), "redis.sock");
        assert_eq!(standalone().suggested_name(), "127.0.0.1");
        assert_eq!(ssh(SshAuth::Agent).label(), "ops@bastion");
    }

    #[test]
    fn parses_addresses() {
        assert_eq!(parse_address("h:1"), Ok(("h".into(), 1)));
        assert_eq!(
            parse_address(" 10.0.0.1:7000 "),
            Ok(("10.0.0.1".into(), 7000))
        );
        assert_eq!(parse_address("[::1]:26379"), Ok(("::1".into(), 26379)));
        for bad in [
            "", "h", "h:", ":1", "h:0", "h:65536", "::1:1", "[::1]", "[::1:1", "a b:1",
        ] {
            assert!(parse_address(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            split_addresses(" a:1, b:2\n[::1]:3,,  "),
            vec!["a:1", "b:2", "[::1]:3"]
        );
        assert!(split_addresses(" ,\n").is_empty());
    }

    fn any_topology() -> impl Strategy<Value = Topology> {
        let text = || "\\PC{0,12}";
        prop_oneof![
            Just(Topology::Standalone),
            proptest::collection::vec(text(), 0..4).prop_map(|seeds| Topology::Cluster { seeds }),
            (
                text(),
                proptest::collection::vec(text(), 0..4),
                proptest::option::of(text()),
                any::<bool>()
            )
                .prop_map(|(master, sentinels, sentinel_username, sentinel_tls)| {
                    Topology::Sentinel {
                        master,
                        sentinels,
                        sentinel_username,
                        sentinel_tls,
                    }
                }),
        ]
    }

    fn any_ssh() -> impl Strategy<Value = SshConfig> {
        let auth = prop_oneof![
            Just(SshAuth::Password),
            "\\PC{0,20}".prop_map(|path| SshAuth::PrivateKey { path: path.into() }),
            Just(SshAuth::Agent),
        ];
        let policy = prop_oneof![Just(HostKeyPolicy::Strict), Just(HostKeyPolicy::AcceptNew)];
        ("\\PC{0,12}", any::<u16>(), "\\PC{0,8}", auth, policy).prop_map(
            |(host, port, user, auth, host_key)| SshConfig {
                host,
                port,
                user,
                auth,
                host_key,
                password: Some("SECRET-ssh".into()),
                passphrase: Some("SECRET-key".into()),
            },
        )
    }

    proptest! {
        #[test]
        fn any_settings_round_trip_through_the_file_without_secrets(
            name in "\\PC{0,12}",
            topology in any_topology(),
            socket in proptest::option::of("\\PC{0,20}"),
            ssh in proptest::option::of(any_ssh()),
        ) {
            let config = ConnectionConfig {
                name,
                topology,
                unix_socket: socket.map(PathBuf::from),
                ssh,
                password: Some("SECRET-redis".into()),
                sentinel_password: Some("SECRET-sentinel".into()),
                ..ConnectionConfig::default()
            };
            let json = serde_json::to_string(&config).unwrap();
            prop_assert!(!json.contains("SECRET-"), "{}", json);
            let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, config.without_secrets());
        }

        #[test]
        fn addresses_round_trip(
            host in prop_oneof![
                "[a-z][a-z0-9.-]{0,20}",
                "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
                "[0-9a-f]{0,4}(:[0-9a-f]{0,4}){2,7}",
            ],
            port in 1u16..,
        ) {
            let text = format_address(&host, port);
            prop_assert_eq!(parse_address(&text), Ok((host.clone(), port)));
            prop_assert_eq!(split_addresses(&format!("{text}, {text}")), vec![text.clone(), text]);
        }

        #[test]
        fn address_parsing_never_panics(text in "\\PC{0,40}") {
            if let Ok((host, port)) = parse_address(&text) {
                prop_assert!(port > 0);
                prop_assert!(!host.is_empty());
                prop_assert!(!host.chars().any(char::is_whitespace));
            }
        }
    }
}
