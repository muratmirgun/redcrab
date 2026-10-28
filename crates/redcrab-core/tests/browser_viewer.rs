//! The key list and value view-models (`browser::KeyBrowser`, `viewer`)
//! end to end against a real server, the way a front end drives them.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400), database 0, and only keys under each test's own
//! prefix. Every browser scan filters on the prefix, so the rows start with
//! its four folders (`it`, the test, the pid, the counter).

#[macro_use]
mod common;

use std::collections::HashSet;

use common::*;
use redcrab_core::{
    Client, Key, KeyType, RowKind, ValueData,
    browser::{self, KeyBrowser, RowView, ScanStatus, Selection},
    viewer::{self, CELL_PREVIEW, PAGE},
};

/// Depth of the keys right under the prefix.
const TOP: u16 = 4;

/// Scans for `query` and feeds every event to the browser, as a front end
/// does.
fn scan_browser(client: &Client, browser: &mut KeyBrowser, query: &str) {
    let request = browser.begin_scan(query);
    let generation = request.generation;
    for event in collect(client.scan_into(request.tree, request.options)) {
        assert!(browser.on_scan_event(generation, event));
    }
}

fn all_rows(browser: &mut KeyBrowser) -> Vec<RowView> {
    let len = browser.len();
    browser.row_views(0..len)
}

fn row_of(browser: &mut KeyBrowser, depth: u16, name: &str) -> usize {
    all_rows(browser)
        .iter()
        .position(|row| row.depth == depth && row.name == name)
        .unwrap_or_else(|| panic!("no row {name} at depth {depth}"))
}

fn set_strings(t: &It, names: &[&str]) {
    let mut pipe = redis::pipe();
    for name in names {
        pipe.cmd("SET").arg(t.key(name)).arg(*name).ignore();
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
}

#[test]
fn key_browser_against_real_keys() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::pipe()
        .set(t.key("user:1:name"), "ada")
        .ignore()
        .hset(t.key("user:1:profile"), "plan", "pro")
        .ignore()
        .rpush(t.key("user:2:jobs"), "a")
        .ignore()
        .set_ex(t.key("session"), "x", 600)
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());

    let mut browser = KeyBrowser::new(b':', Some(browser::DEFAULT_KEY_LIMIT));
    // Plain text matches anywhere in the key.
    scan_browser(&client, &mut browser, t.prefix());
    assert!(
        matches!(
            browser.status(),
            ScanStatus::Done {
                keys: 4,
                truncated: false,
                ..
            }
        ),
        "{:?}",
        browser.status()
    );
    assert!(browser.status_text().starts_with("4 matches · "));
    assert_eq!(browser.truncation_notice(), None);
    assert_eq!(browser.key_count(), 4);

    // A small filtered result opens every folder.
    let rows = all_rows(&mut browser);
    let keys: Vec<(u16, &str)> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Key)
        .map(|r| (r.depth, r.name.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            (TOP + 2, "name"),
            (TOP + 2, "profile"),
            (TOP + 2, "jobs"),
            (TOP, "session")
        ]
    );
    assert!(
        rows.iter()
            .filter(|r| r.kind == RowKind::Folder)
            .all(|r| r.expanded)
    );
    let user = &rows[row_of(&mut browser, TOP, "user")];
    assert_eq!((user.kind, user.key_count), (RowKind::Folder, 3));

    let len = browser.len();
    let request = browser.take_meta_request(0..len).unwrap();
    assert_eq!(request.keys.len(), 4);
    // Asked for once until answered.
    assert!(browser.take_meta_request(0..len).is_none());
    let result = block_on(client.key_meta_permitted(request.keys.clone()));
    assert!(browser.apply_meta(request, result));
    assert!(browser.take_meta_request(0..len).is_none());
    let types: Vec<(String, KeyType, bool)> = all_rows(&mut browser)
        .into_iter()
        .filter(|r| r.kind == RowKind::Key)
        .map(|r| {
            let meta = r.meta.unwrap();
            (r.name, meta.ty, meta.ttl_ms > 0)
        })
        .collect();
    assert_eq!(
        types,
        [
            ("name".into(), KeyType::String, false),
            ("profile".into(), KeyType::Hash, false),
            ("jobs".into(), KeyType::List, false),
            ("session".into(), KeyType::String, true),
        ]
    );

    // Selecting a key hands it out for the value view.
    let ix = row_of(&mut browser, TOP + 2, "jobs");
    let key = browser.select(ix, true).unwrap();
    assert_eq!(&*key, t.key("user:2:jobs").as_bytes());
    assert!(browser.row_view(ix).unwrap().selected);
    let loaded = block_on(viewer::load(&client, key)).unwrap();
    assert_eq!(loaded.meta.ty, KeyType::List);
    assert_eq!(loaded.row_cells(0), (Some("0".into()), "a".into()));
}

#[test]
fn keyboard_navigation_over_a_real_tree() {
    let Some(t) = it!() else { return };
    set_strings(&t, &["a:1", "a:2", "b:1", "c"]);
    let client = t.connect(standalone());
    let mut browser = KeyBrowser::new(b':', None);
    scan_browser(&client, &mut browser, t.prefix());
    // it, test, pid, n, a, 1, 2, b, 1, c
    assert_eq!(browser.len(), 10);

    // Down from nothing starts at the top; down onto a key selects it.
    assert_eq!(browser.select_next(), Some(Selection { ix: 0, key: None }));
    for ix in 1..5 {
        assert_eq!(browser.select_next(), Some(Selection { ix, key: None }));
    }
    let Some(Selection {
        ix: 5,
        key: Some(key),
    }) = browser.select_next()
    else {
        panic!("a:1 not selected");
    };
    assert_eq!(&*key, t.key("a:1").as_bytes());
    assert_eq!(browser.selected_key(), Some(key));

    // Left on a key goes to its folder, left again closes it.
    assert_eq!(browser.collapse(), Some(Selection { ix: 4, key: None }));
    assert_eq!(browser.collapse(), None);
    assert_eq!(browser.len(), 8);
    assert!(!browser.row_view(4).unwrap().expanded);
    assert!(browser.expand());
    assert!(!browser.expand(), "already open");
    assert_eq!(browser.len(), 10);

    // The end of the list: holding down on the last row reloads nothing.
    let last = browser.len() - 1;
    let ix = browser.select(last, false);
    assert_eq!(ix.as_deref(), Some(t.key("c").as_bytes()));
    assert_eq!(
        browser.select_next(),
        Some(Selection {
            ix: last,
            key: None
        })
    );
    let up = browser.select_prev().unwrap();
    assert_eq!(up.key.as_deref(), Some(t.key("b:1").as_bytes()));

    // A click on a folder toggles it.
    let b = row_of(&mut browser, TOP, "b");
    assert_eq!(browser.select(b, true), None);
    assert_eq!(browser.len(), 9);
    browser.collapse_all();
    assert_eq!(browser.len(), 1);
    assert_eq!(browser.selected_index(), None, "b is out of sight");
}

#[test]
fn a_rescan_keeps_the_selection() {
    let Some(t) = it!() else { return };
    set_strings(&t, &["a:1", "b:1", "b:2"]);
    let client = t.connect(standalone());
    let mut browser = KeyBrowser::new(b':', None);
    scan_browser(&client, &mut browser, t.prefix());
    let ix = row_of(&mut browser, TOP + 1, "2");
    let key = browser.select(ix, false).unwrap();

    // A new key sorts in before the selected one.
    set_strings(&t, &["b:0"]);
    scan_browser(&client, &mut browser, t.prefix());
    assert_eq!(browser.selected_key(), Some(key));
    let ix = browser.selected_index().unwrap();
    let row = browser.row_view(ix).unwrap();
    assert_eq!(
        (row.depth, row.name.as_str(), row.selected),
        (TOP + 1, "2", true)
    );
    assert_eq!(all_rows(&mut browser)[ix - 1].name, "1");
}

#[test]
fn answers_for_an_older_scan_are_dropped() {
    let Some(t) = it!() else { return };
    set_strings(&t, &["a", "b"]);
    let client = t.connect(standalone());
    let mut browser = KeyBrowser::new(b':', None);
    let old = browser.begin_scan(t.prefix());
    let new = browser.begin_scan(t.prefix());
    for event in collect(client.scan_into(old.tree, old.options)) {
        assert!(!browser.on_scan_event(old.generation, event));
    }
    assert_eq!(browser.status(), &ScanStatus::Scanning { keys: 0 });
    assert_eq!(browser.len(), 0, "the old scan's keys are not shown");
    for event in collect(client.scan_into(new.tree, new.options)) {
        assert!(browser.on_scan_event(new.generation, event));
    }
    assert!(matches!(browser.status(), ScanStatus::Done { keys: 2, .. }));

    // Type/TTL fetched for a tree that has since been replaced is dropped.
    let len = browser.len();
    let request = browser.take_meta_request(0..len).unwrap();
    let result = block_on(client.key_meta_permitted(request.keys.clone()));
    scan_browser(&client, &mut browser, t.prefix());
    assert!(!browser.apply_meta(request, result));
    assert!(all_rows(&mut browser).iter().all(|row| row.meta.is_none()));
    // A failed fetch is asked again the next time.
    let request = browser.take_meta_request(0..len).unwrap();
    assert!(!browser.apply_meta(request, Err(anyhow::anyhow!("offline"))));
    assert!(browser.take_meta_request(0..len).is_some());
}

#[test]
fn deleting_a_key_removes_its_row() {
    let Some(t) = it!() else { return };
    set_strings(&t, &["a:1", "a:2", "c"]);
    let client = t.connect(standalone());
    let mut browser = KeyBrowser::new(b':', None);
    scan_browser(&client, &mut browser, t.prefix());
    let ix = row_of(&mut browser, TOP, "c");
    let key = browser.select(ix, false).unwrap();
    assert_eq!(block_on(client.delete(vec![key.clone()])).unwrap(), 1);
    browser.remove_key(&key);
    assert_eq!(browser.selected_key(), None);
    assert_eq!(browser.key_count(), 2);
    assert!(all_rows(&mut browser).iter().all(|row| row.name != "c"));

    // The last key of a folder takes the folder with it.
    for name in ["a:1", "a:2"] {
        browser.remove_key(t.key(name).as_bytes());
    }
    assert_eq!(browser.key_count(), 0);
    assert!(browser.is_empty());
}

#[test]
fn small_filtered_scans_open_every_folder() {
    let Some(t) = it!() else { return };
    let small = t.key("small:");
    let big = t.key("big:");
    let mut pipe = redis::pipe();
    for i in 0..browser::EXPAND_ALL_MAX {
        pipe.set(format!("{small}{i}"), 1).ignore();
    }
    for i in 0..=browser::EXPAND_ALL_MAX {
        pipe.set(format!("{big}{i}"), 1).ignore();
    }
    pipe.query::<()>(&mut t.seed0()).unwrap();
    let client = t.connect(standalone());

    let mut browser = KeyBrowser::new(b':', None);
    scan_browser(&client, &mut browser, &small);
    // The prefix's folders, `small`, and every key.
    assert_eq!(browser.len(), 5 + browser::EXPAND_ALL_MAX);
    scan_browser(&client, &mut browser, &big);
    assert_eq!(browser.key_count(), browser::EXPAND_ALL_MAX + 1);
    assert_eq!(browser.len(), 1, "one more match keeps the folders closed");
}

#[test]
fn the_key_list_stops_at_its_limit() {
    let Some(t) = it!() else { return };
    let names: Vec<String> = (0..60).map(|i| format!("k{i}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    set_strings(&t, &names);
    let client = t.connect(standalone());
    let mut browser = KeyBrowser::new(b':', Some(25));
    scan_browser(&client, &mut browser, t.prefix());
    assert!(matches!(
        browser.status(),
        ScanStatus::Done {
            keys: 25,
            truncated: true,
            ..
        }
    ));
    assert_eq!(browser.key_count(), 25);
    assert_eq!(
        browser.truncation_notice().as_deref(),
        Some("Showing the first 25 keys. Narrow the filter to see the rest.")
    );
    assert!(browser.status_text().starts_with("25 matches · "));
}

#[test]
fn keys_of_any_bytes_from_list_to_value() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let binary = t.bkey(b"bin\x00\xff");
    let emoji = t.bkey("🦀:claws".as_bytes());
    redis::pipe()
        .cmd("SET")
        .arg(&*binary)
        .arg(b"\x01value")
        .ignore()
        .cmd("SET")
        .arg(&*emoji)
        .arg("two")
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let mut browser = KeyBrowser::new(b':', None);
    scan_browser(&client, &mut browser, t.prefix());

    // Names are escaped to one printable line; the key stays the bytes.
    let ix = row_of(&mut browser, TOP, "bin\\x00\\xff");
    let key = browser.select(ix, false).unwrap();
    assert_eq!(key, binary);
    let loaded = block_on(viewer::load(&client, key)).unwrap();
    assert!(matches!(&loaded.data, ValueData::String(v) if v == b"\x01value"));
    // Quoted the way redis-cli writes it, so it saves back the same bytes.
    let text = loaded.text(true).unwrap();
    assert_eq!(text.text, "\"\\x01value\"");
    assert!(text.editable);
    let edit = loaded.text_edit(true).unwrap();
    assert_eq!(loaded.save_text(&edit, &text.text).unwrap(), None);

    let ix = row_of(&mut browser, TOP + 1, "claws");
    assert_eq!(browser.select(ix, false), Some(emoji));
    assert!(all_rows(&mut browser).iter().any(|row| row.name == "🦀"));
}

#[test]
fn the_value_view_of_every_type() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let long = "x".repeat(10_000);
    redis::pipe()
        .cmd("HSET")
        .arg(t.key("hash"))
        .arg("f1")
        .arg("v1")
        .arg("f2")
        .arg(&long)
        .ignore()
        .cmd("RPUSH")
        .arg(t.key("list"))
        .arg("a")
        .arg(b"b\x00")
        .ignore()
        .cmd("SADD")
        .arg(t.key("set"))
        .arg("m")
        .ignore()
        .cmd("ZADD")
        .arg(t.key("zset"))
        .arg(1.5)
        .arg("m")
        .ignore()
        .cmd("XADD")
        .arg(t.key("stream"))
        .arg("1-1")
        .arg("f")
        .arg("v")
        .arg("g")
        .arg("w\"")
        .ignore()
        .cmd("JSON.SET")
        .arg(t.key("json"))
        .arg("$")
        .arg(r#"{"a":[1,2]}"#)
        .ignore()
        .cmd("BF.ADD")
        .arg(t.key("bloom"))
        .arg("x")
        .ignore()
        .cmd("SET")
        .arg(t.key("string"))
        .arg("hello")
        .arg("EX")
        .arg(100)
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let load = |name: &str| block_on(viewer::load(&client, t.bkey(name))).unwrap();

    let hash = load("hash");
    let columns = hash.columns();
    assert_eq!((columns.first, columns.second), (Some("Field"), "Value"));
    assert_eq!(hash.row_count(), 2);
    assert_eq!(hash.row_cells(0), (Some("f1".into()), "v1".into()));
    // A cell shows the start of a long value; the detail pane all of it.
    assert_eq!(hash.row_cells(1).1.len(), CELL_PREVIEW);
    assert_eq!(hash.row_value(1).unwrap(), long.as_bytes());
    assert_eq!(hash.summary().as_deref(), Some("2 items"));
    assert_eq!(hash.footer(), "Showing 2 of 2");
    assert_eq!(hash.ttl_text(), "No expiry");
    assert!(hash.text(true).is_none() && !hash.has_more());
    assert!(hash.memory.is_some_and(|bytes| bytes > 10_000));

    let list = load("list");
    assert_eq!(list.columns().first, Some("#"));
    assert_eq!(list.row_cells(1), (Some("1".into()), "b\\x00".into()));
    assert_eq!(list.row_value(1).unwrap(), b"b\x00");

    let set = load("set");
    assert_eq!(
        (set.columns().first, set.columns().second),
        (None, "Member")
    );
    assert_eq!(set.row_cells(0), (None, "m".into()));

    let zset = load("zset");
    assert_eq!(zset.columns().first, Some("Score"));
    assert_eq!(zset.row_cells(0), (Some("1.5".into()), "m".into()));

    let stream = load("stream");
    assert_eq!(stream.columns().first, Some("ID"));
    assert_eq!(
        stream.row_cells(0),
        (Some("1-1".into()), "f=v  g=w\"".into())
    );
    assert_eq!(stream.row_value(0).unwrap(), br#"{"f":"v","g":"w\""}"#);
    assert!(stream.row_text(0).unwrap().is_json);

    let json = load("json");
    assert_eq!(json.meta.ty, KeyType::Json);
    let text = json.text(true).unwrap();
    assert!(
        text.is_json && text.editable,
        "JSON documents are edited as JSON, saved with JSON.SET"
    );
    assert_eq!(json.row_count(), 0);
    assert_eq!(json.summary().as_deref(), Some("11 B"));

    let bloom = load("bloom");
    assert!(matches!(&bloom.data, ValueData::Unsupported(name) if &**name == "MBbloom--"));
    assert_eq!((bloom.summary(), bloom.row_count()), (None, 0));
    assert!(bloom.text(true).is_none());

    let string = load("string");
    assert_eq!(string.summary().as_deref(), Some("5 B"));
    assert!(
        string.ttl_text().starts_with("1m "),
        "{}",
        string.ttl_text()
    );
    let text = string.text(true).unwrap();
    assert!(text.editable && !text.is_json);
    assert_eq!(string.preview_notice(), None);

    let missing = load("missing");
    assert!(matches!(missing.data, ValueData::Missing));
    assert_eq!(missing.meta.ty, KeyType::None);
}

#[test]
fn the_value_view_pages_every_collection_to_the_end() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let n = 1203;
    let members: Vec<String> = (0..n).map(|i| format!("m{i:05}")).collect();
    let fields: Vec<(&String, usize)> = members.iter().zip(0..).collect();
    let scores: Vec<(usize, &String)> = (0..).zip(&members).collect();
    let mut pipe = redis::pipe();
    pipe.cmd("HSET").arg(t.key("hash")).arg(&fields).ignore();
    pipe.cmd("SADD").arg(t.key("set")).arg(&members).ignore();
    pipe.cmd("RPUSH").arg(t.key("list")).arg(&members).ignore();
    pipe.cmd("ZADD").arg(t.key("zset")).arg(&scores).ignore();
    for (i, member) in members.iter().enumerate() {
        pipe.cmd("XADD")
            .arg(t.key("stream"))
            .arg(format!("{}-0", i + 1))
            .arg("m")
            .arg(member)
            .ignore();
    }
    // A stream of exactly two pages ends with the second.
    for i in 0..2 * PAGE {
        pipe.cmd("XADD")
            .arg(t.key("stream2"))
            .arg(format!("{}-0", i + 1))
            .arg("i")
            .arg(i)
            .ignore();
    }
    pipe.query::<()>(&mut con).unwrap();
    let client = t.connect(standalone());

    for (name, total) in [
        ("hash", n),
        ("set", n),
        ("list", n),
        ("zset", n),
        ("stream", n),
        ("stream2", 2 * PAGE),
    ] {
        let mut loaded = block_on(viewer::load(&client, t.bkey(name))).unwrap();
        let mut pages = 1;
        while let Some(more) = loaded.load_more(&client) {
            assert!(loaded.apply_page(block_on(more).unwrap()), "{name}");
            pages += 1;
            assert!(pages < 100, "{name} does not end");
        }
        assert!(!loaded.has_more());
        assert_eq!(loaded.row_count(), total, "{name}");
        assert_eq!(loaded.total, total as u64, "{name}");
        let footer = format!(
            "Showing {0} of {0}",
            redcrab_core::format::count(total as u64)
        );
        assert_eq!(loaded.footer(), footer, "{name}");
        // Each element once.
        let cells: HashSet<String> = (0..total)
            .map(|ix| {
                let (first, second) = loaded.row_cells(ix);
                format!("{first:?}{second}")
            })
            .collect();
        assert_eq!(cells.len(), total, "{name}");
        if matches!(name, "list" | "zset" | "stream") {
            assert!(pages >= total.div_ceil(PAGE), "{name}: {pages} pages");
        }
        if name == "stream2" {
            assert_eq!(pages, 2, "no empty last page");
        }
    }
}

#[test]
fn a_page_of_another_load_is_refused() {
    let Some(t) = it!() else { return };
    let items: Vec<String> = (0..1203).map(|i| format!("item {i}")).collect();
    redis::cmd("RPUSH")
        .arg(t.key("list"))
        .arg(&items)
        .query::<()>(&mut t.seed0())
        .unwrap();
    let client = t.connect(standalone());
    let key: Key = t.bkey("list");

    let mut loaded = block_on(viewer::load(&client, key.clone())).unwrap();
    assert_eq!(loaded.row_count(), PAGE);
    assert_eq!(loaded.footer(), "Showing 500 of 1,203");

    let reload = block_on(viewer::load(&client, key.clone())).unwrap();
    let stale = block_on(reload.load_more(&client).unwrap()).unwrap();
    assert!(!loaded.apply_page(stale));
    // The same page twice is applied once.
    let page = block_on(loaded.load_more(&client).unwrap()).unwrap();
    let again = block_on(loaded.load_more(&client).unwrap()).unwrap();
    assert!(loaded.apply_page(page));
    assert!(!loaded.apply_page(again));
    assert_eq!(loaded.row_count(), 2 * PAGE);
    assert_eq!(loaded.row_cells(999).1, "item 999");
}

#[test]
fn edited_text_is_saved_back_with_its_ttl() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let json = r#"{"zeta":1.50,"alpha":[1,2]}"#;
    redis::pipe()
        .cmd("SET")
        .arg(t.key("json"))
        .arg(json)
        .arg("EX")
        .arg(100)
        .ignore()
        .cmd("SET")
        .arg(t.key("text"))
        .arg("line one\nline two")
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let stored = |con: &mut redis::Connection, name: &str| -> (String, i64) {
        redis::pipe()
            .cmd("GET")
            .arg(t.key(name))
            .cmd("PTTL")
            .arg(t.key(name))
            .query(con)
            .unwrap()
    };

    let loaded = block_on(viewer::load(&client, t.bkey("json"))).unwrap();
    let text = loaded.text(client.ensure_writable().is_ok()).unwrap();
    assert!(text.is_json && text.reformatted_json && text.editable);
    assert!(text.text.contains('\n'), "shown indented");
    // Untouched, it saves back byte for byte.
    block_on(client.set_string(loaded.key.clone(), text.bytes_to_save(&text.text))).unwrap();
    assert_eq!(stored(&mut con, "json").0, json);
    // Edited, it goes back compact, key order and number spelling kept.
    let edited = text.text.replace("1.50", "2.50");
    block_on(client.set_string(loaded.key.clone(), text.bytes_to_save(&edited))).unwrap();
    let (value, ttl) = stored(&mut con, "json");
    assert_eq!(value, r#"{"zeta":2.50,"alpha":[1,2]}"#);
    assert!((98_000..=100_000).contains(&ttl), "the TTL is kept: {ttl}");
    // Text that no longer parses is saved as typed.
    block_on(client.set_string(loaded.key.clone(), text.bytes_to_save("{oops"))).unwrap();
    assert_eq!(stored(&mut con, "json").0, "{oops");

    let loaded = block_on(viewer::load(&client, t.bkey("text"))).unwrap();
    let text = loaded.text(true).unwrap();
    assert_eq!(text.text, "line one\nline two");
    assert!(text.editable && !text.is_json);
    let edited = text.bytes_to_save("line one\nline 2");
    block_on(client.set_string(loaded.key.clone(), edited)).unwrap();
    assert_eq!(stored(&mut con, "text"), ("line one\nline 2".into(), -1));
}

#[test]
fn big_strings_are_previewed_read_only() {
    let Some(t) = it!() else { return };
    redis::cmd("SET")
        .arg(t.key("big"))
        .arg("x".repeat((1 << 20) + 10))
        .query::<()>(&mut t.seed0())
        .unwrap();
    let client = t.connect(standalone());
    let big = block_on(viewer::load(&client, t.bkey("big"))).unwrap();
    assert!(big.is_preview());
    assert!(!big.has_more());
    assert!(!big.text(true).unwrap().editable);
    assert_eq!(
        big.preview_notice().as_deref(),
        Some("Showing the first 1.0 MB of 1.0 MB. Large values are read-only.")
    );
}

#[test]
fn a_value_is_loaded_as_what_it_is_now() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    set_strings(&t, &["k"]);
    let client = t.connect(standalone());
    let mut browser = KeyBrowser::new(b':', None);
    scan_browser(&client, &mut browser, t.prefix());
    let len = browser.len();
    let request = browser.take_meta_request(0..len).unwrap();
    let result = block_on(client.key_meta_permitted(request.keys.clone()));
    browser.apply_meta(request, result);
    let ix = row_of(&mut browser, TOP, "k");
    assert_eq!(
        browser.row_view(ix).unwrap().meta.unwrap().ty,
        KeyType::String
    );

    // Another client turns it into a hash; the list's type is stale now.
    redis::pipe()
        .cmd("DEL")
        .arg(t.key("k"))
        .ignore()
        .cmd("HSET")
        .arg(t.key("k"))
        .arg("f")
        .arg("v")
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let key = browser.select(ix, false).unwrap();
    let loaded = block_on(viewer::load(&client, key)).unwrap();
    assert_eq!(loaded.meta.ty, KeyType::Hash);
    assert_eq!(loaded.row_cells(0), (Some("f".into()), "v".into()));
}
