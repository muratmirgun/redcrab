mod connectivity;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use redis::{ConnectionAddr, IntoConnectionInfo, RedisConnectionInfo};
use serde::{Deserialize, Serialize};

pub use self::connectivity::{
    ConfigField, HostKeyPolicy, InvalidConfig, SshAuth, SshConfig, Topology, UNIX_SOCKETS,
    format_address, parse_address, split_addresses,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    /// Never written to disk; saved passwords live in the credential store
    /// (see [`crate::store`]). Still read, so old files can be migrated.
    #[serde(skip_serializing)]
    pub password: Option<String>,
    pub db: u32,
    /// Splits key names into folders in the key tree.
    pub separator: char,
    /// `None` connects over plain TCP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
    /// Refuses every change to the server, in the app and in the console
    /// (see [`crate::Client::ensure_writable`]).
    pub read_only: bool,
    /// Tags the connection so a production server is hard to mistake.
    pub environment: Environment,
    /// One server, a cluster, or a master found through Sentinel.
    pub topology: Topology,
    /// Connect to this local socket instead of `host`/`port` (standalone
    /// servers only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unix_socket: Option<PathBuf>,
    /// Reach `host`/`port` through this SSH server (standalone servers only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    /// The password for the sentinels of a [`Topology::Sentinel`]
    /// connection. Never written to disk; it lives in the credential store.
    #[serde(skip)]
    pub sentinel_password: Option<String>,
}

/// What a server is used for, shown as a colored tag. Stored in lower case
/// (`"production"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Environment {
    #[default]
    None,
    Development,
    Staging,
    Production,
}

impl Environment {
    pub const ALL: [Self; 4] = [
        Self::None,
        Self::Development,
        Self::Staging,
        Self::Production,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Development => "Development",
            Self::Staging => "Staging",
            Self::Production => "Production",
        }
    }

    /// The name in saved files.
    fn key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }
}

impl Serialize for Environment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.key())
    }
}

/// Tags this version does not know (written by a newer one) load as `None`
/// instead of making the whole connections file unreadable.
impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let key = Option::<String>::deserialize(deserializer)?.unwrap_or_default();
        Ok(Self::ALL
            .into_iter()
            .find(|environment| environment.key().eq_ignore_ascii_case(&key))
            .unwrap_or_default())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// Accept any server certificate. Only for self-signed development
    /// servers: it gives up protection against interception.
    pub insecure: bool,
    /// PEM file with the CA to trust instead of the system roots.
    pub ca_file: Option<PathBuf>,
    /// PEM client certificate and key, for servers that require mutual TLS.
    pub client_cert_file: Option<PathBuf>,
    pub client_key_file: Option<PathBuf>,
}

impl TlsConfig {
    pub(crate) fn certificates(&self) -> Result<Option<redis::TlsCertificates>> {
        let read = |path: &Path, what: &str| {
            std::fs::read(path)
                .with_context(|| format!("could not read the {what} {}", path.display()))
        };
        let client_tls = match (&self.client_cert_file, &self.client_key_file) {
            (Some(cert), Some(key)) => Some(redis::ClientTlsConfig {
                client_cert: read(cert, "client certificate")?,
                client_key: read(key, "client key")?,
            }),
            (None, None) => None,
            _ => bail!("mutual TLS needs both a client certificate and a client key"),
        };
        let root_cert = self
            .ca_file
            .as_deref()
            .map(|p| read(p, "CA certificate"))
            .transpose()?;
        Ok(
            (client_tls.is_some() || root_cert.is_some()).then_some(redis::TlsCertificates {
                client_tls,
                root_cert,
            }),
        )
    }
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            name: "localhost".into(),
            host: "127.0.0.1".into(),
            port: 6379,
            username: None,
            password: None,
            db: 0,
            separator: ':',
            tls: None,
            read_only: false,
            environment: Environment::None,
            topology: Topology::Standalone,
            unix_socket: None,
            ssh: None,
            sentinel_password: None,
        }
    }
}

impl ConnectionConfig {
    /// Parses `[redis[s]://][user[:password]@]host[:port][/db][#insecure]`.
    /// `rediss://` means TLS, and `#insecure` skips certificate checks, as in
    /// redis-cli and redis-rs.
    pub fn parse_url(url: &str) -> Result<Self> {
        let (rest, insecure) = match url.split_once('#') {
            Some((rest, "insecure")) => (rest, true),
            Some((_, other)) => bail!("unsupported URL fragment #{other}; only #insecure is known"),
            None => (url, false),
        };
        let (rest, tls) = match rest.strip_prefix("rediss://") {
            Some(rest) => (
                rest,
                Some(TlsConfig {
                    insecure,
                    ..Default::default()
                }),
            ),
            None => {
                anyhow::ensure!(!insecure, "#insecure only applies to rediss:// URLs");
                (rest.strip_prefix("redis://").unwrap_or(rest), None)
            }
        };
        let (auth, rest) = match rest.rsplit_once('@') {
            Some((auth, rest)) => (Some(auth), rest),
            None => (None, rest),
        };
        let (address, db) = match rest.split_once('/') {
            Some((address, db)) if !db.is_empty() => (address, db.parse()?),
            Some((address, _)) => (address, 0),
            None => (rest, 0),
        };
        let (host, port) = match address.rsplit_once(':') {
            Some((host, port)) => (host, port.parse()?),
            None => (address, 6379),
        };
        anyhow::ensure!(!host.is_empty(), "missing host in {url}");
        let (username, password) = match auth.map(|a| a.split_once(':')) {
            Some(Some((user, pass))) => (Some(user), Some(pass)),
            // A lone credential is a password, like redis-cli's `-a`.
            Some(None) => (None, auth),
            None => (None, None),
        };
        let owned = |s: Option<&str>| s.filter(|s| !s.is_empty()).map(str::to_string);
        Ok(Self {
            name: address.to_string(),
            host: host.to_string(),
            port,
            username: owned(username),
            password: owned(password),
            db,
            tls,
            ..Self::default()
        })
    }

    /// The settings to open a URL start (see [`Self::parse_url`]) with, when
    /// `saved` holds a connection of the same name. A URL start is named
    /// after its address, so running the same URL again finds the entry the
    /// first run saved, which the user may since have tagged or made
    /// read-only.
    ///
    /// A URL only says where to connect and how to log in, so that is all it
    /// changes; everything else (separator, TLS files, environment) comes
    /// from the saved connection. It can never lower a guard rail: a
    /// read-only connection stays read-only, its environment tag stays, and
    /// TLS stays on even when the URL leaves out `rediss://`, since sending
    /// the saved password in clear text would be worse than failing to
    /// connect. A URL cannot express a topology, a Unix socket or an SSH
    /// tunnel, so those come from the saved connection too: a URL never
    /// skips the tunnel the server is reached through.
    pub fn with_saved(self, saved: &[ConnectionConfig]) -> ConnectionConfig {
        let Some(saved) = saved.iter().find(|c| c.name == self.name) else {
            return self;
        };
        // Without credentials in the URL the saved user logs in, with the
        // password saved for it.
        let credentials = self.username.is_some() || self.password.is_some();
        ConnectionConfig {
            name: saved.name.clone(),
            host: self.host,
            port: self.port,
            username: if credentials {
                self.username
            } else {
                saved.username.clone()
            },
            password: self.password,
            db: self.db,
            separator: saved.separator,
            tls: match (self.tls, &saved.tls) {
                (Some(url), Some(saved)) => Some(TlsConfig {
                    insecure: url.insecure || saved.insecure,
                    ..saved.clone()
                }),
                (url, saved) => url.or_else(|| saved.clone()),
            },
            read_only: self.read_only || saved.read_only,
            environment: match self.environment {
                Environment::None => saved.environment,
                tagged => tagged,
            },
            topology: saved.topology.clone(),
            unix_socket: saved.unix_socket.clone(),
            ssh: saved.ssh.clone(),
            sentinel_password: None,
        }
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The separator as a single byte; non-ASCII separators fall back to `:`.
    pub fn separator_byte(&self) -> u8 {
        if self.separator.is_ascii() {
            self.separator as u8
        } else {
            b':'
        }
    }

    /// The per-connection settings every server of this connection gets:
    /// database, user, password and the library name shown in CLIENT LIST.
    pub(crate) fn redis_settings(&self) -> RedisConnectionInfo {
        let mut redis = RedisConnectionInfo::default()
            .set_db(self.db.into())
            .set_lib_name("redcrab", env!("CARGO_PKG_VERSION"));
        if let Some(username) = self.username.as_deref().filter(|u| !u.is_empty()) {
            redis = redis.set_username(username);
        }
        if let Some(password) = self.password.as_deref().filter(|p| !p.is_empty()) {
            redis = redis.set_password(password);
        }
        redis
    }

    /// Builds the redis-rs client for the configured server: the Unix
    /// socket, or `host`/`port`. Reads any TLS certificate files.
    pub(crate) fn client(&self) -> Result<redis::Client> {
        match &self.unix_socket {
            Some(path) => self.client_for(Target::Unix(path)),
            None => self.client_for(Target::Tcp(&self.host, self.port)),
        }
    }

    /// Builds the redis-rs client for one server of this connection, with
    /// its credentials and TLS settings. Reads any TLS certificate files.
    pub(crate) fn client_for(&self, target: Target<'_>) -> Result<redis::Client> {
        let (host, port, tunneled) = match target {
            Target::Unix(path) => {
                let info = ConnectionAddr::Unix(path.to_path_buf())
                    .into_connection_info()?
                    .set_redis_settings(self.redis_settings());
                return Ok(redis::Client::open(info)?);
            }
            Target::Tcp(host, port) => (host.to_string(), port, false),
            Target::Tunnel(local) => (local.ip().to_string(), local.port(), true),
        };
        let Some(tls) = &self.tls else {
            let info = ConnectionAddr::Tcp(host, port)
                .into_connection_info()?
                .set_redis_settings(self.redis_settings());
            return Ok(redis::Client::open(info)?);
        };
        install_crypto_provider();
        let info = ConnectionAddr::TcpTls {
            host,
            port,
            insecure: tls.insecure,
            tls_params: None,
        }
        .into_connection_info()?
        .set_redis_settings(self.redis_settings());
        let client = match tls.certificates()? {
            Some(certificates) => redis::Client::build_with_tls(info, certificates)?,
            None => redis::Client::open(info)?,
        };
        if !tunneled {
            return Ok(client);
        }
        // Through an SSH tunnel the client connects to 127.0.0.1 while the
        // certificate names the Redis host, and redis-rs checks the name
        // against the address it connects to (it has no way to connect to
        // one address and check another name). So only here the name check
        // is skipped; the chain is still verified, against the CA file,
        // which `validate` requires through a tunnel: with the system's
        // roots any public certificate would pass, and the SSH host key
        // only vouches for the bastion, not for its hop to Redis. See "SSH
        // tunnels" in the README.
        let info = client.get_connection_info().clone();
        let mut addr = info.addr().clone();
        addr.set_danger_accept_invalid_hostnames(true);
        Ok(redis::Client::open(info.set_addr(addr))?)
    }
}

/// The server a redis-rs client is built for (see
/// [`ConnectionConfig::client_for`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Target<'a> {
    Tcp(&'a str, u16),
    Unix(&'a Path),
    /// The local end of an SSH tunnel to the configured host and port.
    Tunnel(SocketAddr),
}

/// rustls needs a process-wide crypto provider when the dependency graph does
/// not settle on exactly one. Installing one twice is harmless.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc};

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

    use super::*;

    /// `tests/fixtures/tls-name`: a CA, and a server certificate it signed
    /// that names only `redis.internal`, valid until 2126.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls-name")
            .join(name)
    }

    fn with_ca() -> ConnectionConfig {
        ConnectionConfig {
            host: "redis.internal".into(),
            tls: Some(TlsConfig {
                ca_file: Some(fixture("ca.crt")),
                ..TlsConfig::default()
            }),
            ..ConnectionConfig::default()
        }
    }

    /// Whether the client built for `target` skips the certificate's name.
    fn skips_name_check(config: &ConnectionConfig, target: Target<'_>) -> bool {
        let client = config.client_for(target).unwrap();
        format!("{:?}", client.get_connection_info().addr())
            .contains("danger_accept_invalid_hostnames: true")
    }

    #[test]
    fn the_name_check_is_skipped_only_through_a_tunnel() {
        let tunnel = || Target::Tunnel("127.0.0.1:6379".parse().unwrap());
        let direct = || Target::Tcp("redis.internal", 6379);
        let system_roots = ConnectionConfig {
            tls: Some(TlsConfig::default()),
            ..with_ca()
        };
        for config in [with_ca(), system_roots] {
            assert!(skips_name_check(&config, tunnel()));
            assert!(!skips_name_check(&config, direct()));
        }
        assert!(!skips_name_check(
            &ConnectionConfig::default(),
            Target::Tunnel("127.0.0.1:6379".parse().unwrap())
        ));
    }

    /// A TLS server of the test's own presenting the fixture certificate
    /// (for `redis.internal` only), answering every command the way Redis
    /// answers PING or CLIENT SETINFO: enough for a client to connect.
    fn named_tls_server(runtime: &tokio::runtime::Runtime) -> SocketAddr {
        install_crypto_provider();
        let read = |name| std::fs::read(fixture(name)).unwrap();
        let certificates = CertificateDer::pem_slice_iter(&read("server.crt"))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(&read("server.key")).unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        runtime.spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(socket).await else {
                        return;
                    };
                    let (read, mut write) = tokio::io::split(stream);
                    let mut read = BufReader::new(read);
                    let mut line = String::new();
                    loop {
                        // `*N`, then N times `$len` and the argument.
                        line.clear();
                        let Ok(n) = read.read_line(&mut line).await else {
                            return;
                        };
                        let Some(count) = line.trim().strip_prefix('*') else {
                            return;
                        };
                        let (Ok(count), true) = (count.parse::<usize>(), n > 0) else {
                            return;
                        };
                        let mut name = Vec::new();
                        for ix in 0..count {
                            line.clear();
                            let _ = read.read_line(&mut line).await;
                            let len: usize = line.trim()[1..].parse().unwrap_or(0);
                            let mut arg = vec![0; len + 2];
                            if read.read_exact(&mut arg).await.is_err() {
                                return;
                            }
                            if ix == 0 {
                                name = arg[..len].to_ascii_uppercase();
                            }
                        }
                        let reply: &[u8] = if name == b"PING" {
                            b"+PONG\r\n"
                        } else {
                            b"+OK\r\n"
                        };
                        if write.write_all(reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    #[test]
    fn only_a_tunneled_connection_accepts_another_name() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let server = named_tls_server(&runtime);
        let ping = |client: redis::Client| {
            runtime.block_on(async move {
                let mut con = client.get_multiplexed_async_connection().await?;
                redis::cmd("PING").query_async::<String>(&mut con).await
            })
        };
        let config = with_ca();

        // Through a tunnel the client connects to 127.0.0.1, which the
        // certificate does not name; the chain, signed by the CA, passes.
        let tunneled = config.client_for(Target::Tunnel(server)).unwrap();
        assert_eq!(ping(tunneled).unwrap(), "PONG");
        // The same server reached directly: the name is checked, and
        // 127.0.0.1 is not it.
        let direct = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: server.port(),
            ..config.clone()
        };
        let err = match futures::executor::block_on(crate::Client::connect(direct)) {
            Ok(_) => panic!("the name was not checked"),
            Err(err) => format!("{err:#}"),
        };
        assert!(
            err.contains("the server's certificate is not valid for 127.0.0.1"),
            "{err}"
        );
        // Through the tunnel the chain still counts: the system's roots do
        // not know the fixture CA (and `validate` refuses this setting).
        let system_roots = ConnectionConfig {
            tls: Some(TlsConfig::default()),
            ..config
        };
        let err = ping(system_roots.client_for(Target::Tunnel(server)).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("UnknownIssuer"), "{err}");
        runtime.shutdown_background();
    }

    #[test]
    fn parses_urls() {
        let c = ConnectionConfig::parse_url("redis://app:s3cret@cache.local:6380/2").unwrap();
        assert_eq!(
            (
                c.host.as_str(),
                c.port,
                c.db,
                c.username.as_deref(),
                c.password.as_deref()
            ),
            ("cache.local", 6380, 2, Some("app"), Some("s3cret"))
        );
        let c = ConnectionConfig::parse_url("localhost").unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.db, c.password),
            ("localhost", 6379, 0, None)
        );
        let c = ConnectionConfig::parse_url("redis://:pw@h:1/").unwrap();
        assert_eq!((c.username, c.password.as_deref()), (None, Some("pw")));
        assert!(ConnectionConfig::parse_url("h:notaport").is_err());
    }

    #[test]
    fn parses_tls_urls() {
        let c =
            ConnectionConfig::parse_url("rediss://default:pw@cache.example.com:6380/1").unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.db),
            ("cache.example.com", 6380, 1)
        );
        assert_eq!(c.tls, Some(TlsConfig::default()));
        let c = ConnectionConfig::parse_url("rediss://127.0.0.1:6390#insecure").unwrap();
        assert!(c.tls.unwrap().insecure);
        assert!(ConnectionConfig::parse_url("redis://h#insecure").is_err());
        assert!(ConnectionConfig::parse_url("rediss://h#nope").is_err());
        assert!(
            ConnectionConfig::parse_url("redis://h")
                .unwrap()
                .tls
                .is_none()
        );
    }

    #[test]
    fn url_starts_keep_the_guard_rails_of_the_saved_connection() {
        let saved = vec![
            ConnectionConfig {
                name: "other".into(),
                ..Default::default()
            },
            ConnectionConfig {
                name: "prod-cache:6379".into(),
                host: "prod-cache".into(),
                port: 6379,
                username: Some("app".into()),
                db: 3,
                separator: '/',
                tls: Some(TlsConfig {
                    insecure: false,
                    ca_file: Some("/etc/ca.pem".into()),
                    client_cert_file: Some("/etc/cert.pem".into()),
                    client_key_file: Some("/etc/key.pem".into()),
                }),
                read_only: true,
                environment: Environment::Production,
                ..Default::default()
            },
        ];
        let url = |url: &str| ConnectionConfig::parse_url(url).unwrap();

        // The same URL again: only what the URL says changes.
        let merged = url("redis://prod-cache:6379").with_saved(&saved);
        assert!(merged.read_only);
        assert_eq!(merged.environment, Environment::Production);
        assert_eq!(merged.name, "prod-cache:6379");
        assert_eq!(merged.username.as_deref(), Some("app"));
        assert_eq!(merged.password, None);
        assert_eq!(merged.separator, '/');
        assert_eq!(merged.db, 0, "the URL picks the database");
        // `redis://` cannot switch TLS off and send the password in clear text.
        assert_eq!(merged.tls, saved[1].tls);

        // Credentials and TLS options the URL gives win; the files stay.
        let merged = url("rediss://ops:pw@prod-cache:6379/5#insecure").with_saved(&saved);
        assert_eq!(
            (merged.username.as_deref(), merged.password.as_deref()),
            (Some("ops"), Some("pw"))
        );
        assert_eq!(merged.db, 5);
        let tls = merged.tls.unwrap();
        assert!(tls.insecure);
        assert_eq!(tls.ca_file.as_deref(), Some(Path::new("/etc/ca.pem")));
        assert_eq!(
            tls.client_key_file.as_deref(),
            Some(Path::new("/etc/key.pem"))
        );
        assert!(merged.read_only);
        // A lone password logs in as the default user, like redis-cli's `-a`.
        let merged = url("redis://:pw@prod-cache:6379").with_saved(&saved);
        assert_eq!(merged.username, None);

        // A saved connection without the guard rails gains nothing, and an
        // insecure one stays insecure.
        let plain = vec![ConnectionConfig {
            name: "dev:6390".into(),
            tls: Some(TlsConfig {
                insecure: true,
                ..Default::default()
            }),
            ..Default::default()
        }];
        let merged = url("rediss://dev:6390").with_saved(&plain);
        assert!(!merged.read_only);
        assert_eq!(merged.environment, Environment::None);
        assert!(merged.tls.unwrap().insecure);

        // Nothing saved under that name: the URL as it is.
        let fresh = url("redis://elsewhere:6379/2");
        assert_eq!(fresh.clone().with_saved(&saved), fresh);
    }

    #[test]
    fn never_serializes_passwords() {
        let config = ConnectionConfig {
            password: Some("s3cret".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("s3cret") && !json.contains("password"),
            "{json}"
        );

        // Files written by older versions still load, so they can be migrated.
        let old: ConnectionConfig =
            serde_json::from_str(r#"{"name":"x","password":"s3cret"}"#).unwrap();
        assert_eq!(old.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn files_without_read_only_or_environment_still_load() {
        // A connection as the previous version saved it.
        let old: Vec<ConnectionConfig> = serde_json::from_str(
            r#"[{"name":"prod","host":"cache.example.com","port":6380,"username":"app","db":2,"separator":":","tls":{"insecure":false,"ca_file":"/etc/ca.pem","client_cert_file":null,"client_key_file":null}}]"#,
        )
        .unwrap();
        let old = &old[0];
        assert_eq!(
            (old.host.as_str(), old.port, old.db),
            ("cache.example.com", 6380, 2)
        );
        assert!(old.tls.is_some());
        assert!(!old.read_only);
        assert_eq!(old.environment, Environment::None);
    }

    #[test]
    fn read_only_and_environment_round_trip() {
        for environment in Environment::ALL {
            let config = ConnectionConfig {
                read_only: true,
                environment,
                ..Default::default()
            };
            let json = serde_json::to_string(&config).unwrap();
            assert!(json.contains(r#""read_only":true"#), "{json}");
            let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(back, config);
        }
        let json = serde_json::to_string(&Environment::Production).unwrap();
        assert_eq!(json, r#""production""#);
    }

    #[test]
    fn unknown_environments_load_as_none() {
        // Written by a newer version with more tags: the rest of the file must survive.
        for value in [r#""qa""#, "null", r#""""#] {
            let json = format!(r#"{{"name":"x","environment":{value},"read_only":true}}"#);
            let config: ConnectionConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(config.environment, Environment::None, "{json}");
            assert!(config.read_only);
        }
        let config: ConnectionConfig =
            serde_json::from_str(r#"{"environment":"Staging"}"#).unwrap();
        assert_eq!(config.environment, Environment::Staging);
    }
}
