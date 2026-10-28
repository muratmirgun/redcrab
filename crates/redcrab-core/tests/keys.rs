//! Requests on keys: `key_meta`, `memory_usage`, `delete`, `rename`,
//! `set_ttl` and `set_string`.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400), database 0, and only keys under each test's prefix.

#[macro_use]
mod common;

use common::*;
use redcrab_core::{EditError, Key, KeyMeta, KeyType};
use redis::Connection;

fn pttl(con: &mut Connection, key: &[u8]) -> i64 {
    redis::cmd("PTTL").arg(key).query(con).unwrap()
}

fn get(con: &mut Connection, key: &[u8]) -> Option<Vec<u8>> {
    redis::cmd("GET").arg(key).query(con).unwrap()
}

fn exists(con: &mut Connection, key: &[u8]) -> bool {
    redis::cmd("EXISTS").arg(key).query::<i64>(con).unwrap() == 1
}

fn set(con: &mut Connection, key: &[u8], value: &[u8]) {
    redis::cmd("SET")
        .arg(key)
        .arg(value)
        .query::<()>(con)
        .unwrap();
}

/// Creates one key of each type the environment's server knows, named
/// after the type.
fn seed_every_type(t: &It) -> Vec<(Key, KeyType)> {
    let mut con = t.seed0();
    let commands: [(&str, &[&str], KeyType); 10] = [
        ("string", &["SET", "v"], KeyType::String),
        ("hash", &["HSET", "f", "v"], KeyType::Hash),
        ("list", &["RPUSH", "a"], KeyType::List),
        ("set", &["SADD", "a"], KeyType::Set),
        ("zset", &["ZADD", "1", "a"], KeyType::ZSet),
        ("stream", &["XADD", "*", "f", "v"], KeyType::Stream),
        ("json", &["JSON.SET", "$", "[1]"], KeyType::Json),
        (
            "bloom",
            &["BF.ADD", "a"],
            KeyType::Other("MBbloom--".into()),
        ),
        (
            "ts",
            &["TS.ADD", "1", "1"],
            KeyType::Other("TSDB-TYPE".into()),
        ),
        (
            "vectors",
            &["VADD", "VALUES", "2", "1", "2", "a"],
            KeyType::Other("vectorset".into()),
        ),
    ];
    commands
        .into_iter()
        .map(|(name, command, ty)| {
            let key = t.bkey(name);
            let mut cmd = redis::cmd(command[0]);
            cmd.arg(&*key).arg(&command[1..]);
            cmd.query::<()>(&mut con).unwrap();
            (key, ty)
        })
        .collect()
}

#[test]
fn key_meta_reports_every_type() {
    let Some(t) = it!() else { return };
    let typed = seed_every_type(&t);
    let client = t.connect(standalone());
    let mut keys: Vec<Key> = typed.iter().map(|(key, _)| key.clone()).collect();
    keys.push(t.bkey("missing"));

    let meta = block_on(client.key_meta(keys)).unwrap();
    let mut expected: Vec<KeyMeta> = typed
        .into_iter()
        .map(|(_, ty)| KeyMeta { ty, ttl_ms: -1 })
        .collect();
    expected.push(KeyMeta {
        ty: KeyType::None,
        ttl_ms: -2,
    });
    assert_eq!(meta, expected);
    assert_eq!(meta[7].ty.label(), "MBbloom--");
    assert_eq!(meta[6].ty.label(), "JSON");
}

#[test]
fn key_meta_reports_ttls_in_milliseconds() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::pipe()
        .cmd("SET")
        .arg(t.key("seconds"))
        .arg(1)
        .arg("EX")
        .arg(100)
        .ignore()
        .cmd("SET")
        .arg(t.key("millis"))
        .arg(1)
        .arg("PX")
        .arg(1500)
        .ignore()
        .cmd("SET")
        .arg(t.key("forever"))
        .arg(1)
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let names = ["seconds", "millis", "forever", "missing"];
    let meta = block_on(client.key_meta(names.iter().map(|n| t.bkey(n)).collect())).unwrap();
    let ttls: Vec<i64> = meta.iter().map(|m| m.ttl_ms).collect();
    assert!((98_000..=100_000).contains(&ttls[0]), "{ttls:?}");
    assert!((1..=1500).contains(&ttls[1]), "{ttls:?}");
    assert_eq!(ttls[2..], [-1, -2]);
}

#[test]
fn key_meta_keeps_the_order_of_many_keys() {
    let Some(t) = it!() else { return };
    let mut pipe = redis::pipe();
    let mut keys = Vec::new();
    for i in 0..1000u32 {
        // Every name is also made binary, to check the keys go out as bytes.
        let key = t.bkey([&i.to_be_bytes()[..], b"\xff"].concat());
        match i % 3 {
            0 => {
                pipe.cmd("SET").arg(&*key).arg(1).ignore();
            }
            1 => {
                pipe.cmd("HSET").arg(&*key).arg("f").arg(1).ignore();
            }
            _ => {} // left missing
        }
        keys.push(key);
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
    let client = t.connect(standalone());
    let meta = block_on(client.key_meta(keys)).unwrap();
    assert_eq!(meta.len(), 1000);
    for (i, meta) in meta.iter().enumerate() {
        let expected = [KeyType::String, KeyType::Hash, KeyType::None][i % 3].clone();
        assert_eq!(meta.ty, expected, "key {i}");
    }

    // No keys, no round trip, no error.
    assert_eq!(block_on(client.key_meta(Vec::new())).unwrap(), []);
}

#[test]
fn memory_usage_in_bytes() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    set(&mut con, &t.bkey("big"), &vec![b'x'; 10_000]);
    let client = t.connect(standalone());
    let bytes = block_on(client.memory_usage(t.bkey("big"))).unwrap();
    assert!(bytes.is_some_and(|b| b >= 10_000), "{bytes:?}");
    assert_eq!(
        block_on(client.memory_usage(t.bkey("missing"))).unwrap(),
        None
    );
}

#[test]
fn delete_unlinks_and_counts() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    set(&mut con, &t.bkey("a"), b"1");
    set(&mut con, &t.bkey("b"), b"2");
    set(&mut con, &t.bkey(b"\x00bin\xff"), b"3");
    let fields: Vec<(String, u32)> = (0..10_000).map(|i| (format!("f{i}"), i)).collect();
    redis::cmd("HSET")
        .arg(t.key("big"))
        .arg(&fields)
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());

    let keys = ["a", "b", "big", "missing"].map(|n| t.bkey(n));
    let mut all = keys.to_vec();
    all.push(t.bkey(b"\x00bin\xff"));
    assert_eq!(block_on(client.delete(all.clone())).unwrap(), 4);
    for key in &all {
        assert!(!exists(&mut con, key));
    }
    assert_eq!(block_on(client.delete(all)).unwrap(), 0);
    assert_eq!(block_on(client.delete(Vec::new())).unwrap(), 0);

    // A key named twice is deleted once.
    set(&mut con, &t.bkey("twice"), b"1");
    let twice = vec![t.bkey("twice"), t.bkey("twice")];
    assert_eq!(block_on(client.delete(twice)).unwrap(), 1);
}

#[test]
fn rename_moves_the_value_and_its_ttl() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::pipe()
        .cmd("HSET")
        .arg(t.key("from"))
        .arg("f")
        .arg("v")
        .ignore()
        .cmd("EXPIRE")
        .arg(t.key("from"))
        .arg(100)
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let to = t.bkey([&b"to:"[..], "\u{1F980}".as_bytes(), b"\xff"].concat());
    block_on(client.rename(t.bkey("from"), to.clone())).unwrap();
    assert!(!exists(&mut con, &t.bkey("from")));
    let value: Option<String> = redis::cmd("HGET")
        .arg(&*to)
        .arg("f")
        .query(&mut con)
        .unwrap();
    assert_eq!(value.as_deref(), Some("v"));
    assert!((98_000..=100_000).contains(&pttl(&mut con, &to)));
}

#[test]
fn rename_refuses_an_existing_target() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    set(&mut con, &t.bkey("a"), b"A");
    set(&mut con, &t.bkey("b"), b"B");
    redis::cmd("RPUSH")
        .arg(t.key("list"))
        .arg("x")
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());

    for target in ["b", "list"] {
        let err = block_on(client.rename(t.bkey("a"), t.bkey(target))).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("a key named {} already exists", t.key(target))
        );
    }
    // Nothing moved or changed.
    assert_eq!(get(&mut con, &t.bkey("a")).as_deref(), Some(&b"A"[..]));
    assert_eq!(get(&mut con, &t.bkey("b")).as_deref(), Some(&b"B"[..]));
    let len: i64 = redis::cmd("LLEN")
        .arg(t.key("list"))
        .query(&mut con)
        .unwrap();
    assert_eq!(len, 1);
}

#[test]
fn rename_of_a_missing_key_fails() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let err = block_on(client.rename(t.bkey("missing"), t.bkey("new"))).unwrap_err();
    assert_eq!(err.downcast_ref(), Some(&EditError::KeyMissing), "{err:#}");
    assert_eq!(err.to_string(), "the key no longer exists");
    assert!(!exists(&mut t.seed0(), &t.bkey("new")));
}

#[test]
fn set_ttl_and_persist() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("k");
    set(&mut con, &key, b"v");
    let client = t.connect(standalone());
    let set_ttl = |key: &Key, seconds| block_on(client.set_ttl(key.clone(), seconds)).unwrap();

    assert!(set_ttl(&key, Some(100)));
    assert!((99_000..=100_000).contains(&pttl(&mut con, &key)));
    assert!(
        set_ttl(&key, Some(5)),
        "a shorter TTL replaces a longer one"
    );
    assert!((1..=5_000).contains(&pttl(&mut con, &key)));
    assert!(set_ttl(&key, None));
    assert_eq!(pttl(&mut con, &key), -1);
    assert!(!set_ttl(&key, None), "nothing to persist");
    assert_eq!(get(&mut con, &key).as_deref(), Some(&b"v"[..]));

    // A missing key is not created, and says so.
    let missing = t.bkey("missing");
    for seconds in [Some(10), None] {
        let err = block_on(client.set_ttl(missing.clone(), seconds)).unwrap_err();
        assert_eq!(err.downcast_ref(), Some(&EditError::KeyMissing), "{err:#}");
    }
    assert!(!exists(&mut con, &missing));

    // A TTL of 0 would delete the key.
    let err = block_on(client.set_ttl(key.clone(), Some(0))).unwrap_err();
    assert!(err.to_string().contains("delete"), "{err:#}");
    assert!(exists(&mut con, &key));
}

#[test]
fn set_ttl_out_of_range_is_refused() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("k");
    set(&mut con, &key, b"v");
    let client = t.connect(standalone());
    let err = block_on(client.set_ttl(key.clone(), Some(u64::MAX))).unwrap_err();
    assert!(format!("{err:#}").contains("out of range"), "{err:#}");
    assert_eq!(pttl(&mut con, &key), -1);
}

#[test]
fn set_string_keeps_the_ttl() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("SET")
        .arg(t.key("expiring"))
        .arg("old")
        .arg("EX")
        .arg(100)
        .query::<()>(&mut con)
        .unwrap();
    set(&mut con, &t.bkey("forever"), b"old");
    let client = t.connect(standalone());
    let save = |name: &str, value: &[u8]| {
        block_on(client.set_string(t.bkey(name), value.to_vec())).unwrap();
        assert_eq!(get(&mut t.seed0(), &t.bkey(name)).as_deref(), Some(value));
    };

    save("expiring", b"new");
    assert!((98_000..=100_000).contains(&pttl(&mut con, &t.bkey("expiring"))));
    save("forever", b"new");
    assert_eq!(pttl(&mut con, &t.bkey("forever")), -1);

    // Values are bytes, of any size.
    save("forever", b"\x00\xff\xfe\r\n\x80 end");
    save("forever", b"");
    assert!(exists(&mut con, &t.bkey("forever")));
    let big: Vec<u8> = (0..2 << 20).map(|i| (i % 253) as u8).collect();
    save("forever", &big);
}

/// An edit is of the value that was loaded: a key deleted since is not
/// created again, and one replaced by another type is not overwritten.
#[test]
fn set_string_refuses_missing_and_other_keys() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("RPUSH")
        .arg(t.key("list"))
        .arg("x")
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let err = block_on(client.set_string(t.bkey("missing"), b"v".to_vec())).unwrap_err();
    assert_eq!(err.downcast_ref(), Some(&EditError::KeyMissing));
    assert!(!exists(&mut con, &t.bkey("missing")));
    let err = block_on(client.set_string(t.bkey("list"), b"v".to_vec())).unwrap_err();
    assert_eq!(
        err.downcast_ref(),
        Some(&EditError::WrongType("list".into()))
    );
    assert_eq!(err.to_string(), "the key now holds a value of type list");
    let len: i64 = redis::cmd("LLEN")
        .arg(t.key("list"))
        .query(&mut con)
        .unwrap();
    assert_eq!(len, 1);
}
