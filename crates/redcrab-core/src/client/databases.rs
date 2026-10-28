//! The numbered databases of a server, and switching between them.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use redis::{Value, cmd, pipe};

use super::Client;
use crate::{
    config::ConnectionConfig,
    format,
    runtime::spawn,
    value::{into_string, pairs},
};

/// How many databases a server has when it does not say (`CONFIG GET` is
/// refused to ACL users without `@dangerous`, and renamed away on some
/// managed services): the Redis default.
pub const DEFAULT_DATABASES: u32 = 16;
/// The most databases [`Client::databases`] lists. Redis takes any count
/// in its configuration; a list a million long helps nobody.
pub const MAX_DATABASES: u32 = 1 << 16;

/// One numbered database of a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbInfo {
    pub index: u32,
    /// Keys in it; `None` when the server did not say (a user without
    /// permission for INFO).
    pub keys: Option<u64>,
    /// Keys in it with a TTL.
    pub expires: Option<u64>,
}

impl DbInfo {
    /// "db3 · 1,234", or just "db3" when the size is not known.
    pub fn label(&self) -> String {
        match self.keys {
            Some(keys) => format!("db{} · {}", self.index, format::count(keys)),
            None => format!("db{}", self.index),
        }
    }
}

impl Client {
    /// Every database of the server, with its key count from
    /// `INFO keyspace`. The number of databases comes from
    /// `CONFIG GET databases`, or is [`DEFAULT_DATABASES`] when the server
    /// will not say, and always covers the databases `INFO` lists. A
    /// cluster node has database 0 only. A user refused INFO (it is
    /// `@dangerous`) costs one more round trip, to `CLUSTER INFO`, so that
    /// a cluster node is still told from a plain server. A cluster client
    /// ([`Client::topology`]) asks nothing: it has database 0 only, and its
    /// size is the sum over the primaries ([`Client::server_info`]).
    pub fn databases(&self) -> impl Future<Output = Result<Vec<DbInfo>>> + Send + 'static {
        let mut con = self.conn.clone();
        let cluster_client = !self.config.topology.supports_databases();
        spawn(async move {
            if cluster_client {
                return Ok(list_databases(None, None, true));
            }
            let replies: Vec<Value> = pipe()
                .cmd("CONFIG")
                .arg("GET")
                .arg("databases")
                .cmd("INFO")
                .arg("keyspace")
                .cmd("INFO")
                .arg("cluster")
                .ignore_errors()
                .query_async(&mut con)
                .await?;
            let [config, keyspace, cluster] = <[Value; 3]>::try_from(replies)
                .map_err(|r| anyhow!("the server sent {} replies to 3 commands", r.len()))?;
            // Any refusal (NOPERM, a renamed command) only means the server
            // did not tell.
            let told = |reply: Value| match reply {
                Value::ServerError(_) => None,
                reply => Some(reply),
            };
            let configured = told(config).and_then(configured_databases);
            let keyspace = told(keyspace).map(|info| parse_keyspace(&into_string(info)));
            let cluster = match told(cluster) {
                Some(info) => info_field(&into_string(info), "cluster_enabled") == Some("1"),
                // Not `@dangerous`, so a user without INFO usually may run
                // it; a plain server refuses it ("cluster support
                // disabled"), as does an ACL that denies it, and then the
                // server is taken for a plain one, as before.
                None => cmd("CLUSTER")
                    .arg("INFO")
                    .query_async::<Value>(&mut con)
                    .await
                    .ok()
                    .and_then(told)
                    .is_some_and(is_cluster_info),
            };
            Ok(list_databases(configured, keyspace.as_ref(), cluster))
        })
    }

    /// A new client for database `db` of the same server, with the same
    /// settings (read-only included) and a connection of its own; this
    /// client stays on its database. Not for a cluster, which has database
    /// 0 only: the server refuses any other ("SELECT is not allowed in
    /// cluster mode").
    ///
    /// Through an SSH tunnel the new client shares this one's tunnel: no
    /// second login, and the host key stays checked against the known_hosts
    /// files and agent this client was connected with
    /// ([`ConnectOptions`](super::ConnectOptions)), not the defaults.
    pub fn with_db(&self, db: u32) -> impl Future<Output = Result<Client>> + Send + 'static {
        let config = ConnectionConfig {
            db,
            ..(*self.config).clone()
        };
        let tunnel = self.connector.shared_tunnel().cloned();
        async move {
            let client = match tunnel {
                Some(tunnel) => {
                    spawn(async move {
                        config.validate()?;
                        let (client, cluster_node) =
                            Client::connect_single_over(Arc::new(config), Some(tunnel)).await?;
                        // As `Client::connect` says for a cluster node
                        // behind a tunnel.
                        anyhow::ensure!(
                            !cluster_node,
                            "{} is a node of a Redis Cluster, which redcrab cannot reach through an SSH tunnel yet",
                            client.config.route_label()
                        );
                        Ok(client)
                    })
                    .await
                }
                None => Client::connect(config).await,
            };
            client.with_context(|| format!("could not switch to database {db}"))
        }
    }
}

/// The databases to list: `configured` many (or the default), at least
/// those `keyspace` mentions, capped at [`MAX_DATABASES`]; one on a
/// cluster. Sizes come from `keyspace`: zero for a database it does not
/// list, unknown without it.
fn list_databases(
    configured: Option<u32>,
    keyspace: Option<&BTreeMap<u32, Sizes>>,
    cluster: bool,
) -> Vec<DbInfo> {
    let listed = keyspace
        .and_then(|k| k.keys().next_back())
        .map_or(0, |&last| last.saturating_add(1));
    let count = if cluster {
        1
    } else {
        configured
            .unwrap_or(DEFAULT_DATABASES)
            .max(listed)
            .clamp(1, MAX_DATABASES)
    };
    (0..count)
        .map(|index| {
            let sizes = keyspace.map(|k| k.get(&index).copied().unwrap_or_default());
            DbInfo {
                index,
                keys: sizes.map(|s| s.keys),
                expires: sizes.map(|s| s.expires),
            }
        })
        .collect()
}

/// Keys and keys with a TTL in one database.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sizes {
    keys: u64,
    expires: u64,
}

/// The value of `databases` in a `CONFIG GET databases` reply (a flat
/// array in RESP2, a map in RESP3).
fn configured_databases(reply: Value) -> Option<u32> {
    pairs(reply)
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(b"databases"))
        .and_then(|(_, value)| std::str::from_utf8(&value).ok()?.trim().parse().ok())
}

/// The databases of an `INFO keyspace` section:
/// `db0:keys=1,expires=0,avg_ttl=0,subexpiry=0`, one line each. Lines that
/// do not read that way are skipped; so are fields it does not know.
fn parse_keyspace(info: &str) -> BTreeMap<u32, Sizes> {
    info.lines()
        .filter_map(|line| {
            let (db, fields) = line.trim().split_once(':')?;
            let index = db.strip_prefix("db")?.parse().ok()?;
            let mut sizes = Sizes::default();
            let mut keys = None;
            for field in fields.split(',') {
                let Some((name, value)) = field.split_once('=') else {
                    continue;
                };
                match name {
                    "keys" => keys = value.parse().ok(),
                    "expires" => sizes.expires = value.parse().unwrap_or(0),
                    _ => {}
                }
            }
            sizes.keys = keys?;
            Some((index, sizes))
        })
        .collect()
}

/// Whether a reply to `CLUSTER INFO` comes from a cluster node: it lists
/// the node's view of the cluster, starting with `cluster_state`.
fn is_cluster_info(reply: Value) -> bool {
    info_field(&into_string(reply), "cluster_state").is_some()
}

/// One `name:value` line of an INFO reply.
fn info_field<'a>(info: &'a str, name: &str) -> Option<&'a str> {
    info.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use proptest::{collection::btree_map, prelude::*};

    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn reads_the_configured_count() {
        let resp2 = Value::Array(vec![bulk("databases"), bulk("32")]);
        assert_eq!(configured_databases(resp2), Some(32));
        let resp3 = Value::Map(vec![(bulk("databases"), bulk("1"))]);
        assert_eq!(configured_databases(resp3), Some(1));
        assert_eq!(configured_databases(Value::Array(vec![])), None);
        let odd = Value::Array(vec![bulk("databases"), bulk("many")]);
        assert_eq!(configured_databases(odd), None);
    }

    #[test]
    fn reads_the_keyspace() {
        let info = "# Keyspace\r\ndb0:keys=1000009,expires=3,avg_ttl=0,subexpiry=0\r\n\
                    db12:keys=4,expires=0,avg_ttl=12\r\nnot a db line\r\ndbx:keys=1\r\n\
                    db3:expires=1\r\n";
        let keyspace = parse_keyspace(info);
        assert_eq!(
            keyspace.into_iter().collect::<Vec<_>>(),
            [
                (
                    0,
                    Sizes {
                        keys: 1_000_009,
                        expires: 3
                    }
                ),
                (
                    12,
                    Sizes {
                        keys: 4,
                        expires: 0
                    }
                ),
            ]
        );
        assert!(parse_keyspace("").is_empty());
        assert!(parse_keyspace("# Keyspace\r\n").is_empty());
    }

    #[test]
    fn lists_every_database() {
        let keyspace = parse_keyspace("db1:keys=5,expires=2\ndb20:keys=1,expires=0\n");
        let list = list_databases(Some(16), Some(&keyspace), false);
        // 16 configured, but db20 exists, so 21 are listed.
        assert_eq!(list.len(), 21);
        assert_eq!(list[0].keys, Some(0));
        assert_eq!((list[1].keys, list[1].expires), (Some(5), Some(2)));
        assert_eq!(list[20].keys, Some(1));
        assert_eq!(list[20].label(), "db20 · 1");

        // Without CONFIG, the default; without INFO, sizes are unknown.
        let list = list_databases(None, None, false);
        assert_eq!(list.len(), DEFAULT_DATABASES as usize);
        assert!(
            list.iter()
                .all(|db| db.keys.is_none() && db.expires.is_none())
        );
        assert_eq!(list[3].label(), "db3");

        // A cluster node has one database.
        let list = list_databases(Some(16), Some(&keyspace), true);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].index, 0);

        // Absurd counts are capped, and there is always database 0.
        assert_eq!(
            list_databases(Some(u32::MAX), None, false).len(),
            MAX_DATABASES as usize
        );
        assert_eq!(list_databases(Some(0), None, false).len(), 1);
        let label = DbInfo {
            index: 0,
            keys: Some(1_000_009),
            expires: None,
        }
        .label();
        assert_eq!(label, "db0 · 1,000,009");
    }

    #[test]
    fn reads_info_fields() {
        let info = "# Cluster\r\ncluster_enabled:1\r\n";
        assert_eq!(info_field(info, "cluster_enabled"), Some("1"));
        assert_eq!(info_field(info, "cluster"), None);
        assert_eq!(info_field("", "cluster_enabled"), None);
    }

    #[test]
    fn reads_cluster_info() {
        let info = "cluster_state:ok\r\ncluster_slots_assigned:16384\r\ncluster_known_nodes:6\r\n";
        assert!(is_cluster_info(bulk(info)));
        // RESP3 sends it as verbatim text.
        assert!(is_cluster_info(Value::VerbatimString {
            format: redis::VerbatimFormat::Text,
            text: "cluster_state:fail\n".into(),
        }));
        assert!(!is_cluster_info(bulk("")));
        assert!(!is_cluster_info(bulk("# Cluster\r\ncluster_enabled:0\r\n")));
        assert!(!is_cluster_info(Value::Nil));
    }

    fn sizes() -> impl Strategy<Value = Sizes> {
        (any::<u64>(), any::<u64>()).prop_map(|(keys, expires)| Sizes { keys, expires })
    }

    proptest! {
        /// Whatever databases INFO lists, with whatever extra fields, line
        /// endings and sections around them, read back as they were.
        #[test]
        fn keyspace_round_trips(
            dbs in btree_map(0u32..100_000, sizes(), 0..20),
            crlf in any::<bool>(),
            extra in "x[a-z_]{0,9}",
        ) {
            let end = if crlf { "\r\n" } else { "\n" };
            let mut info = format!("# Keyspace{end}");
            for (db, s) in &dbs {
                info.push_str(&format!(
                    "db{db}:keys={},expires={},avg_ttl=0,{extra}=7{end}",
                    s.keys, s.expires
                ));
            }
            info.push_str(&format!("{extra}:1{end}"));
            prop_assert_eq!(parse_keyspace(&info), dbs);
        }

        /// Any text at all reads without panicking.
        #[test]
        fn keyspace_reads_any_text(info in ".*") {
            let _ = parse_keyspace(&info);
        }
    }
}
