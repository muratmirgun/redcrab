//! Operations on every key matching a pattern: counting them and deleting
//! them. Both walk the keyspace with SCAN, node after node, and report
//! their progress as [`Updates`]: dropping those abandons the walk, and a
//! delete can also be stopped with its [`Stopper`](crate::progress::Stopper)
//! and still say how far it got.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use hashbrown::HashTable;
use redis::{ServerError, ServerErrorKind, Value, cmd, pipe};

use super::{
    Client, Conn, ReadOnlyError,
    cluster::unowned_error,
    conn::{CursorStep, NodeCursor},
};
use crate::{
    format,
    progress::{self, Reporter, Updates},
    runtime::runtime,
};

/// Keys per UNLINK; on a cluster node, single-key UNLINKs per pipeline.
pub const DELETE_BATCH: usize = 500;
/// SCAN's COUNT hint while counting: as the key browser scans.
const COUNT_SCAN_COUNT: u32 = 2000;
/// SCAN's COUNT hint while deleting. A page's keys are deleted in the same
/// round trip that asks for the next page, so this bounds the keys one
/// round trip deletes.
const DELETE_SCAN_COUNT: u32 = 1000;
/// Distinct keys [`Client::count_matching`] remembers to drop the keys
/// SCAN returns twice. Past that a count may include a few twice, and says
/// so (`exact: false`); remembering costs about 16 bytes a key.
pub const COUNT_EXACT_MAX: usize = 2_000_000;

/// How far [`Client::count_matching`] has got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanProgress {
    /// Still walking; `keys` distinct matching keys so far.
    Counting { keys: u64 },
    /// Every node was walked. `exact` is false when there were too many
    /// keys to remember them all (see [`COUNT_EXACT_MAX`]).
    Done {
        keys: u64,
        elapsed: Duration,
        exact: bool,
    },
    /// The walk stopped at an error, after counting `keys`.
    Failed { keys: u64, error: String },
}

/// What a [`Client::delete_matching`] walk has done so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeleteStats {
    /// Keys SCAN returned for the pattern. SCAN may return a key twice;
    /// it then counts twice, and is deleted once.
    pub scanned: u64,
    /// Keys UNLINK removed. Fewer than `scanned` when keys expired or were
    /// deleted by someone else in the meantime.
    pub deleted: u64,
    /// Keys the server would not delete (ACL key patterns, a slot moved to
    /// another node) and that were skipped.
    pub failed: u64,
    /// Why the first of them was refused.
    pub first_error: Option<String>,
}

/// How far [`Client::delete_matching`] has got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteProgress {
    Deleting(DeleteStats),
    /// Every node was walked.
    Done {
        stats: DeleteStats,
        elapsed: Duration,
    },
    /// The walk stopped when asked to ([`Stopper::stop`]), once the round
    /// trip it had in flight was answered: `stats` is everything it did,
    /// and no UNLINK went out after it.
    ///
    /// [`Stopper::stop`]: crate::progress::Stopper::stop
    Stopped {
        stats: DeleteStats,
    },
    /// The walk stopped at an error, after doing what `stats` says (and,
    /// when the error says so, perhaps a little more).
    Failed {
        stats: DeleteStats,
        error: String,
    },
}

impl Client {
    /// Counts the keys matching a glob `pattern` (bytes, so that a folder
    /// prefix that is not UTF-8 can be matched; see
    /// [`browser::folder_pattern`](crate::browser::folder_pattern)), with a
    /// SCAN walk of every node. Keys SCAN returns twice are counted once.
    /// A slow reader sees the latest count, never a backlog. A count has
    /// nothing to undo: dropping the updates (or stopping them) ends it
    /// without a last word.
    pub fn count_matching(&self, pattern: impl Into<Vec<u8>>) -> Updates<ScanProgress> {
        let pattern = pattern.into();
        let conn = self.conn.clone();
        let (reporter, updates) = progress::channel();
        runtime().spawn(async move {
            let started = Instant::now();
            let mut distinct = Distinct::default();
            let (nodes, unowned) = conn.walk_nodes().await;
            for mut con in nodes {
                let mut cursor = NodeCursor::start(&con);
                // What the nodes before this one counted, to go back to if
                // the walk of this one starts over.
                let before = distinct.count;
                loop {
                    if reporter.is_abandoned() || reporter.stop_requested() {
                        return;
                    }
                    let mut scan = cmd("SCAN");
                    scan.arg(cursor.at())
                        .arg("MATCH")
                        .arg(&pattern)
                        .arg("COUNT")
                        .arg(COUNT_SCAN_COUNT);
                    let (next, keys) = match scan.query_async::<(u64, Vec<Vec<u8>>)>(&mut con).await
                    {
                        Ok(page) => page,
                        Err(err) => {
                            reporter.update(ScanProgress::Failed {
                                keys: distinct.count,
                                error: describe(&err),
                            });
                            return;
                        }
                    };
                    let step = match cursor.advance(next, con.epoch()) {
                        Ok(step) => step,
                        Err(err) => {
                            reporter.update(ScanProgress::Failed {
                                keys: distinct.count,
                                error: format!("{err:#}"),
                            });
                            return;
                        }
                    };
                    if step == CursorStep::Restarted {
                        // Counted on another server; the new one is
                        // counted from the start.
                        distinct.restart(before);
                    } else {
                        for key in &keys {
                            distinct.insert(key);
                        }
                    }
                    reporter.update(ScanProgress::Counting {
                        keys: distinct.count,
                    });
                    if step == CursorStep::Done {
                        break;
                    }
                }
                // A key lives on one node only, so nothing seen on this one
                // comes up again on the next.
                distinct.forget();
            }
            if unowned > 0 {
                // Keys in slots no known primary serves were not counted.
                reporter.update(ScanProgress::Failed {
                    keys: distinct.count,
                    error: format!("{:#}", unowned_error(unowned)),
                });
                return;
            }
            reporter.update(ScanProgress::Done {
                keys: distinct.count,
                elapsed: started.elapsed(),
                exact: distinct.exact,
            });
        });
        updates
    }

    /// Deletes every key matching a glob `pattern` (bytes, as for
    /// [`Self::count_matching`]) with UNLINK, node after node, deleting
    /// what each SCAN page returned in the round trip that fetches the
    /// next page. A slow reader sees the latest stats, like the count's.
    ///
    /// To stop, use the updates' [`Stopper`](crate::progress::Stopper) and
    /// read on: the walk sends no UNLINK after it notices, and ends with
    /// [`DeleteProgress::Stopped`] once the round trip it has in flight is
    /// answered, so the stats count every key that went. Dropping the
    /// updates also ends the walk after that round trip, but then nobody
    /// hears what it did.
    ///
    /// Only keys SCAN returned for the pattern are ever deleted, so no key
    /// outside it is touched. A key that starts to match while the walk
    /// runs (created after a count the user confirmed, say) may be deleted
    /// too: the pattern is what was confirmed, not a list of keys.
    ///
    /// Keys the server refuses one by one (ACL key patterns, a slot that
    /// moved) are skipped and counted in [`DeleteStats::failed`]; any other
    /// error (a read-only replica, no permission for UNLINK at all, a lost
    /// connection) ends the walk with [`DeleteProgress::Failed`]. On a
    /// cluster node, whose commands may only name keys of one slot, every
    /// key gets an UNLINK of its own.
    ///
    /// Refused with [`ReadOnlyError`] on a read-only connection, before
    /// anything is sent.
    pub fn delete_matching(
        &self,
        pattern: impl Into<Vec<u8>>,
    ) -> Result<Updates<DeleteProgress>, ReadOnlyError> {
        self.ensure_writable()?;
        let pattern = pattern.into();
        let conn = self.conn.clone();
        let (reporter, updates) = progress::channel();
        runtime().spawn(async move {
            let started = Instant::now();
            let mut stats = DeleteStats::default();
            let mut walked = Ok(Walked::All);
            let (nodes, unowned) = conn.walk_nodes().await;
            for con in nodes {
                walked = delete_on_node(con, &pattern, &mut stats, &reporter).await;
                if !matches!(walked, Ok(Walked::All)) {
                    break;
                }
            }
            if unowned > 0 && matches!(walked, Ok(Walked::All)) {
                // Matching keys in slots no known primary serves are left.
                walked = Err(unowned_error(unowned));
            }
            let event = match walked {
                Ok(Walked::All) => DeleteProgress::Done {
                    stats,
                    elapsed: started.elapsed(),
                },
                Ok(Walked::Stopped) => DeleteProgress::Stopped { stats },
                // Nobody is listening any more.
                Ok(Walked::Abandoned) => return,
                Err(err) => DeleteProgress::Failed {
                    stats,
                    error: format!("{err:#}"),
                },
            };
            reporter.update(event);
        });
        Ok(updates)
    }
}

/// Distinct keys by their 64-bit hash, so that remembering a key costs a
/// hash, not the key. Two keys sharing a hash would count once; among two
/// million keys that happens with a chance of about one in ten million.
pub(crate) struct Distinct {
    hashes: HashTable<u64>,
    hasher: foldhash::fast::RandomState,
    count: u64,
    exact: bool,
}

impl Default for Distinct {
    fn default() -> Self {
        Self {
            hashes: HashTable::new(),
            hasher: foldhash::fast::RandomState::default(),
            count: 0,
            exact: true,
        }
    }
}

impl Distinct {
    /// Counts `key` unless it was seen; `false` when it was.
    pub(crate) fn insert(&mut self, key: &[u8]) -> bool {
        use std::hash::BuildHasher as _;
        let hash = self.hasher.hash_one(key);
        if self.hashes.find(hash, |&h| h == hash).is_some() {
            return false;
        }
        self.count += 1;
        if self.hashes.len() < COUNT_EXACT_MAX {
            self.hashes.insert_unique(hash, hash, |&h| h);
        } else {
            // Keys past the limit are counted without being remembered, so
            // one SCAN returns twice is counted twice.
            self.exact = false;
        }
        true
    }

    /// Forgets the keys but keeps the count, for the next node.
    pub(crate) fn forget(&mut self) {
        self.hashes.clear();
    }

    /// Forgets the keys of the node being walked, whose walk starts over,
    /// and goes back to `count`, what the nodes before it counted.
    fn restart(&mut self, count: u64) {
        self.hashes.clear();
        self.count = count;
    }
}

/// How a walk over one node ended, short of an error.
#[derive(Debug, PartialEq, Eq)]
enum Walked {
    /// Every matching key of the node was walked.
    All,
    /// The reader asked the walk to stop.
    Stopped,
    /// The reader went away: the walk ends quietly.
    Abandoned,
}

/// Deletes the keys matching `pattern` on one node, adding to `stats` as
/// each round trip is answered.
async fn delete_on_node(
    mut con: Conn,
    pattern: &[u8],
    stats: &mut DeleteStats,
    reporter: &Reporter<DeleteProgress>,
) -> Result<Walked> {
    let mut one_slot = con.one_slot_per_command();
    let mut cursor = NodeCursor::start(&con);
    let mut scanning = true;
    // The keys of the last page, deleted with the request for the next.
    let mut keys: Vec<Vec<u8>> = Vec::new();
    loop {
        if reporter.is_abandoned() {
            return Ok(Walked::Abandoned);
        }
        if keys.is_empty() && !scanning {
            return Ok(Walked::All);
        }
        // Only between round trips: UNLINKs once sent run, and their
        // replies have to be counted.
        if reporter.stop_requested() {
            return Ok(Walked::Stopped);
        }
        let mut p = pipe();
        let unlinks = add_unlinks(&mut p, &keys, one_slot);
        if scanning {
            p.cmd("SCAN")
                .arg(cursor.at())
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(DELETE_SCAN_COUNT);
        }
        // Errors come back in place of each command's reply.
        let mut replies: Vec<Value> = p
            .ignore_errors()
            .query_async(&mut con)
            .await
            .map_err(|err| unanswered(&err, &keys))?;
        let page = scanning.then(|| {
            replies
                .pop()
                .ok_or_else(|| anyhow!("the server sent no reply to SCAN"))
                .and_then(scan_page)
        });
        anyhow::ensure!(
            replies.len() == unlinks,
            "the server sent {} replies to {unlinks} UNLINK commands",
            replies.len()
        );
        // The UNLINKs ran before the SCAN, so they count even if it failed.
        let retry = count_unlinks(&keys, replies, one_slot, stats)?;
        if retry.cross_slot {
            // A cluster node, connected to as a plain server.
            one_slot = true;
        }
        if !retry.keys.is_empty() {
            unlink_one_by_one(&mut con, &retry.keys, stats).await?;
        }
        match page.transpose()? {
            Some((next, page)) => {
                stats.scanned += page.len() as u64;
                // Keys the server just said match: deleted next round trip
                // even when the walk starts over, as they match all the
                // same.
                keys = page;
                // After a move to another master the walk starts over
                // there, or it would skip keys and still end "Done".
                scanning = cursor.advance(next, con.epoch())? != CursorStep::Done;
            }
            None => keys.clear(),
        }
        reporter.update(DeleteProgress::Deleting(stats.clone()));
    }
}

/// The error for a round trip that got no answer (a lost connection, a
/// timeout): its UNLINKs for `keys` may have run or not.
fn unanswered(err: &redis::RedisError, keys: &[Vec<u8>]) -> anyhow::Error {
    match keys.len() {
        0 => anyhow!(describe(err)),
        1 => anyhow!("{}; the last key sent may have been deleted", describe(err)),
        n => anyhow!(
            "{}; up to {} more keys may have been deleted",
            describe(err),
            format::count(n as u64)
        ),
    }
}

/// Adds the UNLINKs for `keys` to `p`: one per [`DELETE_BATCH`] keys, or
/// one per key when commands must keep to one slot. Returns how many.
fn add_unlinks(p: &mut redis::Pipeline, keys: &[Vec<u8>], one_slot: bool) -> usize {
    let batch = if one_slot { 1 } else { DELETE_BATCH };
    let mut commands = 0;
    for chunk in keys.chunks(batch) {
        p.cmd("UNLINK").arg(chunk);
        commands += 1;
    }
    commands
}

/// Keys whose UNLINK has to be sent again, one key per command.
#[derive(Debug, Default)]
struct Retry {
    keys: Vec<Vec<u8>>,
    /// A command failed for naming keys of several slots.
    cross_slot: bool,
}

/// Adds the replies to the UNLINKs [`add_unlinks`] made for `keys` to
/// `stats`. A refused key of a single-key command is counted as failed; a
/// multi-key command refused for one of its keys (or for spanning slots)
/// is handed back to be retried key by key, so the other keys still go.
/// An error that ends the walk comes back once every reply is counted:
/// the commands after the one that failed ran too.
fn count_unlinks(
    keys: &[Vec<u8>],
    replies: Vec<Value>,
    one_slot: bool,
    stats: &mut DeleteStats,
) -> Result<Retry> {
    let batch = if one_slot { 1 } else { DELETE_BATCH };
    let mut retry = Retry::default();
    let mut fatal = None;
    for (chunk, reply) in keys.chunks(batch).zip(replies) {
        match reply {
            Value::Int(n) => stats.deleted += u64::try_from(n).unwrap_or(0),
            Value::ServerError(err) => match refusal(&err) {
                Refusal::Fatal => {
                    fatal.get_or_insert_with(|| anyhow!(server_message(&err)));
                }
                Refusal::CrossSlot => {
                    retry.cross_slot = true;
                    retry.keys.extend_from_slice(chunk);
                }
                Refusal::Key if chunk.len() > 1 => retry.keys.extend_from_slice(chunk),
                Refusal::Key => skip(stats, &err),
            },
            other => {
                fatal.get_or_insert_with(|| anyhow!("unexpected reply to UNLINK: {other:?}"));
            }
        }
    }
    match fatal {
        Some(err) => Err(err),
        None => Ok(retry),
    }
}

/// Deletes `keys` with one UNLINK each, in one round trip, skipping the
/// keys the server refuses.
async fn unlink_one_by_one(
    con: &mut Conn,
    keys: &[Vec<u8>],
    stats: &mut DeleteStats,
) -> Result<()> {
    let mut p = pipe();
    add_unlinks(&mut p, keys, true);
    let replies: Vec<Value> = p
        .ignore_errors()
        .query_async(con)
        .await
        .map_err(|err| unanswered(&err, keys))?;
    anyhow::ensure!(
        replies.len() == keys.len(),
        "the server sent {} replies to {} UNLINK commands",
        replies.len(),
        keys.len()
    );
    let retry = count_unlinks(keys, replies, true, stats)?;
    // Single-key commands are never handed back.
    debug_assert!(retry.keys.is_empty());
    Ok(())
}

fn skip(stats: &mut DeleteStats, err: &ServerError) {
    stats.failed += 1;
    if stats.first_error.is_none() {
        stats.first_error = Some(server_message(err));
    }
}

/// A server error as the server and redis-cli put it:
/// "NOPERM No permissions to access a key".
fn server_message(err: &ServerError) -> String {
    match err.details() {
        Some(details) => format!("{} {details}", err.code()),
        None => err.code().to_string(),
    }
}

/// [`server_message`] for a server's error, the library's own words for
/// anything else (a lost connection).
fn describe(err: &redis::RedisError) -> String {
    match (err.code(), err.detail()) {
        (Some(code), Some(detail)) => format!("{code} {detail}"),
        _ => err.to_string(),
    }
}

/// How an UNLINK error affects the walk.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// About the keys named: skip them (one by one, to find out which).
    Key,
    /// The keys are in several slots of a cluster node.
    CrossSlot,
    /// About the connection, the user or the server: stop.
    Fatal,
}

fn refusal(err: &ServerError) -> Refusal {
    match err.kind() {
        Some(ServerErrorKind::CrossSlot) => Refusal::CrossSlot,
        Some(ServerErrorKind::Moved | ServerErrorKind::Ask | ServerErrorKind::TryAgain) => {
            Refusal::Key
        }
        // "No permissions to access a key" is about a key; "User x has no
        // permissions to run the 'unlink' command" would fail for every key.
        Some(ServerErrorKind::NoPerm)
            if !err.details().is_some_and(|d| d.contains("to run the")) =>
        {
            Refusal::Key
        }
        _ => Refusal::Fatal,
    }
}

/// A SCAN reply: the next cursor and the page's keys.
fn scan_page(reply: Value) -> Result<(u64, Vec<Vec<u8>>)> {
    if let Value::ServerError(err) = reply {
        return Err(anyhow!(server_message(&err)));
    }
    redis::from_redis_value(reply)
        .map_err(|err| anyhow!("the server sent an unexpected reply to SCAN: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_error(line: &str) -> ServerError {
        let value = redis::parse_redis_value(format!("-{line}\r\n").as_bytes()).unwrap();
        match value {
            Value::ServerError(err) => err,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sorts_unlink_errors() {
        let cases = [
            ("NOPERM No permissions to access a key", Refusal::Key),
            ("MOVED 5526 127.0.0.1:7001", Refusal::Key),
            ("ASK 5526 127.0.0.1:7001", Refusal::Key),
            (
                "TRYAGAIN Multiple keys request during rehashing of slot",
                Refusal::Key,
            ),
            (
                "CROSSSLOT Keys in request don't hash to the same slot",
                Refusal::CrossSlot,
            ),
            (
                "NOPERM User app has no permissions to run the 'unlink' command",
                Refusal::Fatal,
            ),
            (
                "READONLY You can't write against a read only replica.",
                Refusal::Fatal,
            ),
            (
                "LOADING Redis is loading the dataset in memory",
                Refusal::Fatal,
            ),
            ("ERR unknown command 'UNLINK'", Refusal::Fatal),
            ("CLUSTERDOWN The cluster is down", Refusal::Fatal),
        ];
        for (line, expected) in cases {
            assert_eq!(refusal(&server_error(line)), expected, "{line}");
        }
    }

    fn keys(n: usize) -> Vec<Vec<u8>> {
        (0..n).map(|i| format!("k{i}").into_bytes()).collect()
    }

    #[test]
    fn batches_unlinks() {
        let mut p = pipe();
        assert_eq!(add_unlinks(&mut p, &keys(0), false), 0);
        assert_eq!(add_unlinks(&mut p, &keys(1), false), 1);
        assert_eq!(add_unlinks(&mut p, &keys(DELETE_BATCH), false), 1);
        assert_eq!(add_unlinks(&mut p, &keys(DELETE_BATCH + 1), false), 2);
        assert_eq!(add_unlinks(&mut p, &keys(3), true), 3);
        let commands = p.cmd_iter().count();
        assert_eq!(commands, 1 + 1 + 2 + 3);
        // Each UNLINK names its chunk's keys, in order.
        let last = p.cmd_iter().last().unwrap();
        assert_eq!(last.args_iter().count(), 2);
    }

    #[test]
    fn counts_unlink_replies() {
        let ks = keys(DELETE_BATCH + 2);
        let mut stats = DeleteStats::default();
        // A whole batch deleted, the second refused for one of its keys.
        let replies = vec![
            Value::Int(DELETE_BATCH as i64),
            Value::ServerError(server_error("NOPERM No permissions to access a key")),
        ];
        let retry = count_unlinks(&ks, replies, false, &mut stats).unwrap();
        assert_eq!(stats.deleted, DELETE_BATCH as u64);
        assert_eq!(retry.keys, ks[DELETE_BATCH..]);
        assert!(!retry.cross_slot);
        assert_eq!(stats.failed, 0, "retried, not failed yet");

        // One key per command: refusals count as failed, the first kept.
        let ks = keys(3);
        let replies = vec![
            Value::Int(1),
            Value::ServerError(server_error("NOPERM No permissions to access a key")),
            Value::ServerError(server_error("MOVED 1 127.0.0.1:7001")),
        ];
        let mut stats = DeleteStats::default();
        let retry = count_unlinks(&ks, replies, true, &mut stats).unwrap();
        assert!(retry.keys.is_empty());
        assert_eq!((stats.deleted, stats.failed), (1, 2));
        assert_eq!(
            stats.first_error.as_deref(),
            Some("NOPERM No permissions to access a key")
        );

        // A key that was already gone is not an error.
        let mut stats = DeleteStats::default();
        count_unlinks(&keys(1), vec![Value::Int(0)], true, &mut stats).unwrap();
        assert_eq!(stats, DeleteStats::default());

        // Spanning slots hands the batch back and switches to single keys.
        let mut stats = DeleteStats::default();
        let replies = vec![Value::ServerError(server_error(
            "CROSSSLOT Keys in request don't hash to the same slot",
        ))];
        let retry = count_unlinks(&keys(3), replies, false, &mut stats).unwrap();
        assert!(retry.cross_slot);
        assert_eq!(retry.keys.len(), 3);

        // Anything else stops the walk.
        let replies = vec![Value::ServerError(server_error(
            "READONLY You can't write against a read only replica.",
        ))];
        let err = count_unlinks(&keys(3), replies, false, &mut stats).unwrap_err();
        assert!(format!("{err:#}").contains("READONLY"), "{err:#}");
        let err = count_unlinks(&keys(1), vec![Value::Nil], true, &mut stats).unwrap_err();
        assert!(err.to_string().contains("unexpected reply"), "{err:#}");

        // The commands after the one that failed ran too, and count: the
        // walk ends, but with what was really deleted.
        let mut stats = DeleteStats::default();
        let replies = vec![
            Value::Int(DELETE_BATCH as i64),
            Value::ServerError(server_error("LOADING Redis is loading the dataset")),
            Value::Int(2),
            Value::ServerError(server_error("NOPERM No permissions to access a key")),
            Value::Nil,
            Value::Int(1),
        ];
        let ks = keys(DELETE_BATCH * 6);
        let err = count_unlinks(&ks, replies, false, &mut stats).unwrap_err();
        assert!(
            format!("{err:#}").starts_with("LOADING"),
            "the first: {err:#}"
        );
        assert_eq!(stats.deleted, DELETE_BATCH as u64 + 3);
        let ks = keys(3);
        let replies = vec![
            Value::ServerError(server_error("READONLY You can't write.")),
            Value::Int(1),
            Value::ServerError(server_error("NOPERM No permissions to access a key")),
        ];
        let mut stats = DeleteStats::default();
        assert!(count_unlinks(&ks, replies, true, &mut stats).is_err());
        assert_eq!((stats.deleted, stats.failed), (1, 1));
    }

    #[test]
    fn an_unanswered_round_trip_says_what_may_have_gone() {
        let err = redis::RedisError::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        let plain = describe(&err);
        assert_eq!(unanswered(&err, &[]).to_string(), plain);
        assert_eq!(
            unanswered(&err, &keys(1)).to_string(),
            format!("{plain}; the last key sent may have been deleted")
        );
        assert_eq!(
            unanswered(&err, &keys(1500)).to_string(),
            format!("{plain}; up to 1,500 more keys may have been deleted")
        );
    }

    #[test]
    fn reads_scan_pages() {
        let reply = Value::Array(vec![
            Value::BulkString(b"17".to_vec()),
            Value::Array(vec![
                Value::BulkString(b"a".to_vec()),
                Value::BulkString(b"\xff".to_vec()),
            ]),
        ]);
        assert_eq!(
            scan_page(reply).unwrap(),
            (17, vec![b"a".to_vec(), b"\xff".to_vec()])
        );
        let err = scan_page(Value::ServerError(server_error(
            "NOPERM User app has no permissions to run the 'scan' command",
        )))
        .unwrap_err();
        assert!(format!("{err:#}").contains("NOPERM"), "{err:#}");
        assert!(scan_page(Value::Int(1)).is_err());
    }

    #[test]
    fn counts_distinct_keys() {
        let mut distinct = Distinct::default();
        for key in ["a", "b", "a", "c", "b"] {
            distinct.insert(key.as_bytes());
        }
        assert_eq!((distinct.count, distinct.exact), (3, true));
        distinct.forget();
        distinct.insert(b"a");
        assert_eq!(distinct.count, 4, "the count goes on across nodes");
    }
}
