//! Redis Cluster (7000-7005, three primaries with a replica each): scans
//! over every primary, per-node batches, renames within and across slots,
//! the console, and read-only refusals. Keys live under each test's prefix
//! and are spread over many slots, so over every primary.

#[macro_use]
mod common;

use std::collections::{HashMap, HashSet};

use common::*;
use redcrab_core::{
    ConnectionConfig, Cursor, Key, KeyType, RawValue, ScanOptions, Topology, ValueData, key_slot,
    viewer,
};

/// Seeds `n` string keys `k0..` under the prefix and returns them.
fn seed_strings(t: &It, n: usize) -> Vec<Key> {
    let mut con = t.seed_cluster();
    (0..n)
        .map(|i| {
            let key = t.bkey(format!("k{i}"));
            redis::cmd("SET")
                .arg(&*key)
                .arg(i)
                .query::<()>(&mut con)
                .unwrap();
            key
        })
        .collect()
}

/// Which primary port serves each key, from the server's own CLUSTER
/// KEYSLOT and CLUSTER SLOTS.
fn owners(keys: &[Key]) -> HashMap<Vec<u8>, u16> {
    let mut con = node(7000);
    let slots: Vec<(u16, u16, Vec<redis::Value>)> = redis::cmd("CLUSTER")
        .arg("SLOTS")
        .query::<Vec<redis::Value>>(&mut con)
        .unwrap()
        .into_iter()
        .map(|range| match range {
            redis::Value::Array(items) => {
                let int = |v: &redis::Value| match v {
                    redis::Value::Int(n) => *n as u16,
                    other => panic!("{other:?}"),
                };
                let primary = match &items[2] {
                    redis::Value::Array(primary) => primary.clone(),
                    other => panic!("{other:?}"),
                };
                (int(&items[0]), int(&items[1]), primary)
            }
            other => panic!("{other:?}"),
        })
        .collect();
    keys.iter()
        .map(|key| {
            let slot: u16 = redis::cmd("CLUSTER")
                .arg("KEYSLOT")
                .arg(&**key)
                .query(&mut con)
                .unwrap();
            let (_, _, primary) = slots
                .iter()
                .find(|(start, end, _)| (*start..=*end).contains(&slot))
                .expect("every slot is served");
            let port = match &primary[1] {
                redis::Value::Int(port) => *port as u16,
                other => panic!("{other:?}"),
            };
            (key.to_vec(), port)
        })
        .collect()
}

#[test]
fn connects_and_reports_the_layout() {
    let Some(t) = it!() else { return };
    let client = t.connect(cluster());
    assert!(client.topology().is_cluster());
    assert!(!client.topology().supports_databases());
    assert_eq!(client.topology_summary(), ["cluster · 3 primaries"]);
    assert_eq!(client.ssh_tunnel(), None);
    block_on(client.ping()).unwrap();

    let keys = seed_strings(&t, 30);
    let info = block_on(client.server_info()).unwrap();
    assert_eq!(info.mode.as_deref(), Some("cluster"));
    assert_eq!((info.nodes, info.primaries), (Some(3), Some(3)));
    assert_eq!(info.partial(), None);
    assert!(info.version.is_some());
    assert!(info.used_memory_human.is_some());
    assert!(info.connected_clients.unwrap() >= 3);
    assert!(info.db_size.unwrap() >= keys.len() as u64);
    assert!(
        info.summary()[0].ends_with("· cluster"),
        "{:?}",
        info.summary()
    );
}

#[test]
fn a_seed_that_is_down_is_skipped() {
    let Some(t) = it!() else { return };
    let config = ConnectionConfig {
        port: 1,
        topology: Topology::Cluster {
            seeds: vec!["127.0.0.1:7002".into()],
        },
        ..cluster()
    };
    let client = t.connect(config);
    assert_eq!(client.topology_summary(), ["cluster · 3 primaries"]);
}

#[test]
fn a_node_set_up_as_standalone_connects_to_its_cluster() {
    let Some(t) = it!() else { return };
    let keys = seed_strings(&t, 60);
    // As a URL start or a connection saved as Standalone reaches it.
    let node = ConnectionConfig {
        topology: Topology::Standalone,
        ..cluster()
    };
    let client = t.connect(node.clone());
    assert!(client.topology().is_cluster());
    assert!(!client.topology().supports_databases());
    assert_eq!(client.topology_summary(), ["cluster · 3 primaries"]);
    // A cluster has no database to name.
    assert!(
        client.location().ends_with(" · cluster of 3 primaries"),
        "{}",
        client.location()
    );
    let found = scan(
        &client,
        ScanOptions {
            pattern: t.pattern(),
            ..ScanOptions::default()
        },
    )
    .unwrap();
    assert_eq!(key_set(&found), key_set(&keys), "every primary's keys");
    let url = ConnectionConfig::parse_url("redis://127.0.0.1:7001").unwrap();
    assert!(t.connect(url).topology().is_cluster());

    // A cluster has only database 0.
    let err = connect_error(ConnectionConfig { db: 2, ..node });
    assert!(
        err.contains("the server is a node of a cluster, which has only database 0"),
        "{err}"
    );
    // A server that is no cluster node stays what it is.
    let single = t.connect(standalone());
    assert_eq!(single.topology(), &Topology::Standalone);
    assert!(single.topology_summary().is_empty());
}

#[test]
fn connecting_explains_what_is_wrong() {
    let Some(_t) = it!() else { return };
    // A standalone server is no cluster.
    let err = connect_error(ConnectionConfig {
        port: 6400,
        topology: Topology::Cluster { seeds: vec![] },
        ..cluster()
    });
    assert!(err.contains("the server is not part of a cluster"), "{err}");
    // Settings that cannot work never reach the network.
    let err = connect_error(ConnectionConfig { db: 2, ..cluster() });
    assert_eq!(err, "a cluster has only database 0");
    let err = connect_error(ConnectionConfig {
        port: 1,
        topology: Topology::Cluster { seeds: vec![] },
        ..cluster()
    });
    assert!(
        err.starts_with("could not connect to cluster 127.0.0.1:1"),
        "{err}"
    );
}

#[test]
fn scan_sees_the_keys_of_every_primary() {
    let Some(t) = it!() else { return };
    let keys = seed_strings(&t, 300);
    let by_node: HashSet<u16> = owners(&keys).into_values().collect();
    assert_eq!(
        by_node.len(),
        3,
        "the keys should be spread over all primaries"
    );

    let client = t.connect(cluster());
    let options = || ScanOptions {
        pattern: t.pattern(),
        count: 50,
        ..ScanOptions::default()
    };
    let found = scan(&client, options()).unwrap();
    assert_eq!(found.len(), keys.len(), "no key twice, none missing");
    assert_eq!(key_set(&found), key_set(&keys));

    let (events, tree) = scan_into(&client, options());
    assert_eq!(done(&events), (300, false));
    let tree = tree.lock().unwrap();
    assert!(keys.iter().all(|key| tree.contains(key)));

    // TYPE filters work per node too.
    let none = scan(
        &client,
        ScanOptions {
            type_filter: Some("hash".into()),
            ..options()
        },
    )
    .unwrap();
    assert!(none.is_empty());
}

#[test]
fn the_limit_counts_keys_over_all_nodes() {
    let Some(t) = it!() else { return };
    let keys = seed_strings(&t, 200);
    let client = t.connect(cluster());
    let options = ScanOptions {
        pattern: t.pattern(),
        count: 30,
        limit: Some(120),
        ..ScanOptions::default()
    };
    let found = scan(&client, options.clone()).unwrap();
    assert_eq!(found.len(), 120);
    assert!(key_set(&found).is_subset(&key_set(&keys)));
    let (events, tree) = scan_into(&client, options);
    assert_eq!(done(&events), (120, true));
    assert_eq!(tree.lock().unwrap().len(), 120);
}

#[test]
fn key_meta_batches_keys_by_node() {
    let Some(t) = it!() else { return };
    let mut con = t.seed_cluster();
    let mut expected: Vec<(Key, KeyType, bool)> = Vec::new();
    let mut add = |name: &[u8], command: &str, args: &[&str], ty: KeyType, ttl: bool| {
        let key = t.bkey(name);
        let mut cmd = redis::cmd(command);
        cmd.arg(&*key);
        for arg in args {
            cmd.arg(*arg);
        }
        cmd.query::<redis::Value>(&mut con).unwrap();
        if ttl {
            redis::cmd("EXPIRE")
                .arg(&*key)
                .arg(1000)
                .query::<()>(&mut con)
                .unwrap();
        }
        expected.push((key, ty, ttl));
    };
    for i in 0..20 {
        add(
            format!("s{i}").as_bytes(),
            "SET",
            &["v"],
            KeyType::String,
            i % 2 == 0,
        );
    }
    add(b"hash", "HSET", &["f", "v"], KeyType::Hash, false);
    add(b"list", "RPUSH", &["a", "b"], KeyType::List, true);
    add(b"set", "SADD", &["a"], KeyType::Set, false);
    add(b"zset", "ZADD", &["1", "a"], KeyType::ZSet, false);
    add(b"stream", "XADD", &["*", "f", "v"], KeyType::Stream, false);
    add(b"bin\xff\x00\x01", "SET", &["v"], KeyType::String, false);
    add(
        "unicode ключ 🔑".as_bytes(),
        "SET",
        &["v"],
        KeyType::String,
        true,
    );
    add(b"{tag}a", "SET", &["v"], KeyType::String, false);
    add(b"{tag}b", "SET", &["v"], KeyType::String, false);
    let missing = t.bkey("missing");

    let client = t.connect(cluster());
    let mut keys: Vec<Key> = expected.iter().map(|(key, ..)| key.clone()).collect();
    keys.push(missing.clone());
    let metas = block_on(client.key_meta(keys.clone())).unwrap();
    assert_eq!(metas.len(), keys.len());
    for ((key, ty, ttl), meta) in expected.iter().zip(&metas) {
        assert_eq!(&meta.ty, ty, "{key:?}");
        assert_eq!(meta.ttl_ms > 0, *ttl, "{key:?}: {}", meta.ttl_ms);
    }
    let last = metas.last().unwrap();
    assert_eq!((last.ty.clone(), last.ttl_ms), (KeyType::None, -2));

    let permitted = block_on(client.key_meta_permitted(keys)).unwrap();
    assert!(permitted.iter().all(Option::is_some));
    assert!(block_on(client.key_meta(Vec::new())).unwrap().is_empty());
}

#[test]
fn values_load_from_their_node() {
    let Some(t) = it!() else { return };
    let mut con = t.seed_cluster();
    let hash = t.bkey("hash");
    let list = t.bkey("list");
    let zset = t.bkey("zset");
    let stream = t.bkey("stream");
    let string = t.bkey("string");
    for i in 0..250 {
        redis::cmd("HSET")
            .arg(&*hash)
            .arg(format!("f{i}"))
            .arg(i)
            .query::<()>(&mut con)
            .unwrap();
        redis::cmd("RPUSH")
            .arg(&*list)
            .arg(i)
            .query::<()>(&mut con)
            .unwrap();
        redis::cmd("ZADD")
            .arg(&*zset)
            .arg(i)
            .arg(format!("m{i}"))
            .query::<()>(&mut con)
            .unwrap();
        redis::cmd("XADD")
            .arg(&*stream)
            .arg("*")
            .arg("i")
            .arg(i)
            .query::<()>(&mut con)
            .unwrap();
    }
    redis::cmd("SET")
        .arg(&*string)
        .arg("hello")
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(cluster());

    let loaded = block_on(viewer::load(&client, string)).unwrap();
    assert!(matches!(&loaded.data, ValueData::String(s) if s == b"hello"));
    for (key, ty) in [
        (hash, KeyType::Hash),
        (list, KeyType::List),
        (zset, KeyType::ZSet),
        (stream, KeyType::Stream),
    ] {
        let mut page = block_on(client.load(key.clone(), ty.clone(), Cursor::Start, 100)).unwrap();
        assert_eq!(page.total, 250, "{ty:?}");
        let mut seen = page.data.len();
        while let Some(next) = page.next.clone() {
            page = block_on(client.load(key.clone(), ty.clone(), next, 100)).unwrap();
            seen += page.data.len();
        }
        // SCAN-based paging may repeat an element; the others never do.
        if ty == KeyType::Hash {
            assert!(seen >= 250);
        } else {
            assert_eq!(seen, 250, "{ty:?}");
        }
        assert!(block_on(client.memory_usage(key)).unwrap().is_some());
    }
}

#[test]
fn deletes_keys_on_every_node() {
    let Some(t) = it!() else { return };
    let mut keys = seed_strings(&t, 120);
    let mut con = t.seed_cluster();
    let binary = t.bkey(b"\xff\xfe del");
    redis::cmd("SET")
        .arg(&*binary)
        .arg(1)
        .query::<()>(&mut con)
        .unwrap();
    keys.push(binary);
    let client = t.connect(cluster());
    let mut with_missing = keys.clone();
    with_missing.push(t.bkey("never-there"));
    assert_eq!(
        block_on(client.delete(with_missing)).unwrap(),
        keys.len() as u64
    );
    for key in &keys {
        let exists: bool = redis::cmd("EXISTS").arg(&**key).query(&mut con).unwrap();
        assert!(!exists, "{key:?}");
    }
    assert_eq!(block_on(client.delete(keys)).unwrap(), 0);
    assert_eq!(block_on(client.delete(Vec::new())).unwrap(), 0);
}

#[test]
fn renames_within_a_slot_and_explains_across_slots() {
    let Some(t) = it!() else { return };
    let mut con = t.seed_cluster();
    let (from, to, taken) = (t.bkey("{user}:a"), t.bkey("{user}:b"), t.bkey("{user}:c"));
    for key in [&from, &taken] {
        redis::cmd("SET")
            .arg(&**key)
            .arg("v")
            .query::<()>(&mut con)
            .unwrap();
    }
    let client = t.connect(cluster());
    block_on(client.rename(from.clone(), to.clone())).unwrap();
    let moved: Option<String> = redis::cmd("GET").arg(&*to).query(&mut con).unwrap();
    assert_eq!(moved.as_deref(), Some("v"));

    let err = block_on(client.rename(to.clone(), taken.clone())).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    let other = t.bkey("elsewhere");
    assert_ne!(key_slot(&to), key_slot(&other));
    let err = block_on(client.rename(to.clone(), other.clone()))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("in a cluster a key can only be renamed to a name in the same hash slot"),
        "{err}"
    );
    assert!(err.contains("same hash tag"), "{err}");
    let still: bool = redis::cmd("EXISTS").arg(&*to).query(&mut con).unwrap();
    assert!(still, "a refused rename leaves the key alone");
}

#[test]
fn single_key_writes_route_by_slot() {
    let Some(t) = it!() else { return };
    let keys = seed_strings(&t, 12);
    let client = t.connect(cluster());
    let mut con = t.seed_cluster();
    for key in &keys {
        assert!(block_on(client.set_ttl(key.clone(), Some(500))).unwrap());
        block_on(client.set_string(key.clone(), b"new \xff value".to_vec())).unwrap();
        let ttl: i64 = redis::cmd("TTL").arg(&**key).query(&mut con).unwrap();
        assert!(ttl > 0, "SET keeps the TTL");
        let value: Vec<u8> = redis::cmd("GET").arg(&**key).query(&mut con).unwrap();
        assert_eq!(value, b"new \xff value");
        assert!(block_on(client.set_ttl(key.clone(), None)).unwrap());
    }
}

#[test]
fn slots_match_the_server() {
    let Some(_t) = it!() else { return };
    let mut con = node(7001);
    let mut keys: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"{".to_vec(),
        b"{}".to_vec(),
        b"a{b}c".to_vec(),
        b"{a}{b}".to_vec(),
        b"\xff\x00\x80".to_vec(),
        "ключ🔑".as_bytes().to_vec(),
    ];
    keys.extend((0..300u32).map(|i| format!("key:{}:{}", i * 7919, i % 13).into_bytes()));
    for key in keys {
        let slot: u16 = redis::cmd("CLUSTER")
            .arg("KEYSLOT")
            .arg(&key)
            .query(&mut con)
            .unwrap();
        assert_eq!(key_slot(&key), slot, "{key:?}");
    }
}

#[test]
fn the_console_routes_by_key_and_refuses_what_cannot_work() {
    let Some(t) = it!() else { return };
    let client = t.connect(cluster());
    let console = client.console();
    let keys: Vec<String> = (0..20).map(|i| t.key(&format!("c{i}"))).collect();
    for key in &keys {
        assert_eq!(ok(&console, &format!("SET {key} v-{key}")), RawValue::Okay);
    }
    for key in &keys {
        assert_eq!(
            ok(&console, &format!("GET {key}")),
            bulk(format!("v-{key}"))
        );
    }
    for line in ["SELECT 1", "MULTI", "WATCH k", "EXEC"] {
        let err = run(&console, line).unwrap_err().to_string();
        assert!(
            err.contains("not available in the console of a cluster"),
            "{line}: {err}"
        );
    }
    assert_eq!(console.db(), 0);
    // Keyless commands go to every primary and are combined.
    match ok(&console, "DBSIZE") {
        RawValue::Int(n) => assert!(n >= keys.len() as i64),
        other => panic!("DBSIZE: {other:?}"),
    }
    // Server errors read like redis-cli's.
    let err = run(&console, &format!("INCR {}", keys[0]))
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("ERR value is not an integer"), "{err}");
    // The console's own connection is separate from the app's.
    drop(console);
    block_on(client.ping()).unwrap();
}

/// `name:value` from an INFO section.
fn info_field(info: &RawValue, name: &str) -> String {
    let text = match info {
        RawValue::BulkString(text) => String::from_utf8_lossy(text).into_owned(),
        RawValue::VerbatimString { text, .. } => text.clone(),
        other => panic!("INFO: {other:?}"),
    };
    text.lines()
        .find_map(|line| line.trim().strip_prefix(name)?.strip_prefix(':'))
        .unwrap_or_else(|| panic!("no {name} in {text}"))
        .to_string()
}

#[test]
fn the_console_sends_commands_without_a_key_to_one_primary() {
    let Some(t) = it!() else { return };
    let client = t.connect(cluster());
    let console = client.console();
    // ROLE, CLIENT ID and a script without keys describe the node they run
    // on: always the same one, and a primary, never a replica.
    let id = ok(&console, "CLIENT ID");
    let port = info_field(
        &ok(&console, "EVAL return(redis.call('INFO','server')) 0"),
        "tcp_port",
    );
    for _ in 0..30 {
        let role = ok(&console, "ROLE");
        assert_eq!(
            role.as_sequence().map(|r| r[0].clone()),
            Some(bulk("master"))
        );
        assert_eq!(ok(&console, "CLIENT ID"), id);
        let script = ok(&console, "EVAL return(redis.call('INFO','server')) 0");
        assert_eq!(info_field(&script, "tcp_port"), port);
    }
    // The seed node, as with redis-cli -c, when it is a primary.
    let port: u16 = port.parse().unwrap();
    if cluster_primaries().contains(&7000) {
        assert_eq!(port, 7000);
    } else {
        assert!(cluster_primaries().contains(&port));
    }
    assert_eq!(
        ok(&console, "CONFIG GET port"),
        RawValue::Array(vec![bulk("port"), bulk(port.to_string())])
    );

    // SCAN would walk one node's keys with a cursor of that node's.
    let err = run(&console, &format!("SCAN 0 MATCH {}", t.pattern()))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("SCAN is not available in the console of a cluster"),
        "{err}"
    );
    // KEYS asks every primary and combines their answers.
    let keys: Vec<String> = (0..30).map(|i| t.key(&format!("k{i}"))).collect();
    for key in &keys {
        ok(&console, &format!("SET {key} 1"));
    }
    match ok(&console, &format!("KEYS {}", t.pattern())) {
        RawValue::Array(found) => assert_eq!(found.len(), keys.len()),
        other => panic!("KEYS: {other:?}"),
    }
}

#[test]
fn read_only_clusters_refuse_every_change() {
    let Some(t) = it!() else { return };
    let keys = seed_strings(&t, 5);
    let client = t.connect(ConnectionConfig {
        read_only: true,
        ..cluster()
    });
    assert!(is_read_only(
        &block_on(client.delete(keys.clone())).unwrap_err()
    ));
    assert!(is_read_only(
        &block_on(client.rename(keys[0].clone(), t.bkey("x"))).unwrap_err()
    ));
    assert!(is_read_only(
        &block_on(client.set_ttl(keys[0].clone(), Some(5))).unwrap_err()
    ));
    assert!(is_read_only(
        &block_on(client.set_string(keys[0].clone(), b"x".to_vec())).unwrap_err()
    ));
    let console = client.console();
    let err = run(&console, &format!("SET {} x", t.key("k0"))).unwrap_err();
    assert!(is_read_only(&err), "{err}");
    assert_eq!(ok(&console, &format!("GET {}", t.key("k0"))), bulk("0"));
    // Nothing changed.
    let mut con = t.seed_cluster();
    for key in &keys {
        let ttl: i64 = redis::cmd("TTL").arg(&**key).query(&mut con).unwrap();
        assert_eq!(ttl, -1);
    }
}
