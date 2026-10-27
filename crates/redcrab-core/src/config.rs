use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use redis::{ConnectionAddr, IntoConnectionInfo, RedisConnectionInfo};
use serde::{Deserialize, Serialize};

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
    fn certificates(&self) -> Result<Option<redis::TlsCertificates>> {
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

    /// Builds the redis-rs client, reading any TLS certificate files.
    pub(crate) fn client(&self) -> Result<redis::Client> {
        let mut redis = RedisConnectionInfo::default()
            .set_db(self.db.into())
            .set_lib_name("redcrab", env!("CARGO_PKG_VERSION"));
        if let Some(username) = self.username.as_deref().filter(|u| !u.is_empty()) {
            redis = redis.set_username(username);
        }
        if let Some(password) = self.password.as_deref().filter(|p| !p.is_empty()) {
            redis = redis.set_password(password);
        }
        let addr = match &self.tls {
            None => ConnectionAddr::Tcp(self.host.clone(), self.port),
            Some(tls) => {
                install_crypto_provider();
                ConnectionAddr::TcpTls {
                    host: self.host.clone(),
                    port: self.port,
                    insecure: tls.insecure,
                    tls_params: None,
                }
            }
        };
        let info = addr.into_connection_info()?.set_redis_settings(redis);
        let certificates = self
            .tls
            .as_ref()
            .map(TlsConfig::certificates)
            .transpose()?
            .flatten();
        Ok(match certificates {
            Some(certificates) => redis::Client::build_with_tls(info, certificates)?,
            None => redis::Client::open(info)?,
        })
    }
}

/// rustls needs a process-wide crypto provider when the dependency graph does
/// not settle on exactly one. Installing one twice is harmless.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
