//! Connections of their own to the server a [`Client`](super::Client) talks
//! to, for the console: one that SELECT, MULTI or a blocking command can
//! use without touching the app's shared connection.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use redis::{
    AsyncConnectionConfig, Cmd, Pipeline, RedisFuture, RedisResult, Value,
    aio::{ConnectionLike, MultiplexedConnection},
    cluster::ClusterClient,
    cluster_async::ClusterConnection,
    cluster_routing::{RoutingInfo, SingleNodeRoutingInfo},
};

use super::{
    cluster::{Cluster, is_topology_error, routes_to_any_node},
    connect_error,
    sentinel::Sentinel,
};
use crate::{
    config::{ConnectionConfig, Target, format_address},
    ssh::Tunnel,
};

/// How to open another connection to the server.
#[derive(Clone)]
pub(crate) enum Connector {
    /// One server: over TCP, a Unix socket, or the SSH tunnel kept here,
    /// which lives as long as any connector or client holding it.
    Direct {
        redis: Arc<redis::Client>,
        tunnel: Option<Arc<Tunnel>>,
    },
    /// A cluster: a cluster connection of its own, routing by key, and the
    /// app's view of the cluster, for the primary that commands without a
    /// key go to.
    Cluster {
        client: Arc<ClusterClient>,
        cluster: Cluster,
    },
    /// Whatever master the sentinels name at the time of connecting.
    Sentinel(Sentinel),
}

/// A connection opened by a [`Connector`].
#[derive(Clone)]
pub(crate) enum OwnConn {
    Plain(MultiplexedConnection),
    Cluster(ClusterConsole),
}

/// A cluster connection for the console. Commands with a key go to the
/// node of its slot, as `redis-cli -c` sends them; commands without one
/// (ROLE, TIME, CONFIG GET, EVAL with no keys, ...) go to one primary, the
/// configured seed node when it is one (see [`Cluster::home`]), rather
/// than to whichever node redis-rs picks, replicas included. Commands that
/// redis-rs sends to several nodes and combines (DBSIZE, KEYS, INFO, ...)
/// still are.
#[derive(Clone)]
pub(crate) struct ClusterConsole {
    con: ClusterConnection,
    cluster: Cluster,
    seed: (String, u16),
}

impl ClusterConsole {
    async fn send(&mut self, cmd: &Cmd) -> RedisResult<Value> {
        let argv: Vec<&[u8]> = cmd
            .args_iter()
            .filter_map(|arg| match arg {
                redis::Arg::Simple(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        let home = if routes_to_any_node(&argv) {
            self.cluster.home(&self.seed.0, self.seed.1)
        } else {
            None
        };
        let Some((node, generation)) = home else {
            return self.con.req_packed_command(cmd).await;
        };
        let route = RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
            host: node.host,
            port: node.port,
        });
        let result = self.con.route_command(cmd.clone(), route).await;
        if let Err(err) = &result
            && is_topology_error(err)
        {
            // The next command then goes to whichever primary took over.
            if let Err(err) = self.cluster.refresh(generation).await {
                tracing::warn!("could not read the cluster layout again: {err}");
            }
        }
        result
    }
}

impl Connector {
    #[cfg(test)]
    pub(crate) fn direct(redis: redis::Client) -> Self {
        Self::Direct {
            redis: Arc::new(redis),
            tunnel: None,
        }
    }

    pub(crate) fn tunnel(&self) -> Option<&Tunnel> {
        match self {
            Self::Direct { tunnel, .. } => tunnel.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn is_cluster(&self) -> bool {
        matches!(self, Self::Cluster { .. })
    }

    /// Opens a connection. `connect_timeout` bounds the connection step;
    /// replies have no timeout, the caller applies its own.
    pub(crate) async fn connect(
        &self,
        config: &ConnectionConfig,
        connect_timeout: Duration,
    ) -> Result<OwnConn> {
        let settings = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(connect_timeout))
            .set_response_timeout(None);
        match self {
            Self::Direct { redis, tunnel } => {
                let started = Instant::now();
                redis
                    .get_multiplexed_async_connection_with_config(&settings)
                    .await
                    .map(OwnConn::Plain)
                    .map_err(|err| {
                        let err = connect_error(err, config, &config.route_label());
                        // Only a problem of this attempt's tunnel channel.
                        match tunnel.as_ref().and_then(|t| t.problem_since(started)) {
                            Some(problem) => err.context(problem),
                            None => err,
                        }
                    })
            }
            Self::Cluster { client, cluster } => {
                let connecting = client.get_async_connection();
                match tokio::time::timeout(connect_timeout * 2, connecting).await {
                    Ok(con) => con
                        .map(|con| {
                            OwnConn::Cluster(ClusterConsole {
                                con,
                                cluster: cluster.clone(),
                                seed: (config.host.trim().to_string(), config.port),
                            })
                        })
                        .map_err(|err| connect_error(err, config, &config.route_label())),
                    Err(_) => anyhow::bail!(
                        "the cluster did not answer within {} s",
                        (connect_timeout * 2).as_secs()
                    ),
                }
            }
            Self::Sentinel(sentinel) => {
                let (host, port) = sentinel.resolve().await?;
                let target = format!(
                    "the master {} at {}",
                    sentinel.master_name(),
                    format_address(&host, port)
                );
                config
                    .client_for(Target::Tcp(&host, port))?
                    .get_multiplexed_async_connection_with_config(&settings)
                    .await
                    .map(OwnConn::Plain)
                    .map_err(|err| connect_error(err, config, &target))
            }
        }
    }
}

impl ConnectionLike for OwnConn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Self::Plain(con) => con.req_packed_command(cmd),
            Self::Cluster(console) => Box::pin(console.send(cmd)),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Self::Plain(con) => con.req_packed_commands(pipeline, offset, count),
            Self::Cluster(console) => console.con.req_packed_commands(pipeline, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Self::Plain(con) => con.get_db(),
            Self::Cluster(console) => console.con.get_db(),
        }
    }
}
