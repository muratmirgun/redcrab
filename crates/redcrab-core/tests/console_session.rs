//! The console's own session: isolation from the app's connection (SELECT,
//! MULTI, WATCH), ordering, errors, reconnecting, cancelling and closing.
//! The read-only gate and the commands the console refuses are in
//! `read_only.rs`.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400): database 0 and `CONSOLE_DB`, each test only with
//! keys under its own prefix.

#[macro_use]
mod common;

use std::time::{Duration, Instant};

use common::*;
use redcrab_core::{
    ConnectionConfig, Cursor, KeyType, RawValue as Value, ScanOptions, ValueData, format, viewer,
};

/// `standalone`, with `t` cleaning up `CONSOLE_DB` too.
fn console_config(t: &It) -> ConnectionConfig {
    t.uses(Server::Standalone, CONSOLE_DB);
    standalone()
}

fn get(con: &mut redis::Connection, key: &str) -> Option<String> {
    redis::cmd("GET").arg(key).query(con).unwrap()
}

#[test]
fn select_in_the_console_stays_in_the_console() {
    let Some(t) = it!() else { return };
    let key = t.key("k");
    let mut db0 = t.seed0();
    redis::cmd("SET")
        .arg(&key)
        .arg("zero")
        .query::<()>(&mut db0)
        .unwrap();
    let client = t.connect(console_config(&t));

    let console = client.console();
    assert_eq!(console.db(), 0);
    assert_eq!(ok(&console, &format!("SELECT {CONSOLE_DB}")), Value::Okay);
    assert_eq!(console.db(), i64::from(CONSOLE_DB));
    assert_eq!(ok(&console, &format!("GET {key}")), Value::Nil);
    ok(&console, &format!("SET {key} five"));
    ok(&console, &format!("SET {} only-in-five", t.key("five")));

    // Every request of the app still goes to database 0.
    let loaded = block_on(viewer::load(&client, t.bkey("k"))).unwrap();
    assert!(matches!(&loaded.data, ValueData::String(v) if v == b"zero"));
    let meta = block_on(client.key_meta(vec![t.bkey("k"), t.bkey("five")])).unwrap();
    assert_eq!(
        (&meta[0].ty, &meta[1].ty),
        (&KeyType::String, &KeyType::None)
    );
    let found = scan(
        &client,
        ScanOptions {
            pattern: t.pattern(),
            ..ScanOptions::default()
        },
    )
    .unwrap();
    assert_eq!(key_set(&found), key_set([&t.bkey("k")]));
    block_on(client.set_string(t.bkey("k"), b"zero again".to_vec())).unwrap();
    assert_eq!(get(&mut db0, &key).as_deref(), Some("zero again"));
    assert_eq!(block_on(client.delete(vec![t.bkey("five")])).unwrap(), 0);
    block_on(client.server_info()).unwrap();

    // The console keeps its database, and another console starts fresh.
    assert_eq!(ok(&console, &format!("GET {key}")), bulk("five"));
    assert_eq!(
        ok(&console, &format!("EXISTS {}", t.key("five"))),
        Value::Int(1)
    );
    let other = client.console();
    assert_eq!(other.db(), 0);
    assert_eq!(ok(&other, &format!("GET {key}")), bulk("zero again"));
}

#[test]
fn a_console_starts_on_the_configured_database() {
    let Some(t) = it!() else { return };
    let client = t.connect(ConnectionConfig {
        db: CONSOLE_DB,
        ..console_config(&t)
    });
    let console = client.console();
    assert_eq!(console.db(), i64::from(CONSOLE_DB));
    ok(&console, &format!("SET {} v", t.key("k")));
    let meta = block_on(client.key_meta(vec![t.bkey("k")])).unwrap();
    assert_eq!(meta[0].ty, KeyType::String);
    assert_eq!(ok(&console, "SELECT 0"), Value::Okay);
    assert_eq!(console.db(), 0);
    assert_eq!(ok(&console, &format!("GET {}", t.key("k"))), Value::Nil);
}

#[test]
fn transactions_in_the_console_do_not_hold_up_the_app() {
    let Some(t) = it!() else { return };
    let key = t.key("k");
    let mut con = t.seed0();
    let client = t.connect(standalone());
    let console = client.console();

    // The app's writes are not queued in the console's MULTI.
    ok(&console, "MULTI");
    assert_eq!(ok(&console, &format!("SET {key} from-console")), queued());
    block_on(client.set_string(t.bkey("k"), b"from-app".to_vec())).unwrap();
    assert_eq!(get(&mut con, &key).as_deref(), Some("from-app"));
    assert!(console.in_transaction());
    assert_eq!(ok(&console, "EXEC"), Value::Array(vec![Value::Okay]));
    assert_eq!(get(&mut con, &key).as_deref(), Some("from-console"));

    // A WATCH in the console sees the app's write as any other client's.
    ok(&console, &format!("WATCH {key}"));
    ok(&console, "MULTI");
    ok(&console, &format!("SET {key} lost"));
    block_on(client.set_string(t.bkey("k"), b"app wins".to_vec())).unwrap();
    assert_eq!(ok(&console, "EXEC"), Value::Nil);
    assert_eq!(get(&mut con, &key).as_deref(), Some("app wins"));
}

#[test]
fn queued_selects_take_effect_with_exec() {
    let Some(t) = it!() else { return };
    let key = t.key("k");
    let client = t.connect(console_config(&t));
    let console = client.console();

    assert_eq!(ok(&console, "MULTI"), Value::Okay);
    assert!(console.in_transaction());
    assert_eq!(ok(&console, &format!("SELECT {CONSOLE_DB}")), queued());
    assert_eq!(console.db(), 0, "a queued SELECT has not run yet");
    ok(&console, &format!("SET {key} v"));
    assert_eq!(
        ok(&console, "EXEC"),
        Value::Array(vec![Value::Okay, Value::Okay])
    );
    assert!(!console.in_transaction());
    assert_eq!(console.db(), i64::from(CONSOLE_DB));
    assert_eq!(ok(&console, &format!("GET {key}")), bulk("v"));

    // DISCARD drops the queued SELECT.
    ok(&console, "MULTI");
    ok(&console, "SELECT 0");
    assert_eq!(ok(&console, "DISCARD"), Value::Okay);
    assert!(!console.in_transaction());
    assert_eq!(console.db(), i64::from(CONSOLE_DB));

    // A failed WATCH makes EXEC return nil; the SELECT does not happen.
    let other = client.console();
    ok(&other, &format!("SELECT {CONSOLE_DB}"));
    ok(&console, &format!("WATCH {key}"));
    ok(&console, "MULTI");
    ok(&console, "SELECT 0");
    ok(&other, &format!("SET {key} changed"));
    assert_eq!(ok(&console, "EXEC"), Value::Nil);
    assert_eq!(console.db(), i64::from(CONSOLE_DB));

    // A SELECT the server rejects inside MULTI does not move the console.
    ok(&console, "MULTI");
    ok(&console, "SELECT 99");
    let reply = ok(&console, "EXEC");
    assert!(
        matches!(&reply, Value::Array(r) if matches!(r[..], [Value::ServerError(_)])),
        "{reply:?}"
    );
    assert_eq!(console.db(), i64::from(CONSOLE_DB));
    let info = client_info(&console);
    assert_eq!(field(&info, "db"), Some(&*CONSOLE_DB.to_string()), "{info}");
}

#[test]
fn exec_answers_for_each_command_and_follows_the_selects_that_ran() {
    let Some(t) = it!() else { return };
    let client = t.connect(console_config(&t));
    let console = client.console();
    let text = t.key("text");
    ok(&console, &format!("SET {text} abc"));

    // One command fails when it runs; the others run, as in redis-cli.
    ok(&console, "MULTI");
    ok(&console, &format!("INCR {text}"));
    ok(&console, &format!("SELECT {CONSOLE_DB}"));
    ok(&console, &format!("SET {} in-five", t.key("k")));
    let reply = ok(&console, "EXEC");
    let Value::Array(replies) = &reply else {
        panic!("{reply:?}");
    };
    assert_eq!(replies.len(), 3, "{reply:?}");
    assert!(
        matches!(&replies[0], Value::ServerError(err) if err.code() == "ERR"),
        "{reply:?}"
    );
    assert_eq!((&replies[1], &replies[2]), (&Value::Okay, &Value::Okay));
    assert_eq!(
        redcrab_core::format::reply(&reply),
        "1) (error) ERR value is not an integer or out of range\n2) OK\n3) OK"
    );

    // The SELECT ran, so the console is on that database now.
    assert_eq!(console.db(), i64::from(CONSOLE_DB));
    let info = client_info(&console);
    assert_eq!(field(&info, "db"), Some(&*CONSOLE_DB.to_string()), "{info}");
    assert_eq!(
        ok(&console, &format!("GET {}", t.key("k"))),
        bulk("in-five")
    );
}

#[test]
fn server_errors_read_like_redis_cli() {
    let Some(t) = it!() else { return };
    let client = t.connect(console_config(&t));
    let console = client.console();
    let err = run(&console, "NOSUCHCOMMAND x").unwrap_err().to_string();
    assert!(err.starts_with("ERR unknown command"), "{err}");
    let err = run(&console, "GET").unwrap_err().to_string();
    assert!(err.starts_with("ERR wrong number of arguments"), "{err}");
    ok(&console, &format!("LPUSH {} x", t.key("list")));
    let err = run(&console, &format!("GET {}", t.key("list")))
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("WRONGTYPE "), "{err}");
    let err = run(&console, "SELECT 99").unwrap_err().to_string();
    assert!(err.starts_with("ERR DB index is out of range"), "{err}");
    // Errors cost neither the connection nor its state.
    ok(&console, &format!("SELECT {CONSOLE_DB}"));
    let _ = run(&console, "NOSUCHCOMMAND");
    let info = client_info(&console);
    assert_eq!(field(&info, "db"), Some(&*CONSOLE_DB.to_string()), "{info}");
    assert_eq!(console.db(), i64::from(CONSOLE_DB));
}

#[test]
fn replies_keep_their_types() {
    let Some(t) = it!() else { return };
    let console = t.connect(standalone()).console();
    let h = t.key("h");
    ok(&console, &format!("HSET {h} f v"));
    assert_eq!(ok(&console, &format!("INCR {}", t.key("n"))), Value::Int(1));
    assert_eq!(
        ok(&console, &format!("GET {}", t.key("missing"))),
        Value::Nil
    );
    assert_eq!(
        ok(&console, &format!("HGETALL {h}")),
        Value::Array(vec![bulk("f"), bulk("v")])
    );
    assert_eq!(ok(&console, "PING"), Value::SimpleString("PONG".into()));
    // Arguments are sent as typed, in UTF-8.
    ok(&console, &format!("SET {} ключ🦀", t.key("u")));
    assert_eq!(ok(&console, &format!("GET {}", t.key("u"))), bulk("ключ🦀"));
    let spaced = vec!["SET".into(), t.key("spaced"), "a b  c".into()];
    assert_eq!(block_on(console.exec(spaced)).unwrap(), Value::Okay);
    assert_eq!(
        ok(&console, &format!("GET {}", t.key("spaced"))),
        bulk("a b  c")
    );
    let err = block_on(console.exec(Vec::<Vec<u8>>::new())).unwrap_err();
    assert_eq!(err.to_string(), "empty command");
}

#[test]
fn commands_run_in_submission_order() {
    let Some(t) = it!() else { return };
    let key = t.key("n");
    let console = t.connect(standalone()).console();
    let pending: Vec<_> = (0..200)
        .map(|_| console.exec(args(&format!("INCR {key}"))))
        .collect();
    let replies: Vec<Value> = pending.into_iter().map(|p| block_on(p).unwrap()).collect();
    let expected: Vec<Value> = (1..=200).map(Value::Int).collect();
    assert_eq!(replies, expected);
}

#[test]
fn reconnects_on_the_selected_database() {
    let Some(t) = it!() else { return };
    let client = t.connect(console_config(&t));
    let console = client.console();
    ok(&console, &format!("SELECT {CONSOLE_DB}"));
    let Value::Int(id) = ok(&console, "CLIENT ID") else {
        panic!("CLIENT ID");
    };
    // Kill the console's connection from another one.
    assert_eq!(
        ok(&client.console(), &format!("CLIENT KILL ID {id}")),
        Value::Int(1)
    );

    // The first command may still see the dead connection; the session
    // reconnects for the next one at the latest.
    let mut info = None;
    for _ in 0..3 {
        if let Ok(Value::BulkString(reply)) = run(&console, "CLIENT INFO") {
            info = Some(String::from_utf8_lossy(&reply).into_owned());
            break;
        }
    }
    let info = info.expect("the console did not reconnect");
    assert_eq!(field(&info, "db"), Some(&*CONSOLE_DB.to_string()), "{info}");
    assert_ne!(field(&info, "id"), Some(&*id.to_string()), "{info}");
    assert_eq!(console.db(), i64::from(CONSOLE_DB));
}

#[test]
fn blocking_commands_wait_for_their_own_timeout() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let console = client.console();
    let queue = t.key("queue");
    let started = Instant::now();
    assert_eq!(ok(&console, &format!("BLPOP {queue} 0.3")), Value::Nil);
    assert!(started.elapsed() >= Duration::from_millis(300));

    // Woken by another client's push.
    let blocked = console.exec(args(&format!("BLPOP {queue} 10")));
    std::thread::sleep(Duration::from_millis(100));
    redis::cmd("RPUSH")
        .arg(&queue)
        .arg("job")
        .query::<()>(&mut t.seed0())
        .unwrap();
    assert_eq!(
        block_on(blocked).unwrap(),
        Value::Array(vec![bulk(&queue), bulk("job")])
    );
}

#[test]
fn cancel_abandons_a_blocking_command() {
    let Some(t) = it!() else { return };
    let console = t.connect(console_config(&t)).console();
    ok(&console, &format!("SELECT {CONSOLE_DB}"));
    let blocked = console.exec(args(&format!("BLPOP {} 0", t.key("queue"))));
    // Cancel from another thread while the command blocks.
    let err = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(300));
            console.cancel();
        });
        block_on(blocked)
    })
    .expect_err("BLPOP 0 returned");
    assert!(err.to_string().starts_with("cancelled"), "{err}");
    // The next command runs on a new connection, on the same database.
    assert_eq!(ok(&console, "PING"), Value::SimpleString("PONG".into()));
    assert_eq!(console.db(), i64::from(CONSOLE_DB));
    let info = client_info(&console);
    assert_eq!(field(&info, "db"), Some(&*CONSOLE_DB.to_string()), "{info}");
}

#[test]
fn cancel_also_drops_the_commands_waiting_behind() {
    let Some(t) = it!() else { return };
    let console = t.connect(standalone()).console();
    let blocked = console.exec(args(&format!("BLPOP {} 0", t.key("queue"))));
    let waiting = console.exec(args(&format!("SET {} x", t.key("never"))));
    // Right away: BLPOP may not even have been sent yet.
    console.cancel();
    for result in [block_on(blocked), block_on(waiting)] {
        let err = result.unwrap_err().to_string();
        assert!(err.starts_with("cancelled"), "{err}");
    }
    assert_eq!(get(&mut t.seed0(), &t.key("never")), None);
    // What comes after the cancel runs.
    assert_eq!(ok(&console, "PING"), Value::SimpleString("PONG".into()));
}

/// Keys that are not text can be typed the way the app shows them
/// (`\xff`), and what the app copies pastes back as the same key.
#[test]
fn binary_keys_can_be_addressed() {
    let Some(t) = it!() else { return };
    let console = t.connect(standalone()).console();
    let key = t.bkey(b"bin:\x00\x01\xff\xfe \"q\"");
    let mut con = t.seed0();
    redis::cmd("SET")
        .arg(&*key)
        .arg(b"\xff\x00v")
        .query::<()>(&mut con)
        .unwrap();
    let line = |line: String| {
        let argv = format::split_args(&line).unwrap();
        block_on(console.exec(argv)).unwrap()
    };
    assert_eq!(
        line(format!("GET {}", format::quote_arg(&key))),
        bulk(b"\xff\x00v")
    );
    // As the key list shows it, inside double quotes (escaping the quotes).
    let shown = format::bytes_inline(&key).replace('"', "\\\"");
    assert_eq!(
        line(format!("TYPE \"{shown}\"")),
        Value::SimpleString("string".into())
    );
    // What "Copy key name" puts on the clipboard.
    assert_eq!(
        line(format!("DEL {}", format::copyable(&key))),
        Value::Int(1)
    );
    let exists: i64 = redis::cmd("EXISTS").arg(&*key).query(&mut con).unwrap();
    assert_eq!(exists, 0);
}

#[test]
fn closing_the_console_closes_its_connection() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let console = client.console();
    let Value::Int(id) = ok(&console, "CLIENT ID") else {
        panic!("CLIENT ID");
    };
    let mut admin = t.seed0();
    let mut listed = || -> bool {
        let list: String = redis::cmd("CLIENT")
            .arg("LIST")
            .arg("ID")
            .arg(id)
            .query(&mut admin)
            .unwrap();
        !list.trim().is_empty()
    };
    assert!(listed());
    drop(console);
    let deadline = Instant::now() + Duration::from_secs(5);
    while listed() {
        assert!(Instant::now() < deadline, "connection {id} is still open");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The app's own connection is not affected.
    block_on(client.ping()).unwrap();
    let page = block_on(client.load(t.bkey("x"), KeyType::String, Cursor::Start, 1)).unwrap();
    assert_eq!(page.total, 0);
}
