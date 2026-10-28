//! Keyspace operations: counting and deleting every key that matches a
//! pattern or sits in a folder (`count_matching`, `delete_matching`), the
//! key browser after a folder delete, and the databases of a server
//! (`databases`, `with_db`).
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400): database 0 with each test's own prefix, and
//! databases 10, 11 and 15 (keyspace operations own them; each is used by
//! one test below, which says so). Also `auth` (6401) with prefixes, and
//! `cluster` (7000-7005), through the cluster client and node by node as
//! plain servers, with keys under each test's prefix.

#[macro_use]
mod common;

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use common::*;
use futures::StreamExt as _;
use proptest::{
    collection::vec,
    prelude::*,
    test_runner::{Config, TestRunner},
};
use redcrab_core::{
    Client, ConnectionConfig, DeleteProgress, DeleteStats, Key, RowKind, ScanProgress,
    browser::{self, KeyBrowser},
    bulk::{BulkDelete, Ending, Phase, Refresh, Stop, Target},
};
use redis::Connection;

/// Owned by `databases_report_their_sizes`, which empties it.
const SIZES_DB: u32 = 10;
/// Owned by `with_db_opens_another_database`.
const SWITCH_DB: u32 = 11;
/// Owned by `delete_matching_is_refused_when_read_only`, which finds the
/// read-only client's connection by it.
const READ_ONLY_DB: u32 = 15;

/// Sets each name under the prefix, in one round trip.
fn seed<N: AsRef<[u8]>>(t: &It, con: &mut Connection, names: impl IntoIterator<Item = N>) {
    let mut pipe = redis::pipe();
    for name in names {
        pipe.cmd("SET").arg(&*t.bkey(name)).arg("v").ignore();
    }
    pipe.query::<()>(con).unwrap();
}

/// `n` keys `{folder}:{i}` under the prefix, a thousand per MSET.
fn seed_folder(t: &It, con: &mut Connection, folder: &str, n: usize) {
    let names: Vec<String> = (0..n).map(|i| format!("{folder}:{i}")).collect();
    for chunk in names.chunks(1000) {
        let mut mset = redis::cmd("MSET");
        for name in chunk {
            mset.arg(&*t.bkey(name)).arg("v");
        }
        mset.query::<()>(con).unwrap();
    }
}

/// Every key matching `pattern`, by a plain SCAN walk.
fn matching(con: &mut Connection, pattern: &[u8]) -> BTreeSet<Vec<u8>> {
    let mut found = BTreeSet::new();
    let mut cursor = 0u64;
    loop {
        let (next, keys): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(1000)
            .query(con)
            .unwrap();
        found.extend(keys);
        if next == 0 {
            return found;
        }
        cursor = next;
    }
}

/// The names under the prefix that are left, as text.
fn left(t: &It, con: &mut Connection) -> BTreeSet<String> {
    matching(con, t.pattern().as_bytes())
        .into_iter()
        .map(|key| String::from_utf8_lossy(&key[t.prefix().len()..]).into_owned())
        .collect()
}

/// The pattern for the folder `name` under the prefix.
fn folder(t: &It, name: impl AsRef<[u8]>) -> Vec<u8> {
    browser::folder_pattern(&t.bkey(name), b':')
}

/// Runs `count_matching` to the end, checks that its progress only goes
/// up, and returns the exact count it ended with.
fn count(client: &Client, pattern: impl Into<Vec<u8>>) -> u64 {
    let events = collect(client.count_matching(pattern));
    let (last, progress) = events.split_last().expect("no events");
    let mut seen = 0;
    for event in progress {
        match event {
            ScanProgress::Counting { keys } => {
                assert!(*keys >= seen, "{events:?}");
                seen = *keys;
            }
            other => panic!("{other:?} before the end: {events:?}"),
        }
    }
    match last {
        ScanProgress::Done {
            keys, exact: true, ..
        } => {
            assert!(*keys >= seen);
            *keys
        }
        other => panic!("the count did not finish: {other:?}"),
    }
}

/// Runs `delete_matching` to the end, checks that its progress only goes
/// up, and returns what it did.
fn delete(client: &Client, pattern: impl Into<Vec<u8>>) -> DeleteStats {
    let events = collect(client.delete_matching(pattern).expect("refused"));
    let (last, progress) = events.split_last().expect("no events");
    let mut seen = DeleteStats::default();
    for event in progress {
        match event {
            DeleteProgress::Deleting(stats) => {
                assert!(stats.scanned >= seen.scanned, "{events:?}");
                assert!(stats.deleted >= seen.deleted, "{events:?}");
                assert!(stats.failed >= seen.failed, "{events:?}");
                assert!(stats.deleted + stats.failed <= stats.scanned, "{stats:?}");
                seen = stats.clone();
            }
            other => panic!("{other:?} before the end"),
        }
    }
    match last {
        DeleteProgress::Done { stats, .. } => {
            assert!(stats.deleted >= seen.deleted);
            assert!(stats.deleted + stats.failed <= stats.scanned, "{stats:?}");
            stats.clone()
        }
        other => panic!("the delete did not finish: {other:?}"),
    }
}

#[test]
fn counts_exactly_the_keys_matching() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    seed_folder(&t, &mut con, "user", 2500);
    seed(
        &t,
        &mut con,
        ["user", "user2:1", "user2:2", "users", "orders:1"],
    );
    let client = t.connect(standalone());

    assert_eq!(count(&client, folder(&t, "user")), 2500);
    assert_eq!(count(&client, folder(&t, "user2")), 2);
    assert_eq!(count(&client, t.pattern()), 2505);
    assert_eq!(count(&client, t.pattern_for(b"user")), 2504);
    assert_eq!(count(&client, t.pattern_for(b"nothing")), 0);
    // A folder that is also a key: the key itself is not in the folder.
    assert_eq!(count(&client, folder(&t, "users")), 0);
}

#[test]
fn deletes_a_folder_and_nothing_else() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    seed_folder(&t, &mut con, "user", 2500);
    // The folder's own key, a key ending in the separator (in the folder),
    // siblings that share the first bytes, and a deeper folder.
    seed(
        &t,
        &mut con,
        ["user", "user:", "user::x", "user2:1", "users", "xuser:1"],
    );
    seed_folder(&t, &mut con, "user:deep", 10);
    let client = t.connect(standalone());

    let stats = delete(&client, folder(&t, "user"));
    assert_eq!(stats.deleted, 2500 + 2 + 10);
    assert_eq!((stats.failed, stats.first_error), (0, None));
    assert!(stats.scanned >= stats.deleted);
    assert_eq!(
        left(&t, &mut con),
        ["user", "user2:1", "users", "xuser:1"]
            .map(String::from)
            .into()
    );

    // Nothing left to delete is not an error.
    let stats = delete(&client, folder(&t, "user"));
    assert_eq!(stats, DeleteStats::default());
}

#[test]
fn deletes_by_pattern() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    seed(&t, &mut con, ["a:1", "a:2", "a:10", "b:1", "b:x", "c"]);
    let client = t.connect(standalone());
    let prefix = String::from_utf8(browser::escape_glob(t.prefix().as_bytes())).unwrap();

    let stats = delete(&client, format!("{prefix}[ab]:?"));
    assert_eq!(stats.deleted, 4);
    assert_eq!(left(&t, &mut con), ["a:10", "c"].map(String::from).into());
}

/// Folder names with glob characters, a backslash, bytes that are not
/// UTF-8 and an empty name: each is deleted alone, and siblings that the
/// unescaped name would match stay.
#[test]
fn folder_names_are_matched_literally() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let folders: [&[u8]; 10] = [
        b"a*",
        b"a?",
        b"[ab]",
        br"a\b",
        b"a]",
        b"[",
        b"\xff\x00",
        "ünï".as_bytes(),
        b"",
        b"*",
    ];
    // Each folder holds two keys; the siblings would match some unescaped
    // folder pattern.
    let siblings: [&[u8]; 9] = [
        b"ab:1",
        b"a*b:1",
        b"ax:1",
        b"a:1",
        b"b:1",
        b"\xff\x01:1",
        b"x:1",
        b"a]b:1",
        b"[x:1",
    ];
    let mut all = BTreeSet::new();
    for name in folders {
        for key in [b"1".as_slice(), b"\xfe two"] {
            all.insert([name, b":", key].concat());
        }
    }
    all.extend(siblings.iter().map(|s| s.to_vec()));
    seed(&t, &mut con, &all);
    let client = t.connect(standalone());
    let names = |con: &mut Connection| -> BTreeSet<Vec<u8>> {
        matching(con, t.pattern().as_bytes())
            .into_iter()
            .map(|key| key[t.prefix().len()..].to_vec())
            .collect()
    };
    assert_eq!(names(&mut con), all);

    for name in folders {
        let pattern = folder(&t, name);
        let shown = String::from_utf8_lossy(name).into_owned();
        assert_eq!(count(&client, pattern.clone()), 2, "{shown}");
        let stats = delete(&client, pattern);
        assert_eq!((stats.deleted, stats.failed), (2, 0), "{shown}");
        for key in [b"1".as_slice(), b"\xfe two"] {
            all.remove(&[name, b":", key].concat());
        }
        assert_eq!(names(&mut con), all, "after deleting {shown}");
    }
    assert_eq!(all, siblings.iter().map(|s| s.to_vec()).collect());
}

/// Bytes that make names interesting: glob characters, the separator, a
/// backslash, bytes that are not UTF-8.
fn tricky(len: std::ops::Range<usize>) -> impl Strategy<Value = Vec<u8>> {
    vec(
        prop_oneof![
            3 => Just(b'a'),
            1 => Just(b'b'),
            3 => Just(b':'),
            1 => Just(b'*'),
            1 => Just(b'?'),
            1 => Just(b'['),
            1 => Just(b']'),
            1 => Just(b'^'),
            1 => Just(b'-'),
            1 => Just(b'\\'),
            1 => Just(0xff),
            1 => Just(0x00),
        ],
        len,
    )
}

/// The server agrees with the folder patterns: for random folder names
/// and keys, inside the folder and next to it, counting and deleting the
/// folder find exactly the keys that start with its name and a separator.
#[test]
fn folder_patterns_match_like_the_server() {
    let Some(t) = it!() else { return };
    let con = RefCell::new(t.seed0());
    let client = t.connect(standalone());
    let case = Cell::new(0u32);
    let mut runner = TestRunner::new(Config {
        cases: 64,
        failure_persistence: None,
        ..Config::default()
    });
    let keys = vec((any::<bool>(), tricky(0..6)), 1..10);
    runner
        .run(&(tricky(0..6), keys), |(name, keys)| {
            case.set(case.get() + 1);
            let mut con = con.borrow_mut();
            // Each case has a namespace of its own under the prefix.
            let base = t.bkey(format!("case{}/", case.get()));
            let prefix = [&base[..], &name].concat();
            let inside = [&prefix[..], b":"].concat();
            let keys: BTreeSet<Vec<u8>> = keys
                .into_iter()
                .map(|(under, rest)| {
                    let start = if under { &inside } else { &base[..] };
                    [start, &rest].concat()
                })
                .collect();
            let mut pipe = redis::pipe();
            for key in &keys {
                pipe.cmd("SET").arg(key).arg("v").ignore();
            }
            pipe.query::<()>(&mut *con).unwrap();
            let expected: BTreeSet<Vec<u8>> = keys
                .iter()
                .filter(|k| k.starts_with(&inside))
                .cloned()
                .collect();

            let pattern = browser::folder_pattern(&prefix, b':');
            prop_assert_eq!(count(&client, pattern.clone()), expected.len() as u64);
            let stats = delete(&client, pattern);
            prop_assert_eq!(stats.deleted, expected.len() as u64);
            let mut base_pattern = browser::escape_glob(&base);
            base_pattern.push(b'*');
            let rest: BTreeSet<Vec<u8>> = keys.difference(&expected).cloned().collect();
            prop_assert_eq!(matching(&mut con, &base_pattern), rest);
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_count_is_a_snapshot_and_the_delete_follows_the_pattern() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    seed_folder(&t, &mut con, "job", 100);
    let client = t.connect(standalone());
    assert_eq!(count(&client, folder(&t, "job")), 100);
    // After the count, half the keys go and a new one starts matching: the
    // delete removes what matches when it runs.
    let gone: Vec<Key> = (0..50).map(|i| t.bkey(format!("job:{i}"))).collect();
    redis::cmd("UNLINK")
        .arg(gone.iter().map(|k| &k[..]).collect::<Vec<_>>())
        .query::<()>(&mut con)
        .unwrap();
    seed(&t, &mut con, ["job:new", "jobs"]);
    let stats = delete(&client, folder(&t, "job"));
    assert_eq!(stats.deleted, 51);
    assert_eq!(left(&t, &mut con), ["jobs".to_string()].into());
}

#[test]
fn dropping_the_receiver_stops_a_delete() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    const KEYS: usize = 60_000;
    seed_folder(&t, &mut con, "bulk", KEYS);
    let client = t.connect(standalone());
    let pattern = folder(&t, "bulk");

    let mut events = client.delete_matching(pattern.clone()).unwrap();
    // Stop once the first keys are gone.
    loop {
        match block_on(events.next()) {
            Some(DeleteProgress::Deleting(stats)) if stats.deleted > 0 => break,
            Some(DeleteProgress::Deleting(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    drop(events);
    // The walk ends after the round trip in flight; wait until nothing
    // changes any more.
    let mut left = matching(&mut con, &pattern).len();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let now = matching(&mut con, &pattern).len();
        if now == left {
            break;
        }
        left = now;
    }
    assert!(left > 0, "the delete ran to the end");
    assert!(left < KEYS, "nothing was deleted");
    // Counting stops the same way.
    let mut events = client.count_matching(pattern);
    assert!(matches!(
        block_on(events.next()),
        Some(ScanProgress::Counting { .. })
    ));
    drop(events);
}

/// Stopping a delete is a request: the walk answers once the round trip
/// it has in flight is back, with every key it deleted. The flow reports
/// exactly what the server lost, however far behind its reader was.
#[test]
fn a_stopped_delete_reports_every_key_that_went() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    // Enough that the walk is far from done when it is stopped.
    const KEYS: usize = 300_000;
    seed_folder(&t, &mut con, "bulk", KEYS);
    let client = t.connect(standalone());
    let target = Target::Folder {
        prefix: t.bkey("bulk"),
        separator: b':',
    };
    let pattern = target.pattern();

    // Stopped before its first round trip: nothing goes, and it says so.
    let updates = client.delete_matching(pattern.clone()).unwrap();
    updates.stopper().stop();
    assert_eq!(
        collect(updates),
        [DeleteProgress::Stopped {
            stats: DeleteStats::default()
        }]
    );
    assert_eq!(matching(&mut con, &pattern).len(), KEYS);

    // As the dialog does it: count, confirm, read an update, wait for the
    // next frame, read again, and the user clicks Stop.
    let mut flow = BulkDelete::new(target);
    for event in collect(client.count_matching(flow.pattern().to_vec())) {
        flow.on_count(event);
    }
    assert_eq!(flow.confirm_label().unwrap(), "Delete 300,000 keys");
    assert!(flow.confirm());
    let mut updates = client.delete_matching(flow.pattern().to_vec()).unwrap();
    let stopper = updates.stopper();
    for _ in 0..2 {
        let event = block_on(updates.next()).expect("an update");
        assert!(flow.on_delete(event));
        std::thread::sleep(Duration::from_millis(66));
    }
    let Phase::Deleting(seen) = flow.phase().clone() else {
        panic!("{:?}", flow.phase());
    };
    assert_eq!(flow.stop(), Stop::Ask);
    stopper.stop();
    // Read on until the walk says how it ended.
    while let Some(event) = block_on(updates.next()) {
        flow.on_delete(event);
    }
    let Phase::Finished {
        stats,
        ending: Ending::Stopped,
    } = flow.phase().clone()
    else {
        panic!("{:?}", flow.phase());
    };
    assert!(stats.deleted >= seen.deleted, "{seen:?} {stats:?}");
    assert_eq!(stats.failed, 0);
    let left = matching(&mut con, &pattern).len();
    assert!(left > 0, "the delete ran to the end");
    assert_eq!(KEYS - left, stats.deleted as usize, "{stats:?}");
    assert_eq!(
        flow.message(),
        format!(
            "Stopped after deleting {} keys.",
            redcrab_core::format::count(stats.deleted)
        )
    );
    assert_eq!(flow.refresh(), Some(Refresh::Rescan));
    // Nothing goes after the walk said it stopped.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(matching(&mut con, &pattern).len(), left);
}

#[test]
fn delete_matching_is_refused_when_read_only() {
    let Some(t) = it!() else { return };
    let mut con = t.seed(Server::Standalone, READ_ONLY_DB);
    seed(&t, &mut con, ["a:1", "a:2"]);
    let mut admin = raw(Server::Standalone, 0);
    let db = READ_ONLY_DB.to_string();
    // Commands run by each redcrab connection on the database.
    let mut commands = || -> Vec<(String, u64)> {
        let list: String = redis::cmd("CLIENT").arg("LIST").query(&mut admin).unwrap();
        list.lines()
            .filter(|line| {
                field(line, "db") == Some(db.as_str()) && field(line, "lib-name") == Some("redcrab")
            })
            .map(|line| {
                let id = field(line, "id").unwrap().to_string();
                (id, field(line, "tot-cmds").unwrap().parse().unwrap())
            })
            .collect()
    };
    let client = t.connect(ConnectionConfig {
        db: READ_ONLY_DB,
        read_only: true,
        ..standalone()
    });
    let before = commands();
    assert_eq!(before.len(), 1, "{before:?}");

    let err = client.delete_matching(t.pattern()).unwrap_err();
    assert_eq!(err.to_string(), "this connection is read-only");
    assert_eq!(commands(), before, "something reached the server");
    assert_eq!(left(&t, &mut con).len(), 2);
    // Counting is a read, and allowed.
    assert_eq!(count(&client, t.pattern()), 2);
    // A database opened from it is read-only too.
    let other = block_on(client.with_db(READ_ONLY_DB)).unwrap();
    assert!(other.config().read_only);
    assert!(other.delete_matching(t.pattern()).is_err());
    assert_eq!(left(&t, &mut con).len(), 2);
}

/// Keys the user's ACL does not cover are skipped and reported, and the
/// others still go.
#[test]
fn keys_the_user_may_not_touch_are_skipped() {
    let Some(t) = it!() else { return };
    // `app` may only touch `app:*`: under its own prefix it may, under the
    // test's it may not. One pattern matches both.
    let app = t.under("app:");
    let mut root = t.seed(Server::Auth, 0);
    app.uses(Server::Auth, 0);
    let mut pipe = redis::pipe();
    for i in 0..600 {
        pipe.cmd("SET")
            .arg(app.key(&format!("k{i}")))
            .arg("v")
            .ignore();
    }
    for i in 0..3 {
        pipe.cmd("SET")
            .arg(t.key(&format!("k{i}")))
            .arg("v")
            .ignore();
    }
    pipe.query::<()>(&mut root).unwrap();
    let test = t.name.rsplit("::").next().unwrap();
    let both = format!("*it:{test}:{}:*", std::process::id());
    assert_eq!(matching(&mut root, both.as_bytes()).len(), 603);

    let client = t.connect(app_user());
    assert_eq!(count(&client, both.clone()), 603, "SCAN lists every key");
    let stats = delete(&client, both.clone());
    assert_eq!((stats.deleted, stats.failed), (600, 3));
    assert_eq!(
        stats.first_error.as_deref(),
        Some("NOPERM No permissions to access a key")
    );
    let rest = matching(&mut root, both.as_bytes());
    assert_eq!(rest.len(), 3);
    assert!(
        rest.iter()
            .all(|key| key.starts_with(t.prefix().as_bytes()))
    );
}

/// An ACL user of this test only, removed at the end.
struct AclUser(String);

impl Drop for AclUser {
    fn drop(&mut self) {
        let _ = redis::cmd("ACL")
            .arg("DELUSER")
            .arg(&self.0)
            .query::<()>(&mut raw(Server::Auth, 0));
    }
}

/// A user who may list keys but not UNLINK them fails the delete at the
/// first batch, instead of skipping every key of the keyspace one by one.
#[test]
fn a_user_who_may_not_unlink_stops_at_once() {
    let Some(t) = it!() else { return };
    let mut root = t.seed(Server::Auth, 0);
    let user = AclUser(format!("it-no-unlink-{}", std::process::id()));
    redis::cmd("ACL")
        .arg("SETUSER")
        .arg(&user.0)
        .arg("on")
        .arg(">pass")
        .arg("~*")
        .arg("+@all")
        .arg("-unlink")
        .query::<()>(&mut root)
        .unwrap();
    seed(&t, &mut root, ["a", "b", "c"]);
    let client = t.connect(auth(Some(&user.0), Some("pass")));

    let events = collect(client.delete_matching(t.pattern()).unwrap());
    match events.last() {
        Some(DeleteProgress::Failed { stats, error }) => {
            assert_eq!((stats.deleted, stats.failed), (0, 0), "{stats:?}");
            assert!(
                error.starts_with("NOPERM ") && error.contains("'unlink'"),
                "{error}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(left(&t, &mut root).len(), 3);
}

// The cluster, through the cluster client (a walk covers every primary)
// and node by node as plain servers. A connection to a node is taken for
// its whole cluster when INFO says so; a user who may not run INFO gets
// the node alone, whose walk sees only the node's own keys and whose
// multi-key commands fail with CROSSSLOT unless all their keys share a
// slot, so deletes there have to go key by key.

/// Every reachable node of the cluster: (port, is a primary).
fn cluster_nodes() -> Vec<(u16, bool)> {
    let nodes: String = redis::cmd("CLUSTER")
        .arg("NODES")
        .query(&mut node(7000))
        .unwrap();
    nodes
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let address = fields.nth(1)?;
            let flags = fields.next()?;
            let port = address
                .split('@')
                .next()?
                .rsplit(':')
                .next()?
                .parse()
                .ok()?;
            (!flags.contains("fail")).then(|| (port, flags.contains("master")))
        })
        .collect()
}

fn node(port: u16) -> Connection {
    redis::Client::open(format!("redis://127.0.0.1:{port}"))
        .and_then(|client| client.get_connection_with_timeout(Duration::from_secs(5)))
        .unwrap_or_else(|err| panic!("could not reach node {port} ({err}); is the cluster up?"))
}

fn node_client(port: u16) -> Client {
    block_on(Client::connect(ConnectionConfig {
        name: format!("it-node-{port}"),
        host: "127.0.0.1".into(),
        port,
        ..ConnectionConfig::default()
    }))
    .unwrap()
}

/// A user without `@dangerous` (so without INFO) on every node of the
/// cluster (ACLs are per node), removed at the end. Connected to as
/// standalone, a node stays a plain server for this user.
struct PlainNodeUser {
    name: String,
    ports: Vec<u16>,
}

impl PlainNodeUser {
    fn new(nodes: &[(u16, bool)]) -> Self {
        let user = Self {
            name: format!("it-plain-node-{}", std::process::id()),
            ports: nodes.iter().map(|n| n.0).collect(),
        };
        for &port in &user.ports {
            redis::cmd("ACL")
                .arg("SETUSER")
                .arg(&user.name)
                .arg(&["reset", "on", ">pass", "~*", "+@all", "-@dangerous"][..])
                .query::<()>(&mut node(port))
                .unwrap();
        }
        user
    }

    fn client(&self, port: u16) -> Client {
        let client = block_on(Client::connect(ConnectionConfig {
            name: format!("it-plain-node-{port}"),
            host: "127.0.0.1".into(),
            port,
            username: Some(self.name.clone()),
            password: Some("pass".into()),
            ..ConnectionConfig::default()
        }))
        .unwrap();
        assert!(!client.topology().is_cluster(), "node {port}");
        client
    }
}

impl Drop for PlainNodeUser {
    fn drop(&mut self) {
        for &port in &self.ports {
            let _ = redis::cmd("ACL")
                .arg("DELUSER")
                .arg(&self.name)
                .query::<()>(&mut node(port));
        }
    }
}

/// Writes keys on the nodes that own their slots, following MOVED.
#[derive(Default)]
struct ClusterWriter {
    nodes: HashMap<u16, Connection>,
}

impl ClusterWriter {
    fn set(&mut self, key: &[u8]) {
        let mut port = 7000;
        for _ in 0..5 {
            let con = self.nodes.entry(port).or_insert_with(|| node(port));
            match redis::cmd("SET").arg(key).arg("v").query::<()>(con) {
                Ok(()) => return,
                // ("127.0.0.1:7001", slot)
                Err(err) => match err.redirect_node().and_then(|(to, _)| to.rsplit_once(':')) {
                    Some((_, to)) => port = to.parse().unwrap(),
                    None => panic!("SET on node {port}: {err}"),
                },
            }
        }
        panic!("too many redirects for {}", String::from_utf8_lossy(key));
    }
}

/// Removes the keys matching a pattern from every primary at the end.
struct ClusterCleanup(String);

impl Drop for ClusterCleanup {
    fn drop(&mut self) {
        for (port, primary) in cluster_nodes() {
            if !primary {
                continue;
            }
            let mut con = node(port);
            for key in matching(&mut con, self.0.as_bytes()) {
                // One key per command: keys of several slots would fail.
                let _ = redis::cmd("UNLINK").arg(key).query::<()>(&mut con);
            }
        }
    }
}

/// Keys under the prefix left on the primaries.
fn cluster_left(t: &It) -> BTreeSet<String> {
    let mut left = BTreeSet::new();
    for (port, primary) in cluster_nodes() {
        if primary {
            for key in matching(&mut node(port), t.pattern().as_bytes()) {
                left.insert(String::from_utf8_lossy(&key[t.prefix().len()..]).into_owned());
            }
        }
    }
    left
}

/// Seeds a 1,500-key folder spread over every slot range (no hash tags in
/// the prefix), plus siblings that share its prefix.
fn seed_cluster_folder(t: &It) {
    let mut writer = ClusterWriter::default();
    for i in 0..1500 {
        writer.set(&t.bkey(format!("user:{i}")));
    }
    for name in ["user", "user2:1", "user2:2", "other:1"] {
        writer.set(&t.bkey(name));
    }
}

/// The cluster client counts and deletes over every primary at once.
#[test]
fn deletes_through_the_cluster_client() {
    let Some(t) = it!() else { return };
    let _cleanup = ClusterCleanup(t.pattern());
    seed_cluster_folder(&t);
    let client = t.connect(cluster());
    assert!(client.topology().is_cluster());
    let pattern = folder(&t, "user");

    assert_eq!(count(&client, pattern.clone()), 1500);
    let stats = delete(&client, pattern);
    assert_eq!(
        (stats.deleted, stats.failed, stats.first_error),
        (1500, 0, None)
    );
    assert_eq!(
        cluster_left(&t),
        ["other:1", "user", "user2:1", "user2:2"]
            .map(String::from)
            .into()
    );

    // And by pattern.
    let prefix = String::from_utf8(browser::escape_glob(t.prefix().as_bytes())).unwrap();
    assert_eq!(delete(&client, format!("{prefix}user*")).deleted, 3);
    assert_eq!(cluster_left(&t), ["other:1".to_string()].into());
}

/// Node by node, as plain servers: each primary deletes its share with one
/// UNLINK per key, and a replica deletes nothing.
#[test]
fn deletes_on_cluster_nodes() {
    let Some(t) = it!() else { return };
    let _cleanup = ClusterCleanup(t.pattern());
    seed_cluster_folder(&t);
    let nodes = cluster_nodes();
    let user = PlainNodeUser::new(&nodes);
    let node_client = |port| user.client(port);
    let primaries: Vec<u16> = nodes.iter().filter(|n| n.1).map(|n| n.0).collect();
    let replicas: Vec<u16> = nodes.iter().filter(|n| !n.1).map(|n| n.0).collect();
    assert!(primaries.len() >= 3, "{nodes:?}");
    let pattern = folder(&t, "user");

    // Each primary holds part of the folder; together they hold all of it.
    let counts: Vec<u64> = primaries
        .iter()
        .map(|&port| count(&node_client(port), pattern.clone()))
        .collect();
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
    assert_eq!(counts.iter().sum::<u64>(), 1500);

    // A replica lists its primary's keys but deletes none of them: every
    // UNLINK is redirected to the primary.
    if let Some(&replica) = replicas.first() {
        // Replication is asynchronous; wait until the replica has the keys.
        let client = node_client(replica);
        let mut seen = 0;
        for _ in 0..50 {
            seen = count(&client, pattern.clone());
            if counts.contains(&seen) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let stats = delete(&client, pattern.clone());
        assert_eq!((stats.deleted, stats.failed), (0, seen), "{stats:?}");
        assert!(
            stats
                .first_error
                .as_deref()
                .is_some_and(|e| e.starts_with("MOVED ")),
            "{stats:?}"
        );
        assert_eq!(cluster_left(&t).len(), 1504);
    }

    // On the primaries every key goes, one UNLINK per key.
    let mut deleted = 0;
    for &port in &primaries {
        let stats = delete(&node_client(port), pattern.clone());
        assert_eq!((stats.failed, stats.first_error), (0, None));
        deleted += stats.deleted;
    }
    assert_eq!(deleted, 1500);
    assert_eq!(
        cluster_left(&t),
        ["other:1", "user", "user2:1", "user2:2"]
            .map(String::from)
            .into()
    );

    // And by pattern.
    let prefix = String::from_utf8(browser::escape_glob(t.prefix().as_bytes())).unwrap();
    let mut deleted = 0;
    for &port in &primaries {
        deleted += delete(&node_client(port), format!("{prefix}user*")).deleted;
    }
    assert_eq!(deleted, 3);
    assert_eq!(cluster_left(&t), ["other:1".to_string()].into());
}

/// The key browser after a folder delete shows what a rescan shows.
#[test]
fn the_browser_follows_a_folder_delete() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    seed_folder(&t, &mut con, "user", 300);
    seed(&t, &mut con, ["user", "user2:1", "z"]);
    let client = t.connect(standalone());
    let mut browser = KeyBrowser::new(b':', None);
    let scan = |browser: &mut KeyBrowser| {
        let request = browser.begin_scan(&t.pattern());
        for event in collect(client.scan_into(request.tree, request.options)) {
            browser.on_scan_event(request.generation, event);
        }
    };
    let rows = |browser: &mut KeyBrowser| -> Vec<(u16, RowKind, String, u32)> {
        let len = browser.len();
        browser
            .row_views(0..len)
            .into_iter()
            .map(|row| (row.depth, row.kind, row.name, row.key_count))
            .collect()
    };
    scan(&mut browser);
    assert_eq!(browser.key_count(), 303);

    let prefix = t.bkey("user");
    let stats = delete(
        &client,
        browser::folder_pattern(&prefix, browser.separator()),
    );
    assert_eq!(stats.deleted, 300);
    assert_eq!(browser.remove_folder(&prefix), 300);
    let after_delete = rows(&mut browser);
    let status = browser.status_text();
    scan(&mut browser);
    assert_eq!(after_delete, rows(&mut browser));
    assert_eq!(browser.key_count(), 3);
    assert!(status.starts_with("3 matches"), "{status}");
}

/// Empties a database this file owns, without FLUSHDB.
fn empty(con: &mut Connection) {
    for key in matching(con, b"*") {
        redis::cmd("UNLINK").arg(key).query::<()>(con).unwrap();
    }
}

#[test]
fn databases_report_their_sizes() {
    let Some(t) = it!() else { return };
    let mut con = t.seed(Server::Standalone, SIZES_DB);
    empty(&mut con);
    seed(&t, &mut con, ["a", "b", "c", "d"]);
    for name in ["e", "f", "g"] {
        redis::cmd("SET")
            .arg(t.key(name))
            .arg("v")
            .arg("EX")
            .arg(1000)
            .query::<()>(&mut con)
            .unwrap();
    }
    let client = t.connect(standalone());
    let databases = block_on(client.databases()).unwrap();
    // The test server keeps the default count.
    assert_eq!(databases.len(), 16);
    assert!(
        databases
            .iter()
            .enumerate()
            .all(|(i, db)| db.index == i as u32)
    );
    let db = &databases[SIZES_DB as usize];
    assert_eq!((db.keys, db.expires), (Some(7), Some(3)));
    assert_eq!(db.label(), "db10 · 7");
    // Every database has a size, if only zero.
    assert!(databases.iter().all(|db| db.keys.is_some()));
}

#[test]
fn databases_without_permission_for_config_or_info() {
    let Some(t) = it!() else { return };
    let client = t.connect(app_user());
    let databases = block_on(client.databases()).unwrap();
    assert_eq!(databases.len(), 16);
    assert!(
        databases
            .iter()
            .all(|db| db.keys.is_none() && db.expires.is_none())
    );
    assert_eq!(databases[3].label(), "db3");
}

/// A node connected to as standalone is taken for its cluster, which has
/// database 0 only (its size is the status bar's, summed over primaries).
#[test]
fn a_cluster_node_has_one_database() {
    let Some(_t) = it!() else { return };
    let client = node_client(7000);
    assert!(client.topology().is_cluster());
    let databases = block_on(client.databases()).unwrap();
    assert_eq!(databases.len(), 1);
    assert_eq!(databases[0].index, 0);
    let err = block_on(client.with_db(1))
        .err()
        .expect("SELECT on a cluster");
    let err = format!("{err:#}");
    assert!(err.starts_with("could not switch to database 1"), "{err}");
    assert!(err.contains("cluster"), "{err}");
}

/// An ACL user of this test on one cluster node only (ACLs are per node),
/// removed at the end.
struct NodeAclUser {
    port: u16,
    name: String,
}

impl Drop for NodeAclUser {
    fn drop(&mut self) {
        let _ = redis::cmd("ACL")
            .arg("DELUSER")
            .arg(&self.name)
            .query::<()>(&mut node(self.port));
    }
}

/// A user without `@dangerous` may run neither INFO nor CONFIG, the two
/// usual ways to tell a cluster node, but may run CLUSTER INFO: the node
/// still has one database, so the switcher stays hidden.
#[test]
fn a_cluster_node_has_one_database_for_users_without_info() {
    let Some(_t) = it!() else { return };
    const PORT: u16 = 7000;
    let user = NodeAclUser {
        port: PORT,
        name: format!("it-no-info-{}", std::process::id()),
    };
    redis::cmd("ACL")
        .arg("SETUSER")
        .arg(&user.name)
        .arg(&["reset", "on", ">pass", "~*", "+@all", "-@dangerous"][..])
        .query::<()>(&mut node(PORT))
        .unwrap();
    let client = block_on(Client::connect(ConnectionConfig {
        name: "it-node-restricted".into(),
        host: "127.0.0.1".into(),
        port: PORT,
        username: Some(user.name.clone()),
        password: Some("pass".into()),
        ..ConnectionConfig::default()
    }))
    .unwrap();
    // The user really is refused both.
    let info = block_on(client.server_info()).unwrap();
    assert_eq!(info.mode, None, "{info:?}");
    let databases = block_on(client.databases()).unwrap();
    assert_eq!(databases.len(), 1, "{databases:?}");
    assert_eq!(databases[0].index, 0);
    assert_eq!(databases[0].keys, None, "INFO keyspace was refused");
}

#[test]
fn with_db_opens_another_database() {
    let Some(t) = it!() else { return };
    let mut con = t.seed(Server::Standalone, SWITCH_DB);
    seed(&t, &mut con, ["there"]);
    let mut con0 = t.seed0();
    seed(&t, &mut con0, ["here"]);
    let client = t.connect(standalone());
    let other = block_on(client.with_db(SWITCH_DB)).unwrap();
    assert_eq!(other.config().db, SWITCH_DB);
    assert_eq!(client.config().db, 0, "the client stays where it was");
    // A front end tells the two apart, to keep answers on the database
    // they came from; clones are the same connection, a second connect
    // with the same settings is not, nor is going back to the same db.
    assert!(!client.same_connection(&other) && !other.same_connection(&client));
    assert!(client.same_connection(&client.clone()));
    assert!(other.clone().same_connection(&other));
    let again = t.connect(standalone());
    assert_eq!(again.config(), client.config());
    assert!(!again.same_connection(&client));
    let back = block_on(other.with_db(0)).unwrap();
    assert!(!back.same_connection(&client));
    let names = |client: &Client| -> BTreeSet<Vec<u8>> {
        key_set(
            &scan(
                client,
                redcrab_core::ScanOptions {
                    pattern: t.pattern(),
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .into_iter()
        .collect()
    };
    assert_eq!(names(&other), [t.bkey("there").to_vec()].into());
    assert_eq!(names(&client), [t.bkey("here").to_vec()].into());
    // Settings other than the database come along.
    assert_eq!(
        ConnectionConfig {
            db: 0,
            ..other.config().clone()
        },
        *client.config()
    );
    // Deletes act on the database the client is on.
    assert_eq!(delete(&other, t.pattern()).deleted, 1);
    assert_eq!(left(&t, &mut con0), ["here".to_string()].into());

    let err = block_on(client.with_db(16)).err().expect("db16 of 16");
    let err = format!("{err:#}");
    assert!(err.starts_with("could not switch to database 16"), "{err}");
    assert!(err.contains("out of range"), "{err}");
}
