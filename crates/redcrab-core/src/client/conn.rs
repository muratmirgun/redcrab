use anyhow::{Result, bail};
use redis::{
    Cmd, Pipeline, RedisFuture, RedisResult, Value,
    aio::{ConnectionLike, ConnectionManager},
    pipe,
};

use super::{
    cluster::{Cluster, ClusterNode},
    sentinel::Sentinel,
};
use crate::commands::{self, CommandClass};

/// Whether a command may change data: anything the command table does not
/// know to be a read counts. Such a command is never sent twice on the
/// connection's own initiative: after a lost connection it may have run.
pub(crate) fn may_write(cmd: &Cmd) -> bool {
    let argv: Vec<String> = cmd
        .args_iter()
        .take(2)
        .filter_map(|arg| match arg {
            redis::Arg::Simple(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        })
        .collect();
    commands::classify(&argv) != CommandClass::Read
}

/// How a [`Client`](super::Client) reaches the server. Every request goes
/// through this type, never through a concrete connection, so that
/// topologies are variants rather than call sites. Clones share the
/// underlying connections.
#[derive(Clone)]
pub(crate) enum Conn {
    /// One server, over a multiplexed connection that reconnects on its own.
    Single(ConnectionManager),
    /// A Redis Cluster; requests route by the slot of their key.
    Cluster(Cluster),
    /// One primary of a cluster; every request goes to it.
    ClusterNode(ClusterNode),
    /// The master Sentinel names, followed across failovers.
    Sentinel(Sentinel),
}

impl Conn {
    /// The connections a full keyspace walk has to SCAN: one per node that
    /// owns keys. A single server is its own only node; a cluster has one
    /// per primary, as last read. A walk that is about to start uses
    /// [`Self::walk_nodes`], which also makes sure the cluster's layout
    /// covers every slot.
    pub(crate) fn scan_nodes(&self) -> Vec<Conn> {
        match self {
            Conn::Single(_) | Conn::ClusterNode(_) | Conn::Sentinel(_) => vec![self.clone()],
            Conn::Cluster(cluster) => cluster.nodes().into_iter().map(Conn::ClusterNode).collect(),
        }
    }

    /// [`Self::scan_nodes`] for a walk, and how many of a cluster's hash
    /// slots none of them serves (0 elsewhere). A cluster's layout with
    /// such slots is read again first. A walk that still misses slots must
    /// say so (see [`super::cluster::unowned_error`]) rather than pass for a
    /// complete one.
    pub(crate) async fn walk_nodes(&self) -> (Vec<Conn>, usize) {
        match self {
            Conn::Cluster(cluster) => {
                let (nodes, unowned) = cluster.walk().await;
                (nodes.into_iter().map(Conn::ClusterNode).collect(), unowned)
            }
            _ => (self.scan_nodes(), 0),
        }
    }

    /// `host:port` of a cluster primary's connection; `None` for the
    /// others.
    pub(crate) fn address(&self) -> Option<String> {
        match self {
            Conn::ClusterNode(node) => Some(node.address()),
            _ => None,
        }
    }

    pub(crate) fn is_cluster(&self) -> bool {
        matches!(self, Conn::Cluster(_) | Conn::ClusterNode(_))
    }

    /// Which server answers now, as far as SCAN cursors care: it changes
    /// when a Sentinel connection moves to another master. A reconnect to
    /// the same server keeps it (cursors survive that), and a cluster
    /// node's requests always go to its own address.
    pub(crate) fn epoch(&self) -> u64 {
        match self {
            Conn::Sentinel(sentinel) => sentinel.generation(),
            Conn::Single(_) | Conn::Cluster(_) | Conn::ClusterNode(_) => 0,
        }
    }

    /// Runs the `per_key` commands `build` adds for each key in as few
    /// round trips as the topology allows: one pipeline on a single server,
    /// one per primary on a cluster (see [`Cluster::per_key`]). Returns the
    /// raw replies in key order, `per_key` per key, each command's error in
    /// its place as [`Value::ServerError`].
    pub(crate) async fn per_key(
        &self,
        keys: &[&[u8]],
        per_key: usize,
        build: &(dyn Fn(&mut Pipeline, &[u8]) + Sync),
    ) -> RedisResult<Vec<Value>> {
        if keys.is_empty() {
            // An empty pipeline is an error to redis-rs.
            return Ok(Vec::new());
        }
        if let Conn::Cluster(cluster) = self {
            return cluster.per_key(keys, per_key, build).await;
        }
        let mut p = pipe();
        for key in keys {
            build(&mut p, key);
        }
        // Errors come back per command, in place of its reply.
        p.ignore_errors().query_async(&mut self.clone()).await
    }

    /// Whether every command must name keys of a single hash slot, as on a
    /// cluster or one of its nodes, so that bulk operations send one key per
    /// command. A plain connection to a cluster node (one a user without INFO
    /// reached as standalone) says no, and the bulk operations switch over on
    /// their first CROSSSLOT error instead.
    pub(crate) fn one_slot_per_command(&self) -> bool {
        match self {
            Conn::Single(_) | Conn::Sentinel(_) => false,
            Conn::Cluster(_) | Conn::ClusterNode(_) => true,
        }
    }
}

impl ConnectionLike for Conn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Conn::Single(con) => con.req_packed_command(cmd),
            Conn::Cluster(con) => con.req_packed_command(cmd),
            Conn::ClusterNode(con) => con.req_packed_command(cmd),
            Conn::Sentinel(con) => con.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Conn::Single(con) => con.req_packed_commands(pipeline, offset, count),
            Conn::Cluster(con) => con.req_packed_commands(pipeline, offset, count),
            Conn::ClusterNode(con) => con.req_packed_commands(pipeline, offset, count),
            Conn::Sentinel(con) => con.req_packed_commands(pipeline, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Conn::Single(con) => con.get_db(),
            Conn::Cluster(con) => con.get_db(),
            Conn::ClusterNode(con) => con.get_db(),
            Conn::Sentinel(con) => con.get_db(),
        }
    }
}

/// Walks a SCAN-family cursor that started at 0 on a connection: where the
/// next round trip asks from, and whether the walk is over.
///
/// A cursor only means something on the server that handed it out: each
/// server orders its table by a random seed of its own. When requests move
/// to another server mid-walk (a Sentinel failover, whether the round trip
/// itself was sent again to the new master or another request switched in
/// between), the cursor would resume at an arbitrary point of the new
/// server's table and silently skip keys while still ending at 0. So the
/// walk starts over at 0 on the new server instead: SCAN may return keys
/// twice anyway, and deleting twice deletes nothing more. A walk that keeps
/// being moved gives up with an error rather than run forever.
#[derive(Debug)]
pub(crate) struct NodeCursor {
    at: u64,
    epoch: u64,
    restarts: u32,
}

/// What a round trip of a [`NodeCursor`] walk says to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorStep {
    /// Use the page, then ask from [`NodeCursor::at`].
    More,
    /// Use the page; the walk is complete.
    Done,
    /// The server changed: drop what the walk learned from its cursors
    /// (the page may be anywhere in the new server's table) and ask from
    /// 0 again.
    Restarted,
}

/// Times a walk starts over before it gives up.
const MAX_RESTARTS: u32 = 3;

impl NodeCursor {
    /// A walk from cursor 0 on `con` as it is now.
    pub(crate) fn start(con: &Conn) -> Self {
        Self::at_epoch(con.epoch())
    }

    fn at_epoch(epoch: u64) -> Self {
        Self {
            at: 0,
            epoch,
            restarts: 0,
        }
    }

    /// The cursor to ask from.
    pub(crate) fn at(&self) -> u64 {
        self.at
    }

    /// After a round trip that asked from [`Self::at`] and got `next`
    /// back, with the connection at `epoch` now ([`Conn::epoch`]).
    pub(crate) fn advance(&mut self, next: u64, epoch: u64) -> Result<CursorStep> {
        if epoch != self.epoch {
            self.restarts += 1;
            if self.restarts > MAX_RESTARTS {
                bail!(
                    "the server changed {} times during the walk (Sentinel failovers); try again once the master is settled",
                    self.restarts
                );
            }
            tracing::info!("the server changed during a SCAN walk; starting it over");
            self.epoch = epoch;
            self.at = 0;
            return Ok(CursorStep::Restarted);
        }
        if next == 0 {
            Ok(CursorStep::Done)
        } else {
            self.at = next;
            Ok(CursorStep::More)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_walk_starts_over_on_another_server() {
        let mut cursor = NodeCursor::at_epoch(4);
        assert_eq!(cursor.at(), 0);
        assert_eq!(cursor.advance(17, 4).unwrap(), CursorStep::More);
        assert_eq!(cursor.at(), 17);
        // The reply came from another master: its cursor means nothing.
        assert_eq!(cursor.advance(99, 5).unwrap(), CursorStep::Restarted);
        assert_eq!(cursor.at(), 0);
        // On the new server, as usual.
        assert_eq!(cursor.advance(3, 5).unwrap(), CursorStep::More);
        assert_eq!(cursor.at(), 3);
        // A move on the round trip that ended the walk starts it over too.
        assert_eq!(cursor.advance(0, 6).unwrap(), CursorStep::Restarted);
        assert_eq!(cursor.advance(0, 6).unwrap(), CursorStep::Done);
        // Moved again and again: it gives up rather than walk forever.
        assert_eq!(cursor.advance(1, 7).unwrap(), CursorStep::Restarted);
        let err = cursor.advance(1, 8).unwrap_err();
        assert!(err.to_string().contains("changed 4 times"), "{err}");
    }
}
