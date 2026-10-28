use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use anyhow::{Result, bail};
use futures::{SinkExt as _, StreamExt as _, channel::mpsc};
use redis::{RedisError, ServerError, Value, cmd};

use super::{
    Client, Key, ScanEvent, ScanOptions,
    cluster::{key_slot, unowned_error},
};
use crate::{
    runtime::{runtime, spawn},
    tree::KeyTree,
    value::{KeyMeta, KeyType, as_i64, into_string},
};

impl Client {
    /// Streams SCAN results in batches until the keyspace has been walked,
    /// node after node, or [`ScanOptions::limit`] keys have been sent.
    /// Dropping the receiver stops the scan.
    pub fn scan(&self, options: ScanOptions) -> mpsc::Receiver<Result<Vec<Key>>> {
        let limit = options.limit;
        self.scan_batches(options, limit, |keys| {
            keys.into_iter().map(Key::from).collect()
        })
    }

    /// The SCAN loop behind [`Self::scan`] and [`Self::scan_into`]. Each
    /// reply's keys go through `map` on the I/O thread; after `limit` keys
    /// the last batch is cut and the loop ends. On a cluster whose slots
    /// are not all served by a primary it knows, the walk ends with an
    /// error after the keys it found, since it cannot be complete.
    fn scan_batches<T: Send + 'static>(
        &self,
        options: ScanOptions,
        limit: Option<usize>,
        map: fn(Vec<Vec<u8>>) -> T,
    ) -> mpsc::Receiver<Result<T>> {
        let (mut tx, rx) = mpsc::channel(8);
        let conn = self.conn.clone();
        runtime().spawn(async move {
            let (nodes, unowned) = conn.walk_nodes().await;
            let mut left = limit.unwrap_or(usize::MAX);
            for mut con in nodes {
                let mut cursor = 0u64;
                while left > 0 {
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
                        Ok((next, mut keys)) => {
                            keys.truncate(left);
                            left -= keys.len();
                            if !keys.is_empty() && tx.send(Ok(map(keys))).await.is_err() {
                                return;
                            }
                            if next == 0 {
                                break;
                            }
                            cursor = next;
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err.into())).await;
                            return;
                        }
                    }
                }
            }
            if left > 0 && unowned > 0 {
                let _ = tx.send(Err(unowned_error(unowned))).await;
            }
        });
        rx
    }

    /// Scans straight into a shared tree on the I/O threads, so the UI thread
    /// only takes the lock to read. Keys go from the reply buffers into the
    /// tree without another copy. Progress events are coalesced: a slow
    /// reader sees fewer `Progress` events, never a backlog. Dropping the
    /// receiver stops the scan.
    ///
    /// With [`ScanOptions::limit`] the scan stops once the tree holds that
    /// many keys and reports `truncated` if it saw a key it had no room for.
    pub fn scan_into(
        &self,
        tree: Arc<Mutex<KeyTree>>,
        options: ScanOptions,
    ) -> mpsc::Receiver<ScanEvent> {
        let (mut tx, rx) = mpsc::channel(1);
        let limit = options.limit;
        // The tree removes duplicates, so it applies the limit, not the loop.
        let mut batches = self.scan_batches(options, None, |keys| keys);
        runtime().spawn(async move {
            let started = Instant::now();
            let mut keys = 0;
            let mut truncated = false;
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
                        let full = limit.is_some_and(|limit| tree.len() >= limit);
                        // A key the tree neither takes nor has is one left out:
                        // past the limit, or (in theory) past the tree's capacity.
                        if (full || !tree.insert(key)) && !tree.contains(key) {
                            truncated = true;
                            break;
                        }
                    }
                    keys = tree.len();
                }
                if truncated {
                    break;
                }
                if let Err(err) = tx.try_send(ScanEvent::Progress { keys })
                    && err.is_disconnected()
                {
                    return;
                }
            }
            // Stops the SCAN loop if it is still running.
            drop(batches);
            let _ = tx
                .send(ScanEvent::Done {
                    keys,
                    elapsed: started.elapsed(),
                    truncated,
                })
                .await;
        });
        rx
    }

    /// Type and TTL for each key, in one pipelined round trip (on a cluster,
    /// one per primary, all at once). Fails if the user may not see one of
    /// the keys; see [`Self::key_meta_permitted`] for a batch that may hold
    /// such keys.
    pub fn key_meta(
        &self,
        keys: Vec<Key>,
    ) -> impl Future<Output = Result<Vec<KeyMeta>>> + Send + 'static {
        let fetch = self.fetch_meta(keys);
        async move {
            fetch
                .await?
                .into_iter()
                .map(|meta| meta.map_err(|err| RedisError::from(err).into()))
                .collect()
        }
    }

    /// Like [`Self::key_meta`], but a key the user's ACL does not let it
    /// see gets `None` instead of failing the whole batch. SCAN lists such
    /// keys too, so a screen of them usually has a few, and the keys the
    /// user may see should still show their type and TTL.
    pub fn key_meta_permitted(
        &self,
        keys: Vec<Key>,
    ) -> impl Future<Output = Result<Vec<Option<KeyMeta>>>> + Send + 'static {
        let fetch = self.fetch_meta(keys);
        async move {
            fetch
                .await?
                .into_iter()
                .map(|meta| match meta {
                    Ok(meta) => Ok(Some(meta)),
                    Err(err) if err.code() == "NOPERM" => Ok(None),
                    Err(err) => Err(RedisError::from(err).into()),
                })
                .collect()
        }
    }

    /// Per key, its type and TTL or the error the server answered for it.
    fn fetch_meta(
        &self,
        keys: Vec<Key>,
    ) -> impl Future<Output = Result<Vec<Result<KeyMeta, ServerError>>>> + Send + 'static {
        let con = self.conn.clone();
        spawn(async move {
            let slices: Vec<&[u8]> = keys.iter().map(|key| &**key).collect();
            let replies = con
                .per_key(&slices, 2, &|p, key| {
                    p.cmd("TYPE").arg(key).cmd("PTTL").arg(key);
                })
                .await?;
            anyhow::ensure!(
                replies.len() == keys.len() * 2,
                "the server sent {} replies for {} keys",
                replies.len(),
                keys.len()
            );
            Ok(replies
                .as_chunks::<2>()
                .0
                .iter()
                .map(|reply| match reply {
                    [Value::ServerError(err), _] | [_, Value::ServerError(err)] => Err(err.clone()),
                    [ty, ttl] => Ok(KeyMeta {
                        ty: KeyType::parse(&into_string(ty.clone())),
                        ttl_ms: as_i64(ttl),
                    }),
                })
                .collect())
        })
    }

    /// `MEMORY USAGE` in bytes, if the server allows it.
    pub fn memory_usage(
        &self,
        key: Key,
    ) -> impl Future<Output = Result<Option<u64>>> + Send + 'static {
        let mut con = self.conn.clone();
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

    /// Deletes keys with UNLINK so large values are freed off the main thread
    /// of the server. Returns how many existed. On a cluster each key gets
    /// an UNLINK of its own (keys in different slots cannot share one), sent
    /// as one pipeline per primary.
    pub fn delete(&self, keys: Vec<Key>) -> impl Future<Output = Result<u64>> + Send + 'static {
        let writable = self.ensure_writable();
        let mut con = self.conn.clone();
        spawn(async move {
            writable?;
            if keys.is_empty() {
                return Ok(0);
            }
            if con.is_cluster() {
                let slices: Vec<&[u8]> = keys.iter().map(|key| &**key).collect();
                let replies = con
                    .per_key(&slices, 1, &|p, key| {
                        p.cmd("UNLINK").arg(key);
                    })
                    .await?;
                let mut deleted = 0;
                for reply in replies {
                    match reply {
                        Value::ServerError(err) => return Err(RedisError::from(err).into()),
                        reply => deleted += u64::try_from(as_i64(&reply)).unwrap_or(0),
                    }
                }
                return Ok(deleted);
            }
            let mut unlink = cmd("UNLINK");
            for key in &keys {
                unlink.arg(&**key);
            }
            Ok(unlink.query_async(&mut con).await?)
        })
    }

    /// Sets the TTL in seconds, or removes it with `None`.
    pub fn set_ttl(
        &self,
        key: Key,
        seconds: Option<u64>,
    ) -> impl Future<Output = Result<bool>> + Send + 'static {
        let writable = self.ensure_writable();
        let mut con = self.conn.clone();
        spawn(async move {
            writable?;
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

    /// Renames without overwriting an existing key. On a cluster both names
    /// must hash to the same slot (a hash tag such as `{user:42}` in both
    /// does that), since a key cannot move between nodes by renaming.
    pub fn rename(&self, from: Key, to: Key) -> impl Future<Output = Result<()>> + Send + 'static {
        let writable = self.ensure_writable();
        let mut con = self.conn.clone();
        spawn(async move {
            writable?;
            let cluster = con.is_cluster();
            if cluster && key_slot(&from) != key_slot(&to) {
                bail!("{}", cross_slot(&from, &to));
            }
            let renamed: bool = match cmd("RENAMENX")
                .arg(&*from)
                .arg(&*to)
                .query_async(&mut con)
                .await
            {
                Err(err) if err.code() == Some("CROSSSLOT") => bail!("{}", cross_slot(&from, &to)),
                renamed => renamed?,
            };
            if !renamed {
                bail!(
                    "a key named {} already exists",
                    String::from_utf8_lossy(&to)
                );
            }
            Ok(())
        })
    }
}

/// Why `from` cannot be renamed to `to` on a cluster.
fn cross_slot(from: &[u8], to: &[u8]) -> String {
    format!(
        "in a cluster a key can only be renamed to a name in the same hash slot, and {} (slot {}) and {} (slot {}) are not; give both names the same hash tag, e.g. {{user:42}}:a and {{user:42}}:b",
        crate::format::bytes_inline(from),
        key_slot(from),
        crate::format::bytes_inline(to),
        key_slot(to)
    )
}
