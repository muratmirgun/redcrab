//! Redis Cluster. Requests that name a key go through redis-rs' cluster
//! connection, which routes them to the node owning the key's slot and
//! follows redirects. A keyspace walk instead visits every primary on its
//! own ([`ClusterNode`]), and batches of per-key commands go out as one
//! pipeline per primary ([`Cluster::per_key`]): a pipeline must stay on one
//! node, and multi-key commands across slots fail with CROSSSLOT.
//!
//! Which primary owns which slot comes from `CLUSTER SHARDS` (or `CLUSTER
//! SLOTS` on servers older than 7), and is read again when a call routed to
//! a node fails in a way that says the layout changed.

use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use futures::future::join_all;
use redis::{
    Cmd, ErrorKind, Pipeline, RedisError, RedisFuture, RedisResult, Value,
    aio::ConnectionLike,
    cluster::{ClusterClient, ClusterClientBuilder},
    cluster_async::ClusterConnection,
    cluster_routing::{Route, RoutingInfo, SingleNodeRoutingInfo, SlotAddr},
    cmd,
};

use super::connect_error;
use crate::config::{ConnectionConfig, Target, parse_address};

/// Slots in a cluster's keyspace.
pub(crate) const SLOT_COUNT: usize = 16384;
/// Marks a slot no primary serves.
const UNOWNED: u16 = u16::MAX;

/// The Redis Cluster hash slot of a key, as `CLUSTER KEYSLOT` computes it:
/// CRC16 (XMODEM) of the key, or of its hash tag (the part between the
/// first `{` and the next `}`, if not empty), modulo 16384. Keys with the
/// same tag share a slot, so multi-key commands and renames work on them.
pub fn key_slot(key: &[u8]) -> u16 {
    let hashed = match key.iter().position(|&b| b == b'{') {
        Some(open) => match key[open + 1..].iter().position(|&b| b == b'}') {
            Some(len) if len > 0 => &key[open + 1..open + 1 + len],
            _ => key,
        },
        None => key,
    };
    crc16(hashed) % SLOT_COUNT as u16
}

/// CRC-16/XMODEM (polynomial 0x1021, initial value 0), as Redis uses it.
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// A primary, at the address redis-rs knows it by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Node {
    pub host: String,
    pub port: u16,
}

/// The primaries of a cluster and which one serves each slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Shards {
    primaries: Vec<Node>,
    /// Per slot, an index into `primaries`, or [`UNOWNED`].
    owners: Vec<u16>,
}

impl Shards {
    fn new() -> Self {
        Self {
            primaries: Vec::new(),
            owners: vec![UNOWNED; SLOT_COUNT],
        }
    }

    pub(crate) fn primaries(&self) -> &[Node] {
        &self.primaries
    }

    /// The index of the primary serving `slot`.
    pub(crate) fn owner(&self, slot: u16) -> Option<usize> {
        match self.owners.get(usize::from(slot)) {
            Some(&UNOWNED) | None => None,
            Some(&owner) => Some(usize::from(owner)),
        }
    }

    /// Records that `node` serves `start..=end` (ignoring nonsense ranges).
    fn assign(&mut self, node: Node, start: i64, end: i64) {
        let ix = match self.primaries.iter().position(|n| *n == node) {
            Some(ix) => ix,
            None => {
                self.primaries.push(node);
                self.primaries.len() - 1
            }
        };
        let last = SLOT_COUNT as i64 - 1;
        if start < 0 || end < start || start > last {
            return;
        }
        for slot in start..=end.min(last) {
            self.owners[slot as usize] = ix as u16;
        }
    }

    /// How many slots no primary serves. A walk over the primaries misses
    /// the keys of these slots, so it must not pass for a complete one.
    pub(crate) fn unowned(&self) -> usize {
        self.owners
            .iter()
            .filter(|&&owner| owner == UNOWNED)
            .count()
    }

    /// Whichever layout leaves fewer slots without a primary; `self` when
    /// they tie.
    fn more_complete(self, other: Self) -> Self {
        if other.unowned() < self.unowned() {
            other
        } else {
            self
        }
    }

    /// Reads a `CLUSTER SHARDS` reply (Redis 7+): per shard, its slot
    /// ranges and its nodes, of which the healthy primary is the one that
    /// serves them. `tls` picks the `tls-port` when a node has both.
    ///
    /// A shard whose primary is down (its health is not `online`) and has
    /// no healthy one yet keeps that primary: in a cluster without
    /// replicas, or until a replica is promoted, it is still the node that
    /// owns those slots. Leaving the shard out would make walks skip its
    /// keys without an error; with it in, they fail on it until it answers
    /// again.
    pub(crate) fn from_shards_reply(reply: &Value, tls: bool) -> Option<Self> {
        let mut shards = Self::new();
        for shard in items(reply)? {
            let shard = fields(shard)?;
            let slots: Vec<i64> = items(field(&shard, "slots")?)?
                .iter()
                .map(int)
                .collect::<Option<_>>()?;
            // Its masters, each with whether it is healthy.
            let masters: Vec<(Node, bool)> = items(field(&shard, "nodes")?)?
                .iter()
                .filter_map(|node| {
                    let node = fields(node)?;
                    let text = |name| field(&node, name).and_then(string);
                    if text("role")? != "master" {
                        return None;
                    }
                    let online = text("health").is_none_or(|h| h == "online");
                    let host = text("endpoint")
                        .filter(|e| !e.is_empty() && e != "?")
                        .or_else(|| text("ip"))?;
                    let port = |name| field(&node, name).and_then(int).filter(|&p| p > 0);
                    let port = if tls {
                        port("tls-port").or_else(|| port("port"))
                    } else {
                        port("port").or_else(|| port("tls-port"))
                    }?;
                    let node = Node {
                        host,
                        port: u16::try_from(port).ok()?,
                    };
                    Some((node, online))
                })
                .collect();
            let primary = masters
                .iter()
                .find(|(_, online)| *online)
                .or_else(|| masters.first())
                .map(|(node, _)| node);
            if let Some(primary) = primary {
                for [start, end] in slots.as_chunks::<2>().0 {
                    shards.assign(primary.clone(), *start, *end);
                }
            }
        }
        Some(shards)
    }

    /// Reads a `CLUSTER SLOTS` reply: per slot range, its primary first.
    pub(crate) fn from_slots_reply(reply: &Value) -> Option<Self> {
        let mut shards = Self::new();
        for range in items(reply)? {
            let range = items(range)?;
            let (start, end, primary) = (int(range.first()?)?, int(range.get(1)?)?, range.get(2)?);
            let primary = items(primary)?;
            let host = string(primary.first()?)?;
            let port = u16::try_from(int(primary.get(1)?)?).ok()?;
            if host.is_empty() || host == "?" || port == 0 {
                continue;
            }
            shards.assign(Node { host, port }, start, end);
        }
        Some(shards)
    }
}

fn items(value: &Value) -> Option<&[Value]> {
    match value {
        Value::Array(items) | Value::Set(items) => Some(items),
        _ => None,
    }
}

/// A flat field/value array (RESP2) or a map (RESP3), as pairs.
fn fields(value: &Value) -> Option<Vec<(String, &Value)>> {
    match value {
        Value::Map(pairs) => pairs.iter().map(|(k, v)| Some((string(k)?, v))).collect(),
        Value::Array(items) => items
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[key, value]| Some((string(key)?, value)))
            .collect(),
        _ => None,
    }
}

fn field<'a>(fields: &[(String, &'a Value)], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| *value)
}

fn string(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        other => string(other)?.parse().ok(),
    }
}

/// Whether redis-rs' cluster connection would send `argv` to any node it
/// likes, replicas included: commands without a key (ROLE, TIME, CLIENT
/// INFO, CONFIG GET, EVAL with no keys, ...) and the few it has no rule
/// for (SCAN, SHUTDOWN, REPLICAOF). Two such commands in a row may reach
/// different nodes, so the console sends them to one primary instead.
///
/// Mirrors `RoutingInfo::for_routable` of redis-rs 1.8, which is not
/// public: commands it routes by key or fans out to several nodes (DBSIZE,
/// KEYS, MGET, ...) answer `false`.
pub(crate) fn routes_to_any_node(argv: &[&[u8]]) -> bool {
    let Some(name) = argv.first() else {
        return false;
    };
    let arg = |ix: usize| argv.get(ix).copied();
    let mut command = name.to_ascii_uppercase();
    if matches!(
        &command[..],
        b"XGROUP"
            | b"OBJECT"
            | b"SLOWLOG"
            | b"FUNCTION"
            | b"MODULE"
            | b"COMMAND"
            | b"PUBSUB"
            | b"CONFIG"
            | b"MEMORY"
            | b"XINFO"
            | b"CLIENT"
            | b"ACL"
            | b"SCRIPT"
            | b"CLUSTER"
            | b"LATENCY"
    ) && let Some(sub) = arg(1)
    {
        command.push(b' ');
        command.extend(sub.to_ascii_uppercase());
    }
    // A key count of 0 (or none that reads) means no key to route by.
    let no_keys = |count_at: usize| {
        let count = arg(count_at)
            .and_then(|n| std::str::from_utf8(n).ok())
            .and_then(|n| n.parse::<u64>().ok());
        count.is_none_or(|n| n == 0) || arg(count_at + 1).is_none()
    };
    match &command[..] {
        // To every node, every primary, or the nodes of each key.
        b"ACL SETUSER"
        | b"ACL DELUSER"
        | b"ACL SAVE"
        | b"CLIENT SETNAME"
        | b"CLIENT SETINFO"
        | b"SLOWLOG GET"
        | b"SLOWLOG LEN"
        | b"SLOWLOG RESET"
        | b"CONFIG SET"
        | b"CONFIG RESETSTAT"
        | b"CONFIG REWRITE"
        | b"SCRIPT FLUSH"
        | b"SCRIPT LOAD"
        | b"LATENCY RESET"
        | b"LATENCY GRAPH"
        | b"LATENCY HISTOGRAM"
        | b"LATENCY HISTORY"
        | b"LATENCY DOCTOR"
        | b"LATENCY LATEST"
        | b"PUBSUB NUMPAT"
        | b"PUBSUB CHANNELS"
        | b"PUBSUB NUMSUB"
        | b"PUBSUB SHARDCHANNELS"
        | b"PUBSUB SHARDNUMSUB"
        | b"SCRIPT KILL"
        | b"FUNCTION KILL"
        | b"FUNCTION STATS"
        | b"DBSIZE"
        | b"DEBUG"
        | b"FLUSHALL"
        | b"FLUSHDB"
        | b"FT._ALIASLIST"
        | b"FT._LIST"
        | b"FUNCTION DELETE"
        | b"FUNCTION FLUSH"
        | b"FUNCTION LOAD"
        | b"FUNCTION RESTORE"
        | b"INFO"
        | b"KEYS"
        | b"MEMORY DOCTOR"
        | b"MEMORY MALLOC-STATS"
        | b"MEMORY PURGE"
        | b"MEMORY STATS"
        | b"PING"
        | b"SCRIPT EXISTS"
        | b"UNWATCH"
        | b"WAIT"
        | b"RANDOMKEY"
        | b"WAITAOF"
        | b"MGET"
        | b"DEL"
        | b"EXISTS"
        | b"UNLINK"
        | b"TOUCH"
        | b"WATCH"
        | b"MSET"
        | b"JSON.MGET"
        | b"JSON.MSET" => false,
        // No rule at all.
        b"SCAN" | b"SHUTDOWN" | b"SLAVEOF" | b"REPLICAOF" => true,
        // `name numkeys key ...` and `name script numkeys key ...`.
        b"LMPOP" | b"SINTERCARD" | b"ZDIFF" | b"ZINTER" | b"ZINTERCARD" | b"ZMPOP" | b"ZUNION" => {
            no_keys(1)
        }
        b"BLMPOP" | b"BZMPOP" | b"EVAL" | b"EVALSHA" | b"EVALSHA_RO" | b"EVAL_RO" | b"FCALL"
        | b"FCALL_RO" => no_keys(2),
        // The key is the second argument.
        b"BITOP"
        | b"MEMORY USAGE"
        | b"PFDEBUG"
        | b"XGROUP CREATE"
        | b"XGROUP CREATECONSUMER"
        | b"XGROUP DELCONSUMER"
        | b"XGROUP DESTROY"
        | b"XGROUP SETID"
        | b"XINFO CONSUMERS"
        | b"XINFO GROUPS"
        | b"XINFO STREAM"
        | b"OBJECT ENCODING"
        | b"OBJECT FREQ"
        | b"OBJECT IDLETIME"
        | b"OBJECT REFCOUNT"
        | b"JSON.DEBUG" => arg(2).is_none(),
        b"XREAD" | b"XREADGROUP" => !argv
            .iter()
            .position(|a| a.eq_ignore_ascii_case(b"STREAMS"))
            .is_some_and(|at| arg(at + 1).is_some()),
        // Routed by a slot number.
        b"CLUSTER ADDSLOTS"
        | b"CLUSTER COUNTKEYSINSLOT"
        | b"CLUSTER DELSLOTS"
        | b"CLUSTER DELSLOTSRANGE"
        | b"CLUSTER GETKEYSINSLOT"
        | b"CLUSTER SETSLOT" => !arg(2)
            .and_then(|n| std::str::from_utf8(n).ok())
            .and_then(|n| n.parse::<u16>().ok())
            .is_some_and(|slot| usize::from(slot) < SLOT_COUNT),
        // Keyless commands with arguments that are no keys.
        b"ACL DRYRUN"
        | b"ACL GENPASS"
        | b"ACL GETUSER"
        | b"ACL HELP"
        | b"ACL LIST"
        | b"ACL LOG"
        | b"ACL USERS"
        | b"ACL WHOAMI"
        | b"AUTH"
        | b"BGSAVE"
        | b"CLIENT GETNAME"
        | b"CLIENT GETREDIR"
        | b"CLIENT ID"
        | b"CLIENT INFO"
        | b"CLIENT KILL"
        | b"CLIENT PAUSE"
        | b"CLIENT REPLY"
        | b"CLIENT TRACKINGINFO"
        | b"CLIENT UNBLOCK"
        | b"CLIENT UNPAUSE"
        | b"CLUSTER COUNT-FAILURE-REPORTS"
        | b"CLUSTER INFO"
        | b"CLUSTER KEYSLOT"
        | b"CLUSTER MEET"
        | b"CLUSTER MYSHARDID"
        | b"CLUSTER NODES"
        | b"CLUSTER REPLICAS"
        | b"CLUSTER RESET"
        | b"CLUSTER SET-CONFIG-EPOCH"
        | b"CLUSTER SHARDS"
        | b"CLUSTER SLOTS"
        | b"COMMAND COUNT"
        | b"COMMAND GETKEYS"
        | b"COMMAND LIST"
        | b"COMMAND"
        | b"CONFIG GET"
        | b"ECHO"
        | b"FT.CREATE"
        | b"FUNCTION LIST"
        | b"LASTSAVE"
        | b"LOLWUT"
        | b"MODULE LIST"
        | b"MODULE LOAD"
        | b"MODULE LOADEX"
        | b"MODULE UNLOAD"
        | b"READONLY"
        | b"READWRITE"
        | b"SAVE"
        | b"SCRIPT SHOW"
        | b"TFCALL"
        | b"TFCALLASYNC"
        | b"TFUNCTION DELETE"
        | b"TFUNCTION LIST"
        | b"TFUNCTION LOAD"
        | b"TIME" => true,
        // Everything else is routed by its first argument, if it has one.
        _ => arg(1).is_none(),
    }
}

/// Whether a failed call to a node means the cluster's layout may have
/// changed: the node is gone or unreachable, or it says the slot moved.
pub(crate) fn is_topology_error(err: &RedisError) -> bool {
    err.kind() == ErrorKind::ClusterConnectionNotFound
        || err.is_io_error()
        || err.is_connection_dropped()
        || err.is_connection_refusal()
        || err.is_timeout()
        || matches!(
            err.code(),
            Some("MOVED" | "ASK" | "TRYAGAIN" | "CLUSTERDOWN" | "READONLY")
        )
}

fn is_topology_reply(value: &Value) -> bool {
    matches!(
        value,
        Value::ServerError(err) if matches!(err.code(), "MOVED" | "ASK" | "TRYAGAIN" | "CLUSTERDOWN")
    )
}

/// The cluster's connection and the slot layout, shared by every clone.
#[derive(Clone)]
pub(crate) struct Cluster {
    con: ClusterConnection,
    layout: Arc<Layout>,
}

struct Layout {
    shards: RwLock<Arc<Shards>>,
    /// Bumped by each refresh, so concurrent failures refresh once.
    generation: AtomicU64,
    refreshing: tokio::sync::Mutex<()>,
    tls: bool,
}

/// How long the app waits for a reply, and for a node to accept a
/// connection.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

impl Cluster {
    /// Connects to the cluster through its seed nodes (`host`/`port`, then
    /// the configured seeds) and reads its layout. Also returns a client
    /// for connections of the console's own, which has no response timeout
    /// of its own (the console applies one per command, and blocking
    /// commands must be able to wait).
    pub(crate) async fn connect(config: &ConnectionConfig) -> Result<(Self, ClusterClient)> {
        let seeds = seeds(config)?;
        let target = config.route_label();
        let builder = |seeds: Vec<redis::ConnectionInfo>| -> Result<ClusterClientBuilder> {
            let mut builder = ClusterClientBuilder::new(seeds)
                .connection_timeout(CONNECT_TIMEOUT)
                .retries(3);
            if let Some(tls) = &config.tls
                && let Some(certificates) = tls.certificates()?
            {
                builder = builder.certs(certificates);
            }
            Ok(builder)
        };
        let app = builder(seeds.clone())?
            .response_timeout(RESPONSE_TIMEOUT)
            .build()?;
        let console = builder(seeds)?.build()?;
        let con = app
            .get_async_connection()
            .await
            .map_err(|err| cluster_connect_error(err, config, &target))?;
        let tls = config.tls.is_some();
        let shards = fetch_shards(&mut con.clone(), tls)
            .await
            .map_err(|err| cluster_connect_error(err, config, &target))?;
        let cluster = Self {
            con,
            layout: Arc::new(Layout {
                shards: RwLock::new(Arc::new(shards)),
                generation: AtomicU64::new(0),
                refreshing: tokio::sync::Mutex::new(()),
                tls,
            }),
        };
        Ok((cluster, console))
    }

    fn shards(&self) -> (Arc<Shards>, u64) {
        // The generation first: a refresh in between then just looks stale.
        let generation = self.layout.generation.load(Ordering::Acquire);
        let shards = self
            .layout
            .shards
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        (shards, generation)
    }

    /// How many primaries serve slots, as last seen.
    pub(crate) fn primary_count(&self) -> usize {
        self.shards().0.primaries().len()
    }

    /// The primary a console sends commands without a key to, as
    /// `redis-cli -c` sends them to the node it connected to: `host`:`port`
    /// if that is a primary, else the one serving slot 0. Read from the
    /// current layout, so it follows a failover.
    pub(crate) fn home(&self, host: &str, port: u16) -> Option<(Node, u64)> {
        let (shards, generation) = self.shards();
        let primaries = shards.primaries();
        primaries
            .iter()
            .find(|node| node.port == port && node.host.eq_ignore_ascii_case(host))
            .or_else(|| shards.owner(0).map(|ix| &primaries[ix]))
            .or_else(|| primaries.first())
            .map(|node| (node.clone(), generation))
    }

    /// One connection per primary, for walks over the whole keyspace.
    pub(crate) fn nodes(&self) -> Vec<ClusterNode> {
        self.nodes_of(&self.shards().0)
    }

    fn nodes_of(&self, shards: &Shards) -> Vec<ClusterNode> {
        shards
            .primaries()
            .iter()
            .map(|node| ClusterNode {
                cluster: self.clone(),
                node: node.clone(),
            })
            .collect()
    }

    /// [`Self::nodes`] for a walk that is about to start, and how many
    /// slots none of them serves. A layout with such slots is read again
    /// first: it may be from a moment when a shard had no primary, and no
    /// failed call would ever say that the shard is back, since a walk
    /// does not visit it.
    pub(crate) async fn walk(&self) -> (Vec<ClusterNode>, usize) {
        let (shards, generation) = self.shards();
        if shards.unowned() > 0
            && let Err(err) = self.refresh(generation).await
        {
            tracing::warn!("could not read the cluster layout again: {err}");
        }
        let (shards, _) = self.shards();
        (self.nodes_of(&shards), shards.unowned())
    }

    /// Reads the layout again, unless another refresh finished since
    /// `seen` (the generation a failed call started with).
    pub(crate) async fn refresh(&self, seen: u64) -> RedisResult<()> {
        let _guard = self.layout.refreshing.lock().await;
        if self.layout.generation.load(Ordering::Acquire) != seen {
            return Ok(());
        }
        let shards = fetch_shards(&mut self.con.clone(), self.layout.tls).await?;
        tracing::info!(
            "cluster layout refreshed: {} primaries",
            shards.primaries().len()
        );
        *self
            .layout
            .shards
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(shards);
        self.layout.generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Runs the `per_key` commands `build` adds for each key, as one
    /// pipeline per primary, all at once. Returns the replies in key order
    /// (`per_key` per key), each command's error in its place as a
    /// [`Value::ServerError`]. Keys whose node failed or answered that the
    /// slot moved are sent once more after the layout is read again.
    pub(crate) async fn per_key(
        &self,
        keys: &[&[u8]],
        per_key: usize,
        build: &(dyn Fn(&mut Pipeline, &[u8]) + Sync),
    ) -> RedisResult<Vec<Value>> {
        let mut replies: Vec<Option<Vec<Value>>> = vec![None; keys.len()];
        let mut pending: Vec<usize> = (0..keys.len()).collect();
        let mut failure = None;
        for attempt in 0..2 {
            let (shards, generation) = self.shards();
            // Keys by the primary serving their slot; unserved slots go to
            // the routed connection, which reports what is wrong with them.
            let mut groups: Vec<(Option<usize>, Vec<usize>)> = Vec::new();
            for &ix in &pending {
                let owner = shards.owner(key_slot(keys[ix]));
                match groups.iter_mut().find(|(o, _)| *o == owner) {
                    Some((_, group)) => group.push(ix),
                    None => groups.push((owner, vec![ix])),
                }
            }
            let requests = groups.into_iter().flat_map(|(owner, group)| {
                let shards = shards.clone();
                // Without an owner, each key goes alone, routed by its slot.
                let batches: Vec<Vec<usize>> = match owner {
                    Some(_) => vec![group],
                    None => group.into_iter().map(|ix| vec![ix]).collect(),
                };
                batches.into_iter().map(move |batch| {
                    let route = match owner {
                        Some(owner) => {
                            let node = &shards.primaries()[owner];
                            SingleNodeRoutingInfo::ByAddress {
                                host: node.host.clone(),
                                port: node.port,
                            }
                        }
                        None => SingleNodeRoutingInfo::SpecificNode(Route::with_key(
                            keys[batch[0]],
                            SlotAddr::Master,
                        )),
                    };
                    let mut pipe = Pipeline::with_capacity(batch.len() * per_key);
                    for &ix in &batch {
                        build(&mut pipe, keys[ix]);
                    }
                    let mut con = self.con.clone();
                    async move {
                        let count = batch.len() * per_key;
                        let result = con.route_pipeline(pipe, 0, count, route).await;
                        (batch, result)
                    }
                })
            });
            let results = join_all(requests).await;
            pending.clear();
            for (batch, result) in results {
                match result {
                    Ok(values) if values.len() == batch.len() * per_key => {
                        for (ix, chunk) in batch.iter().zip(values.chunks(per_key)) {
                            if attempt == 0 && chunk.iter().any(is_topology_reply) {
                                pending.push(*ix);
                            } else {
                                replies[*ix] = Some(chunk.to_vec());
                            }
                        }
                    }
                    Ok(values) => {
                        failure = Some(RedisError::from((
                            ErrorKind::Parse,
                            "a cluster node sent the wrong number of replies",
                            format!("{} for {} commands", values.len(), batch.len() * per_key),
                        )));
                    }
                    Err(err) => {
                        if attempt == 0 && is_topology_error(&err) {
                            pending.extend(batch);
                        }
                        failure = Some(err);
                    }
                }
            }
            if pending.is_empty() {
                break;
            }
            if attempt == 0 {
                // A refresh that fails leaves the old layout; the keys are
                // still tried once more.
                if let Err(err) = self.refresh(generation).await {
                    tracing::warn!("could not read the cluster layout again: {err}");
                }
                failure = None;
            }
        }
        match replies.into_iter().collect::<Option<Vec<_>>>() {
            Some(replies) => Ok(replies.into_iter().flatten().collect()),
            None => Err(failure.unwrap_or_else(|| {
                RedisError::from((ErrorKind::Io, "a cluster node did not answer"))
            })),
        }
    }
}

impl ConnectionLike for Cluster {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        self.con.req_packed_command(cmd)
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        self.con.req_packed_commands(pipeline, offset, count)
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// The cluster connection with every command and pipeline sent to one
/// primary, for SCAN and other per-node requests. A call that fails because
/// the layout changed reads the layout again before it reports the failure,
/// so the next walk visits the current primaries.
#[derive(Clone)]
pub(crate) struct ClusterNode {
    cluster: Cluster,
    node: Node,
}

impl ClusterNode {
    fn route(&self) -> SingleNodeRoutingInfo {
        SingleNodeRoutingInfo::ByAddress {
            host: self.node.host.clone(),
            port: self.node.port,
        }
    }

    /// `host:port` of the primary.
    pub(crate) fn address(&self) -> String {
        crate::config::format_address(&self.node.host, self.node.port)
    }

    async fn after_failure(&self, err: &RedisError, generation: u64) {
        if is_topology_error(err) {
            tracing::info!(
                "cluster node {} failed ({err}); reading the layout again",
                self.address()
            );
            if let Err(err) = self.cluster.refresh(generation).await {
                tracing::warn!("could not read the cluster layout again: {err}");
            }
        }
    }
}

impl ConnectionLike for ClusterNode {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        Box::pin(async move {
            let (_, generation) = self.cluster.shards();
            let route = RoutingInfo::SingleNode(self.route());
            let result = self.cluster.con.route_command(cmd.clone(), route).await;
            if let Err(err) = &result {
                self.after_failure(err, generation).await;
            }
            result
        })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let (_, generation) = self.cluster.shards();
            let route = self.route();
            let result = self
                .cluster
                .con
                .route_pipeline(pipeline.clone(), offset, count, route)
                .await;
            if let Err(err) = &result {
                self.after_failure(err, generation).await;
            }
            result
        })
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// The message for a walk that could not visit every slot.
pub(crate) fn unowned_error(unowned: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "{unowned} of the cluster's {SLOT_COUNT} hash slots have no primary, so keys in them are missing; check CLUSTER SHARDS"
    )
}

/// The layout, from `CLUSTER SHARDS`, or `CLUSTER SLOTS` where that is
/// unknown (Redis 6) or leaves slots without a primary.
async fn fetch_shards(con: &mut ClusterConnection, tls: bool) -> RedisResult<Shards> {
    let route = || RoutingInfo::SingleNode(SingleNodeRoutingInfo::RandomPrimary);
    let mut shards = cmd("CLUSTER");
    shards.arg("SHARDS");
    let parsed = match con.route_command(shards, route()).await {
        Ok(reply) => Shards::from_shards_reply(&reply, tls),
        Err(err) if err.kind() == ErrorKind::Io || err.is_unrecoverable_error() => {
            return Err(err);
        }
        Err(_) => None,
    }
    .filter(|parsed| !parsed.primaries().is_empty());
    if let Some(parsed) = &parsed
        && parsed.unowned() == 0
    {
        return Ok(parsed.clone());
    }
    let mut slots = cmd("CLUSTER");
    slots.arg("SLOTS");
    let from_slots = match con.route_command(slots, route()).await {
        Ok(reply) => Shards::from_slots_reply(&reply).ok_or_else(|| {
            RedisError::from((
                ErrorKind::Parse,
                "could not read the reply to CLUSTER SLOTS",
            ))
        }),
        Err(err) => Err(err),
    };
    match (parsed, from_slots) {
        (Some(parsed), Ok(from_slots)) => Ok(parsed.more_complete(from_slots)),
        (Some(parsed), Err(err)) => {
            tracing::debug!("CLUSTER SLOTS failed ({err}); keeping CLUSTER SHARDS' layout");
            Ok(parsed)
        }
        (None, from_slots) => from_slots,
    }
}

/// The seed nodes' connection settings: `host`/`port`, then `seeds`.
fn seeds(config: &ConnectionConfig) -> Result<Vec<redis::ConnectionInfo>> {
    let mut addresses = vec![(config.host.trim().to_string(), config.port)];
    if let crate::config::Topology::Cluster { seeds } = &config.topology {
        for seed in seeds.iter().filter(|s| !s.trim().is_empty()) {
            let address =
                parse_address(seed).map_err(|why| anyhow::anyhow!("seed node {seed:?}: {why}"))?;
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
    }
    addresses
        .iter()
        .map(|(host, port)| {
            Ok(config
                .client_for(Target::Tcp(host, *port))
                .context("invalid seed node")?
                .get_connection_info()
                .clone())
        })
        .collect()
}

/// [`connect_error`], plus a hint when the server is not a cluster node.
fn cluster_connect_error(
    err: RedisError,
    config: &ConnectionConfig,
    target: &str,
) -> anyhow::Error {
    let not_cluster = err.to_string().contains("cluster support disabled");
    let err = connect_error(err, config, target);
    if not_cluster {
        err.context("the server is not part of a cluster; choose Standalone")
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn slots_match_redis() {
        // Values from CLUSTER KEYSLOT.
        assert_eq!(key_slot(b"foo"), 12182);
        assert_eq!(key_slot(b"bar"), 5061);
        assert_eq!(key_slot(b""), 0);
        assert_eq!(key_slot(b"123456789"), 12739);
        assert_eq!(key_slot(b"{user1000}.following"), key_slot(b"user1000"));
        assert_eq!(key_slot(b"{user1000}.followers"), key_slot(b"user1000"));
        // An empty tag does not count; only the first `{` counts.
        assert_eq!(key_slot(b"foo{}{bar}"), crc16(b"foo{}{bar}") % 16384);
        assert_eq!(key_slot(b"foo{{bar}}zap"), key_slot(b"{bar"));
        assert_eq!(key_slot(b"foo{bar}{zap}"), key_slot(b"bar"));
        assert_eq!(key_slot(b"no}tag{"), crc16(b"no}tag{") % 16384);
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    /// A CLUSTER SHARDS reply (RESP2) for `layout`: per shard, its ranges
    /// and its primary's port; each also gets a replica.
    fn shards_reply(layout: &[(Vec<(u16, u16)>, u16)]) -> Value {
        Value::Array(
            layout
                .iter()
                .map(|(ranges, port)| {
                    let node = |port: u16, role: &str, health: &str| {
                        Value::Array(vec![
                            bulk("id"),
                            bulk("x"),
                            bulk("port"),
                            Value::Int(i64::from(port)),
                            bulk("ip"),
                            bulk("10.0.0.1"),
                            bulk("endpoint"),
                            bulk("127.0.0.1"),
                            bulk("role"),
                            bulk(role),
                            bulk("replication-offset"),
                            Value::Int(1),
                            bulk("health"),
                            bulk(health),
                        ])
                    };
                    Value::Array(vec![
                        bulk("slots"),
                        Value::Array(
                            ranges
                                .iter()
                                .flat_map(|&(a, b)| [Value::Int(a.into()), Value::Int(b.into())])
                                .collect(),
                        ),
                        bulk("nodes"),
                        Value::Array(vec![
                            node(port + 1000, "replica", "online"),
                            node(port + 2000, "master", "failed"),
                            node(*port, "master", "online"),
                        ]),
                    ])
                })
                .collect(),
        )
    }

    /// The same layout as CLUSTER SLOTS says it.
    fn slots_reply(layout: &[(Vec<(u16, u16)>, u16)]) -> Value {
        Value::Array(
            layout
                .iter()
                .flat_map(|(ranges, port)| {
                    ranges.iter().map(move |&(a, b)| {
                        Value::Array(vec![
                            Value::Int(a.into()),
                            Value::Int(b.into()),
                            Value::Array(vec![
                                bulk("127.0.0.1"),
                                Value::Int((*port).into()),
                                bulk("id"),
                            ]),
                            Value::Array(vec![
                                bulk("127.0.0.1"),
                                Value::Int((*port + 1000).into()),
                                bulk("id2"),
                            ]),
                        ])
                    })
                })
                .collect(),
        )
    }

    #[test]
    fn reads_both_layout_replies() {
        let layout = vec![
            (vec![(0, 5460)], 7000),
            (vec![(5461, 10922)], 7001),
            (vec![(10923, 16000), (16001, 16383)], 7002),
        ];
        for shards in [
            Shards::from_shards_reply(&shards_reply(&layout), false).unwrap(),
            Shards::from_slots_reply(&slots_reply(&layout)).unwrap(),
        ] {
            assert_eq!(shards.primaries().len(), 3);
            let port = |slot| shards.primaries()[shards.owner(slot).unwrap()].port;
            assert_eq!((port(0), port(5460), port(5461)), (7000, 7000, 7001));
            assert_eq!((port(10923), port(16383)), (7002, 7002));
            assert!(shards.primaries().iter().all(|n| n.host == "127.0.0.1"));
        }

        // RESP3 maps read the same; a TLS cluster uses tls-port.
        let node = Value::Map(vec![
            (bulk("port"), Value::Int(0)),
            (bulk("tls-port"), Value::Int(7443)),
            (bulk("ip"), bulk("10.1.1.1")),
            (bulk("endpoint"), bulk("?")),
            (bulk("role"), bulk("master")),
        ]);
        let reply = Value::Array(vec![Value::Map(vec![
            (
                bulk("slots"),
                Value::Array(vec![Value::Int(0), Value::Int(16383)]),
            ),
            (bulk("nodes"), Value::Array(vec![node])),
        ])]);
        let shards = Shards::from_shards_reply(&reply, true).unwrap();
        assert_eq!(
            shards.primaries(),
            [Node {
                host: "10.1.1.1".into(),
                port: 7443
            }]
        );

        assert_eq!(Shards::from_shards_reply(&Value::Nil, false), None);
        assert_eq!(Shards::from_slots_reply(&bulk("x")), None);
    }

    /// One CLUSTER SHARDS shard (RESP2): `slots` and its nodes as
    /// `(port, role, health)`.
    fn shard(slots: (i64, i64), nodes: &[(u16, &str, &str)]) -> Value {
        let nodes = nodes
            .iter()
            .map(|&(port, role, health)| {
                Value::Array(vec![
                    bulk("port"),
                    Value::Int(port.into()),
                    bulk("endpoint"),
                    bulk("127.0.0.1"),
                    bulk("role"),
                    bulk(role),
                    bulk("health"),
                    bulk(health),
                ])
            })
            .collect();
        Value::Array(vec![
            bulk("slots"),
            Value::Array(vec![Value::Int(slots.0), Value::Int(slots.1)]),
            bulk("nodes"),
            Value::Array(nodes),
        ])
    }

    #[test]
    fn a_shard_whose_primary_is_down_keeps_it() {
        // Without replicas, or before one is promoted, the failed master
        // still owns its slots: walks must fail on it, not skip its keys.
        let reply = Value::Array(vec![
            shard((0, 5460), &[(7000, "master", "online")]),
            shard((5461, 10922), &[(7001, "master", "fail")]),
            shard(
                (10923, 16383),
                &[(7005, "replica", "online"), (7002, "master", "loading")],
            ),
        ]);
        let shards = Shards::from_shards_reply(&reply, false).unwrap();
        assert_eq!(shards.unowned(), 0);
        let port = |shards: &Shards, slot| shards.primaries()[shards.owner(slot).unwrap()].port;
        assert_eq!(
            [0, 5461, 16383].map(|slot| port(&shards, slot)),
            [7000, 7001, 7002]
        );

        // Once a replica has taken over, the healthy master serves the
        // slots, wherever the failed one is listed.
        for nodes in [
            [(7001, "master", "fail"), (7004, "master", "online")],
            [(7004, "master", "online"), (7001, "master", "fail")],
        ] {
            let reply = Value::Array(vec![shard((0, 16383), &nodes)]);
            let shards = Shards::from_shards_reply(&reply, false).unwrap();
            assert_eq!(port(&shards, 0), 7004);
            assert_eq!(shards.primaries().len(), 1);
        }

        // A shard that lists no master at all leaves its slots unowned.
        let reply = Value::Array(vec![
            shard((0, 99), &[(7003, "replica", "online")]),
            shard((100, 16383), &[(7000, "master", "online")]),
        ]);
        let shards = Shards::from_shards_reply(&reply, false).unwrap();
        assert_eq!(shards.unowned(), 100);
        assert_eq!(shards.owner(99), None);
    }

    #[test]
    fn commands_without_a_key_are_told_apart() {
        let any = |line: &str| {
            let argv: Vec<&[u8]> = line.split_whitespace().map(str::as_bytes).collect();
            routes_to_any_node(&argv)
        };
        for line in [
            "ROLE",
            "role",
            "TIME",
            "LASTSAVE",
            "ECHO hi",
            "CLIENT INFO",
            "client id",
            "CONFIG GET maxmemory",
            "CONFIG",
            "CLUSTER INFO",
            "ACL WHOAMI",
            "EVAL script 0",
            "eval script 0 arg",
            "EVALSHA abc 0",
            "FCALL f 0",
            "EVAL script x k",
            "EVAL script 1",
            "LMPOP 0 LEFT",
            "SCAN 0",
            "SHUTDOWN",
            "REPLICAOF NO ONE",
            "MEMORY USAGE",
            "XREAD COUNT 1",
            "CLUSTER COUNTKEYSINSLOT x",
            "CLUSTER COUNTKEYSINSLOT 16384",
        ] {
            assert!(any(line), "{line} would go to any node");
        }
        for line in [
            "",
            "GET k",
            "set k v",
            "HGET h f",
            "DBSIZE",
            "KEYS *",
            "INFO",
            "PING",
            "RANDOMKEY",
            "MGET a b",
            "DEL a b",
            "EVAL script 1 k",
            "FCALL f 2 a b",
            "LMPOP 1 k LEFT",
            "MEMORY USAGE k",
            "OBJECT ENCODING k",
            "XINFO STREAM s",
            "XREAD COUNT 1 STREAMS s 0",
            "CLUSTER COUNTKEYSINSLOT 5",
            "CONFIG SET a b",
            "CLIENT SETNAME x",
        ] {
            assert!(!any(line), "{line} has a route of its own");
        }
    }

    #[test]
    fn the_more_complete_layout_wins() {
        let layout = |ranges: &[(i64, i64, u16)]| {
            let mut shards = Shards::new();
            for &(start, end, port) in ranges {
                shards.assign(
                    Node {
                        host: "h".into(),
                        port,
                    },
                    start,
                    end,
                );
            }
            shards
        };
        let partial = layout(&[(0, 9999, 1)]);
        let full = layout(&[(0, 9999, 1), (10000, 16383, 2)]);
        assert_eq!(partial.unowned(), 6384);
        assert_eq!(full.unowned(), 0);
        assert_eq!(partial.clone().more_complete(full.clone()), full);
        assert_eq!(full.clone().more_complete(partial.clone()), full);
        // A tie keeps the first (CLUSTER SHARDS', which knows TLS ports).
        let other = layout(&[(0, 9999, 3)]);
        assert_eq!(partial.clone().more_complete(other), partial);
        assert_eq!(Shards::new().unowned(), SLOT_COUNT);
    }

    /// Against the test cluster (`REDCRAB_IT=1`, ports 7000-7005): a layout
    /// that lacks a shard, as one read while a primary was down used to,
    /// is read again before the next walk, so the walk sees every key
    /// instead of silently missing a third of them. Lives here because it
    /// plants that layout.
    #[test]
    fn a_layout_missing_a_shard_is_read_again_before_a_walk() {
        use futures::{StreamExt as _, executor::block_on};

        use crate::{Client, KeyTree, ScanEvent, ScanOptions, client::Conn, config::Topology};

        if std::env::var("REDCRAB_IT").as_deref() != Ok("1") {
            eprintln!(
                "skipped a_layout_missing_a_shard_is_read_again_before_a_walk: REDCRAB_IT is not 1"
            );
            return;
        }
        let config = ConnectionConfig {
            name: "it-cluster-gap".into(),
            host: "127.0.0.1".into(),
            port: 7000,
            topology: Topology::Cluster { seeds: vec![] },
            ..ConnectionConfig::default()
        };
        let client = block_on(Client::connect(config)).expect("is scripts/test-env.sh up?");
        let Conn::Cluster(cluster) = &client.conn else {
            panic!("not a cluster connection");
        };
        let real = cluster.shards().0;
        assert_eq!((real.primaries().len(), real.unowned()), (3, 0));
        // The layout without the first primary's shard.
        let plant_partial_layout = || {
            let mut partial = Shards::new();
            for slot in 0..SLOT_COUNT as u16 {
                if let Some(owner) = real.owner(slot).filter(|&owner| owner != 0) {
                    partial.assign(real.primaries()[owner].clone(), slot.into(), slot.into());
                }
            }
            assert!(partial.unowned() > 0);
            *cluster.layout.shards.write().unwrap() = Arc::new(partial);
            assert_eq!(client.topology_summary(), ["cluster · 2 primaries"]);
        };

        let prefix = format!("it:gap:{}:", std::process::id());
        let keys: Vec<crate::Key> = (0..60)
            .map(|i| format!("{prefix}{i}").into_bytes().into())
            .collect();
        let mut con = client.conn.clone();
        block_on(crate::runtime::spawn({
            let keys = keys.clone();
            async move {
                for key in &keys {
                    cmd("SET")
                        .arg(&**key)
                        .arg(1)
                        .query_async::<()>(&mut con)
                        .await?;
                }
                Ok(())
            }
        }))
        .unwrap();
        let owners: std::collections::HashSet<_> = keys
            .iter()
            .filter_map(|key| real.owner(key_slot(key)))
            .collect();
        assert_eq!(owners.len(), 3, "the keys should be on every primary");
        let options = || ScanOptions {
            pattern: format!("{prefix}*"),
            ..ScanOptions::default()
        };

        // Requests by key are routed by the server's layout and never fail,
        // so they cannot be what notices the gap.
        plant_partial_layout();
        let metas = block_on(client.key_meta(keys.clone())).unwrap();
        assert!(metas.iter().all(|m| m.ty == crate::KeyType::String));
        assert_eq!(client.topology_summary(), ["cluster · 2 primaries"]);

        // A walk reads the layout again first, and sees every key.
        let found: usize = block_on(client.scan(options()).collect::<Vec<_>>())
            .into_iter()
            .map(|batch| batch.unwrap().len())
            .sum();
        assert_eq!(found, keys.len());
        assert_eq!(client.topology_summary(), ["cluster · 3 primaries"]);

        plant_partial_layout();
        let tree = Arc::new(std::sync::Mutex::new(KeyTree::new(Some(b':'))));
        let events: Vec<_> = block_on(client.scan_into(tree.clone(), options()).collect());
        assert!(
            matches!(
                events.last(),
                Some(ScanEvent::Done {
                    keys: 60,
                    truncated: false,
                    ..
                })
            ),
            "{events:?}"
        );

        // So does the status bar's summary over the primaries.
        plant_partial_layout();
        let info = block_on(client.server_info()).unwrap();
        assert_eq!(info.nodes, Some(3));

        assert_eq!(
            block_on(client.delete(keys.clone())).unwrap(),
            keys.len() as u64
        );
    }

    /// Against the test cluster (`REDCRAB_IT=1`, ports 7000-7005): a layout
    /// that names a primary which is gone makes calls routed to it fail;
    /// that failure reads the layout again, so batches are retried on the
    /// right nodes at once, and the next walk visits the real primaries.
    /// Lives here rather than in `tests/` because it plants the stale layout.
    #[test]
    fn a_failed_node_call_refreshes_the_layout() {
        use futures::{StreamExt as _, executor::block_on};

        use crate::{Client, ScanOptions, client::Conn, config::Topology};

        if std::env::var("REDCRAB_IT").as_deref() != Ok("1") {
            eprintln!("skipped a_failed_node_call_refreshes_the_layout: REDCRAB_IT is not 1");
            return;
        }
        let config = ConnectionConfig {
            name: "it-cluster-refresh".into(),
            host: "127.0.0.1".into(),
            port: 7000,
            topology: Topology::Cluster { seeds: vec![] },
            ..ConnectionConfig::default()
        };
        let client = block_on(Client::connect(config)).expect("is scripts/test-env.sh up?");
        let Conn::Cluster(cluster) = &client.conn else {
            panic!("not a cluster connection");
        };
        let real = cluster.shards().0;
        let nodes = |shards: &Shards| {
            let mut nodes = shards.primaries().to_vec();
            nodes.sort_by_key(|n| n.port);
            nodes
        };
        assert_eq!(real.primaries().len(), 3);
        let plant_stale_layout = || {
            let mut stale = (*real).clone();
            stale.primaries[0] = Node {
                host: "127.0.0.1".into(),
                port: 1,
            };
            *cluster.layout.shards.write().unwrap() = Arc::new(stale);
        };

        let prefix = format!("it:refresh:{}:", std::process::id());
        let keys: Vec<crate::Key> = (0..60)
            .map(|i| format!("{prefix}{i}").into_bytes().into())
            .collect();
        for key in &keys {
            block_on(crate::runtime::spawn({
                let mut con = client.conn.clone();
                let key = key.clone();
                async move {
                    Ok(cmd("SET")
                        .arg(&*key)
                        .arg(1)
                        .query_async::<()>(&mut con)
                        .await?)
                }
            }))
            .unwrap();
        }

        // A batch: the keys of the missing node are retried after a refresh.
        plant_stale_layout();
        let metas = block_on(client.key_meta(keys.clone())).unwrap();
        assert!(metas.iter().all(|m| m.ty == crate::KeyType::String));
        assert_eq!(nodes(&cluster.shards().0), nodes(&real));

        // A walk: fails at the missing node, which refreshes the layout.
        plant_stale_layout();
        let options = || ScanOptions {
            pattern: format!("{prefix}*"),
            ..ScanOptions::default()
        };
        let results: Vec<_> = block_on(client.scan(options()).collect());
        assert!(results.iter().any(Result::is_err), "the stale node fails");
        assert_eq!(nodes(&cluster.shards().0), nodes(&real));
        assert_eq!(client.topology_summary(), ["cluster · 3 primaries"]);
        let found: usize = block_on(client.scan(options()).collect::<Vec<_>>())
            .into_iter()
            .map(|batch| batch.unwrap().len())
            .sum();
        assert_eq!(found, keys.len());

        assert_eq!(
            block_on(client.delete(keys.clone())).unwrap(),
            keys.len() as u64
        );
    }

    proptest! {
        #[test]
        fn slots_agree_with_redis_rs(key in proptest::collection::vec(any::<u8>(), 0..40)) {
            prop_assert_eq!(
                Route::new(key_slot(&key), SlotAddr::Master),
                Route::with_key(&key, SlotAddr::Master)
            );
        }

        #[test]
        fn hash_tags_pick_the_slot(
            before in "[^{]{0,10}",
            tag in "[^}]{1,10}",
            after in "\\PC{0,10}",
        ) {
            let key = format!("{before}{{{tag}}}{after}");
            prop_assert_eq!(key_slot(key.as_bytes()), key_slot(tag.as_bytes()));
        }

        #[test]
        fn layouts_survive_both_replies(cuts in proptest::collection::btree_set(1u16..16383, 0..12)) {
            // Contiguous ranges between the cuts, each its own primary.
            let mut bounds: Vec<u16> = vec![0];
            bounds.extend(cuts.iter().copied());
            bounds.push(16384);
            let layout: Vec<(Vec<(u16, u16)>, u16)> = bounds
                .windows(2)
                .enumerate()
                .map(|(ix, w)| (vec![(w[0], w[1] - 1)], 7000 + ix as u16))
                .collect();
            for shards in [
                Shards::from_shards_reply(&shards_reply(&layout), false).unwrap(),
                Shards::from_slots_reply(&slots_reply(&layout)).unwrap(),
            ] {
                prop_assert_eq!(shards.primaries().len(), layout.len());
                for (ranges, port) in &layout {
                    for &(a, b) in ranges {
                        for slot in [a, b, a + (b - a) / 2] {
                            let owner = shards.owner(slot).unwrap();
                            prop_assert_eq!(shards.primaries()[owner].port, *port);
                        }
                    }
                }
            }
        }

        #[test]
        fn layout_parsing_never_panics(ints in proptest::collection::vec(-5i64..20000, 0..12), text in "\\PC{0,8}") {
            let noise = Value::Array(vec![
                Value::Array(ints.iter().map(|&n| Value::Int(n)).collect()),
                Value::Array(vec![bulk("slots"), Value::Array(ints.iter().map(|&n| Value::Int(n)).collect()), bulk(&text), Value::Nil]),
                Value::Array(vec![Value::Int(ints.first().copied().unwrap_or(0)), Value::Int(ints.last().copied().unwrap_or(0)), Value::Array(vec![bulk(&text), Value::Int(ints.len() as i64)])]),
            ]);
            let _ = Shards::from_shards_reply(&noise, false);
            let _ = Shards::from_slots_reply(&noise);
        }
    }
}
