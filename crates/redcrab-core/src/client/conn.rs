use redis::{
    Cmd, Pipeline, RedisFuture, Value,
    aio::{ConnectionLike, ConnectionManager},
};

/// How a [`Client`](super::Client) reaches the server. Every request goes
/// through this type, never through a concrete connection, so that new
/// topologies (cluster, sentinel) are new variants rather than new call
/// sites. Clones share the underlying connection.
#[derive(Clone)]
pub(crate) enum Conn {
    /// One server, over a multiplexed connection that reconnects on its own.
    Single(ConnectionManager),
}

impl Conn {
    /// The connections a full keyspace walk has to SCAN: one per node that
    /// owns keys. A single server is its own only node.
    pub(crate) fn scan_nodes(&self) -> Vec<Conn> {
        match self {
            Conn::Single(_) => vec![self.clone()],
        }
    }
}

impl ConnectionLike for Conn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Conn::Single(con) => con.req_packed_command(cmd),
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
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Conn::Single(con) => con.get_db(),
        }
    }
}
