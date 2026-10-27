//! SCAN and `scan_into`: patterns, the TYPE filter, the key limit, keys of
//! any bytes, duplicates, and the tree a scan builds.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400), database 0, and only keys under each test's prefix.
//! Patterns are anchored at the prefix, so other suites' keys never match.

#[macro_use]
mod common;

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use common::*;
use redcrab_core::{Key, KeyTree, RowKind, ScanEvent, ScanOptions, browser};

/// Sets every name under the prefix to its own name.
fn seed_strings(t: &It, names: &[&[u8]]) {
    let mut pipe = redis::pipe();
    for name in names {
        pipe.cmd("SET").arg(&*t.bkey(name)).arg(*name).ignore();
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
}

fn options(pattern: String) -> ScanOptions {
    ScanOptions {
        pattern,
        ..ScanOptions::default()
    }
}

fn keys(t: &It, names: &[&[u8]]) -> HashSet<Vec<u8>> {
    names.iter().map(|name| t.bkey(name).to_vec()).collect()
}

#[test]
fn scan_matches_a_pattern() {
    let Some(t) = it!() else { return };
    let names: [&[u8]; 5] = [b"user:1", b"user:2", b"user:10", b"users", b"order:1"];
    seed_strings(&t, &names);
    let client = t.connect(standalone());
    let prefix = String::from_utf8(browser::escape_glob(t.prefix().as_bytes())).unwrap();

    for (pattern, expected) in [
        (t.pattern_for(b"user:"), &names[..3]),
        (format!("{prefix}user:?"), &names[..2]),
        (format!("{prefix}user*"), &names[..4]),
        (format!("{prefix}[uo]*"), &names[..]),
        (t.pattern(), &names[..]),
        (format!("{prefix}nothing*"), &[][..]),
    ] {
        // One key per round trip, and all of them in one.
        for count in [1, 2000] {
            let found = scan(
                &client,
                ScanOptions {
                    count,
                    ..options(pattern.clone())
                },
            )
            .unwrap();
            assert_eq!(
                key_set(&found),
                keys(&t, expected),
                "{pattern} COUNT {count}"
            );
        }
        let (events, tree) = scan_into(&client, options(pattern.clone()));
        assert_eq!(done(&events), (expected.len(), false), "{pattern}");
        assert_eq!(tree.lock().unwrap().len(), expected.len());
    }
}

#[test]
fn scan_filters_by_type() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let commands: [(&str, &[&str]); 10] = [
        ("string", &["SET", "{}", "v"]),
        ("hash", &["HSET", "{}", "f", "v"]),
        ("list", &["RPUSH", "{}", "a"]),
        ("set", &["SADD", "{}", "a"]),
        ("zset", &["ZADD", "{}", "1", "a"]),
        ("stream", &["XADD", "{}", "*", "f", "v"]),
        ("ReJSON-RL", &["JSON.SET", "{}", "$", r#"{"a":1}"#]),
        ("MBbloom--", &["BF.ADD", "{}", "a"]),
        ("TSDB-TYPE", &["TS.ADD", "{}", "1", "1"]),
        ("vectorset", &["VADD", "{}", "VALUES", "2", "1", "2", "a"]),
    ];
    for (ty, command) in commands {
        let mut cmd = redis::cmd(command[0]);
        for arg in &command[1..] {
            cmd.arg(if *arg == "{}" {
                t.key(ty)
            } else {
                arg.to_string()
            });
        }
        cmd.query::<()>(&mut con).unwrap();
    }
    let client = t.connect(standalone());
    for (ty, _) in commands {
        let typed = ScanOptions {
            type_filter: Some(ty.into()),
            ..options(t.pattern())
        };
        let found = scan(&client, typed.clone()).unwrap();
        assert_eq!(key_set(&found), keys(&t, &[ty.as_bytes()]), "TYPE {ty}");
        let (events, tree) = scan_into(&client, typed);
        assert_eq!(done(&events), (1, false), "TYPE {ty}");
        assert!(tree.lock().unwrap().contains(&t.bkey(ty)));
    }
    // A type no key has, or no type at all, finds nothing.
    let none = ScanOptions {
        type_filter: Some("no-such-type".into()),
        ..options(t.pattern())
    };
    assert!(scan(&client, none).unwrap().is_empty());
}

/// Names that are not plain text: bytes that are not UTF-8, control
/// characters, combining characters, emoji, glob characters, separators at
/// the ends and doubled, and the empty name (the prefix itself is a key).
const ODD_NAMES: [&[u8]; 20] = [
    b"bin:\x00\x01\xff\xfe",
    b"bin:\x80",
    b"bin:\xc3",
    "uni:ключ".as_bytes(),
    "uni:🦀".as_bytes(),
    "uni:e\u{301}".as_bytes(),
    "uni:日本語".as_bytes(),
    b"glob:*",
    b"glob:?",
    b"glob:[ab]",
    b"glob:\\",
    b"glob:a]b",
    b"ws:a b",
    b"ws:tab\there",
    b"ws:new\nline",
    b"ws:cr\r",
    b"sep::double",
    b"sep:trailing:",
    b":lead",
    b"",
];

#[test]
fn keys_of_any_bytes_come_back_exactly() {
    let Some(t) = it!() else { return };
    seed_strings(&t, &ODD_NAMES);
    let client = t.connect(standalone());
    let expected = keys(&t, &ODD_NAMES);
    assert_eq!(expected.len(), ODD_NAMES.len());

    let found = scan(&client, options(t.pattern())).unwrap();
    assert_eq!(key_set(&found), expected);

    let (events, tree) = scan_into(&client, options(t.pattern()));
    assert_eq!(done(&events), (ODD_NAMES.len(), false));
    let tree = tree.lock().unwrap();
    for name in ODD_NAMES {
        let key = t.bkey(name);
        assert!(tree.contains(&key), "{:?}", String::from_utf8_lossy(&key));
        let node = tree.find(&key).unwrap();
        assert_eq!(*tree.key(node), *key);
    }
}

#[test]
fn escaped_glob_characters_match_literally() {
    let Some(t) = it!() else { return };
    let names: [&[u8]; 9] = [
        b"lit*", b"litX", b"lit?", b"lit[x]", b"litx", b"lit\\", b"lit\\x", b"lit]", b"lit^",
    ];
    seed_strings(&t, &names);
    let client = t.connect(standalone());
    for name in names {
        let key = t.bkey(name);
        // The whole key, escaped, with nothing after it.
        let exact = String::from_utf8(browser::escape_glob(&key)).unwrap();
        let found = scan(&client, options(exact.clone())).unwrap();
        assert_eq!(key_set(&found), key_set([&key]), "{exact}");
    }
    // Unescaped, the same characters are globs.
    let prefix = String::from_utf8(browser::escape_glob(t.prefix().as_bytes())).unwrap();
    let found = scan(&client, options(format!("{prefix}lit[x]"))).unwrap();
    assert_eq!(key_set(&found), keys(&t, &[b"litx"]));
    let found = scan(&client, options(format!("{prefix}lit?"))).unwrap();
    assert_eq!(found.len(), 7, "lit*, litX, lit?, litx, lit\\, lit], lit^");
}

#[test]
fn scan_into_builds_the_folder_tree() {
    let Some(t) = it!() else { return };
    let names: [&[u8]; 7] = [b"a:b:c", b"a:b", b"a:x", b"b", b"c:", b"c::d", b"d:e:f:g"];
    seed_strings(&t, &names);
    let client = t.connect(standalone());
    let (events, tree) = scan_into(&client, options(t.pattern()));
    assert_eq!(done(&events), (names.len(), false));

    let mut tree = tree.lock().unwrap();
    assert_eq!(tree.len(), names.len());
    // The prefix ends in a separator, so its last segment is a folder.
    let base = t.prefix().trim_end_matches(':').as_bytes();
    let base = tree.find(base).expect("the prefix's folder");
    assert_eq!(tree.key_count(base), names.len() as u32);
    let folder =
        |tree: &KeyTree, name: &str| tree.key_count(tree.find(t.key(name).as_bytes()).unwrap());
    assert_eq!(folder(&tree, "a"), 3);
    assert_eq!(folder(&tree, "a:b"), 2, "a key and the folder of a:b:c");
    assert_eq!(folder(&tree, "c"), 2);
    assert_eq!(folder(&tree, "d:e:f"), 1);

    // Fully expanded, the rows list every key exactly once, and collapsed
    // only the top folder.
    tree.expand_all();
    let rows = tree.rows();
    let listed: Vec<Key> = rows
        .iter()
        .filter(|row| row.kind == RowKind::Key)
        .map(|row| tree.key(row.node))
        .collect();
    assert_eq!(listed.len(), names.len());
    assert_eq!(key_set(&listed), keys(&t, &names));
    tree.collapse_all();
    let rows = tree.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(tree.name(rows[0].node), b"it");
}

#[test]
fn scan_into_stops_at_the_key_limit() {
    let Some(t) = it!() else { return };
    let mut pipe = redis::pipe();
    for i in 0..1000 {
        pipe.set(t.key(&format!("k:{i}")), i).ignore();
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
    let client = t.connect(standalone());
    // A small COUNT makes the limit fall in the middle of a batch.
    let options = |limit| ScanOptions {
        count: 70,
        limit,
        ..options(t.pattern())
    };
    for (limit, keys, truncated) in [
        (Some(300), 300, true),
        (Some(0), 0, true),
        (Some(1), 1, true),
        (Some(999), 999, true),
        (Some(1000), 1000, false),
        (Some(1001), 1000, false),
        (None, 1000, false),
    ] {
        let (events, tree) = scan_into(&client, options(limit));
        assert_eq!(done(&events), (keys, truncated), "limit {limit:?}");
        assert_eq!(tree.lock().unwrap().len(), keys, "limit {limit:?}");
    }
}

#[test]
fn scan_sends_at_most_the_limit() {
    let Some(t) = it!() else { return };
    let mut pipe = redis::pipe();
    for i in 0..400 {
        pipe.set(t.key(&i.to_string()), i).ignore();
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
    let client = t.connect(standalone());
    let sent = |limit| {
        scan(
            &client,
            ScanOptions {
                count: 64,
                limit,
                ..options(t.pattern())
            },
        )
        .unwrap()
        .len()
    };
    assert_eq!(sent(Some(150)), 150);
    assert_eq!(sent(Some(0)), 0);
    // SCAN may repeat a key, so without a limit it sends at least every key.
    assert!(sent(None) >= 400);
}

#[test]
fn keys_already_in_the_tree_count_once() {
    let Some(t) = it!() else { return };
    let mut pipe = redis::pipe();
    for i in 0..200 {
        pipe.set(t.key(&format!("{i:03}")), i).ignore();
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
    let client = t.connect(standalone());
    // A tree that already holds half of the keys, as a rescan into the same
    // tree, or SCAN returning a key twice, would leave it.
    let prefilled = || {
        let mut tree = KeyTree::new(Some(b':'));
        for i in (0..200).step_by(2) {
            tree.insert(t.key(&format!("{i:03}")).as_bytes());
        }
        Arc::new(Mutex::new(tree))
    };
    for (limit, keys, truncated) in [
        (Some(200), 200, false),
        (Some(150), 150, true),
        (Some(100), 100, true),
        (None, 200, false),
    ] {
        let tree = prefilled();
        let events = collect(client.scan_into(
            tree.clone(),
            ScanOptions {
                count: 25,
                limit,
                ..options(t.pattern())
            },
        ));
        assert_eq!(done(&events), (keys, truncated), "limit {limit:?}");
        assert_eq!(tree.lock().unwrap().len(), keys);
    }
}

#[test]
fn every_key_is_found_while_the_keyspace_changes() {
    let Some(t) = it!() else { return };
    let stable: Vec<String> = (0..2000).map(|i| t.key(&format!("stable:{i}"))).collect();
    let mut pipe = redis::pipe();
    for key in &stable {
        pipe.set(key, 1).ignore();
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
    let client = t.connect(standalone());

    // Keys come and go while the scan runs, so the server's table grows and
    // rehashes under the cursor, which is when SCAN repeats keys.
    let (events, tree) = std::thread::scope(|scope| {
        let t = &t;
        let mut con = t.seed0();
        let churn = scope.spawn(move || {
            for round in 0..20 {
                let mut pipe = redis::pipe();
                for i in 0..1000 {
                    pipe.set(t.key(&format!("churn:{round}:{i}")), 1).ignore();
                }
                pipe.query::<()>(&mut con).unwrap();
            }
            let mut pipe = redis::pipe();
            for round in 0..20 {
                for i in 0..1000 {
                    pipe.del(t.key(&format!("churn:{round}:{i}"))).ignore();
                }
            }
            pipe.query::<()>(&mut con).unwrap();
        });
        let scanned = scan_into(
            &client,
            ScanOptions {
                count: 10,
                ..options(t.pattern())
            },
        );
        churn.join().unwrap();
        scanned
    });
    let (keys, truncated) = done(&events);
    assert!(!truncated);
    let tree = tree.lock().unwrap();
    assert_eq!(tree.len(), keys, "Done counts distinct keys");
    // SCAN guarantees every key that existed for the whole scan.
    for key in &stable {
        assert!(tree.contains(key.as_bytes()), "{key} missing");
    }
}

#[test]
fn progress_counts_up_to_done() {
    let Some(t) = it!() else { return };
    let mut pipe = redis::pipe();
    for i in 0..3000 {
        pipe.set(t.key(&i.to_string()), i).ignore();
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
    let client = t.connect(standalone());
    let (events, _) = scan_into(
        &client,
        ScanOptions {
            count: 100,
            ..options(t.pattern())
        },
    );
    let mut last = 0;
    for event in &events[..events.len() - 1] {
        let ScanEvent::Progress { keys } = event else {
            panic!("{event:?} before the end");
        };
        assert!(*keys >= last && *keys <= 3000, "{events:?}");
        last = *keys;
    }
    assert_eq!(done(&events), (3000, false));
}

#[test]
fn an_empty_match_finishes_with_no_keys() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let (events, tree) = scan_into(&client, options(t.pattern()));
    assert_eq!(done(&events), (0, false));
    assert!(tree.lock().unwrap().is_empty());
    assert!(scan(&client, options(t.pattern())).unwrap().is_empty());
}

#[test]
fn a_failed_scan_is_reported() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    // The server refuses COUNT 0.
    let refused = ScanOptions {
        count: 0,
        ..options(t.pattern())
    };
    let (events, _) = scan_into(&client, refused.clone());
    match &events[..] {
        [ScanEvent::Failed(err)] => assert!(err.contains("syntax error"), "{err}"),
        other => panic!("{other:?}"),
    }
    let err = scan(&client, refused).unwrap_err();
    assert!(format!("{err:#}").contains("syntax error"), "{err:#}");
    // The connection is fine afterwards.
    block_on(client.ping()).unwrap();
}
