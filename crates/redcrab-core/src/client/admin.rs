use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::future::join_all;
use redis::{Value, cmd, pipe};

use super::{Client, Conn, ConsoleSession, ServerInfo};
use crate::{
    runtime::spawn,
    value::{as_i64, into_string},
};

impl Client {
    pub fn ping(&self) -> impl Future<Output = Result<Duration>> + Send + 'static {
        let mut con = self.conn.clone();
        spawn(async move {
            let started = Instant::now();
            cmd("PING").query_async::<()>(&mut con).await?;
            Ok(started.elapsed())
        })
    }

    /// Version, mode, memory, clients and uptime from INFO, and the size of
    /// the selected database. A user without permission for INFO (ACL users
    /// often lack `@dangerous`) or DBSIZE gets those facts as `None` instead
    /// of an error, since the connection itself works.
    ///
    /// On a cluster the facts are summed over the primaries that answered
    /// (keys, memory, clients; the uptime is the shortest), the mode is
    /// "cluster" and [`ServerInfo::nodes`] says how many primaries answered.
    pub fn server_info(&self) -> impl Future<Output = Result<ServerInfo>> + Send + 'static {
        let con = self.conn.clone();
        spawn(async move {
            if !con.is_cluster() {
                return Ok(node_info(&mut con.clone()).await?.info);
            }
            let (nodes, _) = con.walk_nodes().await;
            let replies = join_all(
                nodes
                    .into_iter()
                    .map(|mut node| async move { node_info(&mut node).await }),
            )
            .await;
            let mut answered = Vec::new();
            let mut failure = None;
            for reply in replies {
                match reply {
                    Ok(info) => answered.push(info),
                    Err(err) => failure = Some(err),
                }
            }
            if answered.is_empty() {
                return Err(failure.unwrap_or_else(|| anyhow!("the cluster has no primaries")));
            }
            Ok(aggregate(&answered))
        })
    }

    /// A console session with its own connection, opened on its first
    /// command, so that SELECT, MULTI or WATCH typed into a console never
    /// affect the app's requests. Keep one per console view.
    pub fn console(&self) -> ConsoleSession {
        ConsoleSession::new(self.connector.clone(), self.config.clone())
    }
}

/// One server's INFO and DBSIZE, and its memory in bytes for summing.
struct NodeInfo {
    info: ServerInfo,
    used_memory: Option<u64>,
}

async fn node_info(con: &mut Conn) -> Result<NodeInfo> {
    let replies: Vec<Value> = pipe()
        .cmd("INFO")
        .cmd("DBSIZE")
        .ignore_errors()
        .query_async(con)
        .await?;
    let mut replies = replies.into_iter();
    let info = match permitted(replies.next())? {
        Value::Nil => None,
        info => Some(into_string(info)),
    };
    let db_size = match permitted(replies.next())? {
        Value::Nil => None,
        size => u64::try_from(as_i64(&size)).ok(),
    };
    let field = |name: &str| {
        info.as_deref()?
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let number = |name: &str| field(name)?.parse().ok();
    Ok(NodeInfo {
        info: ServerInfo {
            version: field("redis_version"),
            mode: field("redis_mode"),
            used_memory_human: field("used_memory_human"),
            connected_clients: number("connected_clients"),
            uptime_secs: number("uptime_in_seconds"),
            db_size,
            nodes: None,
        },
        used_memory: number("used_memory"),
    })
}

/// The facts of a cluster's primaries as one.
fn aggregate(nodes: &[NodeInfo]) -> ServerInfo {
    // A sum is only right if every node told.
    let sum = |get: fn(&NodeInfo) -> Option<u64>| -> Option<u64> {
        nodes.iter().map(get).sum::<Option<u64>>()
    };
    ServerInfo {
        version: nodes.iter().find_map(|n| n.info.version.clone()),
        mode: Some("cluster".into()),
        used_memory_human: sum(|n| n.used_memory).map(human_bytes),
        connected_clients: sum(|n| n.info.connected_clients),
        uptime_secs: nodes.iter().filter_map(|n| n.info.uptime_secs).min(),
        db_size: sum(|n| n.info.db_size),
        nodes: Some(nodes.len()),
    }
}

/// A byte count the way INFO's `used_memory_human` writes it: `912B`,
/// `1.20K`, `3.45M`, `2.00G`.
pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["K", "M", "G", "T", "P"];
    if n < 1024 {
        return format!("{n}B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2}{}", UNITS[unit])
}

/// A reply of a pipeline sent with `ignore_errors`: the value, nil if the
/// user lacks the permission for the command, or the error.
fn permitted(reply: Option<Value>) -> Result<Value> {
    match reply {
        Some(Value::ServerError(err)) if err.code() == "NOPERM" => Ok(Value::Nil),
        Some(Value::ServerError(err)) => Err(redis::RedisError::from(err).into()),
        Some(value) => Ok(value),
        None => Err(anyhow!("the server sent too few replies")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_like_info() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1024), "1.00K");
        assert_eq!(human_bytes(1_258_291), "1.20M");
        assert_eq!(human_bytes(3 << 30), "3.00G");
        assert_eq!(human_bytes(u64::MAX), "16384.00P");
    }

    #[test]
    fn cluster_facts_are_summed() {
        let node = |keys, memory, clients, uptime| NodeInfo {
            info: ServerInfo {
                version: Some("8.0.2".into()),
                mode: Some("cluster".into()),
                used_memory_human: Some("ignored".into()),
                connected_clients: Some(clients),
                uptime_secs: Some(uptime),
                db_size: keys,
                nodes: None,
            },
            used_memory: Some(memory),
        };
        let info = aggregate(&[node(Some(10), 1024, 2, 50), node(Some(5), 2048, 3, 40)]);
        assert_eq!(info.db_size, Some(15));
        assert_eq!(info.used_memory_human.as_deref(), Some("3.00K"));
        assert_eq!(info.connected_clients, Some(5));
        assert_eq!(info.uptime_secs, Some(40));
        assert_eq!(info.nodes, Some(2));
        assert_eq!(
            info.server_label().as_deref(),
            Some("Redis 8.0.2 · cluster")
        );
        // A node that would not say: no sum rather than a wrong one.
        let info = aggregate(&[node(Some(10), 1, 1, 1), node(None, 1, 1, 1)]);
        assert_eq!(info.db_size, None);
    }
}
