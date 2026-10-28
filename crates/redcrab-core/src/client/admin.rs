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
    /// "cluster", and [`ServerInfo::nodes`] and [`ServerInfo::primaries`]
    /// say how many primaries answered out of how many were asked. Sums
    /// that leave out part of the cluster (a primary that did not answer,
    /// slots without one) say so in [`ServerInfo::partial`].
    pub fn server_info(&self) -> impl Future<Output = Result<ServerInfo>> + Send + 'static {
        let con = self.conn.clone();
        spawn(async move {
            if !con.is_cluster() {
                return Ok(node_info(&mut con.clone()).await?.info);
            }
            let (nodes, unowned) = con.walk_nodes().await;
            let asked = nodes.len();
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
            if let Some(err) = failure.filter(|_| answered.len() < asked) {
                tracing::warn!(
                    "{} of {asked} cluster primaries did not answer INFO: {err:#}",
                    asked - answered.len()
                );
            }
            Ok(aggregate(&answered, asked, unowned))
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
            ..ServerInfo::default()
        },
        used_memory: number("used_memory"),
    })
}

/// The facts of a cluster's primaries as one: `nodes` answered of
/// `asked`, and `unowned` slots had no primary to ask.
fn aggregate(nodes: &[NodeInfo], asked: usize, unowned: usize) -> ServerInfo {
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
        primaries: Some(asked),
        unowned_slots: unowned,
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
                ..ServerInfo::default()
            },
            used_memory: Some(memory),
        };
        let info = aggregate(
            &[node(Some(10), 1024, 2, 50), node(Some(5), 2048, 3, 40)],
            2,
            0,
        );
        assert_eq!(info.db_size, Some(15));
        assert_eq!(info.used_memory_human.as_deref(), Some("3.00K"));
        assert_eq!(info.connected_clients, Some(5));
        assert_eq!(info.uptime_secs, Some(40));
        assert_eq!(info.nodes, Some(2));
        assert_eq!(
            info.server_label().as_deref(),
            Some("Redis 8.0.2 · cluster")
        );
        assert_eq!(info.partial(), None);
        assert_eq!(
            info.summary(),
            [
                "Redis 8.0.2 · cluster",
                "15 keys",
                "3.00K used",
                "5 clients"
            ]
        );
        // A node that would not say: no sum rather than a wrong one.
        let info = aggregate(&[node(Some(10), 1, 1, 1), node(None, 1, 1, 1)], 2, 0);
        assert_eq!(info.db_size, None);
    }

    #[test]
    fn partial_cluster_sums_say_so() {
        let node = |keys| NodeInfo {
            info: ServerInfo {
                version: Some("8.0.2".into()),
                db_size: Some(keys),
                connected_clients: Some(2),
                ..ServerInfo::default()
            },
            used_memory: Some(2048),
        };
        // One primary of three did not answer.
        let info = aggregate(&[node(1000), node(500)], 3, 0);
        assert_eq!(info.db_size, Some(1500));
        assert_eq!((info.nodes, info.primaries), (Some(2), Some(3)));
        assert_eq!(info.partial().as_deref(), Some("2 of 3 primaries answered"));
        assert_eq!(
            info.summary(),
            [
                "Redis 8.0.2 · cluster",
                "2 of 3 primaries answered",
                "at least 1,500 keys",
                "at least 4.00K used",
                "at least 4 clients",
            ]
        );
        // Slots without a primary leave their keys out too.
        let info = aggregate(&[node(1), node(1), node(1)], 3, 1);
        assert_eq!(info.partial().as_deref(), Some("1 slot has no primary"));
        let info = aggregate(&[node(1)], 3, 5461);
        assert_eq!(
            info.partial().as_deref(),
            Some("1 of 3 primaries answered, 5,461 slots have no primary")
        );
        // A single server is never partial.
        assert_eq!(node(1).info.partial(), None);
    }
}
