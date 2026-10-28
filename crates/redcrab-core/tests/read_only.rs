//! The read-only gate: on a read-only connection every mutating `Client`
//! method and every console command that is not known to be a read is
//! refused before anything is sent, while reading keeps working. Also the
//! commands the console refuses on any connection.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400): database 0 with each test's own prefix, plus
//! `READ_ONLY_DB` and `SCRATCH_DB`, each owned by one test (see `common`).
//!
//! Commands that would wreck the shared server if the gate let them through
//! (SHUTDOWN, FLUSHALL, REPLICAOF, SCRIPT FLUSH, ...) are only checked by
//! the console's unit tests, which never send anything.

#[macro_use]
mod common;

use std::collections::HashMap;

use common::*;
use redcrab_core::{
    Client, ConnectionConfig, ConsoleSession, Key, KeyType, ListEnd, NewValue, RawValue,
    ReadOnlyError, RowKind, ScanOptions, ValueData,
    browser::{KeyBrowser, ScanStatus},
    viewer,
};
use redis::Connection;

fn read_only(db: u32) -> ConnectionConfig {
    ConnectionConfig {
        db,
        read_only: true,
        ..standalone()
    }
}

/// Runs a command whose arguments may contain spaces.
fn exec(console: &ConsoleSession, argv: &[&str]) -> anyhow::Result<RawValue> {
    block_on(console.exec(argv.iter().copied()))
}

/// One key of each kind the console tests read and try to write, under
/// the prefix and named after the kind.
fn seed(t: &It) -> Connection {
    let mut con = t.seed0();
    redis::pipe()
        .cmd("SET")
        .arg(t.key("s"))
        .arg("before")
        .arg("EX")
        .arg(1000)
        .ignore()
        .cmd("HSET")
        .arg(t.key("h"))
        .arg("f")
        .arg("v")
        .ignore()
        .cmd("RPUSH")
        .arg(t.key("l"))
        .arg("b")
        .arg("a")
        .ignore()
        .cmd("SADD")
        .arg(t.key("st"))
        .arg("a")
        .ignore()
        .cmd("ZADD")
        .arg(t.key("z"))
        .arg(1)
        .arg("a")
        .ignore()
        .cmd("XADD")
        .arg(t.key("x"))
        .arg("1-1")
        .arg("f")
        .arg("v")
        .ignore()
        .cmd("JSON.SET")
        .arg(t.key("j"))
        .arg("$")
        .arg(r#"{"a":1}"#)
        .ignore()
        .cmd("BF.ADD")
        .arg(t.key("b"))
        .arg("a")
        .ignore()
        .cmd("PFADD")
        .arg(t.key("hll"))
        .arg("a")
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    con
}

/// Every key under the prefix with its serialized value and TTL, to tell
/// whether anything changed.
fn snapshot(t: &It, con: &mut Connection) -> HashMap<Vec<u8>, (Vec<u8>, i64)> {
    let keys: Vec<Vec<u8>> = redis::cmd("KEYS").arg(t.pattern()).query(con).unwrap();
    keys.into_iter()
        .map(|key| {
            let (dump, ttl): (Vec<u8>, i64) = redis::pipe()
                .cmd("DUMP")
                .arg(&key)
                .cmd("PTTL")
                .arg(&key)
                .query(con)
                .unwrap();
            // The TTL counts down; it only matters whether there is one.
            (key, (dump, ttl.signum()))
        })
        .collect()
}

/// `line`, run on `console` with `{}` standing for keys under the prefix,
/// must be refused as read-only, naming `name`.
fn refused(t: &It, console: &ConsoleSession, line: &str, name: &str) -> ReadOnlyError {
    let argv: Vec<String> = line
        .split_whitespace()
        .map(|arg| match arg.strip_prefix("{}") {
            Some(name) => t.key(name),
            None => arg.to_string(),
        })
        .collect();
    let err = block_on(console.exec(argv)).expect_err(line);
    let read_only = err
        .downcast_ref::<ReadOnlyError>()
        .unwrap_or_else(|| panic!("{line}: {err:#}"))
        .clone();
    assert_eq!(read_only.command(), Some(name), "{line}");
    assert!(
        err.to_string()
            .ends_with(", and this connection is read-only"),
        "{line}: {err}"
    );
    read_only
}

/// Every change to a value or a new key, on the keys `seed` made (`k`
/// names them), each with the overwrite flags set where there are any.
fn every_change(client: &Client, k: &dyn Fn(&str) -> Key) -> Vec<anyhow::Result<()>> {
    let v = || b"v".to_vec();
    vec![
        block_on(client.set_string(k("s"), b"after".to_vec())),
        block_on(client.set_json(k("j"), "[2]".into())),
        block_on(client.hash_set(k("h"), v(), v(), true)).map(drop),
        block_on(client.hash_delete(k("h"), vec![b"f".to_vec()])).map(drop),
        // Refused even when there would be nothing to do.
        block_on(client.hash_delete(k("h"), Vec::new())).map(drop),
        block_on(client.hash_rename_field(k("h"), b"f".to_vec(), v(), None, true)),
        block_on(client.list_push(k("l"), vec![v()], ListEnd::Head)).map(drop),
        block_on(client.list_set(k("l"), 0, b"b".to_vec(), v())),
        block_on(client.list_remove_at(k("l"), vec![(0, b"b".to_vec())])).map(drop),
        block_on(client.list_remove(k("l"), b"a".to_vec())).map(drop),
        block_on(client.set_add(k("st"), vec![v()])).map(drop),
        block_on(client.set_remove(k("st"), vec![b"a".to_vec()])).map(drop),
        block_on(client.set_replace(k("st"), b"a".to_vec(), v(), true)).map(drop),
        block_on(client.zset_add(k("z"), v(), 1.0, true)).map(drop),
        block_on(client.zset_rename_member(k("z"), b"a".to_vec(), v(), Some(2.0), true)),
        block_on(client.zset_remove(k("z"), vec![b"a".to_vec()])).map(drop),
        block_on(client.stream_add(k("x"), "*".into(), vec![(v(), v())])).map(drop),
        block_on(client.stream_delete(k("x"), vec!["1-1".into()])).map(drop),
        block_on(client.create_key(k("new"), NewValue::String(v()), Some(10))),
        block_on(client.create_key(k("new"), NewValue::Hash(vec![(v(), v())]), None)),
    ]
}

#[test]
fn every_mutating_method_is_refused() {
    let Some(t) = it!() else { return };
    let mut con = seed(&t);
    let before = snapshot(&t, &mut con);
    let client = t.connect(read_only(0));
    let writable = t.connect(standalone());
    assert_eq!(client.ensure_writable(), Err(ReadOnlyError::default()));
    assert_eq!(writable.ensure_writable(), Ok(()));

    let k = |name: &str| t.bkey(name);
    let mut failures = vec![
        block_on(client.delete(vec![k("s"), k("h")])).map(drop),
        // Refused even when there would be nothing to do.
        block_on(client.delete(Vec::new())).map(drop),
        block_on(client.rename(k("s"), k("renamed"))),
        block_on(client.set_ttl(k("h"), Some(100))).map(drop),
        block_on(client.set_ttl(k("s"), None)).map(drop),
        block_on(client.set_string(k("new"), b"created".to_vec())),
    ];
    failures.extend(every_change(&client, &k));
    // The edit models' changes go through the same methods.
    let loaded = block_on(viewer::load(&client, k("h"))).unwrap();
    let delete = loaded.delete_rows(&[0]).unwrap();
    failures.push(block_on(delete.send(&client)).map(drop));
    let create = viewer::new_key(viewer::NewKeyType::String, &t.key("n"), "", "v", "1h").unwrap();
    failures.push(block_on(create.send(&client)).map(drop));
    assert!(failures.len() > 25);
    for result in failures {
        let err = result.expect_err("a read-only client changed the server");
        let read_only = err
            .downcast_ref::<ReadOnlyError>()
            .expect("a ReadOnlyError");
        assert_eq!(read_only.command(), None);
        assert_eq!(err.to_string(), "this connection is read-only");
    }
    assert_eq!(snapshot(&t, &mut con), before);
}

/// Owns `READ_ONLY_DB`: the only redcrab connections there are this
/// test's, so the server's own counters show what reached it.
#[test]
fn nothing_reaches_the_server() {
    let Some(t) = it!() else { return };
    let mut con = t.seed(Server::Standalone, READ_ONLY_DB);
    redis::cmd("SET")
        .arg(t.key("k"))
        .arg("v")
        .query::<()>(&mut con)
        .unwrap();
    let mut admin = raw(Server::Standalone, 0);
    let db = READ_ONLY_DB.to_string();
    // (id, commands run) of each redcrab connection on the database.
    let mut connections = || -> Vec<(String, u64)> {
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

    let client = t.connect(read_only(READ_ONLY_DB));
    let start = connections();
    assert_eq!(start.len(), 1, "{start:?}");
    let k = || t.bkey("k");
    let _ = block_on(client.delete(vec![k()])).unwrap_err();
    let _ = block_on(client.rename(k(), t.bkey("other"))).unwrap_err();
    let _ = block_on(client.set_ttl(k(), Some(1))).unwrap_err();
    let _ = block_on(client.set_ttl(k(), None)).unwrap_err();
    for result in every_change(&client, &|_| k()) {
        result.unwrap_err();
    }
    // A console of a read-only client connects on its first allowed
    // command; refused ones never get that far.
    let console = client.console();
    for line in [
        "SET k x",
        "FLUSHDB",
        "CONFIG SET maxmemory 1",
        "NOSUCH",
        "EVAL x 0",
        "SUBSCRIBE ch",
        // Read-only scripts could run SLOWLOG RESET or CLUSTER FAILOVER.
        "EVAL_RO return 0",
        "EVALSHA_RO 0000 0",
        "FCALL_RO f 0",
        "PFCOUNT k",
        "CF.COMPACT k",
        "FT.CURSOR DEL idx 1",
    ] {
        run(&console, line).unwrap_err();
    }
    assert_eq!(connections(), start);

    // The counter does count: one PING is one command.
    block_on(client.ping()).unwrap();
    assert_eq!(connections(), [(start[0].0.clone(), start[0].1 + 1)]);
    ok(&console, "PING");
    assert_eq!(connections().len(), 2);
}

#[test]
fn reading_keeps_working() {
    let Some(t) = it!() else { return };
    seed(&t);
    let client = t.connect(read_only(0));
    block_on(client.ping()).unwrap();
    assert!(block_on(client.server_info()).unwrap().db_size > Some(0));

    let options = ScanOptions {
        pattern: t.pattern(),
        ..ScanOptions::default()
    };
    assert_eq!(scan(&client, options.clone()).unwrap().len(), 9);
    let (events, _) = scan_into(&client, options);
    assert_eq!(done(&events), (9, false));
    let meta = block_on(client.key_meta(vec![t.bkey("h")])).unwrap();
    assert_eq!(meta[0].ty, KeyType::Hash);
    assert!(
        block_on(client.memory_usage(t.bkey("s")))
            .unwrap()
            .is_some()
    );

    // The value view loads everything and offers no editing.
    let writable = client.ensure_writable().is_ok();
    for (name, ty) in [
        ("s", KeyType::String),
        ("h", KeyType::Hash),
        ("l", KeyType::List),
        ("st", KeyType::Set),
        ("z", KeyType::ZSet),
        ("x", KeyType::Stream),
        ("j", KeyType::Json),
    ] {
        let loaded = block_on(viewer::load(&client, t.bkey(name))).unwrap();
        assert_eq!(loaded.meta.ty, ty);
        if let Some(text) = loaded.text(writable) {
            assert!(!text.editable, "{name}");
        }
    }
    let loaded = block_on(viewer::load(&client, t.bkey("s"))).unwrap();
    assert!(matches!(&loaded.data, ValueData::String(v) if v == b"before"));

    // So does the key list.
    let mut browser = KeyBrowser::new(b':', None);
    let request = browser.begin_scan(t.prefix());
    let generation = request.generation;
    for event in collect(client.scan_into(request.tree, request.options)) {
        browser.on_scan_event(generation, event);
    }
    assert!(matches!(browser.status(), ScanStatus::Done { keys: 9, .. }));
    let len = browser.len();
    let request = browser.take_meta_request(0..len).unwrap();
    let result = block_on(client.key_meta_permitted(request.keys.clone()));
    assert!(browser.apply_meta(request, result));
    let rows = browser.row_views(0..len);
    assert!(
        rows.iter()
            .filter(|row| row.kind == RowKind::Key)
            .all(|row| row.meta.is_some())
    );
}

#[test]
fn console_allows_reads() {
    let Some(t) = it!() else { return };
    let mut con = seed(&t);
    let sha: String = redis::cmd("SCRIPT")
        .arg("LOAD")
        .arg("return 1")
        .query(&mut con)
        .unwrap();
    let console = t.connect(read_only(0)).console();
    let key = |name: &str| t.key(name);
    let lines = [
        format!("GET {}", key("s")),
        format!("MGET {} {}", key("s"), key("missing")),
        format!("STRLEN {}", key("s")),
        format!("GETRANGE {} 0 2", key("s")),
        format!("EXISTS {}", key("s")),
        format!("TYPE {}", key("h")),
        format!("TTL {}", key("s")),
        format!("PTTL {}", key("s")),
        format!("OBJECT ENCODING {}", key("s")),
        format!("MEMORY USAGE {}", key("s")),
        format!("DUMP {}", key("s")),
        format!("HGETALL {}", key("h")),
        format!("HGET {} f", key("h")),
        format!("HSCAN {} 0", key("h")),
        format!("LRANGE {} 0 -1", key("l")),
        format!("LINDEX {} 0", key("l")),
        format!("SORT_RO {} ALPHA", key("l")),
        format!("SMEMBERS {}", key("st")),
        format!("SISMEMBER {} a", key("st")),
        format!("ZRANGE {} 0 -1 WITHSCORES", key("z")),
        format!("ZSCORE {} a", key("z")),
        format!("XRANGE {} - +", key("x")),
        format!("XINFO STREAM {}", key("x")),
        format!("JSON.GET {}", key("j")),
        format!("JSON.TYPE {}", key("j")),
        format!("BF.EXISTS {} a", key("b")),
        format!("BITFIELD_RO {} GET u8 0", key("s")),
        format!("SCAN 0 MATCH {} COUNT 100", t.pattern()),
        format!("KEYS {}", t.pattern()),
        "DBSIZE".into(),
        "INFO".into(),
        "INFO keyspace".into(),
        "CONFIG GET maxmemory".into(),
        "CLIENT LIST".into(),
        "CLIENT INFO".into(),
        "CLIENT GETNAME".into(),
        "CLIENT ID".into(),
        "SLOWLOG GET 1".into(),
        "SLOWLOG LEN".into(),
        "LATENCY LATEST".into(),
        "MEMORY STATS".into(),
        "COMMAND COUNT".into(),
        "COMMAND DOCS GET".into(),
        "TIME".into(),
        "LASTSAVE".into(),
        "ROLE".into(),
        "ACL WHOAMI".into(),
        "ACL CAT".into(),
        "ACL USERS".into(),
        "MODULE LIST".into(),
        format!("SCRIPT EXISTS {sha}"),
        "FUNCTION LIST".into(),
        "FUNCTION STATS".into(),
        "PUBSUB CHANNELS".into(),
        "PING".into(),
        "ECHO hi".into(),
    ];
    for line in &lines {
        // Allowed through, and answered by the server without an error.
        ok(&console, line);
    }
    assert_eq!(ok(&console, &format!("GET {}", key("s"))), bulk("before"));
}

#[test]
fn console_refuses_writes() {
    let Some(t) = it!() else { return };
    let mut con = seed(&t);
    // A cuckoo filter with deletions, which CF.COMPACT would rewrite.
    let mut cf = redis::pipe();
    cf.cmd("CF.RESERVE")
        .arg(t.key("cf"))
        .arg(4)
        .arg("BUCKETSIZE")
        .arg(1)
        .arg("EXPANSION")
        .arg(1)
        .ignore();
    for i in 0..8 {
        cf.cmd("CF.ADD")
            .arg(t.key("cf"))
            .arg(format!("i{i}"))
            .ignore();
    }
    for i in 0..6 {
        cf.cmd("CF.DEL")
            .arg(t.key("cf"))
            .arg(format!("i{i}"))
            .ignore();
    }
    cf.query::<()>(&mut con).unwrap();
    let before = snapshot(&t, &mut con);
    let console = t.connect(read_only(0)).console();
    // `{}name` is the key `name` under the prefix.
    let lines = [
        ("SET {}s after", "SET"),
        ("set {}s after", "SET"),
        ("SETNX {}new 1", "SETNX"),
        ("SETEX {}s 10 x", "SETEX"),
        ("MSET {}s 1 {}new 2", "MSET"),
        ("APPEND {}s x", "APPEND"),
        ("SETRANGE {}s 0 x", "SETRANGE"),
        ("INCR {}n", "INCR"),
        ("INCRBYFLOAT {}n 1.5", "INCRBYFLOAT"),
        ("GETSET {}s x", "GETSET"),
        ("GETDEL {}s", "GETDEL"),
        ("GETEX {}s PERSIST", "GETEX"),
        ("SETBIT {}s 1 1", "SETBIT"),
        ("BITFIELD {}s SET u8 0 1", "BITFIELD"),
        ("DEL {}s", "DEL"),
        ("UNLINK {}s", "UNLINK"),
        ("EXPIRE {}s 1", "EXPIRE"),
        ("PEXPIREAT {}s 1", "PEXPIREAT"),
        ("PERSIST {}s", "PERSIST"),
        ("RENAME {}s {}renamed", "RENAME"),
        ("RENAMENX {}s {}renamed", "RENAMENX"),
        ("COPY {}s {}copy", "COPY"),
        ("MOVE {}s 3", "MOVE"),
        ("HSET {}h f x", "HSET"),
        ("HDEL {}h f", "HDEL"),
        ("HINCRBY {}h n 1", "HINCRBY"),
        ("HGETDEL {}h FIELDS 1 f", "HGETDEL"),
        ("HEXPIRE {}h 10 FIELDS 1 f", "HEXPIRE"),
        ("LPUSH {}l x", "LPUSH"),
        ("RPOP {}l", "RPOP"),
        ("LSET {}l 0 x", "LSET"),
        ("LTRIM {}l 1 0", "LTRIM"),
        ("LMOVE {}l {}l2 LEFT RIGHT", "LMOVE"),
        // Blocking writes are refused before they could block.
        ("BLPOP {}l 0", "BLPOP"),
        ("SADD {}st x", "SADD"),
        ("SPOP {}st", "SPOP"),
        ("SINTERSTORE {}dst {}st", "SINTERSTORE"),
        ("ZADD {}z 2 b", "ZADD"),
        ("ZPOPMIN {}z", "ZPOPMIN"),
        ("ZRANGESTORE {}dst {}z 0 -1", "ZRANGESTORE"),
        ("XADD {}x * f v", "XADD"),
        ("XDEL {}x 1-1", "XDEL"),
        ("XTRIM {}x MAXLEN 0", "XTRIM"),
        ("XGROUP CREATE {}x g $", "XGROUP CREATE"),
        ("xgroup create {}x g $", "XGROUP CREATE"),
        ("XREADGROUP GROUP g c STREAMS {}x >", "XREADGROUP"),
        ("PFADD {}hll b", "PFADD"),
        ("PFMERGE {}hll {}hll", "PFMERGE"),
        // Declared `readonly`, but they rewrite the value: PFCOUNT caches
        // the count in it (aborting other clients' WATCHes on it too),
        // CF.COMPACT moves the filter's entries.
        ("PFCOUNT {}hll", "PFCOUNT"),
        ("pfcount {}hll {}hll", "PFCOUNT"),
        ("CF.COMPACT {}cf", "CF.COMPACT"),
        ("GEOADD {}geo 13.4 52.5 berlin", "GEOADD"),
        ("SORT {}l ALPHA STORE {}dst", "SORT"),
        ("JSON.SET {}j $ 2", "JSON.SET"),
        ("JSON.DEL {}j", "JSON.DEL"),
        ("json.numincrby {}j $.a 1", "JSON.NUMINCRBY"),
        ("BF.ADD {}b z", "BF.ADD"),
        ("TS.ADD {}ts 1 1", "TS.ADD"),
        ("VADD {}v VALUES 2 1 2 a", "VADD"),
        ("PUBLISH {}ch hi", "PUBLISH"),
        ("SPUBLISH {}ch hi", "SPUBLISH"),
        ("MIGRATE 127.0.0.1 1 {}s 0 100", "MIGRATE"),
        ("RESTORE {}restored 0 x", "RESTORE"),
    ];
    for (line, name) in lines {
        refused(&t, &console, line, name);
    }
    assert_eq!(
        refused(&t, &console, "SET {}s x", "SET").to_string(),
        "SET can modify data, and this connection is read-only"
    );
    assert_eq!(snapshot(&t, &mut con), before);
    // Still a working console.
    assert_eq!(ok(&console, &format!("GET {}", t.key("s"))), bulk("before"));
}

/// Owns `SCRATCH_DB`, an empty database the console works in, so that a
/// command the gate wrongly let through could not touch other tests' keys.
#[test]
fn console_refuses_admin_commands() {
    let Some(t) = it!() else { return };
    let console = t.connect(read_only(SCRATCH_DB)).console();
    let admin = " is an administrative command, ";
    let lines = [
        ("CONFIG SET maxmemory-samples 5", "CONFIG SET", admin),
        ("config set maxmemory-samples 5", "CONFIG SET", admin),
        ("CONFIG REWRITE", "CONFIG REWRITE", admin),
        ("CONFIG RESETSTAT", "CONFIG RESETSTAT", admin),
        ("CLIENT KILL ID 999999999", "CLIENT KILL", admin),
        ("CLIENT PAUSE 1 WRITE", "CLIENT PAUSE", admin),
        ("CLIENT UNPAUSE", "CLIENT UNPAUSE", admin),
        ("FLUSHDB", "FLUSHDB", admin),
        ("FLUSHDB ASYNC", "FLUSHDB", admin),
        ("SWAPDB 12 12", "SWAPDB", admin),
        ("SLOWLOG RESET", "SLOWLOG RESET", admin),
        ("LATENCY RESET", "LATENCY RESET", admin),
        ("MEMORY PURGE", "MEMORY PURGE", admin),
        ("ACL DELUSER it-nobody", "ACL DELUSER", admin),
        ("BGSAVE", "BGSAVE", admin),
        ("BGREWRITEAOF", "BGREWRITEAOF", admin),
        ("MODULE LOAD /no/such/module.so", "MODULE LOAD", admin),
        ("SCRIPT KILL", "SCRIPT KILL", admin),
        ("FUNCTION KILL", "FUNCTION KILL", admin),
        ("DEBUG SLEEP 0", "DEBUG", admin),
        // Declared `readonly`, but they free other clients' cursors.
        ("FT.CURSOR DEL it-no-index 1", "FT.CURSOR DEL", admin),
        ("FT.CURSOR GC it-no-index 0", "FT.CURSOR GC", admin),
        // Redis files it under @write rather than @admin.
        (
            "FUNCTION DELETE it_no_such_library",
            "FUNCTION DELETE",
            " can modify data, ",
        ),
    ];
    for (line, name, what) in lines {
        let err = refused(&t, &console, line, name);
        assert!(err.to_string().contains(what), "{line}: {err}");
    }
    // The database is as empty as it was.
    assert_eq!(ok(&console, "DBSIZE"), RawValue::Int(0));
}

#[test]
fn console_refuses_unknown_commands() {
    let Some(t) = it!() else { return };
    let console = t.connect(read_only(0)).console();
    for (line, name) in [
        ("NOSUCHCOMMAND", "NOSUCHCOMMAND"),
        ("nosuch.command x", "NOSUCH.COMMAND"),
        ("CONFIG BOGUS x", "CONFIG BOGUS"),
        ("client frobnicate", "CLIENT FROBNICATE"),
    ] {
        let err = refused(&t, &console, line, name);
        assert_eq!(
            err.to_string(),
            format!("{name} is not a command known to be safe, and this connection is read-only")
        );
    }
    // A writable console lets the server answer for them.
    let writable = t.connect(standalone()).console();
    let err = run(&writable, "NOSUCHCOMMAND").unwrap_err();
    assert!(!is_read_only(&err));
    assert!(err.to_string().starts_with("ERR unknown command"), "{err}");
}

/// A function library of the test's own, removed on drop.
struct Library {
    name: String,
}

impl Library {
    /// Loads `get` (flagged no-writes) and `set` functions.
    fn load(t: &It) -> Self {
        let name: String = t
            .prefix()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let code = format!(
            "#!lua name={name}\n\
             redis.register_function{{function_name='{name}get', callback=function(keys) return redis.call('GET', keys[1]) end, flags={{'no-writes'}}}}\n\
             redis.register_function('{name}set', function(keys, args) return redis.call('SET', keys[1], args[1]) end)"
        );
        redis::cmd("FUNCTION")
            .arg("LOAD")
            .arg(code)
            .query::<()>(&mut raw(Server::Standalone, 0))
            .unwrap();
        Self { name }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = redis::cmd("FUNCTION")
            .arg("DELETE")
            .arg(&self.name)
            .query::<()>(&mut raw(Server::Standalone, 0));
    }
}

#[test]
fn console_scripts_and_functions() {
    let Some(t) = it!() else { return };
    let mut con = seed(&t);
    let library = Library::load(&t);
    let get = format!("{}get", library.name);
    let set = format!("{}set", library.name);
    let read = "return redis.call('GET', KEYS[1])";
    let write = "return redis.call('SET', KEYS[1], 'after')";
    let sha: String = redis::cmd("SCRIPT")
        .arg("LOAD")
        .arg(read)
        .query(&mut con)
        .unwrap();
    let before = snapshot(&t, &mut con);
    let console = t.connect(read_only(0)).console();
    let s = t.key("s");
    let s = s.as_str();

    // The read-only variants are refused too: the server stops writes in
    // them, but not administrative commands, which would get around the
    // gate in one line.
    for (argv, name) in [
        (vec!["EVAL_RO", read, "1", s], "EVAL_RO"),
        (
            vec!["EVAL_RO", "return redis.call('SLOWLOG', 'RESET')", "0"],
            "EVAL_RO",
        ),
        (
            vec!["eval_ro", "return redis.call('MEMORY', 'PURGE')", "0"],
            "EVAL_RO",
        ),
        (vec!["EVALSHA_RO", &sha, "1", s], "EVALSHA_RO"),
        (vec!["FCALL_RO", &get, "1", s], "FCALL_RO"),
    ] {
        let err = exec(&console, &argv).expect_err(name);
        let read_only = err.downcast_ref::<ReadOnlyError>().expect(name);
        assert_eq!(read_only.command(), Some(name));
        assert_eq!(
            err.to_string(),
            format!(
                "{name} runs a script, which can still run administrative commands, and this connection is read-only"
            )
        );
    }
    // A writable console runs them, and the server still stops a write
    // inside one.
    let writable = t.connect(standalone()).console();
    assert_eq!(
        exec(&writable, &["EVAL_RO", read, "1", s]).unwrap(),
        bulk("before")
    );
    assert_eq!(
        exec(&writable, &["EVALSHA_RO", &sha, "1", s]).unwrap(),
        bulk("before")
    );
    assert_eq!(
        exec(&writable, &["FCALL_RO", &get, "1", s]).unwrap(),
        bulk("before")
    );
    let err = exec(&writable, &["EVAL_RO", write, "1", s]).unwrap_err();
    assert!(
        err.to_string().contains("Write commands are not allowed"),
        "{err}"
    );

    // The others may write, whatever the script does.
    for (argv, name) in [
        (vec!["EVAL", read, "1", s], "EVAL"),
        (vec!["eval", write, "1", s], "EVAL"),
        (vec!["EVALSHA", &sha, "1", s], "EVALSHA"),
        (vec!["FCALL", &get, "1", s], "FCALL"),
        (vec!["FCALL", &set, "1", s, "x"], "FCALL"),
        (vec!["SCRIPT", "LOAD", write], "SCRIPT LOAD"),
        (vec!["FUNCTION", "LOAD", "#!lua name=x"], "FUNCTION LOAD"),
    ] {
        let err = exec(&console, &argv).expect_err(name);
        let read_only = err.downcast_ref::<ReadOnlyError>().expect(name);
        assert_eq!(read_only.command(), Some(name));
    }
    assert_eq!(snapshot(&t, &mut con), before);
}

#[test]
fn console_transactions_check_each_queued_command() {
    let Some(t) = it!() else { return };
    let mut con = seed(&t);
    let before = snapshot(&t, &mut con);
    let console = t.connect(read_only(0)).console();
    let s = t.key("s");

    // A write queued inside MULTI is refused when typed, and EXEC then runs
    // nothing, as after a queuing error on the server.
    for (line, refusal) in [
        (format!("SET {s} after"), "read-only"),
        (format!("INCR {s}"), "read-only"),
        ("FLUSHDB".to_string(), "read-only"),
        ("SUBSCRIBE ch".to_string(), "not available in the console"),
    ] {
        assert_eq!(ok(&console, "MULTI"), RawValue::Okay);
        assert_eq!(ok(&console, &format!("GET {s}")), queued());
        let err = run(&console, &line).unwrap_err();
        assert!(err.to_string().contains(refusal), "{line}: {err}");
        assert!(console.in_transaction());
        assert_eq!(ok(&console, &format!("HGET {} f", t.key("h"))), queued());
        let err = run(&console, "EXEC").unwrap_err();
        assert!(err.to_string().starts_with("EXECABORT"), "{err}");
        assert!(!console.in_transaction());
    }
    assert_eq!(snapshot(&t, &mut con), before);

    // Reads alone go through, and the server-side transaction state is
    // clean after the aborts above.
    ok(&console, "MULTI");
    ok(&console, &format!("GET {s}"));
    ok(&console, &format!("HGET {} f", t.key("h")));
    assert_eq!(
        ok(&console, "EXEC"),
        RawValue::Array(vec![bulk("before"), bulk("v")])
    );
    ok(&console, "MULTI");
    ok(&console, &format!("GET {s}"));
    assert_eq!(ok(&console, "DISCARD"), RawValue::Okay);
    assert!(!console.in_transaction());

    // WATCH works as usual: another client's write aborts the EXEC.
    ok(&console, &format!("WATCH {s}"));
    ok(&console, "MULTI");
    ok(&console, &format!("GET {s}"));
    redis::cmd("SET")
        .arg(&s)
        .arg("changed")
        .query::<()>(&mut con)
        .unwrap();
    assert_eq!(ok(&console, "EXEC"), RawValue::Nil);
    ok(&console, &format!("WATCH {s}"));
    assert_eq!(ok(&console, "UNWATCH"), RawValue::Okay);

    // Connection state outside the transaction commands stays refused.
    for (line, name) in [("CLIENT SETNAME x", "CLIENT SETNAME"), ("AUTH a b", "AUTH")] {
        let err = refused(&t, &console, line, name);
        assert!(
            err.to_string()
                .contains("changes the state of the connection")
        );
    }
}

#[test]
fn the_console_refuses_streaming_and_session_commands_everywhere() {
    let Some(t) = it!() else { return };
    let writable = t.connect(standalone());
    let locked = t.connect(read_only(0));
    for console in [writable.console(), locked.console()] {
        for line in [
            "SUBSCRIBE ch",
            "subscribe ch",
            "PSUBSCRIBE *",
            "SSUBSCRIBE ch",
            "UNSUBSCRIBE",
            "PUNSUBSCRIBE",
            "SUNSUBSCRIBE",
            "MONITOR",
            "monitor",
            "SYNC",
            "PSYNC ? -1",
            "REPLCONF listening-port 1",
            "RESET",
            "QUIT",
            "HELLO 3",
            "HELLO",
            "CLIENT REPLY OFF",
            "client reply skip",
            "SCRIPT DEBUG YES",
        ] {
            let err = run(&console, line).expect_err(line);
            assert!(!is_read_only(&err), "{line}: {err}");
            assert!(
                err.to_string()
                    .contains(" is not available in the console: "),
                "{line}: {err}"
            );
        }
        // The session is as it was: no subscription, no MONITOR, RESP2.
        let info = client_info(&console);
        assert_eq!(field(&info, "sub"), Some("0"), "{info}");
        assert_eq!(field(&info, "psub"), Some("0"), "{info}");
        assert_eq!(field(&info, "flags"), Some("N"), "{info}");
        assert_eq!(field(&info, "resp"), Some("2"), "{info}");
        assert_eq!(ok(&console, "PING"), RawValue::SimpleString("PONG".into()));
    }
}
