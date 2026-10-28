//! Changes to values: every collection edit, whole string and JSON values,
//! new keys, and what each refuses: a key gone or of another type, an
//! element that exists or is gone, a list element that changed. Also
//! races between clients, binary and Unicode data, and the same edits
//! through the cluster client.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400), database 0, and only keys under each test's
//! prefix; the cluster test uses keys under a hash tag of its own, checks
//! them on whichever primary (7000-7005) serves it, and removes them.

#[macro_use]
mod common;

use std::{sync::Arc, thread};

use common::*;
use redcrab_core::{Client, ConnectionConfig, EditError, Key, ListEnd, NewValue, Removed};
use redis::Connection;

fn edit_error(result: anyhow::Result<impl std::fmt::Debug>) -> EditError {
    let err = result.expect_err("the change was expected to be refused");
    err.downcast_ref::<EditError>()
        .unwrap_or_else(|| panic!("not an EditError: {err:#}"))
        .clone()
}

/// What a delete says: `count` removed, `left` left.
fn removed(count: u64, left: u64) -> Removed {
    Removed { count, left }
}

fn cmd<T: redis::FromRedisValue>(con: &mut Connection, args: &[&[u8]]) -> T {
    let mut c = redis::cmd(std::str::from_utf8(args[0]).unwrap());
    for arg in &args[1..] {
        c.arg(*arg);
    }
    c.query(con)
        .unwrap_or_else(|err| panic!("{:?}: {err}", String::from_utf8_lossy(args[0])))
}

fn hgetall(con: &mut Connection, key: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = cmd(con, &[b"HGETALL", key]);
    pairs.sort();
    pairs
}

fn lrange(con: &mut Connection, key: &[u8]) -> Vec<Vec<u8>> {
    cmd(con, &[b"LRANGE", key, b"0", b"-1"])
}

fn smembers(con: &mut Connection, key: &[u8]) -> Vec<Vec<u8>> {
    let mut members: Vec<Vec<u8>> = cmd(con, &[b"SMEMBERS", key]);
    members.sort();
    members
}

fn zrange(con: &mut Connection, key: &[u8]) -> Vec<(Vec<u8>, f64)> {
    cmd(con, &[b"ZRANGE", key, b"0", b"-1", b"WITHSCORES"])
}

fn pttl(con: &mut Connection, key: &[u8]) -> i64 {
    cmd(con, &[b"PTTL", key])
}

fn exists(con: &mut Connection, key: &[u8]) -> bool {
    cmd::<i64>(con, &[b"EXISTS", key]) == 1
}

fn key_type(con: &mut Connection, key: &[u8]) -> String {
    cmd(con, &[b"TYPE", key])
}

/// A hash field's TTL in milliseconds (-1 none, -2 no field).
fn field_ttl(con: &mut Connection, key: &[u8], field: &[u8]) -> i64 {
    let ttls: Vec<i64> = cmd(con, &[b"HPTTL", key, b"FIELDS", b"1", field]);
    ttls[0]
}

const BIN: &[u8] = b"\x00\xff\xfe bin\r\n";
const UNICODE: &str = "grüße 🦀 \u{202e}";

#[test]
fn strings_and_json_documents() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::pipe()
        .cmd("JSON.SET")
        .arg(t.key("doc"))
        .arg("$")
        .arg(r#"{"a":1}"#)
        .ignore()
        .cmd("EXPIRE")
        .arg(t.key("doc"))
        .arg(100)
        .ignore()
        .cmd("SET")
        .arg(t.key("s"))
        .arg("v")
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());

    let doc = r#"{"b":[1,2,{"c":"ü"}],"n":null}"#;
    block_on(client.set_json(t.bkey("doc"), doc.into())).unwrap();
    let stored: String = cmd(&mut con, &[b"JSON.GET", &t.bkey("doc")]);
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(
        stored,
        serde_json::from_str::<serde_json::Value>(doc).unwrap()
    );
    assert!(pttl(&mut con, &t.bkey("doc")) > 90_000, "the TTL is kept");

    // Invalid JSON never reaches the server.
    let err = block_on(client.set_json(t.bkey("doc"), "{".into())).unwrap_err();
    assert!(err.to_string().starts_with("not valid JSON"), "{err:#}");
    assert_eq!(
        edit_error(block_on(client.set_json(t.bkey("s"), "1".into()))),
        EditError::WrongType("string".into())
    );
    assert_eq!(
        edit_error(block_on(client.set_json(t.bkey("missing"), "1".into()))),
        EditError::KeyMissing
    );
    assert!(!exists(&mut con, &t.bkey("missing")));
    // A JSON document is not overwritten as a string.
    assert_eq!(
        edit_error(block_on(client.set_string(t.bkey("doc"), b"x".to_vec()))),
        EditError::WrongType("ReJSON-RL".into())
    );
}

/// An ACL user of `standalone` made for one test, removed at the end.
struct AclUser(String);

impl Drop for AclUser {
    fn drop(&mut self) {
        let mut con = raw(Server::Standalone, 0);
        let _: redis::RedisResult<()> = redis::cmd("ACL")
            .arg("DELUSER")
            .arg(&self.0)
            .query(&mut con);
    }
}

/// Users may be denied scripts (`-@scripting` is a common hardening): a
/// string is still saved with the same checks, and TTLs still change.
#[test]
fn strings_and_ttls_change_without_scripting() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let user = AclUser(format!("it-noscript-{}", std::process::id()));
    redis::cmd("ACL")
        .arg("SETUSER")
        .arg(&user.0)
        .arg("on")
        .arg(">it-pass")
        .arg(format!("~{}*", t.prefix()))
        .arg("+@all")
        .arg("-@scripting")
        .query::<()>(&mut con)
        .unwrap();
    redis::pipe()
        .cmd("SET")
        .arg(t.key("s"))
        .arg("v")
        .arg("PX")
        .arg(100_000)
        .ignore()
        .cmd("HSET")
        .arg(t.key("h"))
        .arg("f")
        .arg("v")
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(ConnectionConfig {
        username: Some(user.0.clone()),
        password: Some("it-pass".into()),
        ..standalone()
    });
    // Scripts really are refused, and the refusal says what it means.
    assert_eq!(
        edit_error(block_on(client.hash_set(
            t.bkey("h"),
            b"f".to_vec(),
            b"w".to_vec(),
            true
        ))),
        EditError::ScriptsRefused
    );
    assert_eq!(
        edit_error(block_on(client.create_key(
            t.bkey("new-hash"),
            NewValue::Hash(vec![(b"f".to_vec(), b"v".to_vec())]),
            None
        ))),
        EditError::ScriptsRefused
    );
    assert!(!exists(&mut con, &t.bkey("new-hash")));
    // A new string does without: SET NX, with its TTL at once.
    let created = t.bkey("new-string");
    block_on(client.create_key(created.clone(), NewValue::String(BIN.to_vec()), Some(60))).unwrap();
    let stored: Vec<u8> = cmd(&mut con, &[b"GET", &created]);
    assert_eq!(stored, BIN);
    assert!((59_000..=60_000).contains(&pttl(&mut con, &created)));
    assert_eq!(
        edit_error(block_on(client.create_key(
            created.clone(),
            NewValue::String(b"other".to_vec()),
            None
        ))),
        EditError::KeyExists
    );
    assert_eq!(
        edit_error(block_on(client.create_key(
            t.bkey("h"),
            NewValue::String(b"x".to_vec()),
            None
        ))),
        EditError::KeyExists
    );
    assert_eq!(key_type(&mut con, &t.bkey("h")), "hash");

    let s = t.bkey("s");
    block_on(client.set_string(s.clone(), BIN.to_vec())).unwrap();
    let stored: Vec<u8> = cmd(&mut con, &[b"GET", &s]);
    assert_eq!(stored, BIN);
    assert!(pttl(&mut con, &s) > 90_000, "the TTL is kept");
    assert_eq!(
        edit_error(block_on(
            client.set_string(t.bkey("missing"), b"x".to_vec())
        )),
        EditError::KeyMissing
    );
    assert!(!exists(&mut con, &t.bkey("missing")));
    assert_eq!(
        edit_error(block_on(client.set_string(t.bkey("h"), b"x".to_vec()))),
        EditError::WrongType("hash".into())
    );
    assert_eq!(key_type(&mut con, &t.bkey("h")), "hash");

    assert!(block_on(client.set_ttl(s.clone(), Some(60))).unwrap());
    assert!((59_000..=60_000).contains(&pttl(&mut con, &s)));
    assert!(block_on(client.set_ttl(s.clone(), None)).unwrap());
    assert_eq!(pttl(&mut con, &s), -1);
    assert!(!block_on(client.set_ttl(s.clone(), None)).unwrap());
    for ttl in [Some(5), None] {
        assert_eq!(
            edit_error(block_on(client.set_ttl(t.bkey("missing"), ttl))),
            EditError::KeyMissing
        );
    }
    assert!(!exists(&mut con, &t.bkey("missing")));
}

#[test]
fn hash_fields() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("h");
    cmd::<()>(&mut con, &[b"HSET", &key, b"a", b"1", BIN, b"bin"]);
    cmd::<()>(&mut con, &[b"EXPIRE", &key, b"100"]);
    let client = t.connect(standalone());
    let set = |field: &[u8], value: &[u8], overwrite| {
        block_on(client.hash_set(key.clone(), field.to_vec(), value.to_vec(), overwrite))
    };

    assert!(set(UNICODE.as_bytes(), BIN, false).unwrap(), "a new field");
    assert_eq!(
        set(b"a", b"2", false).map(drop).unwrap_err().downcast_ref(),
        Some(&EditError::ElementExists)
    );
    assert!(!set(b"a", b"2", true).unwrap(), "an existing field");
    assert!(!set(BIN, b"\x00", true).unwrap());
    assert_eq!(
        hgetall(&mut con, &key),
        [
            (BIN.to_vec(), b"\x00".to_vec()),
            (b"a".to_vec(), b"2".to_vec()),
            (UNICODE.as_bytes().to_vec(), BIN.to_vec()),
        ]
    );
    assert!(pttl(&mut con, &key) > 90_000, "the key's TTL is untouched");

    // A field's own TTL survives a change of its value.
    let _: Vec<i64> = cmd(&mut con, &[b"HEXPIRE", &key, b"500", b"FIELDS", b"1", b"a"]);
    set(b"a", b"3", true).unwrap();
    assert!(field_ttl(&mut con, &key, b"a") > 490_000);

    // Gone keys are not created; other types are not touched.
    assert_eq!(
        edit_error(block_on(client.hash_set(
            t.bkey("missing"),
            b"f".to_vec(),
            b"v".to_vec(),
            true
        ))),
        EditError::KeyMissing
    );
    assert!(!exists(&mut con, &t.bkey("missing")));
    cmd::<()>(&mut con, &[b"SET", &t.bkey("s"), b"v"]);
    assert_eq!(
        edit_error(block_on(client.hash_set(
            t.bkey("s"),
            b"f".to_vec(),
            b"v".to_vec(),
            true
        ))),
        EditError::WrongType("string".into())
    );

    // Deleting counts; the last field takes the key with it.
    assert_eq!(
        block_on(client.hash_delete(key.clone(), vec![b"a".to_vec(), b"nope".to_vec()])).unwrap(),
        removed(1, 2)
    );
    let rest = vec![BIN.to_vec(), UNICODE.as_bytes().to_vec()];
    assert_eq!(
        block_on(client.hash_delete(key.clone(), rest)).unwrap(),
        removed(2, 0)
    );
    assert!(!exists(&mut con, &key));
    assert_eq!(
        edit_error(block_on(
            client.hash_delete(key.clone(), vec![b"a".to_vec()])
        )),
        EditError::KeyMissing
    );
    // Even deleting nothing says the key is gone.
    assert_eq!(
        edit_error(block_on(client.hash_delete(key.clone(), Vec::new()))),
        EditError::KeyMissing
    );
}

#[test]
fn hash_field_renames() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("h");
    cmd::<()>(&mut con, &[b"HSET", &key, b"only", b"v"]);
    cmd::<()>(&mut con, &[b"EXPIRE", &key, b"100"]);
    let _: Vec<i64> = cmd(
        &mut con,
        &[b"HEXPIRE", &key, b"500", b"FIELDS", b"1", b"only"],
    );
    let client = t.connect(standalone());
    let rename = |from: &[u8], to: &[u8], value: Option<&[u8]>, overwrite| {
        block_on(client.hash_rename_field(
            key.clone(),
            from.to_vec(),
            to.to_vec(),
            value.map(<[u8]>::to_vec),
            overwrite,
        ))
    };

    // The only field: the key never empties, so it keeps its TTL; the
    // field keeps its value and its own TTL.
    rename(b"only", BIN, None, false).unwrap();
    assert_eq!(hgetall(&mut con, &key), [(BIN.to_vec(), b"v".to_vec())]);
    assert!(pttl(&mut con, &key) > 90_000);
    assert!(field_ttl(&mut con, &key, BIN) > 490_000);
    // Renamed with a new value at once.
    rename(BIN, UNICODE.as_bytes(), Some(b"new"), false).unwrap();
    assert_eq!(
        hgetall(&mut con, &key),
        [(UNICODE.as_bytes().to_vec(), b"new".to_vec())]
    );
    // To the same name: just the value.
    rename(
        UNICODE.as_bytes(),
        UNICODE.as_bytes(),
        Some(b"newer"),
        false,
    )
    .unwrap();
    assert_eq!(
        hgetall(&mut con, &key),
        [(UNICODE.as_bytes().to_vec(), b"newer".to_vec())]
    );

    cmd::<()>(&mut con, &[b"HSET", &key, b"other", b"o"]);
    assert_eq!(
        edit_error(rename(UNICODE.as_bytes(), b"other", None, false)),
        EditError::ElementExists
    );
    assert_eq!(hgetall(&mut con, &key).len(), 2, "nothing changed");
    rename(UNICODE.as_bytes(), b"other", None, true).unwrap();
    assert_eq!(
        hgetall(&mut con, &key),
        [(b"other".to_vec(), b"newer".to_vec())]
    );
    assert_eq!(
        edit_error(rename(b"gone", b"x", None, false)),
        EditError::ElementMissing
    );
}

#[test]
fn list_elements() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("l");
    cmd::<()>(&mut con, &[b"RPUSH", &key, b"m"]);
    let client = t.connect(standalone());
    let push = |values: &[&[u8]], end| {
        block_on(client.list_push(
            key.clone(),
            values.iter().map(|v| v.to_vec()).collect(),
            end,
        ))
    };

    assert_eq!(push(&[b"a", b"b"], ListEnd::Head).unwrap(), 3);
    assert_eq!(push(&[BIN, b"z"], ListEnd::Tail).unwrap(), 5);
    assert_eq!(lrange(&mut con, &key), [&b"a"[..], b"b", b"m", BIN, b"z"]);
    assert_eq!(
        edit_error(block_on(client.list_push(
            t.bkey("missing"),
            vec![b"x".to_vec()],
            ListEnd::Tail
        ))),
        EditError::KeyMissing
    );
    assert!(!exists(&mut con, &t.bkey("missing")));
    assert!(push(&[], ListEnd::Tail).is_err());

    // Set at an index only while it holds what was loaded.
    block_on(client.list_set(key.clone(), 3, BIN.to_vec(), UNICODE.into())).unwrap();
    assert_eq!(
        edit_error(block_on(client.list_set(
            key.clone(),
            3,
            BIN.to_vec(),
            b"x".to_vec()
        ))),
        EditError::Changed
    );
    assert_eq!(
        edit_error(block_on(client.list_set(
            key.clone(),
            99,
            b"a".to_vec(),
            b"x".to_vec()
        ))),
        EditError::Changed
    );
    // Negative indexes count from the tail, as in Redis.
    block_on(client.list_set(key.clone(), -1, b"z".to_vec(), b"zz".to_vec())).unwrap();
    assert_eq!(
        lrange(&mut con, &key),
        [&b"a"[..], b"b", b"m", UNICODE.as_bytes(), b"zz"]
    );

    // Remove at indexes, all checked first.
    let at = |elements: &[(i64, &[u8])]| {
        block_on(client.list_remove_at(
            key.clone(),
            elements.iter().map(|(i, e)| (*i, e.to_vec())).collect(),
        ))
    };
    assert_eq!(
        edit_error(at(&[(0, b"a"), (1, b"wrong")])),
        EditError::Changed
    );
    assert_eq!(lrange(&mut con, &key).len(), 5, "nothing removed");
    assert_eq!(at(&[(1, b"b"), (0, b"a")]).unwrap(), removed(2, 3));
    assert_eq!(
        lrange(&mut con, &key),
        [&b"m"[..], UNICODE.as_bytes(), b"zz"]
    );
    // Of equal elements, only the one at the index goes.
    cmd::<()>(&mut con, &[b"RPUSH", &key, b"dup", b"dup", b"dup"]);
    assert_eq!(at(&[(4, b"dup")]).unwrap(), removed(1, 5));
    assert_eq!(
        lrange(&mut con, &key),
        [&b"m"[..], UNICODE.as_bytes(), b"zz", b"dup", b"dup"]
    );
    // One occurrence of a value, from the head.
    assert!(block_on(client.list_remove(key.clone(), b"dup".to_vec())).unwrap());
    assert!(!block_on(client.list_remove(key.clone(), b"nope".to_vec())).unwrap());
    assert_eq!(
        lrange(&mut con, &key),
        [&b"m"[..], UNICODE.as_bytes(), b"zz", b"dup"]
    );
    // Removing everything removes the key.
    let all: Vec<(i64, Vec<u8>)> = lrange(&mut con, &key)
        .into_iter()
        .enumerate()
        .map(|(i, e)| (i as i64, e))
        .collect();
    assert_eq!(
        block_on(client.list_remove_at(key.clone(), all)).unwrap(),
        removed(4, 0)
    );
    assert!(!exists(&mut con, &key));
}

#[test]
fn set_members() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("s");
    cmd::<()>(&mut con, &[b"SADD", &key, b"a"]);
    cmd::<()>(&mut con, &[b"EXPIRE", &key, b"100"]);
    let client = t.connect(standalone());

    assert_eq!(
        block_on(client.set_add(
            key.clone(),
            vec![b"a".to_vec(), BIN.to_vec(), UNICODE.into()]
        ))
        .unwrap(),
        2
    );
    // Replaced: the new member is added before the old one goes.
    assert!(
        block_on(client.set_replace(key.clone(), b"a".to_vec(), b"b".to_vec(), false)).unwrap()
    );
    assert_eq!(
        smembers(&mut con, &key),
        [BIN, &b"b"[..], UNICODE.as_bytes()]
    );
    assert_eq!(
        edit_error(block_on(client.set_replace(
            key.clone(),
            b"b".to_vec(),
            BIN.to_vec(),
            false
        ))),
        EditError::ElementExists
    );
    assert!(!block_on(client.set_replace(key.clone(), b"b".to_vec(), BIN.to_vec(), true)).unwrap());
    assert_eq!(smembers(&mut con, &key), [BIN, UNICODE.as_bytes()]);
    assert_eq!(
        edit_error(block_on(client.set_replace(
            key.clone(),
            b"gone".to_vec(),
            b"x".to_vec(),
            false
        ))),
        EditError::ElementMissing
    );
    assert!(pttl(&mut con, &key) > 90_000);

    assert_eq!(
        block_on(client.set_remove(key.clone(), vec![BIN.to_vec(), b"nope".to_vec()])).unwrap(),
        removed(1, 1)
    );
    // A set of one, replaced: never empty, keeps its TTL.
    assert!(
        block_on(client.set_replace(key.clone(), UNICODE.into(), b"last".to_vec(), false)).unwrap()
    );
    assert_eq!(smembers(&mut con, &key), [b"last"]);
    assert!(pttl(&mut con, &key) > 90_000);
    assert_eq!(
        edit_error(block_on(
            client.set_add(t.bkey("missing"), vec![b"x".to_vec()])
        )),
        EditError::KeyMissing
    );
    assert!(!exists(&mut con, &t.bkey("missing")));
}

#[test]
fn sorted_set_members() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("z");
    // 0.1 + 0.2, a score with every digit significant.
    cmd::<()>(
        &mut con,
        &[
            b"ZADD",
            &key,
            b"0.30000000000000004",
            b"precise",
            b"1",
            b"one",
        ],
    );
    let client = t.connect(standalone());

    assert!(
        block_on(client.zset_add(key.clone(), BIN.to_vec(), f64::NEG_INFINITY, false)).unwrap()
    );
    assert!(block_on(client.zset_add(key.clone(), UNICODE.into(), 1e300, false)).unwrap());
    assert_eq!(
        edit_error(block_on(client.zset_add(
            key.clone(),
            b"one".to_vec(),
            5.0,
            false
        ))),
        EditError::ElementExists
    );
    assert!(!block_on(client.zset_add(key.clone(), b"one".to_vec(), f64::INFINITY, true)).unwrap());
    assert!(block_on(client.zset_add(key.clone(), b"x".to_vec(), f64::NAN, false)).is_err());

    // Renamed, the score stays exactly what it was.
    block_on(client.zset_rename_member(
        key.clone(),
        b"precise".to_vec(),
        b"renamed".to_vec(),
        None,
        false,
    ))
    .unwrap();
    // A new score only.
    block_on(client.zset_rename_member(
        key.clone(),
        UNICODE.into(),
        UNICODE.into(),
        Some(-2.5),
        false,
    ))
    .unwrap();
    assert_eq!(
        zrange(&mut con, &key),
        [
            (BIN.to_vec(), f64::NEG_INFINITY),
            (UNICODE.as_bytes().to_vec(), -2.5),
            (b"renamed".to_vec(), 0.1 + 0.2),
            (b"one".to_vec(), f64::INFINITY),
        ]
    );
    assert_eq!(
        edit_error(block_on(client.zset_rename_member(
            key.clone(),
            b"renamed".to_vec(),
            b"one".to_vec(),
            None,
            false
        ))),
        EditError::ElementExists
    );
    block_on(client.zset_rename_member(
        key.clone(),
        b"renamed".to_vec(),
        b"one".to_vec(),
        Some(7.0),
        true,
    ))
    .unwrap();
    assert_eq!(
        zrange(&mut con, &key).last().unwrap(),
        &(b"one".to_vec(), 7.0)
    );
    assert_eq!(
        edit_error(block_on(client.zset_rename_member(
            key.clone(),
            b"gone".to_vec(),
            b"x".to_vec(),
            None,
            false
        ))),
        EditError::ElementMissing
    );
    assert_eq!(
        block_on(client.zset_remove(
            key.clone(),
            vec![BIN.to_vec(), b"one".to_vec(), b"nope".to_vec()]
        ))
        .unwrap(),
        removed(2, 1)
    );
    assert_eq!(zrange(&mut con, &key).len(), 1);
}

#[test]
fn stream_entries() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("x");
    cmd::<()>(&mut con, &[b"XADD", &key, b"5-0", b"f", b"v"]);
    let client = t.connect(standalone());
    let add = |id: &str, fields: &[(&[u8], &[u8])]| {
        block_on(
            client.stream_add(
                key.clone(),
                id.into(),
                fields
                    .iter()
                    .map(|(f, v)| (f.to_vec(), v.to_vec()))
                    .collect(),
            ),
        )
    };

    assert_eq!(add("6-1", &[(BIN, UNICODE.as_bytes())]).unwrap(), "6-1");
    assert_eq!(add("7-*", &[(b"a", b"1")]).unwrap(), "7-0");
    let auto = add("*", &[(b"a", b"1"), (b"a", b"2")]).unwrap();
    assert!(auto.contains('-'), "{auto}");
    // Not past the last entry: the server says so.
    let err = add("6-5", &[(b"a", b"1")]).unwrap_err();
    assert!(format!("{err:#}").contains("equal or smaller"), "{err:#}");
    // Malformed IDs and empty entries never reach it.
    for bad in ["", "x", "1-", "-1", "1-2-3", "1.5"] {
        assert!(add(bad, &[(b"a", b"1")]).is_err(), "{bad:?}");
    }
    assert!(add("*", &[]).is_err());
    let len: i64 = cmd(&mut con, &[b"XLEN", &key]);
    assert_eq!(len, 4);
    type Entry = (String, Vec<(Vec<u8>, Vec<u8>)>);
    let entry: Vec<Entry> = cmd(&mut con, &[b"XRANGE", &key, b"6-1", b"6-1"]);
    assert_eq!(entry[0].1, [(BIN.to_vec(), UNICODE.as_bytes().to_vec())]);

    assert_eq!(
        block_on(client.stream_delete(key.clone(), vec!["5-0".into(), "6-1".into(), "9-9".into()]))
            .unwrap(),
        removed(2, 2)
    );
    // Its last entries deleted, a stream stays, with its TTL.
    cmd::<()>(&mut con, &[b"EXPIRE", &key, b"500"]);
    let ids: Vec<Entry> = cmd(&mut con, &[b"XRANGE", &key, b"-", b"+"]);
    let ids = ids.into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        block_on(client.stream_delete(key.clone(), ids)).unwrap(),
        removed(2, 0)
    );
    let ty: String = cmd(&mut con, &[b"TYPE", &key]);
    assert_eq!(ty, "stream");
    assert!(pttl(&mut con, &key) > 400_000);
    assert_eq!(
        edit_error(block_on(client.stream_add(
            t.bkey("missing"),
            "*".into(),
            vec![(b"f".to_vec(), b"v".to_vec())]
        ))),
        EditError::KeyMissing
    );
    assert!(!exists(&mut con, &t.bkey("missing")));
}

#[test]
fn new_keys_of_every_type() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let client = t.connect(standalone());
    let many: Vec<(Vec<u8>, Vec<u8>)> = (0..2500)
        .map(|i| (format!("f{i}").into_bytes(), vec![b'v'; i % 7]))
        .collect();
    let values = [
        ("string", NewValue::String(BIN.to_vec()), "string"),
        ("hash", NewValue::Hash(many.clone()), "hash"),
        (
            "list",
            NewValue::List(vec![b"a".to_vec(), BIN.to_vec()]),
            "list",
        ),
        (
            "set",
            NewValue::Set((0..1500).map(|i: i32| i.to_string().into_bytes()).collect()),
            "set",
        ),
        (
            "zset",
            NewValue::ZSet(vec![(b"m".to_vec(), -1.5), (UNICODE.into(), f64::INFINITY)]),
            "zset",
        ),
        (
            "stream",
            NewValue::Stream {
                id: "*".into(),
                fields: vec![(b"f".to_vec(), BIN.to_vec())],
            },
            "stream",
        ),
        ("json", NewValue::Json(r#"{"a":[1,2]}"#.into()), "ReJSON-RL"),
    ];
    for (name, value, ty) in values {
        let key = t.bkey([name.as_bytes(), b":", BIN].concat());
        block_on(client.create_key(key.clone(), value.clone(), Some(100))).unwrap();
        assert_eq!(key_type(&mut con, &key), ty, "{name}");
        assert!((99_000..=100_000).contains(&pttl(&mut con, &key)), "{name}");
        // Never over an existing key, whatever its type.
        assert_eq!(
            edit_error(block_on(client.create_key(
                key.clone(),
                NewValue::String(b"x".to_vec()),
                None
            ))),
            EditError::KeyExists,
            "{name}"
        );
        assert_eq!(key_type(&mut con, &key), ty, "{name}: untouched");
    }
    let hash = t.bkey([&b"hash:"[..], BIN].concat());
    let mut stored = hgetall(&mut con, &hash);
    let mut expected = many;
    stored.sort();
    expected.sort();
    assert_eq!(stored, expected, "every chunk of the hash");
    let set = t.bkey([&b"set:"[..], BIN].concat());
    assert_eq!(cmd::<i64>(&mut con, &[b"SCARD", &set]), 1500);

    // No TTL means none.
    block_on(client.create_key(t.bkey("plain"), NewValue::String(Vec::new()), None)).unwrap();
    assert_eq!(pttl(&mut con, &t.bkey("plain")), -1);
    assert!(
        exists(&mut con, &t.bkey("plain")),
        "an empty string is a value"
    );
}

/// Whatever fails on the way, a new key is there whole or not at all.
#[test]
fn new_keys_are_never_half_created() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let client = t.connect(standalone());
    let refused = [
        // The server refuses these.
        NewValue::Stream {
            id: "0-0".into(),
            fields: vec![(b"f".to_vec(), b"v".to_vec())],
        },
        // These never get that far.
        NewValue::Hash(Vec::new()),
        NewValue::List(Vec::new()),
        NewValue::Set(Vec::new()),
        NewValue::ZSet(vec![(b"m".to_vec(), f64::NAN)]),
        NewValue::Stream {
            id: "nope".into(),
            fields: vec![(b"f".to_vec(), b"v".to_vec())],
        },
        NewValue::Stream {
            id: "*".into(),
            fields: Vec::new(),
        },
        NewValue::Json("{".into()),
    ];
    for value in refused {
        let result = block_on(client.create_key(t.bkey("k"), value.clone(), Some(10)));
        assert!(result.is_err(), "{value:?}");
        assert!(!exists(&mut con, &t.bkey("k")), "{value:?}");
    }
    // TTLs out of range are refused before anything is written.
    for ttl in [0, u64::MAX] {
        let result =
            block_on(client.create_key(t.bkey("k"), NewValue::String(b"v".to_vec()), Some(ttl)));
        assert!(result.is_err(), "{ttl}");
        assert!(!exists(&mut con, &t.bkey("k")));
    }
}

/// Two clients creating the same key at once: exactly one wins, and the
/// key holds its value.
#[test]
fn concurrent_creates_have_one_winner() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let clients: Vec<Client> = (0..8).map(|_| t.connect(standalone())).collect();
    for round in 0..20 {
        let key = t.bkey(format!("race{round}"));
        let results: Vec<_> = thread::scope(|scope| {
            let creates: Vec<_> = clients
                .iter()
                .enumerate()
                .map(|(i, client)| {
                    let key = key.clone();
                    scope.spawn(move || {
                        let value = NewValue::List(vec![i.to_string().into_bytes()]);
                        block_on(client.create_key(key, value, None))
                    })
                })
                .collect();
            creates.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<usize> = results
            .iter()
            .enumerate()
            .filter(|(_, r)| r.is_ok())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(winners.len(), 1, "round {round}: {results:?}");
        for result in results.into_iter().filter(Result::is_err) {
            assert_eq!(edit_error(result), EditError::KeyExists);
        }
        assert_eq!(
            lrange(&mut con, &key),
            [winners[0].to_string().into_bytes()]
        );
    }
}

/// Two clients changing the same list element from the same loaded value:
/// the first wins, the second learns the list changed and writes nothing.
#[test]
fn concurrent_list_edits_do_not_overwrite_each_other() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("l");
    let a = Arc::new(t.connect(standalone()));
    let b = Arc::new(t.connect(standalone()));
    for round in 0..30 {
        cmd::<()>(&mut con, &[b"DEL", &key]);
        cmd::<()>(&mut con, &[b"RPUSH", &key, b"x", b"loaded", b"y"]);
        let edits: Vec<_> = [(a.clone(), "from a"), (b.clone(), "from b")]
            .into_iter()
            .map(|(client, value)| {
                let key = key.clone();
                thread::spawn(move || {
                    block_on(client.list_set(key, 1, b"loaded".to_vec(), value.into()))
                })
            })
            .collect();
        let results: Vec<_> = edits.into_iter().map(|h| h.join().unwrap()).collect();
        let won: Vec<&str> = ["from a", "from b"]
            .into_iter()
            .zip(&results)
            .filter(|(_, r)| r.is_ok())
            .map(|(v, _)| v)
            .collect();
        assert_eq!(won.len(), 1, "round {round}");
        let loser = results.into_iter().find(Result::is_err).unwrap();
        assert_eq!(edit_error(loser), EditError::Changed);
        assert_eq!(lrange(&mut con, &key)[1], won[0].as_bytes());
    }
}

/// A key deleted, or replaced by another type, between loading and saving
/// is neither recreated nor overwritten, by any edit.
#[test]
fn edits_of_a_key_that_went_away() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let client = t.connect(standalone());
    let k = || t.bkey("k");
    let v = || b"v".to_vec();
    let check = |con: &mut Connection, expected: EditError, still: Option<&str>| {
        let results = [
            block_on(client.set_string(k(), v())).map(drop),
            block_on(client.hash_set(k(), v(), v(), true)).map(drop),
            block_on(client.hash_rename_field(k(), v(), v(), None, true)).map(drop),
            block_on(client.list_push(k(), vec![v()], ListEnd::Head)).map(drop),
            block_on(client.list_set(k(), 0, v(), v())).map(drop),
            block_on(client.list_remove_at(k(), vec![(0, v())])).map(drop),
            block_on(client.list_remove(k(), v())).map(drop),
            block_on(client.set_add(k(), vec![v()])).map(drop),
            block_on(client.set_remove(k(), vec![v()])).map(drop),
            block_on(client.set_replace(k(), v(), v(), true)).map(drop),
            block_on(client.zset_add(k(), v(), 1.0, true)).map(drop),
            block_on(client.zset_rename_member(k(), v(), v(), None, true)).map(drop),
            block_on(client.zset_remove(k(), vec![v()])).map(drop),
            block_on(client.stream_add(k(), "*".into(), vec![(v(), v())])).map(drop),
            block_on(client.stream_delete(k(), vec!["1-1".into()])).map(drop),
            block_on(client.set_json(k(), "1".into())).map(drop),
            block_on(client.set_ttl(k(), Some(10))).map(drop),
        ];
        let refusals = results
            .iter()
            .filter(|result| {
                result
                    .as_ref()
                    .err()
                    .and_then(|err| err.downcast_ref::<EditError>())
                    == Some(&expected)
            })
            .count();
        match still {
            None => assert!(!exists(con, &k())),
            Some(ty) => assert_eq!(key_type(con, &k()), ty),
        }
        refusals
    };
    // Missing: every one of them refuses.
    assert_eq!(check(&mut con, EditError::KeyMissing, None), 17);
    // A string where something else was: all but the string edits and the
    // TTL refuse.
    cmd::<()>(&mut con, &[b"SET", &k(), b"text"]);
    assert_eq!(
        check(
            &mut con,
            EditError::WrongType("string".into()),
            Some("string")
        ),
        15
    );
    let value: Vec<u8> = cmd(&mut con, &[b"GET", &k()]);
    assert_eq!(value, b"v", "the string edit itself went through");
}

/// The same edits through the cluster client: each is one script on one
/// key, so it is routed to the primary that owns the key's slot. A rename
/// across slots is refused with a clear message.
#[test]
fn edits_through_the_cluster_client() {
    let Some(t) = it!() else { return };
    let cluster = Cluster::new(&t);
    let client = block_on(Client::connect(common::cluster())).unwrap();
    assert!(client.topology().is_cluster());
    let mut con = cluster.con();
    let k = |name: &str| cluster.key(name);

    block_on(client.create_key(
        k("h"),
        NewValue::Hash(vec![(b"f".to_vec(), BIN.to_vec())]),
        Some(100),
    ))
    .unwrap();
    block_on(client.hash_set(k("h"), b"g".to_vec(), b"2".to_vec(), false)).unwrap();
    block_on(client.hash_rename_field(k("h"), b"f".to_vec(), b"f2".to_vec(), None, false)).unwrap();
    assert_eq!(
        hgetall(&mut con, &k("h")),
        [
            (b"f2".to_vec(), BIN.to_vec()),
            (b"g".to_vec(), b"2".to_vec())
        ]
    );
    block_on(client.create_key(k("l"), NewValue::List(vec![b"a".to_vec()]), None)).unwrap();
    block_on(client.list_push(k("l"), vec![b"b".to_vec()], ListEnd::Tail)).unwrap();
    block_on(client.list_set(k("l"), 0, b"a".to_vec(), b"A".to_vec())).unwrap();
    block_on(client.list_remove_at(k("l"), vec![(1, b"b".to_vec())])).unwrap();
    assert_eq!(lrange(&mut con, &k("l")), [b"A"]);
    block_on(client.create_key(k("s"), NewValue::Set(vec![b"a".to_vec()]), None)).unwrap();
    block_on(client.set_replace(k("s"), b"a".to_vec(), b"b".to_vec(), false)).unwrap();
    assert_eq!(smembers(&mut con, &k("s")), [b"b"]);
    block_on(client.create_key(k("z"), NewValue::ZSet(vec![(b"a".to_vec(), 1.0)]), None)).unwrap();
    block_on(client.zset_rename_member(k("z"), b"a".to_vec(), b"b".to_vec(), Some(2.0), false))
        .unwrap();
    assert_eq!(zrange(&mut con, &k("z")), [(b"b".to_vec(), 2.0)]);
    block_on(client.create_key(
        k("x"),
        NewValue::Stream {
            id: "1-1".into(),
            fields: vec![(b"f".to_vec(), b"v".to_vec())],
        },
        None,
    ))
    .unwrap();
    assert_eq!(
        block_on(client.stream_add(k("x"), "2-1".into(), vec![(b"f".to_vec(), b"w".to_vec())]))
            .unwrap(),
        "2-1"
    );
    block_on(client.create_key(k("str"), NewValue::String(b"v".to_vec()), None)).unwrap();
    block_on(client.set_string(k("str"), b"w".to_vec())).unwrap();
    block_on(client.set_ttl(k("str"), Some(50))).unwrap();
    assert!(pttl(&mut con, &k("str")) > 40_000);
    // Same slot (same hash tag): renames work.
    block_on(client.rename(k("str"), k("str2"))).unwrap();
    assert!(exists(&mut con, &k("str2")));

    // Another slot: refused, with what to do about it.
    let elsewhere = Key::from(&b"{redcrab-it-elsewhere}x"[..]);
    let err = block_on(client.rename(k("str2"), elsewhere)).unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("same hash slot") && message.contains("hash tag"),
        "{message}"
    );
    assert!(exists(&mut con, &k("str2")));
}

/// Keys under a hash tag of the test's own, on the cluster primary that
/// owns its slot; removed when dropped.
struct Cluster {
    tag: String,
    port: u16,
}

impl Cluster {
    fn new(t: &It) -> Self {
        let tag = format!("{{{}}}", t.prefix());
        let mut seed = redis::Client::open("redis://127.0.0.1:7000/")
            .unwrap()
            .get_connection()
            .expect("the cluster on 7000-7005; is scripts/test-env.sh up?");
        let slot: i64 = redis::cmd("CLUSTER")
            .arg("KEYSLOT")
            .arg(&tag)
            .query(&mut seed)
            .unwrap();
        // [[start, end, [ip, port, id], replica...], ...]
        let slots: Vec<redis::Value> = redis::cmd("CLUSTER").arg("SLOTS").query(&mut seed).unwrap();
        let port = slots
            .iter()
            .find_map(|range| {
                let redis::Value::Array(parts) = range else {
                    return None;
                };
                let (redis::Value::Int(start), redis::Value::Int(end)) = (&parts[0], &parts[1])
                else {
                    return None;
                };
                if !(start..=end).contains(&&slot) {
                    return None;
                }
                let redis::Value::Array(primary) = &parts[2] else {
                    return None;
                };
                match primary[1] {
                    redis::Value::Int(port) => u16::try_from(port).ok(),
                    _ => None,
                }
            })
            .expect("a primary for the slot");
        Self { tag, port }
    }

    fn key(&self, name: &str) -> Key {
        Key::from(format!("{}{name}", self.tag).as_bytes())
    }

    fn con(&self) -> Connection {
        redis::Client::open(format!("redis://127.0.0.1:{}/", self.port))
            .unwrap()
            .get_connection()
            .unwrap()
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let mut con = self.con();
        let pattern = String::from_utf8(redcrab_core::browser::escape_glob(self.tag.as_bytes()))
            .unwrap()
            + "*";
        let keys: Vec<Vec<u8>> = redis::cmd("KEYS")
            .arg(pattern)
            .query(&mut con)
            .unwrap_or_default();
        if !keys.is_empty() {
            let _: Result<i64, _> = redis::cmd("UNLINK").arg(keys).query(&mut con);
        }
    }
}
