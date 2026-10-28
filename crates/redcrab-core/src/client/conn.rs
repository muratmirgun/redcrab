use redis::{
    Cmd, Pipeline, RedisFuture, RedisResult, Value,
    aio::{ConnectionLike, ConnectionManager},
    pipe,
};

use super::{
    cluster::{Cluster, ClusterNode},
    sentinel::Sentinel,
};

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

    pub(crate) fn is_cluster(&self) -> bool {
        matches!(self, Conn::Cluster(_) | Conn::ClusterNode(_))
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
