//! The console prompt against real servers: the history of commands that
//! really ran (never a secret, even one the server took), and the command
//! table against the server it was generated from (completion and hints
//! know every command; command lines the server runs fit their usage).
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses `auth`
//! (6401) and `standalone` (6400), database 0, each test only with keys
//! under its own prefix. History files go to a temporary directory.

#[macro_use]
mod common;

use std::{fs, path::PathBuf};

use common::*;
use redcrab_core::{
    RawValue as Value,
    console::{self, CandidateKind, History, HistoryFile, Prompt},
    format,
};

/// A directory of the test's own, removed when it ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(t: &It) -> Self {
        let name = t.prefix().replace(':', "-");
        let dir = std::env::temp_dir().join(format!("redcrab-{name}"));
        let _ = fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `line` as the GUI does: into the prompt (and, when kept, the
/// history file), then on the console session.
fn submit(
    prompt: &mut Prompt,
    file: &HistoryFile,
    console: &redcrab_core::ConsoleSession,
    line: &str,
) -> anyhow::Result<Value> {
    if prompt.submit(line) {
        file.append_in_background(line.to_string());
    }
    let args = format::split_args(line).map_err(anyhow::Error::msg)?;
    block_on(console.exec(args))
}

#[test]
fn the_history_never_keeps_credentials_that_ran() {
    let Some(t) = it!() else { return };
    let client = t.connect(Server::Auth.admin());
    let console = client.console();
    let dir = TempDir::new(&t);
    let file = HistoryFile::at(dir.0.join("auth.history"));
    let mut prompt = Prompt::new(History::new());

    let set = format!("SET {} v", t.key("k"));
    let get = format!("GET {}", t.key("k"));
    let run = |prompt: &mut Prompt, line: &str| submit(prompt, &file, &console, line);
    run(&mut prompt, &set).unwrap();
    // Each of these really authenticates the console's connection.
    assert_eq!(run(&mut prompt, "AUTH rootpass").unwrap(), Value::Okay);
    assert_eq!(run(&mut prompt, "auth app apppass").unwrap(), Value::Okay);
    assert_eq!(
        run(&mut prompt, "ACL WHOAMI").unwrap(),
        bulk("app"),
        "the console switched users"
    );
    assert_eq!(
        run(&mut prompt, "AUTH default rootpass").unwrap(),
        Value::Okay
    );
    // A wrong one is an error from the server, and kept out all the same.
    assert!(run(&mut prompt, "AUTH default wrong-s3cret").is_err());
    // The console refuses HELLO; it is never kept either.
    assert!(run(&mut prompt, "HELLO 3 AUTH default rootpass").is_err());
    // Reading the password back is no better.
    run(&mut prompt, "CONFIG GET requirepass").unwrap();
    assert_eq!(run(&mut prompt, &get).unwrap(), bulk("v"));
    // Erring on the safe side: a key merely named like a credential keeps
    // its line out too.
    run(&mut prompt, &format!("SET {} x", t.key("api-secret"))).unwrap();
    run(&mut prompt, "ACL WHOAMI").unwrap();
    HistoryFile::flush_background();

    let kept = [set.as_str(), "ACL WHOAMI", get.as_str(), "ACL WHOAMI"];
    assert_eq!(prompt.history().iter().collect::<Vec<_>>(), kept);
    let text = fs::read_to_string(file.path()).unwrap();
    for secret in ["rootpass", "apppass", "s3cret", "requirepass"] {
        assert!(!text.contains(secret), "{secret} in {text:?}");
    }
    let loaded = file.load().unwrap();
    assert_eq!(loaded.iter().collect::<Vec<_>>(), kept);

    // A console opened later browses what this one ran, newest first.
    let mut later = Prompt::new(loaded);
    assert_eq!(later.up("").as_deref(), Some("ACL WHOAMI"));
    assert_eq!(later.up("").as_deref(), Some(get.as_str()));
}

#[test]
fn the_history_never_keeps_credentials_in_scripts() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let console = client.console();
    let dir = TempDir::new(&t);
    let file = HistoryFile::at(dir.0.join("standalone.history"));
    let mut prompt = Prompt::new(History::new());
    let run = |prompt: &mut Prompt, line: &str| submit(prompt, &file, &console, line);

    let key = t.key("k");
    let set = format!("SET {key} v");
    run(&mut prompt, &set).unwrap();
    // The script has the server log in to another one (the `auth` server,
    // by its name on the test environment's network): the password really
    // goes out, and the other server refuses it.
    let migrate = format!(
        "EVAL \"return redis.call('MIGRATE','redcrab-it-auth','6401',KEYS[1],'0','2000','COPY','AUTH','wrong-s3cret')\" 1 {key}"
    );
    let err = format!("{:#}", run(&mut prompt, &migrate).unwrap_err());
    assert!(
        err.contains("WRONGPASS"),
        "the target got no password: {err}"
    );
    let migrate2 = format!(
        "EVAL \"return redis.call('MIGRATE',ARGV[1],ARGV[2],KEYS[1],'0','2000','COPY','AUTH2','app',ARGV[3])\" 1 {key} redcrab-it-auth 6401 wrong-s3cret"
    );
    let err = format!("{:#}", run(&mut prompt, &migrate2).unwrap_err());
    assert!(
        err.contains("WRONGPASS"),
        "the target got no password: {err}"
    );
    // Scripts may not log in or change users themselves; the lines carry
    // the secret all the same.
    assert!(run(&mut prompt, "EVAL \"return redis.call('AUTH','s3cret')\" 0").is_err());
    assert!(
        run(
            &mut prompt,
            "EVAL_RO \"return redis.call('ACL','SETUSER','bob','>s3cret')\" 0"
        )
        .is_err()
    );
    // Loaded for later, with the secret in the script cache.
    run(
        &mut prompt,
        "SCRIPT LOAD \"return redis.call('MIGRATE','h','1','k','0','1','AUTH','s3cret')\"",
    )
    .unwrap();
    // A script without credentials is kept.
    let get = format!("EVAL \"return redis.call('GET', KEYS[1])\" 1 {key}");
    assert_eq!(run(&mut prompt, &get).unwrap(), bulk("v"));
    HistoryFile::flush_background();

    let kept = [set.as_str(), get.as_str()];
    assert_eq!(prompt.history().iter().collect::<Vec<_>>(), kept);
    let text = fs::read_to_string(file.path()).unwrap();
    assert!(!text.contains("s3cret"), "{text:?}");
    assert_eq!(file.load().unwrap().iter().collect::<Vec<_>>(), kept);
}

#[test]
fn every_command_of_the_server_completes_and_hints() {
    let Some(_t) = it!() else { return };
    let mut con = raw(Server::Standalone, 0);
    let names: Vec<String> = redis::cmd("COMMAND").arg("LIST").query(&mut con).unwrap();
    let info: String = redis::cmd("INFO").arg("server").query(&mut con).unwrap();
    let version = info
        .lines()
        .find_map(|line| line.strip_prefix("redis_version:"))
        .unwrap()
        .trim()
        .to_string();
    // The table says which server it was generated from.
    let table = include_str!("../src/commands/table.rs");
    let same_server = table
        .lines()
        .next()
        .is_some_and(|header| header.contains(&format!("from Redis {version} ")));

    let mut missing = Vec::new();
    for name in &names {
        let name = name.replace('|', " ");
        // Module internals (`_FT.CURSOR READ`) are not for typing.
        if name.starts_with('_') {
            continue;
        }
        let Some(spec) =
            redcrab_core::commands::lookup(name.split(' ').next().unwrap(), name.split(' ').nth(1))
        else {
            missing.push(name);
            continue;
        };
        if spec.flags.contains(redcrab_core::commands::Flags::SYSCMD) {
            continue;
        }
        // Typed in full, the command is the first candidate, in the case
        // typed.
        let typed = name.to_lowercase();
        let completion = console::complete(&typed, typed.len());
        let first = &completion.candidates[0];
        assert_eq!(first.name, spec.name, "{typed}");
        assert_eq!(
            first.text,
            typed.rsplit(' ').next().unwrap(),
            "{typed}: {first:?}"
        );
        let kind = if name.contains(' ') {
            CandidateKind::Subcommand
        } else {
            CandidateKind::Command
        };
        assert_eq!(first.kind, kind, "{typed}");
        // Its name alone fits, and the hint is the rest of its usage.
        assert_eq!(console::fits(&typed), Some(true), "{typed}");
        let usage = spec.syntax[spec.name.len()..].trim();
        let usage = usage.split_whitespace().collect::<Vec<_>>().join(" ");
        let expected = (!usage.is_empty()).then(|| format!(" {usage}"));
        assert_eq!(console::hint(&typed), expected, "{typed}");
    }
    if same_server {
        assert!(
            missing.is_empty(),
            "the server has commands the table lacks: {missing:?}"
        );
    } else if !missing.is_empty() {
        eprintln!(
            "the table is from another Redis than {version}; it lacks {missing:?}. Regenerate it with scripts/gen-commands.sh"
        );
    }
}

/// Command lines the server runs, with `{p}` for the test's key prefix.
/// Options are often out of the docs' order: servers read them in any.
const LINES: &[&str] = &[
    "SET {p}s v EX 100 NX GET",
    "SET {p}s v2 KEEPTTL XX",
    "GETEX {p}s PERSIST",
    "SET {p}n 10",
    "INCRBY {p}n 5",
    "BITCOUNT {p}s 0 -1 BYTE",
    "COPY {p}s {p}s2 REPLACE",
    "EXPIRE {p}s2 100 NX",
    "ZADD {p}z NX CH 1 a 2 b",
    "ZADD {p}z XX GT CH INCR 5 a",
    "ZRANGE {p}z 0 -1 WITHSCORES REV",
    "ZRANGE {p}z (1 +inf BYSCORE LIMIT 0 10",
    "HSET {p}h f1 v1 f2 v2",
    "HGETALL {p}h",
    "OBJECT ENCODING {p}h",
    "MEMORY USAGE {p}h SAMPLES 5",
    "LPUSH {p}l c b a",
    "LPOS {p}l a MAXLEN 10 RANK 1 COUNT 0",
    "SORT {p}l ALPHA LIMIT 0 2 DESC",
    "XADD {p}x MAXLEN ~ 100 * f v",
    "XADD {p}x NOMKSTREAM * f v",
    "XRANGE {p}x - + COUNT 10",
    "XGROUP CREATE {p}x grp $ MKSTREAM",
    "XREADGROUP GROUP grp c COUNT 1 STREAMS {p}x >",
    "XAUTOCLAIM {p}x grp c 0 0-0 COUNT 10 JUSTID",
    "GEOADD {p}g 13.361389 38.115556 Palermo",
    "GEOSEARCH {p}g FROMLONLAT 15 37 BYRADIUS 200 km ASC WITHDIST COUNT 5",
    "SCAN 0 TYPE string COUNT 10 MATCH {p}*",
    "EVAL \"return 1\" 0",
    "CLIENT LIST TYPE normal",
    "CONFIG GET maxmemory",
    "FUNCTION LIST WITHCODE LIBRARYNAME none",
    "JSON.SET {p}j $ '{\"a\":1}' NX",
    "JSON.GET {p}j INDENT \"  \" $",
    "TS.ADD {p}t * 1 RETENTION 1000 LABELS a b",
    "TS.RANGE {p}t - + COUNT 10",
    "BF.ADD {p}b x",
    "BF.INSERT {p}b2 CAPACITY 100 ERROR 0.01 ITEMS a b",
];

#[test]
fn lines_the_server_runs_fit_their_usage() {
    let Some(t) = it!() else { return };
    let client = t.connect(standalone());
    let console = client.console();
    for line in LINES {
        let line = line.replace("{p}", t.prefix());
        let args = format::split_args(&line).unwrap();
        if let Err(err) = block_on(console.exec(args.clone())) {
            panic!("{line}: the server refused it: {err:#}");
        }
        assert_eq!(console::fits(&line), Some(true), "{line}");
        // Every word so far fits too, and something may still follow it.
        for words in 1..args.len() {
            let typed: Vec<String> = args[..words].iter().map(|a| format::quote_arg(a)).collect();
            let typed = typed.join(" ");
            assert_eq!(console::fits(&typed), Some(true), "{typed}");
            assert!(console::hint(&typed).is_some(), "no hint after {typed}");
        }
    }
}
