//! The server tools' requests: INFO, the slow log and the client list of one
//! server at a time ([`ServerNode`]). They read about the server, not its
//! keys; the two that change it (resetting the slow log, killing a client)
//! are refused on a read-only connection, like every other change.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use redis::{Value, cmd, pipe};

use super::{Client, Conn, ReadOnlyError};
use crate::{
    runtime::spawn,
    server::{ClientList, Info, SlowEntry, SlowLog},
    value::into_string,
};

/// One server the tools look at: the server itself, a Sentinel's current
/// master, or one primary of a cluster.
#[derive(Clone)]
pub struct ServerNode {
    conn: Conn,
    label: String,
    read_only: bool,
}

impl Client {
    /// The servers the tools can look at: this one, or each primary of a
    /// cluster.
    pub fn server_nodes(
        &self,
    ) -> impl Future<Output = Result<Vec<ServerNode>>> + Send + 'static + use<> {
        let conn = self.conn.clone();
        let read_only = self.config.read_only;
        let single = self.location();
        spawn(async move {
            if !conn.is_cluster() {
                return Ok(vec![ServerNode {
                    conn,
                    label: single,
                    read_only,
                }]);
            }
            let (nodes, _) = conn.walk_nodes().await;
            let mut nodes: Vec<ServerNode> = nodes
                .into_iter()
                .map(|conn| ServerNode {
                    label: conn.address().unwrap_or_default(),
                    conn,
                    read_only,
                })
                .collect();
            nodes.sort_by(|a, b| a.label.cmp(&b.label));
            Ok(nodes)
        })
    }
}

impl ServerNode {
    /// `host:port` of a cluster primary; where the connection goes
    /// otherwise.
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn info(&self) -> impl Future<Output = Result<Info>> + Send + 'static + use<> {
        let mut con = self.conn.clone();
        spawn(async move {
            let text: String = cmd("INFO")
                .query_async(&mut con)
                .await
                .context("INFO failed")?;
            Ok(Info::parse(&text))
        })
    }

    /// The `count` newest entries of the slow log, and how it is set up.
    /// The settings are left out where CONFIG GET is not allowed.
    pub fn slow_log(
        &self,
        count: usize,
    ) -> impl Future<Output = Result<SlowLog>> + Send + 'static + use<> {
        let mut con = self.conn.clone();
        spawn(async move {
            let replies: Vec<Value> = pipe()
                .cmd("SLOWLOG")
                .arg("GET")
                .arg(count)
                .cmd("SLOWLOG")
                .arg("LEN")
                .cmd("CONFIG")
                .arg("GET")
                .arg("slowlog-*")
                .ignore_errors()
                .query_async(&mut con)
                .await?;
            let mut replies = replies.into_iter();
            let entries = match replies.next() {
                Some(Value::ServerError(err)) => {
                    return Err(redis::RedisError::from(err)).context("SLOWLOG GET failed");
                }
                Some(Value::Array(entries)) => entries.into_iter().filter_map(slow_entry).collect(),
                _ => Vec::new(),
            };
            let len = match replies.next() {
                Some(Value::Int(n)) => u64::try_from(n).ok(),
                _ => None,
            };
            let mut log = SlowLog {
                entries,
                len,
                ..SlowLog::default()
            };
            if let Some(config) = replies.next() {
                for (name, value) in config_pairs(config) {
                    match name.as_str() {
                        "slowlog-log-slower-than" => log.slower_than_micros = value.parse().ok(),
                        "slowlog-max-len" => log.max_len = value.parse().ok(),
                        _ => {}
                    }
                }
            }
            Ok(log)
        })
    }

    /// Empties the slow log.
    pub fn reset_slow_log(&self) -> impl Future<Output = Result<()>> + Send + 'static + use<> {
        let mut con = self.conn.clone();
        let refused = self.refused();
        spawn(async move {
            if let Some(err) = refused {
                return Err(err.into());
            }
            cmd("SLOWLOG")
                .arg("RESET")
                .query_async::<()>(&mut con)
                .await
                .context("SLOWLOG RESET failed")
        })
    }

    /// Every client connected, and which of them is redcrab's own
    /// connection.
    pub fn clients(&self) -> impl Future<Output = Result<ClientList>> + Send + 'static + use<> {
        let mut con = self.conn.clone();
        spawn(async move {
            let replies: Vec<Value> = pipe()
                .cmd("CLIENT")
                .arg("LIST")
                .cmd("CLIENT")
                .arg("ID")
                .ignore_errors()
                .query_async(&mut con)
                .await?;
            let mut replies = replies.into_iter();
            let list = match replies.next() {
                Some(Value::ServerError(err)) => {
                    return Err(redis::RedisError::from(err)).context("CLIENT LIST failed");
                }
                Some(list) => into_string(list),
                None => return Err(anyhow!("the server sent no client list")),
            };
            let own_id = match replies.next() {
                Some(Value::Int(id)) => u64::try_from(id).ok(),
                _ => None,
            };
            Ok(ClientList::parse(&list, own_id))
        })
    }

    /// Disconnects the client `id`. `false` when it was gone already.
    pub fn kill_client(
        &self,
        id: u64,
    ) -> impl Future<Output = Result<bool>> + Send + 'static + use<> {
        let mut con = self.conn.clone();
        let refused = self.refused();
        spawn(async move {
            if let Some(err) = refused {
                return Err(err.into());
            }
            let killed: i64 = cmd("CLIENT")
                .arg("KILL")
                .arg("ID")
                .arg(id)
                .query_async(&mut con)
                .await
                .context("CLIENT KILL failed")?;
            Ok(killed > 0)
        })
    }

    fn refused(&self) -> Option<ReadOnlyError> {
        self.read_only.then(ReadOnlyError::default)
    }
}

/// One SLOWLOG GET entry: id, time, microseconds, arguments, and from Redis
/// 4 on the client's address and name.
fn slow_entry(entry: Value) -> Option<SlowEntry> {
    let Value::Array(fields) = entry else {
        return None;
    };
    let mut fields = fields.into_iter();
    let number = |v: Option<Value>| match v {
        Some(Value::Int(n)) => u64::try_from(n).ok(),
        _ => None,
    };
    let id = number(fields.next())?;
    let at = number(fields.next())?;
    let duration = Duration::from_micros(number(fields.next())?);
    let args = match fields.next() {
        Some(Value::Array(args)) => args
            .into_iter()
            .map(|arg| match arg {
                Value::BulkString(bytes) => bytes,
                other => into_string(other).into_bytes(),
            })
            .collect(),
        _ => Vec::new(),
    };
    let text = |v: Option<Value>| match v {
        Some(Value::BulkString(bytes)) => {
            Some(String::from_utf8_lossy(&bytes).into_owned()).filter(|s| !s.is_empty())
        }
        _ => None,
    };
    Some(SlowEntry {
        id,
        at,
        duration,
        args,
        client: text(fields.next()),
        client_name: text(fields.next()),
    })
}

/// CONFIG GET's reply as name and value: a flat array (RESP2) or a map
/// (RESP3). Nothing for an error, such as a managed service that renamed
/// CONFIG away.
fn config_pairs(reply: Value) -> Vec<(String, String)> {
    match reply {
        Value::Array(items) => items
            .chunks(2)
            .filter_map(|pair| match pair {
                [name, value] => Some((into_string(name.clone()), into_string(value.clone()))),
                _ => None,
            })
            .collect(),
        Value::Map(pairs) => pairs
            .into_iter()
            .map(|(name, value)| (into_string(name), into_string(value)))
            .collect(),
        _ => Vec::new(),
    }
}
