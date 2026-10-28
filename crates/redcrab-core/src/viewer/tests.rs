use super::*;

use crate::{formats::ValueFormat, value::StreamEntry};

fn loaded(ty: KeyType, data: ValueData, total: u64, next: Option<Cursor>) -> Loaded {
    Loaded::new(
        Key::from(&b"k"[..]),
        KeyMeta { ty, ttl_ms: -1 },
        ValuePage { data, total, next },
        Some(1024),
    )
}

fn page(data: ValueData, total: u64, next: Option<Cursor>) -> ValuePage {
    ValuePage { data, total, next }
}

fn bytes(items: &[&str]) -> Vec<Vec<u8>> {
    items.iter().map(|s| s.as_bytes().to_vec()).collect()
}

#[test]
fn picks_columns_per_type() {
    assert_eq!(
        columns(&KeyType::Hash),
        Columns {
            first: Some("Field"),
            second: "Value",
            first_share: Some(0.35)
        }
    );
    assert_eq!(columns(&KeyType::List).first, Some("#"));
    assert_eq!(columns(&KeyType::Set).first, None);
    assert_eq!(columns(&KeyType::Set).second, "Member");
    assert_eq!(columns(&KeyType::ZSet).first, Some("Score"));
    assert_eq!(columns(&KeyType::Stream).second, "Fields");
    assert_eq!(columns(&KeyType::String).second, "");
}

#[test]
fn formats_cells_on_one_short_line() {
    let hash = loaded(
        KeyType::Hash,
        ValueData::Hash(vec![
            (b"name".to_vec(), b"multi\nline\x00".to_vec()),
            (b"big".to_vec(), vec![b'x'; 10_000]),
        ]),
        2,
        None,
    );
    assert_eq!(hash.row_count(), 2);
    assert_eq!(
        hash.row_cells(0),
        (Some("name".into()), "multi\\nline\\x00".into())
    );
    assert_eq!(hash.row_cells(1).1.len(), CELL_PREVIEW);
    assert_eq!(hash.row_cells(2), (None, String::new()));

    let list = loaded(KeyType::List, ValueData::List(bytes(&["a", "b"])), 2, None);
    assert_eq!(list.row_cells(1), (Some("1".into()), "b".into()));

    let set = loaded(KeyType::Set, ValueData::Set(bytes(&["m"])), 1, None);
    assert_eq!(set.row_cells(0), (None, "m".into()));

    let zset = loaded(
        KeyType::ZSet,
        ValueData::ZSet(vec![(b"p".to_vec(), 1.5), (b"q".to_vec(), -3.0)]),
        2,
        None,
    );
    assert_eq!(zset.row_cells(0), (Some("1.5".into()), "p".into()));
    assert_eq!(zset.row_cells(1).0.as_deref(), Some("-3"));

    let fields = (0..100)
        .map(|i| (format!("f{i}").into_bytes(), vec![b'v'; 20]))
        .collect();
    let stream = loaded(
        KeyType::Stream,
        ValueData::Stream(vec![
            StreamEntry {
                id: "1-0".into(),
                fields: vec![
                    (b"a".to_vec(), b"1".to_vec()),
                    (b"b".to_vec(), b"2".to_vec()),
                ],
            },
            StreamEntry {
                id: "2-0".into(),
                fields,
            },
        ]),
        2,
        None,
    );
    assert_eq!(stream.row_cells(0), (Some("1-0".into()), "a=1  b=2".into()));
    // Long entries are cut after the field that crosses the preview size.
    let long = stream.row_cells(1).1;
    assert!(
        long.len() > CELL_PREVIEW && long.len() < CELL_PREVIEW + 40,
        "{}",
        long.len()
    );
}

#[test]
fn text_values_have_no_rows() {
    let string = loaded(KeyType::String, ValueData::String(b"x".to_vec()), 1, None);
    assert_eq!(string.row_count(), 0);
    assert_eq!(string.row_cells(0), (None, String::new()));
    assert_eq!(string.row_value(0), None);
    let missing = loaded(KeyType::None, ValueData::Missing, 0, None);
    assert_eq!(missing.row_count(), 0);
    assert!(missing.text(true).is_none());
}

#[test]
fn row_values_are_whole() {
    let hash = loaded(
        KeyType::Hash,
        ValueData::Hash(vec![(b"f".to_vec(), vec![b'x'; 10_000])]),
        1,
        None,
    );
    assert_eq!(hash.row_value(0).unwrap().len(), 10_000);
    assert_eq!(hash.row_value(1), None);
    let zset = loaded(
        KeyType::ZSet,
        ValueData::ZSet(vec![(b"member".to_vec(), 2.0)]),
        1,
        None,
    );
    assert_eq!(zset.row_value(0).unwrap(), b"member");
    let list = loaded(KeyType::List, ValueData::List(bytes(&["a"])), 1, None);
    assert_eq!(list.row_value(0).unwrap(), b"a");
    let text = list.row_text(0).unwrap();
    assert!(!text.editable);
}

#[test]
fn stream_entries_become_ordered_json() {
    let stream = loaded(
        KeyType::Stream,
        ValueData::Stream(vec![StreamEntry {
            id: "1-0".into(),
            fields: vec![
                (b"zeta".to_vec(), b"1".to_vec()),
                (b"alpha".to_vec(), b"say \"hi\"\n".to_vec()),
                (b"zeta".to_vec(), b"\xff".to_vec()),
            ],
        }]),
        1,
        None,
    );
    let json = String::from_utf8(stream.row_value(0).unwrap()).unwrap();
    // Field order and repeated fields are kept; bytes that are not text are escaped.
    assert_eq!(
        json,
        r#"{"zeta":"1","alpha":"say \"hi\"\n","zeta":"\\xff"}"#
    );
    let text = stream.row_text(0).unwrap();
    assert!(text.is_json && !text.editable);
    assert!(text.text.starts_with("{\n  \"zeta\": \"1\",\n  \"alpha\""));
}

#[test]
fn strings_are_editable_only_when_whole_and_writable() {
    let whole = loaded(
        KeyType::String,
        ValueData::String(b"hello".to_vec()),
        5,
        None,
    );
    let text = whole.text(true).unwrap();
    assert_eq!(text.text, "hello");
    assert!(text.editable && !text.is_json && !text.reformatted_json);
    assert!(!whole.text(false).unwrap().editable, "read-only connection");
    assert!(!whole.is_preview());
    assert_eq!(whole.preview_notice(), None);

    let preview = loaded(
        KeyType::String,
        ValueData::String(vec![b'x'; 1 << 20]),
        5 << 20,
        None,
    );
    assert!(preview.is_preview());
    assert!(!preview.text(true).unwrap().editable);
    assert_eq!(
        preview.preview_notice().as_deref(),
        Some("Showing the first 1.0 MB of 5.0 MB. Large values are read-only.")
    );

    let binary = loaded(
        KeyType::String,
        ValueData::String(b"a\x00b".to_vec()),
        3,
        None,
    );
    // Quoted like redis-cli, so the escapes read back as the same bytes.
    let text = binary.text(true).unwrap();
    assert_eq!(text.text, r#""a\x00b""#);
    assert!(text.editable);
    // Shown for reading only, escaped without quotes.
    let text = TextValue::new(b"a\x00b", true);
    assert_eq!(text.text, "a\\x00b");
    assert!(
        !text.editable,
        "escaped text would not save back the same bytes"
    );

    // JSON documents are saved with JSON.SET, and only as JSON.
    let json = loaded(KeyType::Json, ValueData::Json(r#"{"a":1}"#.into()), 7, None);
    let text = json.text(true).unwrap();
    assert!(text.is_json && text.editable);
    assert!(!json.text(false).unwrap().editable, "read-only connection");
    assert_eq!(json.text_edit(true).unwrap().formats(), [ValueFormat::Json]);
}

#[test]
fn pretty_prints_json_without_changing_it() {
    let stored = r#"{"zeta":1.50,"alpha":[1,2,{}],"big":123456789012345678901234567890,"s":"a, b: {c} [d] \"e\"","empty":[],"nested":{"x":null}}"#;
    let text = TextValue::new(stored.as_bytes(), true);
    assert!(text.is_json && text.reformatted_json && text.editable);
    assert_eq!(
        text.text,
        r#"{
  "zeta": 1.50,
  "alpha": [
    1,
    2,
    {}
  ],
  "big": 123456789012345678901234567890,
  "s": "a, b: {c} [d] \"e\"",
  "empty": [],
  "nested": {
    "x": null
  }
}"#
    );
    // Unchanged text saves back exactly as stored: same key order, same numbers.
    assert_eq!(text.bytes_to_save(&text.text), stored.as_bytes());
    // An edit saves compact.
    let edited = text.text.replace("1.50", "2");
    assert_eq!(
        String::from_utf8(text.bytes_to_save(&edited)).unwrap(),
        stored.replace("1.50", "2")
    );
    // Text that no longer parses is saved as typed rather than mangled.
    assert_eq!(text.bytes_to_save("{\"a\": "), b"{\"a\": ");
}

#[test]
fn leaves_json_that_needs_no_indenting_alone() {
    let pretty = "{\n  \"a\": [\n    1\n  ]\n}";
    let text = TextValue::new(pretty.as_bytes(), true);
    assert!(text.is_json && !text.reformatted_json);
    assert_eq!(text.text, pretty);
    // Saved as typed, whitespace included.
    let edited = "{\n  \"a\": [\n    2\n  ]\n}";
    assert_eq!(text.bytes_to_save(edited), edited.as_bytes());

    for not_a_document in ["123", "\"str\"", "{oops", "[1,]", " ", "", "nul"] {
        let text = TextValue::new(not_a_document.as_bytes(), true);
        assert!(!text.is_json, "{not_a_document:?}");
        assert_eq!(text.text, not_a_document);
        assert_eq!(text.bytes_to_save("x"), b"x");
    }
    // Spaced some other way: shown and saved as stored.
    let text = TextValue::new(b"  [ ]  ", true);
    assert!(text.is_json && !text.reformatted_json);
    assert_eq!(text.text, "  [ ]  ");
    let unicode = TextValue::new("{\"ü\":\"🦀\"}".as_bytes(), true);
    assert_eq!(unicode.text, "{\n  \"ü\": \"🦀\"\n}");
}

#[test]
fn saves_edited_json_in_the_spacing_it_was_stored_in() {
    // Python's `json.dumps`: a space after each `,` and `:`.
    let stored = r#"{"a": 1, "b": [1, 2, {"c": "x, y: z"}], "d": {}}"#;
    let text = TextValue::new(stored.as_bytes(), true);
    assert!(text.is_json && text.reformatted_json && text.editable);
    assert!(
        text.text.starts_with("{\n  \"a\": 1,\n  \"b\": [\n"),
        "{}",
        text.text
    );
    assert_eq!(text.bytes_to_save(&text.text), stored.as_bytes());
    let edited = text.text.replace(": 1,", ": 3,");
    assert_eq!(
        String::from_utf8(text.bytes_to_save(&edited)).unwrap(),
        r#"{"a": 3, "b": [1, 2, {"c": "x, y: z"}], "d": {}}"#
    );

    // Only commas, or only colons, spaced.
    for stored in [r#"{"a":1, "b":2}"#, r#"{"a": 1,"b": 2}"#, "[1, 2]"] {
        let text = TextValue::new(stored.as_bytes(), true);
        assert!(text.reformatted_json, "{stored}");
        assert_eq!(
            text.bytes_to_save(&text.text),
            stored.as_bytes(),
            "{stored}"
        );
    }

    // Laid out by someone else: kept exactly, whatever is edited.
    for stored in [
        "{\n    \"a\": 1,\n    \"b\": 2\n}",
        "{\n\t\"a\": 1\n}",
        r#"{"a" : 1}"#,
        r#"{"a": 1,  "b": 2}"#,
    ] {
        let text = TextValue::new(stored.as_bytes(), true);
        assert!(
            text.is_json && !text.reformatted_json && text.editable,
            "{stored}"
        );
        assert_eq!(text.text, stored);
        let edited = stored.replace('1', "3");
        assert_eq!(text.bytes_to_save(&edited), edited.as_bytes(), "{stored}");
    }

    // CRLF line breaks (JSON written on Windows): shown as LF, as laid
    // out, editable, and saved with CRLF again.
    let stored = "{\r\n    \"a\": 1,\r\n    \"b\": [true]\r\n}\r\n";
    let text = TextValue::new(stored.as_bytes(), true);
    assert!(text.is_json && !text.reformatted_json && text.editable);
    assert_eq!(text.notice, None);
    assert_eq!(text.text, stored.replace("\r\n", "\n"));
    assert_eq!(text.bytes_to_save(&text.text), stored.as_bytes());
    let edited = text.text.replace('1', "2");
    assert_eq!(
        text.bytes_to_save(&edited),
        stored.replace('1', "2").as_bytes()
    );
    assert_eq!(
        text.bytes_to_save(&format!("{edited}\n")),
        format!("{}\r\n", stored.replace('1', "2")).as_bytes()
    );

    // A lone `\r`, or line breaks of both kinds: they would not stay as
    // they are, so indented for reading, read-only, and saying so.
    for stored in ["{\"a\":\r1}", "{\r\n\"a\": 1\n}"] {
        let text = TextValue::new(stored.as_bytes(), true);
        assert!(text.is_json && !text.editable, "{stored:?}");
        let notice = text.notice.unwrap();
        assert!(
            notice.contains("JSON") && notice.contains("Text"),
            "{notice}"
        );
    }
}

/// Picking rows for deleting never throws away what was typed into the
/// open one.
#[test]
fn row_clicks_keep_unsaved_edits() {
    let mut picked = BTreeSet::new();
    // A click opens a row and picks it.
    assert_eq!(
        click_row(&mut picked, None, false, 1, false),
        RowClick::Open(1)
    );
    assert_eq!(picked, BTreeSet::from([1]));
    // ⌘-clicks pick more, or fewer, and leave the open row (edited or not).
    for dirty in [false, true] {
        assert_eq!(
            click_row(&mut picked, Some(1), dirty, 3, true),
            RowClick::Keep
        );
        assert_eq!(
            click_row(&mut picked, Some(1), dirty, 4, true),
            RowClick::Keep
        );
        assert_eq!(picked, BTreeSet::from([1, 3, 4]));
        assert_eq!(
            click_row(&mut picked, Some(1), dirty, 3, true),
            RowClick::Keep
        );
        assert_eq!(
            click_row(&mut picked, Some(1), dirty, 4, true),
            RowClick::Keep
        );
    }
    assert_eq!(picked, BTreeSet::from([1]));
    // Another row while the open one has edits: ask, change nothing yet.
    assert_eq!(
        click_row(&mut picked, Some(1), true, 2, false),
        RowClick::AskFirst(2)
    );
    assert_eq!(picked, BTreeSet::from([1]));
    // Agreed to: open it.
    assert_eq!(
        click_row(&mut picked, Some(1), false, 2, false),
        RowClick::Open(2)
    );
    assert_eq!(picked, BTreeSet::from([2]));
    // The open row itself: kept as it is, edits and all.
    picked.insert(5);
    assert_eq!(
        click_row(&mut picked, Some(2), true, 2, false),
        RowClick::Keep
    );
    assert_eq!(picked, BTreeSet::from([2]));
}

#[test]
fn summarizes_values() {
    let list = loaded(
        KeyType::List,
        ValueData::List(bytes(&["a", "b"])),
        20_000,
        Some(Cursor::Offset(2)),
    );
    assert_eq!(list.summary().as_deref(), Some("20,000 items"));
    assert_eq!(list.footer(), "Showing 2 of 20,000");
    assert_eq!(list.ttl_text(), "No expiry");
    assert!(list.has_more());
    let mut string = loaded(
        KeyType::String,
        ValueData::String(b"x".to_vec()),
        1536,
        None,
    );
    string.meta.ttl_ms = 90_000;
    assert_eq!(string.summary().as_deref(), Some("1.5 KB"));
    assert_eq!(string.ttl_text(), "1m 30s");
    assert!(!string.has_more());
    let other = loaded(
        KeyType::Other("MBbloom--".into()),
        ValueData::Unsupported("MBbloom--".into()),
        0,
        None,
    );
    assert_eq!(other.summary(), None);
}

#[test]
fn appends_pages_in_order_only() {
    let mut list = loaded(
        KeyType::List,
        ValueData::List(bytes(&["a", "b"])),
        5,
        Some(Cursor::Offset(2)),
    );
    let other_load = loaded(
        KeyType::List,
        ValueData::List(vec![]),
        5,
        Some(Cursor::Offset(2)),
    );

    // A page fetched for another load of the same key is dropped.
    let stray = MorePage {
        load: other_load.id,
        from: Cursor::Offset(2),
        page: page(ValueData::List(bytes(&["zz"])), 5, None),
    };
    assert!(!list.apply_page(stray));

    let next = MorePage {
        load: list.id,
        from: Cursor::Offset(2),
        page: page(
            ValueData::List(bytes(&["c", "d"])),
            6,
            Some(Cursor::Offset(4)),
        ),
    };
    let duplicate = MorePage {
        load: list.id,
        from: Cursor::Offset(2),
        page: page(
            ValueData::List(bytes(&["c", "d"])),
            6,
            Some(Cursor::Offset(4)),
        ),
    };
    assert!(list.apply_page(next));
    assert!(!list.apply_page(duplicate), "the same page twice");
    assert_eq!(list.row_count(), 4);
    assert_eq!(list.total, 6);
    assert_eq!(list.row_cells(3).1, "d");

    let last = MorePage {
        load: list.id,
        from: Cursor::Offset(4),
        page: page(ValueData::List(bytes(&["e", "f"])), 6, None),
    };
    assert!(list.apply_page(last));
    assert!(!list.has_more());
    assert_eq!(list.footer(), "Showing 6 of 6");
}
