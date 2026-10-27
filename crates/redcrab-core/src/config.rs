use anyhow::Result;
use redis::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo, RedisConnectionInfo};
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
        }
    }
}

impl ConnectionConfig {
    /// Parses `[redis://][user[:password]@]host[:port][/db]`.
    pub fn parse_url(url: &str) -> Result<Self> {
        let rest = url.strip_prefix("redis://").unwrap_or(url);
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

    pub(crate) fn connection_info(&self) -> Result<ConnectionInfo> {
        let mut redis = RedisConnectionInfo::default()
            .set_db(self.db.into())
            .set_lib_name("redcrab", env!("CARGO_PKG_VERSION"));
        if let Some(username) = self.username.as_deref().filter(|u| !u.is_empty()) {
            redis = redis.set_username(username);
        }
        if let Some(password) = self.password.as_deref().filter(|p| !p.is_empty()) {
            redis = redis.set_password(password);
        }
        Ok(ConnectionAddr::Tcp(self.host.clone(), self.port)
            .into_connection_info()?
            .set_redis_settings(redis))
    }
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
