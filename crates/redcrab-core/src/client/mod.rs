//! The connection to one server and every request the app makes over it.
//!
//! Requests are split by topic: `keys` (scan and key-level operations),
//! `values` (reading and writing values), `admin` (server-level requests) and
//! `console` (the console's own session). They all talk to Redis through
//! [`Conn`], which hides the topology.

mod admin;
mod conn;
mod console;
mod keys;
mod values;

use std::{fmt, sync::Arc, time::Duration};

use anyhow::Result;
use redis::{
    ErrorKind,
    aio::{ConnectionManager, ConnectionManagerConfig},
    cmd,
};

pub(crate) use self::conn::Conn;
pub use self::console::ConsoleSession;
use crate::{
    commands::{CommandClass, READ_ONLY_SCRIPTS},
    config::ConnectionConfig,
    runtime::spawn,
};

/// Keys are binary-safe and shared between the tree, caches and requests.
pub type Key = Arc<[u8]>;

/// A cheap-to-clone handle to one Redis server. Every clone shares a single
/// multiplexed connection that reconnects on its own.
#[derive(Clone)]
pub struct Client {
    conn: Conn,
    /// The settings `conn` was opened with, for connections of its own (the
    /// console's). Shared, since it may hold certificate bytes.
    redis: Arc<redis::Client>,
    config: Arc<ConnectionConfig>,
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
    /// Keys in the selected database.
    pub db_size: Option<u64>,
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
/// The library's own message stays at the end of the chain.
fn connect_error(err: redis::RedisError, config: &ConnectionConfig) -> anyhow::Error {
    let message = err.to_string();
    let has = |needle: &str| message.contains(needle);
    let tls = config.tls.is_some();
    // redis-rs reports a refused AUTH as "Password authentication failed",
    // without the server's WRONGPASS.
    let auth_failed = err.kind() == ErrorKind::AuthenticationFailed;
    let hint = if has("UnknownIssuer") {
        Some("the server's certificate is not signed by a trusted CA; add its CA file, or allow untrusted certificates for a self-signed development server".to_string())
    } else if has("NotValidForName") {
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
    } else if auth_failed || has("WRONGPASS") || has("invalid username-password") {
        Some("wrong username or password".into())
    } else if has("Connection refused") {
        Some(format!("nothing is listening on {}", config.address()))
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
    err.context(format!("could not connect to {}", config.address()))
}

impl Client {
    pub async fn connect(config: ConnectionConfig) -> Result<Self> {
        spawn(async move {
            // Reads certificate files, so it runs here rather than on the caller's thread.
            let client = config.client()?;
            let manager_config = ConnectionManagerConfig::new()
                .set_connection_timeout(Some(Duration::from_secs(5)))
                .set_response_timeout(Some(Duration::from_secs(30)))
                .set_number_of_retries(1);
            let mut con = ConnectionManager::new_with_config(client.clone(), manager_config)
                .await
                .map_err(|err| connect_error(err, &config))?;
            // Without credentials the handshake succeeds even where the
            // server requires them; only the first command gets NOAUTH. Ask
            // now, so that a connection nothing can be done with fails here,
            // with a hint, and not on every request after.
            match cmd("PING").query_async::<()>(&mut con).await {
                // A user that may not PING is still logged in.
                Err(err) if err.code() != Some("NOPERM") => {
                    return Err(connect_error(err, &config));
                }
                _ => {}
            }
            Ok(Self {
                conn: Conn::Single(con),
                redis: Arc::new(client),
                config: Arc::new(config),
            })
        })
        .await
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
