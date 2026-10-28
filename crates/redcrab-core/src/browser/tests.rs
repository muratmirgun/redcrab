use std::time::Duration;

use anyhow::anyhow;
use proptest::prelude::*;

use super::*;
use crate::value::KeyType;

/// Runs a "scan" the way `Client::scan_into` would: keys go into the
/// request's tree, then the events come back.
fn scan(browser: &mut KeyBrowser, query: &str, keys: &[&str]) -> u64 {
    let request = browser.begin_scan(query);
    {
        let mut tree = request.tree.lock().unwrap();
        for key in keys {
            tree.insert(key.as_bytes());
        }
    }
    browser.on_scan_event(
        request.generation,
        ScanEvent::Done {
            keys: keys.len(),
            elapsed: Duration::from_millis(12),
            truncated: false,
        },
    );
    request.generation
}

fn labels(browser: &mut KeyBrowser) -> Vec<String> {
    let len = browser.len();
    browser
        .row_views(0..len)
        .into_iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth as usize);
            let selected = if row.selected { " *" } else { "" };
            match row.kind {
                RowKind::Folder => {
                    let sign = if row.expanded { '-' } else { '+' };
                    format!("{indent}{sign}{}/ ({}){selected}", row.name, row.key_count)
                }
                RowKind::Key => format!("{indent}{}{selected}", row.name),
            }
        })
        .collect()
}

fn meta(ty: KeyType) -> KeyMeta {
    KeyMeta { ty, ttl_ms: -1 }
}

#[test]
fn turns_queries_into_patterns() {
    assert_eq!(pattern_for(""), "*");
    assert_eq!(pattern_for("   "), "*");
    assert_eq!(pattern_for(" session "), "*session*");
    assert_eq!(pattern_for("user:*"), "user:*");
    assert_eq!(pattern_for("a?c"), "a?c");
    assert_eq!(pattern_for("[ab]x"), "[ab]x");
}

#[test]
fn escapes_glob_characters() {
    assert_eq!(escape_glob(b"plain:key"), b"plain:key");
    assert_eq!(escape_glob(br"a*b?c[d]e\f"), br"a\*b\?c\[d\]e\\f");
    assert_eq!(escape_glob(b"\xff*"), b"\xff\\*");
    assert_eq!(escape_glob(b""), b"");
}

#[test]
fn begin_scan_starts_a_new_generation() {
    let mut browser = KeyBrowser::new(b':', Some(10));
    let first = browser.begin_scan("user");
    assert_eq!(first.options.pattern, "*user*");
    assert_eq!(first.options.limit, Some(10));
    assert_eq!(browser.status(), &ScanStatus::Scanning { keys: 0 });
    assert_eq!(browser.status_text(), "Scanning…");
    assert!(browser.is_scanning());

    let second = browser.begin_scan("");
    assert_ne!(first.generation, second.generation);
    assert!(!Arc::ptr_eq(&first.tree, &second.tree));
    assert_eq!(second.options.pattern, "*");

    // Events of the replaced scan change nothing.
    first.tree.lock().unwrap().insert(b"stale");
    assert!(!browser.on_scan_event(first.generation, ScanEvent::Progress { keys: 1 }));
    assert_eq!(browser.status(), &ScanStatus::Scanning { keys: 0 });
    assert!(browser.is_empty());
}

#[test]
fn shows_keys_as_they_stream_in() {
    let mut browser = KeyBrowser::new(b':', None);
    let request = browser.begin_scan("");
    request.tree.lock().unwrap().insert(b"b");
    assert!(browser.on_scan_event(request.generation, ScanEvent::Progress { keys: 1 }));
    assert_eq!(labels(&mut browser), ["b"]);
    assert_eq!(browser.status_text(), "Scanning… 1 keys");

    request.tree.lock().unwrap().insert(b"a:x");
    browser.on_scan_event(request.generation, ScanEvent::Progress { keys: 2 });
    assert_eq!(labels(&mut browser), ["+a/ (1)", "b"]);

    browser.on_scan_event(
        request.generation,
        ScanEvent::Done {
            keys: 2,
            elapsed: Duration::from_millis(1500),
            truncated: false,
        },
    );
    assert_eq!(browser.status_text(), "2 keys · 1500 ms");
    assert!(!browser.is_scanning());
    assert_eq!(browser.truncation_notice(), None);
    // Unfiltered scans leave folders closed, however few keys there are.
    assert_eq!(labels(&mut browser), ["+a/ (1)", "b"]);
    assert_eq!(browser.key_count(), 2);
}

#[test]
fn small_filtered_results_open_every_folder() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "x", &["a:b:x1", "a:x2", "x3"]);
    assert_eq!(
        labels(&mut browser),
        ["-a/ (2)", "  -b/ (1)", "    x1", "  x2", "x3"]
    );
    assert_eq!(browser.status_text(), "3 matches · 12 ms");

    // Above the threshold, folders stay closed.
    let keys: Vec<String> = (0..=EXPAND_ALL_MAX).map(|i| format!("f:x{i}")).collect();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    scan(&mut browser, "x", &keys);
    assert_eq!(
        labels(&mut browser),
        [format!("+f/ ({})", EXPAND_ALL_MAX + 1)]
    );
}

#[test]
fn reports_truncation_and_failure() {
    let mut browser = KeyBrowser::new(b':', Some(2_000_000));
    let request = browser.begin_scan("");
    browser.on_scan_event(
        request.generation,
        ScanEvent::Done {
            keys: 2_000_000,
            elapsed: Duration::from_secs(1),
            truncated: true,
        },
    );
    assert_eq!(
        browser.truncation_notice().as_deref(),
        Some("Showing the first 2,000,000 keys. Narrow the filter to see the rest.")
    );

    let request = browser.begin_scan("");
    assert_eq!(browser.truncation_notice(), None);
    browser.on_scan_event(request.generation, ScanEvent::Failed("boom".into()));
    assert_eq!(browser.status(), &ScanStatus::Failed("boom".into()));
    assert_eq!(browser.status_text(), "Scan failed");
    assert!(!browser.is_scanning());
}

#[test]
fn row_views_escape_names_and_clamp_ranges() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["bin\n\u{0}:k", "plain"]);
    let rows = browser.row_views(0..100);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "bin\\n\\x00");
    assert_eq!(rows[0].kind, RowKind::Folder);
    assert_eq!(rows[0].key_count, 1);
    assert!(!rows[0].expanded);
    assert_eq!(rows[1].name, "plain");
    assert_eq!(rows[1].key_count, 1);
    assert!(browser.row_views(5..9).is_empty());
    assert_eq!(browser.row_view(1).map(|r| r.name), Some("plain".into()));
    assert_eq!(browser.row_view(2), None);
}

#[test]
fn fetches_meta_for_visible_keys_once() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["f:x", "a", "b", "c"]);
    // Rows: f/, a, b, c. Folders need no meta.
    let request = browser.take_meta_request(0..3).unwrap();
    assert_eq!(request.keys, [Key::from(&b"a"[..]), Key::from(&b"b"[..])]);
    // Already pending: only `c` is new.
    let second = browser.take_meta_request(0..4).unwrap();
    assert_eq!(second.keys, [Key::from(&b"c"[..])]);
    assert!(browser.take_meta_request(0..4).is_none());

    assert!(browser.apply_meta(
        request,
        Ok(vec![Some(meta(KeyType::Hash)), Some(meta(KeyType::List))])
    ));
    let rows = browser.row_views(0..4);
    assert_eq!(rows[0].meta, None);
    assert_eq!(rows[1].meta, Some(meta(KeyType::Hash)));
    assert_eq!(rows[2].meta, Some(meta(KeyType::List)));
    assert_eq!(rows[3].meta, None);

    // A failed fetch changes nothing visible and is retried later.
    assert!(!browser.apply_meta(second, Err(anyhow!("connection lost"))));
    let retry = browser.take_meta_request(0..4).unwrap();
    assert_eq!(retry.keys, [Key::from(&b"c"[..])]);

    // Answers for a replaced tree are dropped.
    scan(&mut browser, "", &["a"]);
    assert!(!browser.apply_meta(retry, Ok(vec![Some(meta(KeyType::Set))])));
    assert_eq!(browser.row_view(0).unwrap().meta, None);
    assert!(browser.take_meta_request(0..1).is_some());
}

/// What a front end does on switching databases: a fresh browser for the
/// new keyspace. Both first scans used to be generation 1, so an answer or
/// a row of the old keyspace landed on whatever node of the new tree had
/// the same id.
#[test]
fn a_reset_drops_what_belongs_to_the_old_keyspace() {
    let mut browser = KeyBrowser::new(b':', Some(10));
    scan(&mut browser, "", &["session:1", "session:2", "cache"]);
    let folder = browser.row_view(0).unwrap().id;
    assert_eq!(folder.kind(), RowKind::Folder);
    let key = browser.row_view(1).unwrap().id;
    let request = browser.take_meta_request(0..3).unwrap();
    assert_eq!(request.keys, [Key::from(&b"cache"[..])]);
    browser.select(1, false);

    browser.reset(b'/');
    assert_eq!(browser.separator(), b'/');
    assert_eq!(browser.status(), &ScanStatus::Idle);
    assert_eq!(browser.len(), 0);
    assert_eq!(browser.selected_key(), None);
    let request_after = browser.begin_scan("");
    assert_eq!(request_after.options.limit, Some(10), "the limit stays");
    assert!(request_after.generation > request.generation);
    scan(&mut browser, "", &["orders/1", "orders/2", "users"]);
    assert_eq!(labels(&mut browser), ["+orders/ (2)", "users"]);

    // The old keyspace's answer is dropped, and its key is still fetched.
    assert!(!browser.apply_meta(request, Ok(vec![Some(meta(KeyType::Hash))])));
    assert!(browser.row_views(0..2).iter().all(|row| row.meta.is_none()));
    let fresh = browser.take_meta_request(0..2).unwrap();
    assert_eq!(fresh.keys, [Key::from(&b"users"[..])]);
    // Rows drawn from it match nothing, though node ids repeat.
    assert_eq!(browser.row_key(folder), None);
    assert_eq!(browser.row_key(key), None);
    assert_eq!(browser.click(key), None);
    assert!(!browser.set_expanded_below(folder, true));
    assert_eq!(labels(&mut browser), ["+orders/ (2)", "users"]);
    // Nor does the old selection come back by name.
    scan(&mut browser, "", &["session:1", "cache"]);
    assert_eq!(browser.selected_key(), None);
}

#[test]
fn bounds_the_meta_cache() {
    let mut browser = KeyBrowser::new(b':', None);
    let keys: Vec<String> = (0..META_CACHE_MAX + 10)
        .map(|i| format!("k{i:06}"))
        .collect();
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    scan(&mut browser, "", &keys);
    let request = browser.take_meta_request(0..META_CACHE_MAX).unwrap();
    let n = request.keys.len();
    browser.apply_meta(request, Ok(vec![Some(meta(KeyType::String)); n]));
    assert_eq!(browser.meta.len(), META_CACHE_MAX);
    let request = browser.take_meta_request(0..META_CACHE_MAX + 10).unwrap();
    assert_eq!(request.keys.len(), 10);
    browser.apply_meta(request, Ok(vec![Some(meta(KeyType::String)); 10]));
    assert_eq!(browser.meta.len(), 10);
}

#[test]
fn clicks_select_keys_and_toggle_folders() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["user:1", "user:2", "zzz"]);
    assert_eq!(browser.select(0, true), None);
    assert_eq!(labels(&mut browser), ["-user/ (2) *", "  1", "  2", "zzz"]);
    assert_eq!(browser.selected_key(), None);
    assert_eq!(browser.select(2, true), Some(Key::from(&b"user:2"[..])));
    assert_eq!(browser.selected_key(), Some(Key::from(&b"user:2"[..])));
    assert_eq!(browser.selected_index(), Some(2));
    assert_eq!(browser.select(0, true), None);
    assert_eq!(labels(&mut browser), ["+user/ (2) *", "zzz"]);
    assert_eq!(browser.select(9, true), None);
}

#[test]
fn navigates_with_the_keyboard() {
    let mut browser = KeyBrowser::new(b':', None);
    // Nothing to move to yet.
    assert_eq!(browser.select_next(), None);
    assert_eq!(browser.select_prev(), None);

    scan(&mut browser, "", &["a:x", "a:y", "b"]);
    // With no selection, either direction starts at the top.
    assert_eq!(browser.select_next(), Some(Selection { ix: 0, key: None }));
    assert!(browser.expand());
    assert!(!browser.expand(), "already open");
    assert_eq!(labels(&mut browser), ["-a/ (2) *", "  x", "  y", "b"]);

    let down = browser.select_next().unwrap();
    assert_eq!((down.ix, down.key.as_deref()), (1, Some(&b"a:x"[..])));
    let down = browser.select_next().unwrap();
    assert_eq!((down.ix, down.key.as_deref()), (2, Some(&b"a:y"[..])));
    let down = browser.select_next().unwrap();
    assert_eq!((down.ix, down.key.as_deref()), (3, Some(&b"b"[..])));
    // At the bottom: stays, and does not hand the same key out again.
    assert_eq!(browser.select_next(), Some(Selection { ix: 3, key: None }));

    let up = browser.select_prev().unwrap();
    assert_eq!((up.ix, up.key.as_deref()), (2, Some(&b"a:y"[..])));
    // Left on a key jumps to its folder; left on the open folder closes it.
    assert_eq!(browser.collapse(), Some(Selection { ix: 0, key: None }));
    assert_eq!(browser.collapse(), None);
    assert_eq!(labels(&mut browser), ["+a/ (2) *", "b"]);
    // Left on a closed top-level folder or key goes nowhere.
    assert_eq!(browser.collapse(), None);
    browser.select(1, false);
    assert_eq!(browser.collapse(), None);
    assert!(!browser.expand(), "a key does not expand");
    assert_eq!(browser.select_prev(), Some(Selection { ix: 0, key: None }));
    assert_eq!(browser.select_prev(), Some(Selection { ix: 0, key: None }));
}

#[test]
fn left_on_a_nested_folder_goes_to_its_parent() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["a:b:c", "a:d"]);
    browser.select(0, true);
    browser.select(1, false);
    assert_eq!(labels(&mut browser), ["-a/ (2)", "  +b/ (1) *", "  d"]);
    assert_eq!(browser.collapse(), Some(Selection { ix: 0, key: None }));
}

#[test]
fn collapse_all_closes_everything() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "a", &["a:b:c", "a:d"]);
    assert_eq!(browser.len(), 4);
    browser.collapse_all();
    assert_eq!(labels(&mut browser), ["+a/ (2)"]);
}

#[test]
fn removes_deleted_keys() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["a", "b"]);
    browser.select(1, false);
    let request = browser.take_meta_request(0..2).unwrap();
    browser.apply_meta(request, Ok(vec![Some(meta(KeyType::String)); 2]));
    browser.remove_key(b"b");
    assert_eq!(labels(&mut browser), ["a"]);
    assert_eq!(browser.selected_index(), None);
    assert_eq!(browser.key_count(), 1);
    // Unknown keys are ignored.
    browser.remove_key(b"nope");
    assert_eq!(browser.len(), 1);
}

#[test]
fn adds_created_keys_open_and_selected() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["a", "user:1", "user:2"]);
    browser.collapse_all();
    assert_eq!(labels(&mut browser), ["+user/ (2)", "a"]);
    // Into a closed folder: it opens, the key is selected.
    assert_eq!(browser.add_key(b"user:new:1"), Some(2));
    assert_eq!(
        labels(&mut browser),
        ["-user/ (3)", "  -new/ (1)", "    1 *", "  1", "  2", "a"]
    );
    assert_eq!(browser.selected_key().as_deref(), Some(&b"user:new:1"[..]));
    // One that is there already is just selected.
    assert_eq!(browser.add_key(b"a"), Some(5));
    assert_eq!(browser.key_count(), 4);
    // A renamed key: the old one goes, the new one comes.
    browser.remove_key(b"a");
    assert_eq!(browser.add_key(b"b\xff"), Some(5));
    assert_eq!(browser.row_view(5).unwrap().name, "b\\xff");

    // The finished scan's count follows: a rename keeps it, a new key
    // adds one.
    assert_eq!(browser.key_count(), 4);
    assert!(
        browser.status_text().starts_with("4 keys"),
        "{}",
        browser.status_text()
    );

    // Its type and TTL are fetched again once forgotten.
    let len = browser.len();
    let request = browser.take_meta_request(0..len).unwrap();
    browser.apply_meta(request, Ok(vec![Some(meta(KeyType::String)); 4]));
    assert!(browser.take_meta_request(0..len).is_none());
    browser.forget_meta(b"user:1");
    browser.forget_meta(b"unknown");
    let request = browser.take_meta_request(0..len).unwrap();
    assert_eq!(request.keys, [Key::from(&b"user:1"[..])]);
}

#[test]
fn patterns_that_match_every_key_are_no_filter() {
    for pattern in ["*", "**", "?*", "*?", "??*", "*?*"] {
        assert!(is_match_all(pattern.as_bytes()), "{pattern}");
    }
    for pattern in ["", "?", "??", "a*", "*a", "[*]", "\\*", "*:*", "* "] {
        assert!(!is_match_all(pattern.as_bytes()), "{pattern}");
    }
    let mut browser = KeyBrowser::new(b':', None);
    for (query, filtered) in [
        ("", false),
        ("  ", false),
        ("*", false),
        (" ** ", false),
        ("?*", false),
        ("user", true),
        ("user:*", true),
        ("?", true),
    ] {
        scan(&mut browser, query, &["a"]);
        assert_eq!(browser.is_filtered(), filtered, "{query:?}");
        let noun = if filtered { "match" } else { "key" };
        assert!(browser.status_text().contains(noun), "{query:?}");
    }
}

#[test]
fn renames_and_new_keys_keep_the_count_of_a_finished_scan() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["a", "b", "c"]);
    assert!(browser.status_text().starts_with("3 keys"));
    // A rename, as the workspace applies it.
    browser.remove_key(b"b");
    browser.add_key(b"d");
    assert!(
        browser.status_text().starts_with("3 keys"),
        "{}",
        browser.status_text()
    );
    browser.add_key(b"e");
    assert_eq!(browser.key_count(), 4);
    assert!(
        browser.status_text().starts_with("4 keys"),
        "{}",
        browser.status_text()
    );
    // Filtered: the noun changes, the count still follows.
    scan(&mut browser, "x*", &["x1", "x2"]);
    browser.add_key(b"x3");
    assert!(
        browser.status_text().starts_with("3 matches"),
        "{}",
        browser.status_text()
    );
}

#[test]
fn keeps_the_selection_across_a_rescan() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["a", "b", "c"]);
    browser.select(1, false);
    // The new tree gets different node ids; the selection follows the key.
    scan(&mut browser, "", &["0", "a", "b", "c"]);
    assert_eq!(browser.selected_index(), Some(2));
    assert_eq!(browser.selected_key(), Some(Key::from(&b"b"[..])));

    // A key that is gone leaves nothing selected.
    scan(&mut browser, "", &["a", "c"]);
    assert_eq!(browser.selected_index(), None);

    // Folders are followed too.
    scan(&mut browser, "", &["f:1", "g:1"]);
    browser.select(1, false);
    scan(&mut browser, "", &["e:1", "f:1", "g:1"]);
    assert_eq!(labels(&mut browser), ["+e/ (1)", "+f/ (1)", "+g/ (1) *"]);
}

#[test]
fn plain_text_queries_match_backslashes_literally() {
    assert_eq!(pattern_for(r"C:\tmp"), r"*C:\\tmp*");
    assert_eq!(pattern_for("x]"), r"*x\]*");
    assert_eq!(pattern_for("ünï"), "*ünï*");
    // Globs are the user's to escape.
    assert_eq!(pattern_for(r"a\*b*"), r"a\*b*");
}

#[test]
fn left_works_on_keys_deeper_than_depths_count() {
    // Depths stop at u16::MAX; the folder a key is in is still found.
    let mut browser = KeyBrowser::new(b':', None);
    let deep = format!("a{}z", ":".repeat(70_000));
    scan(&mut browser, "z", &[&deep, "b"]);
    let len = browser.len();
    assert_eq!(len, 70_000 + 2);
    let ix = len - 2;
    assert_eq!(browser.select(ix, false).as_deref(), Some(deep.as_bytes()));
    assert_eq!(browser.row_view(ix).unwrap().depth, u16::MAX);
    assert_eq!(
        browser.collapse(),
        Some(Selection {
            ix: ix - 1,
            key: None
        })
    );
    // The folder is open, so the next Left closes it.
    assert_eq!(browser.collapse(), None);
    assert_eq!(browser.len(), len - 1);
}

#[test]
fn clicks_act_on_the_row_that_was_drawn() {
    let mut browser = KeyBrowser::new(b':', None);
    let request = browser.begin_scan("");
    let insert = |keys: &[&str]| {
        let mut tree = request.tree.lock().unwrap();
        for key in keys {
            tree.insert(key.as_bytes());
        }
    };
    insert(&["user:1040", "user:1041", "user:1042", "user:1043", "zzz"]);
    browser.on_scan_event(request.generation, ScanEvent::Progress { keys: 5 });
    browser.select(0, true);
    // The frame the user clicks in.
    let len = browser.len();
    let drawn = browser.row_views(0..len);
    let row = |name: &str| drawn.iter().find(|r| r.name == name).unwrap().id;
    let (clicked, folder) = (row("1042"), row("user"));

    // Keys land above it before that frame is replaced.
    insert(&["user:1000", "user:1001"]);
    browser.on_scan_event(request.generation, ScanEvent::Progress { keys: 7 });
    assert_eq!(browser.click(clicked).as_deref(), Some(&b"user:1042"[..]));
    assert_eq!(browser.selected_key().as_deref(), Some(&b"user:1042"[..]));
    assert_eq!(browser.selected_index(), Some(5));
    // A folder click opens or closes that folder, not the row now there.
    assert_eq!(browser.click(folder), None);
    assert_eq!(labels(&mut browser), ["+user/ (6) *", "zzz"]);

    // A key deleted since, or a row of an earlier scan, is ignored.
    browser.remove_key(b"user:1042");
    assert_eq!(browser.click(clicked), None);
    assert_eq!(browser.selected_key(), None);
    let old = row("zzz");
    scan(&mut browser, "", &["zzz"]);
    assert_eq!(browser.click(old), None);
    assert_eq!(browser.selected_index(), None);
}

#[test]
fn keys_the_user_may_not_see_are_not_asked_for_again() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["app:a", "other", "secret"]);
    browser.select(0, true);
    // Rows: app/, a, other, secret.
    let request = browser.take_meta_request(0..4).unwrap();
    assert_eq!(request.keys.len(), 3);
    assert!(browser.apply_meta(request, Ok(vec![Some(meta(KeyType::String)), None, None])));
    let rows = browser.row_views(0..4);
    assert_eq!(rows[1].meta, Some(meta(KeyType::String)));
    assert_eq!((rows[2].meta.as_ref(), rows[3].meta.as_ref()), (None, None));
    // Redraws do not send the same request again.
    assert!(browser.take_meta_request(0..4).is_none());
    // An answer with nothing to show changes nothing visible.
    scan(&mut browser, "", &["secret"]);
    let request = browser.take_meta_request(0..1).unwrap();
    assert!(!browser.apply_meta(request, Ok(vec![None])));
    assert!(browser.take_meta_request(0..1).is_none());
}

#[test]
fn folder_patterns_take_the_prefix_literally() {
    assert_eq!(folder_pattern(b"user", b':'), b"user:*");
    assert_eq!(folder_pattern(b"", b':'), b":*");
    assert_eq!(folder_pattern(br"a*b?[c]\d", b':'), br"a\*b\?\[c\]\\d:*");
    assert_eq!(folder_pattern(b"\xff\x00", b':'), b"\xff\x00:*");
    // A separator that is itself a glob character is escaped too.
    assert_eq!(folder_pattern(b"a", b'*'), br"a\**");
    assert_eq!(folder_pattern(b"a", b'\\'), br"a\\*");
}

#[test]
fn row_keys_follow_the_drawn_row() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["user", "user:1", "user:2:x", "z"]);
    let len = browser.len();
    let drawn = browser.row_views(0..len);
    let row = |name: &str, kind: RowKind| {
        drawn
            .iter()
            .find(|r| r.name == name && r.kind == kind)
            .unwrap()
            .id
    };
    let folder = row("user", RowKind::Folder);
    let key = row("user", RowKind::Key);
    let z = row("z", RowKind::Key);
    assert_eq!(browser.row_key(folder).as_deref(), Some(&b"user"[..]));
    assert_eq!(browser.row_key(key).as_deref(), Some(&b"user"[..]));
    assert_eq!(browser.row_key(z).as_deref(), Some(&b"z"[..]));

    // Gone rows and rows of an earlier scan stand for nothing.
    browser.remove_folder(b"user");
    assert_eq!(browser.row_key(folder), None);
    assert_eq!(browser.row_key(key).as_deref(), Some(&b"user"[..]));
    scan(&mut browser, "", &["z"]);
    assert_eq!(browser.row_key(z), None);
}

#[test]
fn expands_and_collapses_below_a_row() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["a:b:c:d", "a:e", "f:g"]);
    let folder = browser.row_views(0..1)[0].id;
    assert!(browser.set_expanded_below(folder, true));
    assert_eq!(
        labels(&mut browser),
        [
            "-a/ (2)",
            "  -b/ (1)",
            "    -c/ (1)",
            "      d",
            "  e",
            "+f/ (1)"
        ]
    );
    assert!(browser.set_expanded_below(folder, false));
    assert_eq!(labels(&mut browser), ["+a/ (2)", "+f/ (1)"]);
    // Key rows have nothing below them, and rows of an earlier scan are
    // ignored.
    scan(&mut browser, "", &["k"]);
    let key = browser.row_views(0..1)[0].id;
    assert!(!browser.set_expanded_below(key, true));
    assert!(!browser.set_expanded_below(folder, true));
}

#[test]
fn removes_a_deleted_folder() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(
        &mut browser,
        "",
        &["user", "user:1", "user:2:a", "user:2:b", "user2:x", "z"],
    );
    assert_eq!(browser.status_text(), "6 keys · 12 ms");
    // Open `user` and `user:2`, select `user:2:b`, and fetch the types of
    // what is on screen.
    browser.select(1, true);
    browser.select(2, true);
    let len = browser.len();
    let b = browser
        .row_views(0..len)
        .iter()
        .position(|r| r.name == "b")
        .unwrap();
    assert_eq!(browser.select(b, false).as_deref(), Some(&b"user:2:b"[..]));
    let request = browser.take_meta_request(0..len).unwrap();
    let n = request.keys.len();
    browser.apply_meta(request, Ok(vec![Some(meta(KeyType::String)); n]));

    assert_eq!(browser.remove_folder(b"user"), 3);
    assert_eq!(labels(&mut browser), ["+user2/ (1)", "user", "z"]);
    assert_eq!(browser.selected_key(), None, "the selected key is gone");
    assert_eq!(browser.key_count(), 3);
    assert_eq!(browser.status_text(), "3 keys · 12 ms");
    // The cache keeps the keys that are left, and only those.
    {
        let tree = browser.tree();
        assert!(browser.meta.keys().all(|&node| tree.is_key(node)));
    }
    assert!(browser.take_meta_request(0..3).is_none());

    // Nothing to remove: unknown folders, keys without anything below.
    assert_eq!(browser.remove_folder(b"user"), 0);
    assert_eq!(browser.remove_folder(b"nope"), 0);
    assert_eq!(browser.remove_folder(b"z"), 0);
    assert_eq!(browser.key_count(), 3);
}

#[test]
fn removing_a_folder_keeps_a_selection_outside_it() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["a:1", "a:2", "b:1", "c"]);
    // Rows: a/, b/, c.
    assert_eq!(browser.select(2, false).as_deref(), Some(&b"c"[..]));
    browser.remove_folder(b"a");
    assert_eq!(browser.selected_key().as_deref(), Some(&b"c"[..]));
    assert_eq!(labels(&mut browser), ["+b/ (1)", "c *"]);
    // A selected folder that empties is no longer selected.
    browser.select(0, false);
    browser.remove_folder(b"b");
    assert_eq!(browser.selected_index(), None);
    assert_eq!(labels(&mut browser), ["c"]);
}

#[test]
fn removing_a_folder_drops_a_pending_reselection_inside_it() {
    let mut browser = KeyBrowser::new(b':', None);
    scan(&mut browser, "", &["a:1", "b"]);
    browser.select(0, true);
    browser.select(1, false);
    assert_eq!(browser.selected_key().as_deref(), Some(&b"a:1"[..]));
    // A rescan carries the selection by key until the new tree has it.
    let request = browser.begin_scan("");
    browser.remove_folder(b"a");
    request.tree.lock().unwrap().insert(b"a:1");
    browser.on_scan_event(request.generation, ScanEvent::Progress { keys: 1 });
    assert_eq!(browser.selected_key(), None);
}

/// Redis's glob matching (`stringmatchlen` in util.c), byte-wise, for
/// checking patterns without a server: escapes, `?`, `*`, and classes with
/// `^`, ranges and escapes.
fn glob_match(pattern: &[u8], string: &[u8]) -> bool {
    let (mut p, mut s) = (pattern, string);
    while let Some(&c) = p.first() {
        match c {
            b'*' => {
                while p.first() == Some(&b'*') {
                    p = &p[1..];
                }
                return p.is_empty() || (0..=s.len()).any(|i| glob_match(p, &s[i..]));
            }
            b'?' => {
                if s.is_empty() {
                    return false;
                }
                s = &s[1..];
                p = &p[1..];
            }
            b'[' => {
                let Some(&b) = s.first() else { return false };
                let mut i = 1;
                let not = p.get(i) == Some(&b'^');
                if not {
                    i += 1;
                }
                let mut hit = false;
                loop {
                    match p.get(i) {
                        // An unterminated class ends at the pattern's end.
                        None => {
                            i -= 1;
                            break;
                        }
                        Some(b']') => break,
                        Some(b'\\') if i + 1 < p.len() => {
                            hit |= p[i + 1] == b;
                            i += 2;
                        }
                        Some(&lo) if p.get(i + 1) == Some(&b'-') && i + 2 < p.len() => {
                            let hi = p[i + 2];
                            let (lo, hi) = if lo > hi { (hi, lo) } else { (lo, hi) };
                            hit |= (lo..=hi).contains(&b);
                            i += 3;
                        }
                        Some(&x) => {
                            hit |= x == b;
                            i += 1;
                        }
                    }
                }
                if hit == not {
                    return false;
                }
                p = &p[(i + 1).min(p.len())..];
                s = &s[1..];
            }
            b'\\' if p.len() >= 2 => {
                if s.first() != Some(&p[1]) {
                    return false;
                }
                p = &p[2..];
                s = &s[1..];
            }
            c => {
                if s.first() != Some(&c) {
                    return false;
                }
                p = &p[1..];
                s = &s[1..];
            }
        }
    }
    s.is_empty()
}

#[test]
fn the_test_matcher_matches_like_redis() {
    assert!(glob_match(b"user:*", b"user:1"));
    assert!(glob_match(b"user:*", b"user:"));
    assert!(!glob_match(b"user:*", b"user"));
    assert!(!glob_match(b"user:*", b"user2:1"));
    assert!(glob_match(b"h?llo", b"hello"));
    assert!(glob_match(b"h[ae]llo", b"hallo"));
    assert!(!glob_match(b"h[^e]llo", b"hello"));
    assert!(glob_match(b"h[a-b]llo", b"hbllo"));
    assert!(glob_match(br"a\*b", b"a*b"));
    assert!(!glob_match(br"a\*b", b"axb"));
    assert!(glob_match(b"a*", b"a*b"));
    assert!(!glob_match(b"a", b"ab"));
}

/// Bytes that make prefixes interesting: glob characters, the separator,
/// a backslash, and bytes that are not UTF-8.
fn tricky_bytes(len: std::ops::Range<usize>) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        prop_oneof![
            3 => Just(b'a'),
            1 => Just(b'b'),
            2 => Just(b':'),
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// The pattern of a folder matches exactly the keys it holds: those
    /// that start with the prefix and the separator.
    #[test]
    fn folder_patterns_match_exactly_the_folder(
        prefix in tricky_bytes(0..8),
        rest in tricky_bytes(0..12),
        separator in prop_oneof![Just(b':'), Just(b'*'), Just(b'\\'), Just(b'/')],
        under in any::<bool>(),
    ) {
        // Half of the keys are built inside the folder, so both outcomes
        // come up often.
        let mut start = prefix.clone();
        start.push(separator);
        let key = if under { [start.as_slice(), &rest].concat() } else { rest };
        let pattern = folder_pattern(&prefix, separator);
        prop_assert_eq!(glob_match(&pattern, &key), key.starts_with(&start));
    }

    /// An escaped string matches itself and nothing else.
    #[test]
    fn escaped_strings_match_only_themselves(
        literal in tricky_bytes(0..10),
        other in tricky_bytes(0..10),
    ) {
        let pattern = escape_glob(&literal);
        prop_assert!(glob_match(&pattern, &literal));
        prop_assert_eq!(glob_match(&pattern, &other), other == literal);
    }
}
