use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use futures::{SinkExt as _, StreamExt as _, channel::mpsc};
use redis::{
    Value,
    aio::{ConnectionManager, ConnectionManagerConfig},
    cmd, pipe,
};

use crate::{
    config::ConnectionConfig,
    runtime::{runtime, spawn},
    tree::KeyTree,
    value::{
        self, Cursor, KeyMeta, KeyType, ValueData, ValuePage, as_i64, flatten, into_bytes,
        into_string, pairs, scan_reply, stream_entries,
    },
};

/// Keys are binary-safe and shared between the tree, caches and requests.
pub type Key = Arc<[u8]>;

/// Strings longer than this are previewed, not loaded whole.
const STRING_PREVIEW_BYTES: usize = 1 << 20;

/// A cheap-to-clone handle to one Redis server. Every clone shares a single
/// multiplexed connection that reconnects on its own.
#[derive(Clone)]
pub struct Client {
    con: ConnectionManager,
    config: Arc<ConnectionConfig>,
}

#[derive(Clone, Debug, Default)]
pub struct ServerInfo {
    pub version: String,
    pub mode: String,
    pub used_memory_human: String,
    pub connected_clients: u64,
    pub uptime_secs: u64,
    pub db_size: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScanEvent {
    Progress { keys: usize },
    Done { keys: usize, elapsed: Duration },
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub pattern: String,
    /// The COUNT hint per SCAN round trip.
    pub count: u32,
    /// Restricts SCAN to one type (`TYPE` argument, Redis 6+).
    pub type_filter: Option<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            pattern: "*".into(),
            count: 2000,
            type_filter: None,
        }
    }
}

impl Client {
    pub async fn connect(config: ConnectionConfig) -> Result<Self> {
        let info = config.connection_info()?;
        spawn(async move {
            let client = redis::Client::open(info)?;
            let manager_config = ConnectionManagerConfig::new()
                .set_connection_timeout(Some(Duration::from_secs(5)))
                .set_response_timeout(Some(Duration::from_secs(30)))
                .set_number_of_retries(1);
            let con = ConnectionManager::new_with_config(client, manager_config)
                .await
                .with_context(|| format!("could not connect to {}", config.address()))?;
            Ok(Self {
                con,
                config: Arc::new(config),
            })
        })
        .await
    }

    pub fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    pub fn ping(&self) -> impl Future<Output = Result<Duration>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            let started = Instant::now();
            cmd("PING").query_async::<()>(&mut con).await?;
            Ok(started.elapsed())
        })
    }

    pub fn server_info(&self) -> impl Future<Output = Result<ServerInfo>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            let (info, db_size): (String, u64) = pipe()
                .cmd("INFO")
                .cmd("DBSIZE")
                .query_async(&mut con)
                .await?;
            let field = |name: &str| {
                info.lines()
                    .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            Ok(ServerInfo {
                version: field("redis_version"),
                mode: field("redis_mode"),
                used_memory_human: field("used_memory_human"),
                connected_clients: field("connected_clients").parse().unwrap_or(0),
                uptime_secs: field("uptime_in_seconds").parse().unwrap_or(0),
                db_size,
            })
        })
    }

    /// Streams SCAN results in batches until the keyspace has been walked.
    /// Dropping the receiver stops the scan.
    pub fn scan(&self, options: ScanOptions) -> mpsc::Receiver<Result<Vec<Key>>> {
        let (mut tx, rx) = mpsc::channel(8);
        let mut con = self.con.clone();
        runtime().spawn(async move {
            let mut cursor = 0u64;
            loop {
                let mut scan = cmd("SCAN");
                scan.arg(cursor)
                    .arg("MATCH")
                    .arg(&options.pattern)
                    .arg("COUNT")
                    .arg(options.count);
                if let Some(ty) = &options.type_filter {
                    scan.arg("TYPE").arg(ty);
                }
                match scan.query_async::<(u64, Vec<Vec<u8>>)>(&mut con).await {
                    Ok((next, keys)) => {
                        if !keys.is_empty() {
                            let batch = keys.into_iter().map(Key::from).collect();
                            if tx.send(Ok(batch)).await.is_err() {
                                return;
                            }
                        }
                        if next == 0 {
                            return;
                        }
                        cursor = next;
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err.into())).await;
                        return;
                    }
                }
            }
        });
        rx
    }

    /// Scans straight into a shared tree on the I/O threads, so the UI thread
    /// only takes the lock to read. Progress events are coalesced: a slow
    /// reader sees fewer `Progress` events, never a backlog. Dropping the
    /// receiver stops the scan.
    pub fn scan_into(
        &self,
        tree: Arc<Mutex<KeyTree>>,
        options: ScanOptions,
    ) -> mpsc::Receiver<ScanEvent> {
        let (mut tx, rx) = mpsc::channel(1);
        let mut batches = self.scan(options);
        runtime().spawn(async move {
            let started = Instant::now();
            let mut keys = 0;
            while let Some(batch) = batches.next().await {
                let batch = match batch {
                    Ok(batch) => batch,
                    Err(err) => {
                        let _ = tx.send(ScanEvent::Failed(format!("{err:#}"))).await;
                        return;
                    }
                };
                {
                    let mut tree = tree.lock().unwrap_or_else(|e| e.into_inner());
                    for key in &batch {
                        tree.insert(key);
                    }
                    keys = tree.len();
                }
                if let Err(err) = tx.try_send(ScanEvent::Progress { keys })
                    && err.is_disconnected()
                {
                    return;
                }
            }
            let _ = tx
                .send(ScanEvent::Done {
                    keys,
                    elapsed: started.elapsed(),
                })
                .await;
        });
        rx
    }

    /// Type and TTL for each key, in one pipelined round trip.
    pub fn key_meta(
        &self,
        keys: Vec<Key>,
    ) -> impl Future<Output = Result<Vec<KeyMeta>>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            let mut p = pipe();
            for key in &keys {
                p.cmd("TYPE").arg(&**key).cmd("PTTL").arg(&**key);
            }
            let replies: Vec<Value> = p.query_async(&mut con).await?;
            Ok(replies
                .as_chunks::<2>()
                .0
                .iter()
                .map(|reply| KeyMeta {
                    ty: KeyType::parse(&into_string(reply[0].clone())),
                    ttl_ms: as_i64(&reply[1]),
                })
                .collect())
        })
    }

    /// `MEMORY USAGE` in bytes, if the server allows it.
    pub fn memory_usage(
        &self,
        key: Key,
    ) -> impl Future<Output = Result<Option<u64>>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            let bytes: Option<u64> = cmd("MEMORY")
                .arg("USAGE")
                .arg(&*key)
                .arg("SAMPLES")
                .arg(0)
                .query_async(&mut con)
                .await
                .ok()
                .flatten();
            Ok(bytes)
        })
    }

    /// Loads up to `count` elements of `key` starting at `cursor`.
    pub fn load(
        &self,
        key: Key,
        ty: KeyType,
        cursor: Cursor,
        count: usize,
    ) -> impl Future<Output = Result<ValuePage>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            let key = &*key;
            let page = match ty {
                KeyType::String => {
                    let (len, head): (u64, Vec<u8>) = pipe()
                        .cmd("STRLEN")
                        .arg(key)
                        .cmd("GETRANGE")
                        .arg(key)
                        .arg(0)
                        .arg(STRING_PREVIEW_BYTES - 1)
                        .query_async(&mut con)
                        .await?;
                    ValuePage {
                        data: ValueData::String(head),
                        total: len,
                        next: None,
                    }
                }
                KeyType::Hash | KeyType::Set => {
                    let (len_cmd, scan_cmd) = if ty == KeyType::Hash {
                        ("HLEN", "HSCAN")
                    } else {
                        ("SCARD", "SSCAN")
                    };
                    let at = match cursor {
                        Cursor::Scan(c) => c,
                        _ => 0,
                    };
                    let (total, reply): (u64, Value) = pipe()
                        .cmd(len_cmd)
                        .arg(key)
                        .cmd(scan_cmd)
                        .arg(key)
                        .arg(at)
                        .arg("COUNT")
                        .arg(count)
                        .query_async(&mut con)
                        .await?;
                    let (next, items) = scan_reply(reply);
                    let data = if ty == KeyType::Hash {
                        ValueData::Hash(pairs(items))
                    } else {
                        ValueData::Set(flatten(items).into_iter().map(into_bytes).collect())
                    };
                    ValuePage {
                        data,
                        total,
                        next: (next != 0).then_some(Cursor::Scan(next)),
                    }
                }
                KeyType::List | KeyType::ZSet => {
                    let offset = match cursor {
                        Cursor::Offset(o) => o,
                        _ => 0,
                    };
                    let stop = offset + count as u64 - 1;
                    let mut p = pipe();
                    if ty == KeyType::List {
                        p.cmd("LLEN")
                            .arg(key)
                            .cmd("LRANGE")
                            .arg(key)
                            .arg(offset)
                            .arg(stop);
                    } else {
                        p.cmd("ZCARD")
                            .arg(key)
                            .cmd("ZRANGE")
                            .arg(key)
                            .arg(offset)
                            .arg(stop)
                            .arg("WITHSCORES");
                    }
                    let (total, reply): (u64, Value) = p.query_async(&mut con).await?;
                    let items = flatten(reply);
                    let data = if ty == KeyType::List {
                        ValueData::List(items.into_iter().map(into_bytes).collect())
                    } else {
                        let mut items = items.into_iter();
                        let mut members = Vec::with_capacity(items.len() / 2);
                        while let (Some(m), Some(s)) = (items.next(), items.next()) {
                            members.push((into_bytes(m), value::as_f64(&s)));
                        }
                        ValueData::ZSet(members)
                    };
                    let end = offset + data.len() as u64;
                    ValuePage {
                        data,
                        total,
                        next: (end < total).then_some(Cursor::Offset(end)),
                    }
                }
                KeyType::Stream => {
                    let start = match &cursor {
                        Cursor::StreamAfter(id) => format!("({id}"),
                        _ => "-".into(),
                    };
                    let (total, reply): (u64, Value) = pipe()
                        .cmd("XLEN")
                        .arg(key)
                        .cmd("XRANGE")
                        .arg(key)
                        .arg(start)
                        .arg("+")
                        .arg("COUNT")
                        .arg(count)
                        .query_async(&mut con)
                        .await?;
                    let entries = stream_entries(reply);
                    let next = (entries.len() == count)
                        .then(|| entries.last().map(|e| Cursor::StreamAfter(e.id.clone())))
                        .flatten();
                    ValuePage {
                        data: ValueData::Stream(entries),
                        total,
                        next,
                    }
                }
                KeyType::Json => {
                    let json: String = cmd("JSON.GET").arg(key).query_async(&mut con).await?;
                    ValuePage {
                        total: json.len() as u64,
                        data: ValueData::Json(json),
                        next: None,
                    }
                }
                KeyType::Other(name) => ValuePage {
                    data: ValueData::Unsupported(name),
                    total: 0,
                    next: None,
                },
                KeyType::None => ValuePage {
                    data: ValueData::Missing,
                    total: 0,
                    next: None,
                },
            };
            Ok(page)
        })
    }

    /// Deletes keys with UNLINK so large values are freed off the main thread
    /// of the server.
    pub fn delete(&self, keys: Vec<Key>) -> impl Future<Output = Result<u64>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            if keys.is_empty() {
                return Ok(0);
            }
            let mut unlink = cmd("UNLINK");
            for key in &keys {
                unlink.arg(&**key);
            }
            Ok(unlink.query_async(&mut con).await?)
        })
    }

    pub fn set_string(
        &self,
        key: Key,
        value: Vec<u8>,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            cmd("SET")
                .arg(&*key)
                .arg(value)
                .arg("KEEPTTL")
                .query_async::<()>(&mut con)
                .await?;
            Ok(())
        })
    }

    /// Sets the TTL in seconds, or removes it with `None`.
    pub fn set_ttl(
        &self,
        key: Key,
        seconds: Option<u64>,
    ) -> impl Future<Output = Result<bool>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            let changed: bool = match seconds {
                Some(s) => {
                    cmd("EXPIRE")
                        .arg(&*key)
                        .arg(s)
                        .query_async(&mut con)
                        .await?
                }
                None => cmd("PERSIST").arg(&*key).query_async(&mut con).await?,
            };
            Ok(changed)
        })
    }

    /// Renames without overwriting an existing key.
    pub fn rename(&self, from: Key, to: Key) -> impl Future<Output = Result<()>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            let renamed: bool = cmd("RENAMENX")
                .arg(&*from)
                .arg(&*to)
                .query_async(&mut con)
                .await?;
            if !renamed {
                bail!(
                    "a key named {} already exists",
                    String::from_utf8_lossy(&to)
                );
            }
            Ok(())
        })
    }

    /// Runs an arbitrary command, as typed into a console.
    pub fn exec(&self, argv: Vec<String>) -> impl Future<Output = Result<Value>> + Send + 'static {
        let mut con = self.con.clone();
        spawn(async move {
            let Some((name, args)) = argv.split_first() else {
                bail!("empty command");
            };
            let mut command = cmd(name);
            for arg in args {
                command.arg(arg);
            }
            Ok(command.query_async(&mut con).await?)
        })
    }
}
