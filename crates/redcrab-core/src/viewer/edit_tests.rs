//! The edit models without Redis: text in every format, row edits, the add
//! and new key forms, typed TTLs and scores, and changes applied to a
//! loaded value.

use proptest::prelude::*;

use super::*;
use crate::{
    ListEnd, NewValue,
    formats::{self, ValueFormat},
    value::StreamEntry,
};

fn key() -> Key {
    Key::from(&b"k"[..])
}

fn loaded(ty: KeyType, data: ValueData, total: u64, next: Option<Cursor>) -> Loaded {
    Loaded::new(
        key(),
        KeyMeta { ty, ttl_ms: -1 },
        ValuePage { data, total, next },
        None,
    )
}

/// What a delete answers: `count` removed, `left` left.
fn removed(count: u64, left: u64) -> Outcome {
    Outcome::Removed(crate::Removed { count, left })
}

fn bytes(items: &[&str]) -> Vec<Vec<u8>> {
    items.iter().map(|s| s.as_bytes().to_vec()).collect()
}

fn hash(pairs: &[(&str, &str)]) -> Loaded {
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = pairs
        .iter()
        .map(|(f, v)| (f.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
    let n = pairs.len() as u64;
    loaded(KeyType::Hash, ValueData::Hash(pairs), n, None)
}

fn zset(members: &[(&str, f64)]) -> Loaded {
    let members: Vec<(Vec<u8>, f64)> = members
        .iter()
        .map(|(m, s)| (m.as_bytes().to_vec(), *s))
        .collect();
    let n = members.len() as u64;
    loaded(KeyType::ZSet, ValueData::ZSet(members), n, None)
}

#[test]
fn strings_open_in_their_detected_format() {
    let gz = formats::gzip(br#"{"a":1}"#);
    let edit = TextEdit::new(gz.clone(), true);
    assert_eq!(edit.format(), ValueFormat::Gzip);
    assert_eq!(edit.text(), "{\n  \"a\": 1\n}");
    assert!(edit.editable() && edit.view().is_json);
    // Unchanged, nothing to save; changed, gzipped again.
    assert_eq!(edit.to_save(edit.text()).unwrap(), None);
    assert!(!edit.is_changed(edit.text()));
    let saved = edit.to_save("{\n  \"a\": 2\n}").unwrap().unwrap();
    assert_eq!(formats::gunzip(&saved, 100).unwrap(), br#"{"a":2}"#);

    let binary = TextEdit::new(vec![0xff, 0x00, 0x10], true);
    assert_eq!(binary.format(), ValueFormat::Hex);
    assert!(!binary.editable());
    assert!(binary.view().notice.as_deref().unwrap().contains("Text"));
    assert_eq!(binary.to_save(binary.text()).unwrap(), None);
    assert!(binary.to_save("changed").is_err());
}

#[test]
fn switching_formats_carries_the_draft_over() {
    let mut edit = TextEdit::new(b"hello".to_vec(), true);
    assert_eq!(edit.format(), ValueFormat::Text);
    // Bytes that are not Base64 cannot be shown as Base64.
    let err = edit.set_format(ValueFormat::Base64, None).unwrap_err();
    assert!(format!("{err:#}").contains("not valid Base64"), "{err:#}");
    assert_eq!(edit.format(), ValueFormat::Text);

    // A draft typed as text, viewed as hex, then as text again.
    edit.set_format(ValueFormat::Hex, Some("hello!")).unwrap();
    assert!(edit.text().contains("68 65 6c 6c 6f 21"), "{}", edit.text());
    assert!(edit.is_changed(edit.text()));
    edit.set_format(ValueFormat::Text, Some(edit.text().to_string().as_str()))
        .unwrap();
    assert_eq!(edit.text(), "hello!");
    assert_eq!(edit.to_save("hello!").unwrap().unwrap(), b"hello!");
    // Typing the stored value back makes it unchanged again.
    assert!(!edit.is_changed("hello"));
    assert_eq!(edit.to_save("hello").unwrap(), None);

    // Binary is shown as hex, and edited as quoted text.
    let mut edit = TextEdit::new(vec![0xff, 0xfe], true);
    assert_eq!(edit.format(), ValueFormat::Hex);
    assert!(edit.set_format(ValueFormat::Base64, None).is_err());
    edit.set_format(ValueFormat::Text, None).unwrap();
    assert_eq!(edit.text(), r#""\xff\xfe""#);
    assert!(edit.editable() && edit.view().notice.is_some());
    assert_eq!(edit.to_save(r#""\xff\xfe""#).unwrap(), None);
    assert_eq!(
        edit.to_save(r#""\xff\x00 \"q\"""#).unwrap().unwrap(),
        b"\xff\x00 \"q\""
    );
    // Unquoted plain text is fine too; unbalanced quotes or two values not.
    assert_eq!(edit.to_save("plain").unwrap().unwrap(), b"plain");
    assert!(edit.to_save(r#""\xff"#).is_err());
    assert!(edit.to_save(r#""a" "b""#).is_err());
    assert!(edit.to_save("").is_err());
    assert_eq!(edit.to_save(r#""""#).unwrap().unwrap(), b"");

    // A value stored as Base64 text is edited as Base64 text.
    let mut edit = TextEdit::new(b"aGVsbG8gd29ybGQ=".to_vec(), true);
    assert_eq!(edit.format(), ValueFormat::Base64);
    assert_eq!(edit.text(), "hello world");
    assert_eq!(
        edit.to_save("hello there").unwrap().unwrap(),
        b"aGVsbG8gdGhlcmU="
    );
    // Seen as text, the same value is the Base64 itself.
    edit.set_format(ValueFormat::Text, None).unwrap();
    assert_eq!(edit.text(), "aGVsbG8gd29ybGQ=");

    // A draft that does not fit its format cannot be carried anywhere.
    let mut edit = TextEdit::new(br#"{"a":1}"#.to_vec(), true);
    assert_eq!(edit.format(), ValueFormat::Json);
    assert!(edit.set_format(ValueFormat::Text, Some("{broken")).is_err());
    assert_eq!(edit.format(), ValueFormat::Json);
    let err = edit.to_save("{broken").unwrap_err().to_string();
    assert!(err.starts_with("not valid JSON"), "{err}");
}

#[test]
fn base64_is_written_back_in_its_own_style() {
    // URL-safe without padding, decoding to text.
    let stored = formats::Base64Style {
        url_safe: true,
        padded: false,
    }
    .encode("~~~ hi there ???".as_bytes());
    assert!(stored.contains('_') || stored.contains('-'), "{stored}");
    let edit = TextEdit::new(stored.clone().into_bytes(), true);
    assert_eq!(edit.format(), ValueFormat::Base64);
    assert_eq!(edit.to_save(edit.text()).unwrap(), None);
    let saved = edit.to_save("~~~ hi there ??? !").unwrap().unwrap();
    assert!(!saved.contains(&b'='));
    assert!(!saved.contains(&b'+') && !saved.contains(&b'/'));
    // Its style shows in the text: nothing to say.
    assert_eq!(edit.view().notice, None);
}

/// Base64 that does not show its alphabet or padding is written back in a
/// guess, and the view says so rather than claiming it keeps the style.
#[test]
fn base64_style_guesses_are_told() {
    // base64url without padding of `{"user":"bob","n":12}`, 28 characters:
    // it fits every style.
    let stored = b"eyJ1c2VyIjoiYm9iIiwibiI6MTJ9".to_vec();
    let edit = TextEdit::new(stored, true);
    assert_eq!(edit.format(), ValueFormat::Base64);
    let notice = edit.view().notice.clone().unwrap();
    assert!(
        notice.contains("standard alphabet") && notice.contains("padded"),
        "{notice}"
    );
    // Only the padding is unknown.
    let stored = formats::Base64Style::default().encode(b"~~~ hi there ???!!");
    assert!(stored.contains(['+', '/']) && stored.len() % 4 == 0 && !stored.contains('='));
    let notice = TextEdit::new(stored.into_bytes(), true)
        .view()
        .notice
        .clone()
        .unwrap();
    assert!(notice.contains("whether it is padded"), "{notice}");
    // Read-only: nothing will be written, nothing to say.
    let edit = TextEdit::new(b"eyJ1c2VyIjoiYm9iIiwibiI6MTJ9".to_vec(), false);
    assert_eq!(edit.view().notice, None);
}

/// A small value can decode into far more text than an editor should get:
/// such a value is shown as stored instead, picked by hand or not.
#[test]
fn decoding_never_makes_too_much_text() {
    // Inflates past the limit: 5 MB of text from a few KB.
    let big = formats::gzip(&vec![b'a'; 5 << 20]);
    // Inflates to 1.5 MB of NULs, each shown as four characters.
    let quoted = formats::gzip(&vec![0u8; 3 << 19]);
    for stored in [big, quoted] {
        assert!(stored.len() < 100_000);
        let mut edit = TextEdit::new(stored.clone(), true);
        assert_eq!(edit.format(), ValueFormat::Text);
        assert!(edit.text().len() <= 4 * stored.len() + 2);
        assert!(edit.editable());
        let notice = edit.view().notice.clone().unwrap();
        assert!(
            notice.contains("Gzip") && notice.contains("too"),
            "{notice}"
        );
        let err = edit.set_format(ValueFormat::Gzip, None).unwrap_err();
        assert!(format!("{err:#}").contains("too"), "{err:#}");
        assert!(formats::decode(&stored, ValueFormat::Gzip).is_err());
    }
    // Decoded text no longer than the value itself is always shown.
    let long = formats::Base64Style::default().encode(&vec![b'a'; 5 << 20]);
    let edit = TextEdit::new(long.into_bytes(), true);
    assert_eq!(edit.format(), ValueFormat::Base64);
    assert_eq!(edit.text().len(), 5 << 20);
}

/// JSON with CRLF line breaks opens as JSON, editable as laid out, and an
/// edit keeps the CRLFs.
#[test]
fn crlf_json_is_edited_as_json() {
    let stored = "{\r\n  \"a\": 1\r\n}";
    let edit = TextEdit::new(stored.as_bytes().to_vec(), true);
    assert_eq!(edit.format(), ValueFormat::Json);
    assert!(edit.editable() && edit.view().notice.is_none());
    assert_eq!(edit.text(), "{\n  \"a\": 1\n}");
    assert_eq!(edit.to_save(edit.text()).unwrap(), None);
    assert_eq!(
        edit.to_save("{\n  \"a\": 2\n}").unwrap(),
        Some(b"{\r\n  \"a\": 2\r\n}".to_vec())
    );
}

/// A value that looks like gzip or MessagePack but does not decode opens
/// as Text, as editable as picking Text makes it, and says why.
#[test]
fn values_that_do_not_decode_edit_as_text() {
    let mut trailing = formats::gzip(b"hello");
    trailing.extend_from_slice(b"garbage");
    // A map whose key is a map: valid MessagePack, not JSON.
    let map_key = b"\x81\x80\x01".to_vec();
    for (stored, looks) in [
        (trailing, ValueFormat::Gzip),
        (map_key, ValueFormat::MessagePack),
    ] {
        assert_eq!(formats::detect(&stored), looks);
        let edit = TextEdit::new(stored.clone(), true);
        assert_eq!(edit.format(), ValueFormat::Text);
        assert!(edit.editable());
        let notice = edit.view().notice.clone().unwrap();
        assert!(
            notice.contains(looks.label()) && notice.contains("does not decode"),
            "{notice}"
        );
        let mut picked = edit.clone();
        picked.set_format(ValueFormat::Text, None).unwrap();
        assert_eq!(picked.text(), edit.text());
        assert!(picked.editable());
        assert_eq!(edit.to_save(edit.text()).unwrap(), None);
        let edited = format!("{}x", &edit.text()[..edit.text().len() - 1]);
        let mut expected = stored.clone();
        expected.push(b'x');
        assert_eq!(
            edit.to_save(&format!("{edited}\"")).unwrap(),
            Some(expected)
        );
    }
}

#[test]
fn messagepack_is_editable_only_when_exact() {
    let exact = formats::encode(r#"{"n":1,"s":"x"}"#, ValueFormat::MessagePack).unwrap();
    let edit = TextEdit::new(exact.clone(), true);
    assert_eq!(edit.format(), ValueFormat::MessagePack);
    assert!(edit.editable() && edit.view().is_json);
    assert_eq!(edit.to_save(edit.text()).unwrap(), None);
    let saved = edit
        .to_save("{\n  \"n\": 2,\n  \"s\": \"x\"\n}")
        .unwrap()
        .unwrap();
    assert_eq!(
        saved,
        formats::encode(r#"{"n":2,"s":"x"}"#, ValueFormat::MessagePack).unwrap()
    );
    assert!(edit.to_save("{oops").is_err());

    // 1 stored as int16: valid, but not written back the same way.
    let long = b"\x81\xa1n\xd1\x00\x01".to_vec();
    let edit = TextEdit::new(long, true);
    assert_eq!(edit.format(), ValueFormat::MessagePack);
    assert!(!edit.editable());
    assert!(edit.view().notice.is_some());
}

#[test]
fn json_documents_are_saved_with_json_set() {
    let doc = loaded(
        KeyType::Json,
        ValueData::Json(r#"{"a":[1,2]}"#.into()),
        11,
        None,
    );
    let edit = doc.text_edit(true).unwrap();
    assert_eq!(edit.format(), ValueFormat::Json);
    assert_eq!(doc.save_text(&edit, edit.text()).unwrap(), None);
    let edited = edit.text().replace('2', "3");
    assert_eq!(
        doc.save_text(&edit, &edited).unwrap(),
        Some(Mutation::SetJson {
            key: key(),
            json: r#"{"a":[1,3]}"#.into()
        })
    );
    assert!(doc.save_text(&edit, "[").is_err());
    let mut edit = edit;
    assert!(edit.set_format(ValueFormat::Text, None).is_err());

    let string = loaded(KeyType::String, ValueData::String(b"v".to_vec()), 1, None);
    let edit = string.text_edit(true).unwrap();
    assert_eq!(
        string.save_text(&edit, "w").unwrap(),
        Some(Mutation::SetString {
            key: key(),
            value: b"w".to_vec()
        })
    );
    // A preview of a big string cannot be saved.
    let preview = loaded(KeyType::String, ValueData::String(b"vv".to_vec()), 10, None);
    let edit = preview.text_edit(true).unwrap();
    assert!(preview.save_text(&edit, "x").is_err());
}

#[test]
fn hash_rows_set_or_rename() {
    let value = hash(&[("name", "Ada"), ("bin\u{1}", "x")]);
    let edit = value.row_edit(0, true).unwrap();
    let name = edit.name().unwrap();
    assert_eq!(
        (name.label, name.text.as_str(), name.editable),
        ("Field", "name", true)
    );
    assert_eq!(edit.value().text(), "Ada");
    assert_eq!(value.save_row(&edit, "Ada", Some("name")).unwrap(), None);
    // A new value only: the field must still exist (never a plain HSET,
    // which would bring back a field deleted or expired since).
    let change = value
        .save_row(&edit, "Grace", Some("name"))
        .unwrap()
        .unwrap();
    assert_eq!(
        change,
        Mutation::HashRenameField {
            key: key(),
            from: b"name".to_vec(),
            to: b"name".to_vec(),
            value: Some(b"Grace".to_vec()),
            overwrite: false
        }
    );
    assert_eq!(change.done_message(&Outcome::Done), "Field saved");
    assert_eq!(
        value.save_row(&edit, "Ada", Some("first")).unwrap(),
        Some(Mutation::HashRenameField {
            key: key(),
            from: b"name".to_vec(),
            to: b"first".to_vec(),
            value: None,
            overwrite: false
        })
    );
    let both = value
        .save_row(&edit, "Grace", Some("first"))
        .unwrap()
        .unwrap();
    assert!(matches!(&both, Mutation::HashRenameField { value: Some(v), .. } if v == b"Grace"));
    assert_eq!(
        both.overwriting(),
        Some(Mutation::HashRenameField {
            key: key(),
            from: b"name".to_vec(),
            to: b"first".to_vec(),
            value: Some(b"Grace".to_vec()),
            overwrite: true
        })
    );
    assert!(
        both.overwrite_question()
            .unwrap()
            .contains("\u{201c}first\u{201d}")
    );

    // A field name that is not plain text cannot be renamed.
    let edit = value.row_edit(1, true).unwrap();
    assert!(!edit.name().unwrap().editable);
    assert!(value.save_row(&edit, "x", Some("other")).is_err());
    assert!(
        value
            .save_row(&edit, "y", Some(&edit.name().unwrap().text.clone()))
            .unwrap()
            .is_some()
    );

    // Nor can a name with a line break, which a one-line input cannot
    // hold: it is shown escaped, so saving the value leaves it alone.
    let value = hash(&[
        ("line1\nline2", "v"),
        ("line1line2", "keep me"),
        ("a\tb", "t"),
    ]);
    let edit = value.row_edit(0, true).unwrap();
    let name = edit.name().unwrap();
    assert_eq!(
        (name.text.as_str(), name.editable),
        ("line1\\nline2", false)
    );
    let name = name.text.clone();
    assert_eq!(
        value.save_row(&edit, "v2", Some(&name)).unwrap(),
        Some(Mutation::HashRenameField {
            key: key(),
            from: b"line1\nline2".to_vec(),
            to: b"line1\nline2".to_vec(),
            value: Some(b"v2".to_vec()),
            overwrite: false
        })
    );
    // What the input would hold with the line break dropped is a rename,
    // which is refused.
    assert!(value.save_row(&edit, "v2", Some("line1line2")).is_err());
    // A tab stays in a one-line input: still editable.
    let edit = value.row_edit(2, true).unwrap();
    assert_eq!(
        (
            edit.name().unwrap().text.as_str(),
            edit.name().unwrap().editable
        ),
        ("a\tb", true)
    );

    // Nothing is editable on a read-only connection.
    let value = hash(&[("name", "Ada")]);
    let edit = value.row_edit(0, false).unwrap();
    assert!(!edit.editable());
    assert!(value.save_row(&edit, "Grace", Some("name")).is_err());
    assert!(value.row_edit(2, true).is_none());
}

#[test]
fn list_set_and_zset_rows() {
    let list = loaded(KeyType::List, ValueData::List(bytes(&["a", "b"])), 2, None);
    let edit = list.row_edit(1, true).unwrap();
    assert!(edit.name().is_none());
    assert_eq!(
        list.save_row(&edit, "c", None).unwrap(),
        Some(Mutation::ListSet {
            key: key(),
            index: 1,
            expected: b"b".to_vec(),
            value: b"c".to_vec()
        })
    );

    let set = loaded(KeyType::Set, ValueData::Set(bytes(&["m"])), 1, None);
    let edit = set.row_edit(0, true).unwrap();
    let replace = set.save_row(&edit, "n", None).unwrap().unwrap();
    assert_eq!(
        replace,
        Mutation::SetReplace {
            key: key(),
            from: b"m".to_vec(),
            to: b"n".to_vec(),
            overwrite: false
        }
    );
    assert!(replace.overwriting().is_some());

    let z = zset(&[("p", 1.5), ("q", f64::INFINITY)]);
    let edit = z.row_edit(0, true).unwrap();
    assert_eq!(edit.name().unwrap().text, "1.5");
    assert_eq!(z.row_edit(1, true).unwrap().name().unwrap().text, "inf");
    assert_eq!(z.save_row(&edit, "p", Some("1.5")).unwrap(), None);
    // Same number spelled differently: no change.
    assert_eq!(z.save_row(&edit, "p", Some("1.50")).unwrap(), None);
    assert_eq!(
        z.save_row(&edit, "p", Some("-2")).unwrap(),
        Some(Mutation::ZSetRename {
            key: key(),
            from: b"p".to_vec(),
            to: b"p".to_vec(),
            score: Some(-2.0),
            overwrite: false
        })
    );
    assert_eq!(
        z.save_row(&edit, "r", Some("1.5")).unwrap(),
        Some(Mutation::ZSetRename {
            key: key(),
            from: b"p".to_vec(),
            to: b"r".to_vec(),
            score: None,
            overwrite: false
        })
    );
    assert!(z.save_row(&edit, "p", Some("lots")).is_err());
    assert!(z.save_row(&edit, "p", Some("nan")).is_err());

    let stream = loaded(
        KeyType::Stream,
        ValueData::Stream(vec![StreamEntry {
            id: "1-0".into(),
            fields: vec![(b"f".to_vec(), b"v".to_vec())],
        }]),
        1,
        None,
    );
    let edit = stream.row_edit(0, true).unwrap();
    assert!(!edit.editable());
    assert_eq!(
        stream.save_row(&edit, edit.value().text(), None).unwrap(),
        None
    );
}

#[test]
fn deletes_selected_rows() {
    let value = hash(&[("a", "1"), ("b", "2"), ("c", "3")]);
    assert_eq!(
        value.delete_rows(&[2, 0, 2, 9]),
        Some(Mutation::HashDelete {
            key: key(),
            fields: bytes(&["a", "c"])
        })
    );
    assert_eq!(value.delete_rows(&[]), None);
    assert_eq!(value.delete_rows(&[7]), None);
    let list = loaded(
        KeyType::List,
        ValueData::List(bytes(&["x", "x", "y"])),
        3,
        None,
    );
    assert_eq!(
        list.delete_rows(&[1, 2]),
        Some(Mutation::ListRemoveAt {
            key: key(),
            elements: vec![(1, b"x".to_vec()), (2, b"y".to_vec())]
        })
    );
    let string = loaded(KeyType::String, ValueData::String(b"s".to_vec()), 1, None);
    assert_eq!(string.delete_rows(&[0]), None);
}

#[test]
fn add_forms_build_changes() {
    let add = |ty: KeyType| AddRow::new(key(), &ty).unwrap();
    assert!(AddRow::new(key(), &KeyType::String).is_none());
    assert_eq!(
        add(KeyType::Hash).build("f", "v", ListEnd::Tail).unwrap(),
        Mutation::HashSet {
            key: key(),
            field: b"f".to_vec(),
            value: b"v".to_vec(),
            overwrite: false
        }
    );
    assert!(add(KeyType::List).has_ends());
    assert_eq!(
        add(KeyType::List).build("", "e", ListEnd::Head).unwrap(),
        Mutation::ListPush {
            key: key(),
            values: bytes(&["e"]),
            end: ListEnd::Head
        }
    );
    assert_eq!(add(KeyType::ZSet).name_label(), Some("Score"));
    assert!(add(KeyType::ZSet).build("x", "m", ListEnd::Tail).is_err());
    assert_eq!(
        add(KeyType::Stream)
            .build(" ", r#"name "Ada Lovelace" age 36"#, ListEnd::Tail)
            .unwrap(),
        Mutation::StreamAdd {
            key: key(),
            id: "*".into(),
            fields: vec![
                (b"name".to_vec(), b"Ada Lovelace".to_vec()),
                (b"age".to_vec(), b"36".to_vec())
            ]
        }
    );
    assert!(
        add(KeyType::Stream)
            .build("1-x", "f v", ListEnd::Tail)
            .is_err()
    );
    assert!(
        add(KeyType::Stream)
            .build("*", "odd", ListEnd::Tail)
            .is_err()
    );
}

#[test]
fn new_key_form() {
    let created = new_key(NewKeyType::Hash, "user:1", "name", "Ada", "1h").unwrap();
    assert_eq!(
        created,
        Mutation::CreateKey {
            key: Key::from(&b"user:1"[..]),
            value: NewValue::Hash(vec![(b"name".to_vec(), b"Ada".to_vec())]),
            ttl: Some(3600)
        }
    );
    assert_eq!(created.done_message(&Outcome::Done), "Created user:1");
    assert!(new_key(NewKeyType::String, "", "", "v", "").is_err());
    assert!(new_key(NewKeyType::Json, "j", "", "{", "").is_err());
    assert!(new_key(NewKeyType::ZSet, "z", "high", "m", "").is_err());
    assert!(new_key(NewKeyType::String, "s", "", "v", "0").is_err());
    assert!(new_key(NewKeyType::String, "s", "", "v", "soon").is_err());
    assert_eq!(
        new_key(NewKeyType::Stream, "s", "", r#"{"b":1,"a":"x"}"#, "").unwrap(),
        Mutation::CreateKey {
            key: Key::from(&b"s"[..]),
            value: NewValue::Stream {
                id: "*".into(),
                fields: vec![
                    (b"b".to_vec(), b"1".to_vec()),
                    (b"a".to_vec(), b"x".to_vec())
                ]
            },
            ttl: None
        }
    );
    for ty in NewKeyType::ALL {
        assert!(!ty.label().is_empty());
    }
}

#[test]
fn ttls_as_typed() {
    for (input, expected) in [
        ("", None),
        ("  ", None),
        ("none", None),
        ("Never", None),
        ("persist", None),
        ("90", Some(90)),
        ("90s", Some(90)),
        ("5m", Some(300)),
        ("1h 30m", Some(5400)),
        ("1h30m", Some(5400)),
        ("1 hour, 2 minutes", Some(3720)),
        ("2d", Some(172_800)),
        ("1w 1s", Some(604_801)),
    ] {
        assert_eq!(parse_ttl(input).unwrap(), expected, "{input:?}");
    }
    for bad in [
        "0",
        "0s",
        "1x",
        "h",
        "1.5h",
        "-5",
        "99999999999999999999",
        "1h 30q",
    ] {
        assert!(parse_ttl(bad).is_err(), "{bad:?}");
    }
    assert_eq!(ttl_input(5400), "1h 30m");
    assert_eq!(ttl_input(90_061), "1d 1h 1m 1s");
    assert_eq!(ttl_input(0), "0s");
    assert_eq!(
        set_ttl(&key(), "").unwrap(),
        Mutation::SetTtl {
            key: key(),
            seconds: None
        }
    );
}

/// The TTL dialog starts with the TTL read at load time; saving it as it
/// is changes nothing (it would push the expiry back to that amount).
#[test]
fn untouched_ttl_inputs_change_nothing() {
    assert_eq!(ttl_prefill(9_999), "10s");
    assert_eq!(ttl_prefill(3_600_000), "1h");
    assert_eq!(ttl_prefill(-1), "");
    assert_eq!(ttl_prefill(0), "");
    let prefilled = ttl_prefill(3_599_001);
    assert_eq!(change_ttl(&key(), &prefilled, &prefilled).unwrap(), None);
    assert_eq!(change_ttl(&key(), &prefilled, " 1h ").unwrap(), None);
    // No expiry, left empty: no PERSIST either.
    assert_eq!(change_ttl(&key(), "", "").unwrap(), None);
    assert_eq!(
        change_ttl(&key(), &prefilled, "2h").unwrap(),
        Some(Mutation::SetTtl {
            key: key(),
            seconds: Some(7200)
        })
    );
    assert_eq!(
        change_ttl(&key(), &prefilled, "").unwrap(),
        Some(Mutation::SetTtl {
            key: key(),
            seconds: None
        })
    );
    assert!(change_ttl(&key(), &prefilled, "soon").is_err());
}

#[test]
fn renames_as_typed() {
    assert_eq!(rename_key(&key(), "k").unwrap(), None);
    assert!(rename_key(&key(), "").is_err());
    assert_eq!(
        rename_key(&key(), "k2 🦀").unwrap(),
        Some(Mutation::Rename {
            from: key(),
            to: Key::from("k2 🦀".as_bytes())
        })
    );
}

#[test]
fn rows_open_across_a_rename_save_under_the_new_name() {
    let mut value = hash(&[("a", "1")]);
    let edit = value.row_edit(0, true).unwrap();
    let renamed = Key::from(&b"k2"[..]);
    let rename = rename_key(&key(), "k2").unwrap().unwrap();
    assert_eq!(value.apply(&rename, &Outcome::Done), Applied::Patched(None));
    let change = value.save_row(&edit, "2", None).unwrap().unwrap();
    assert_eq!(change.key(), &renamed);
}

#[test]
fn applies_changes_to_hashes() {
    let mut value = hash(&[("a", "1"), ("b", "2")]);
    let set = Mutation::HashSet {
        key: key(),
        field: b"b".to_vec(),
        value: b"20".to_vec(),
        overwrite: true,
    };
    assert_eq!(
        value.apply(&set, &Outcome::Created(false)),
        Applied::Patched(Some(1))
    );
    assert_eq!(value.row_value(1).unwrap(), b"20");
    let add = Mutation::HashSet {
        key: key(),
        field: b"c".to_vec(),
        value: b"3".to_vec(),
        overwrite: false,
    };
    assert_eq!(
        value.apply(&add, &Outcome::Created(true)),
        Applied::Patched(Some(2))
    );
    assert_eq!(value.total, 3);
    // Renamed onto an existing field: that row goes.
    let rename = Mutation::HashRenameField {
        key: key(),
        from: b"c".to_vec(),
        to: b"a".to_vec(),
        value: None,
        overwrite: true,
    };
    assert_eq!(
        value.apply(&rename, &Outcome::Done),
        Applied::Patched(Some(1))
    );
    assert_eq!(value.row_cells(1), (Some("a".into()), "3".into()));
    assert_eq!(value.total, 2);
    let delete = value.delete_rows(&[0, 1]).unwrap();
    assert_eq!(value.apply(&delete, &removed(2, 0)), Applied::KeyGone);
    assert_eq!(value.meta.ty, KeyType::None);

    // Another key's change is not this value's.
    let mut value = hash(&[("a", "1")]);
    let other = Mutation::HashSet {
        key: Key::from(&b"other"[..]),
        field: b"a".to_vec(),
        value: b"x".to_vec(),
        overwrite: true,
    };
    assert_eq!(
        value.apply(&other, &Outcome::Created(false)),
        Applied::Reload
    );
    // A new field of a value not loaded to the end may come with a later page.
    let mut partial = loaded(
        KeyType::Hash,
        ValueData::Hash(vec![(b"a".to_vec(), b"1".to_vec())]),
        900,
        Some(Cursor::Scan(17)),
    );
    assert_eq!(
        partial.apply(&add, &Outcome::Created(true)),
        Applied::Reload
    );
}

#[test]
fn applies_changes_to_lists() {
    let mut list = loaded(
        KeyType::List,
        ValueData::List(bytes(&["a", "b", "c"])),
        10,
        Some(Cursor::Offset(3)),
    );
    let push = Mutation::ListPush {
        key: key(),
        values: bytes(&["x", "y"]),
        end: ListEnd::Head,
    };
    assert_eq!(
        list.apply(&push, &Outcome::Count(12)),
        Applied::Patched(Some(0))
    );
    assert_eq!(list.row_cells(1).1, "y");
    assert_eq!(
        (list.total, list.next.clone()),
        (12, Some(Cursor::Offset(5)))
    );
    let tail = Mutation::ListPush {
        key: key(),
        values: bytes(&["z"]),
        end: ListEnd::Tail,
    };
    // Not loaded to the end: the element is on a later page.
    assert_eq!(
        list.apply(&tail, &Outcome::Count(13)),
        Applied::Patched(None)
    );
    assert_eq!(list.row_count(), 5);
    let remove = list.delete_rows(&[0, 1]).unwrap();
    assert_eq!(list.apply(&remove, &removed(2, 11)), Applied::Patched(None));
    assert_eq!(list.row_cells(0), (Some("0".into()), "a".into()));
    assert_eq!(
        (list.total, list.next.clone()),
        (11, Some(Cursor::Offset(3)))
    );
    let set = Mutation::ListSet {
        key: key(),
        index: 2,
        expected: b"c".to_vec(),
        value: b"C".to_vec(),
    };
    assert_eq!(list.apply(&set, &Outcome::Done), Applied::Patched(Some(2)));
    assert_eq!(list.row_value(2).unwrap(), b"C");
}

#[test]
fn applies_changes_to_sets_zsets_and_streams() {
    let mut set = loaded(KeyType::Set, ValueData::Set(bytes(&["a", "b"])), 2, None);
    let add = Mutation::SetAdd {
        key: key(),
        members: bytes(&["c"]),
    };
    assert_eq!(
        set.apply(&add, &Outcome::Count(1)),
        Applied::Patched(Some(2))
    );
    let replace = Mutation::SetReplace {
        key: key(),
        from: b"a".to_vec(),
        to: b"c".to_vec(),
        overwrite: true,
    };
    // `c` was a member already: `a` just went.
    assert_eq!(
        set.apply(&replace, &Outcome::Created(false)),
        Applied::Patched(Some(1))
    );
    assert_eq!((set.row_count(), set.total), (2, 2));

    let mut z = zset(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
    let rename = Mutation::ZSetRename {
        key: key(),
        from: b"a".to_vec(),
        to: b"d".to_vec(),
        score: Some(2.5),
        overwrite: false,
    };
    assert_eq!(z.apply(&rename, &Outcome::Done), Applied::Patched(Some(1)));
    let order: Vec<String> = (0..3).map(|ix| z.row_cells(ix).1).collect();
    assert_eq!(order, ["b", "d", "c"]);
    // Equal scores sort by member.
    let add = Mutation::ZSetAdd {
        key: key(),
        member: b"a".to_vec(),
        score: 2.0,
        overwrite: false,
    };
    assert_eq!(
        z.apply(&add, &Outcome::Created(true)),
        Applied::Patched(Some(0))
    );
    assert_eq!(z.total, 4);

    let mut stream = loaded(KeyType::Stream, ValueData::Stream(Vec::new()), 0, None);
    let entry = Mutation::StreamAdd {
        key: key(),
        id: "*".into(),
        fields: vec![(b"f".to_vec(), b"v".to_vec())],
    };
    assert_eq!(
        stream.apply(&entry, &Outcome::StreamId("5-0".into())),
        Applied::Patched(Some(0))
    );
    assert_eq!(stream.row_cells(0).0.as_deref(), Some("5-0"));
    // Its last entry deleted, a stream stays: Redis keeps it, empty.
    let delete = stream.delete_rows(&[0]).unwrap();
    assert_eq!(
        stream.apply(&delete, &removed(1, 0)),
        Applied::Patched(None)
    );
    assert_eq!(
        (stream.meta.ty.clone(), stream.row_count(), stream.total),
        (KeyType::Stream, 0, 0)
    );
    assert!(matches!(stream.data, ValueData::Stream(_)));

    // TTL and rename change the header.
    let mut value = hash(&[("a", "1")]);
    let ttl = Mutation::SetTtl {
        key: key(),
        seconds: Some(60),
    };
    assert_eq!(value.apply(&ttl, &Outcome::Done), Applied::Patched(None));
    assert_eq!(value.meta.ttl_ms, 60_000);
    let rename = Mutation::Rename {
        from: key(),
        to: Key::from(&b"new"[..]),
    };
    assert_eq!(value.apply(&rename, &Outcome::Done), Applied::Patched(None));
    assert_eq!(&*value.key, b"new");
}

/// A name written that was not loaded may come again with a later page of
/// a value not loaded to the end, so it is loaded again instead of shown
/// twice. Names already loaded, or unchanged, are patched in place.
#[test]
fn new_names_in_partly_loaded_values_reload() {
    let more = Some(Cursor::Scan(7));
    let pairs = vec![
        (b"a".to_vec(), b"1".to_vec()),
        (b"b".to_vec(), b"2".to_vec()),
    ];
    let partial = || {
        loaded(
            KeyType::Hash,
            ValueData::Hash(pairs.clone()),
            9,
            more.clone(),
        )
    };
    let rename = |to: &str, value: Option<&str>, overwrite: bool| Mutation::HashRenameField {
        key: key(),
        from: b"a".to_vec(),
        to: to.as_bytes().to_vec(),
        value: value.map(|v| v.as_bytes().to_vec()),
        overwrite,
    };
    for overwrite in [false, true] {
        let mut hash = partial();
        assert_eq!(
            hash.apply(&rename("new", None, overwrite), &Outcome::Done),
            Applied::Reload
        );
    }
    // Onto a loaded field, or a value only: nothing new to come.
    let mut hash = partial();
    assert_eq!(
        hash.apply(&rename("b", None, true), &Outcome::Done),
        Applied::Patched(Some(0))
    );
    let mut hash = partial();
    assert_eq!(
        hash.apply(&rename("a", Some("9"), false), &Outcome::Done),
        Applied::Patched(Some(0))
    );
    assert_eq!(hash.row_cells(0), (Some("a".into()), "9".into()));

    let partial = || {
        loaded(
            KeyType::Set,
            ValueData::Set(bytes(&["m", "n"])),
            9,
            more.clone(),
        )
    };
    let replace = |to: &str| Mutation::SetReplace {
        key: key(),
        from: b"m".to_vec(),
        to: to.as_bytes().to_vec(),
        overwrite: false,
    };
    let mut set = partial();
    assert_eq!(
        set.apply(&replace("new"), &Outcome::Created(true)),
        Applied::Reload
    );
    // `to` was a member already: `m` just went.
    let mut set = partial();
    assert_eq!(
        set.apply(&replace("n"), &Outcome::Created(false)),
        Applied::Patched(Some(0))
    );
    // Loaded to the end, a new member is patched in.
    let mut set = loaded(KeyType::Set, ValueData::Set(bytes(&["m", "n"])), 2, None);
    assert_eq!(
        set.apply(&replace("new"), &Outcome::Created(true)),
        Applied::Patched(Some(0))
    );
}

/// Whether a delete emptied the key is the server's answer, not the total
/// counted at load time, which others may have changed since.
#[test]
fn deletes_trust_what_the_server_says_is_left() {
    // Someone added a member after the load: deleting both loaded ones
    // leaves it, so the key is not gone, and the rows are loaded again.
    let mut set = loaded(KeyType::Set, ValueData::Set(bytes(&["a", "b"])), 2, None);
    let delete = set.delete_rows(&[0, 1]).unwrap();
    assert_eq!(set.apply(&delete, &removed(2, 1)), Applied::Reload);
    assert_eq!(set.meta.ty, KeyType::Set);
    assert_eq!(set.total, 1);

    // Someone deleted one: removing the two left empties the key.
    let mut value = hash(&[("a", "1"), ("b", "2"), ("c", "3")]);
    let delete = value.delete_rows(&[0, 1]).unwrap();
    assert_eq!(value.apply(&delete, &removed(2, 0)), Applied::KeyGone);
    assert_eq!(value.meta.ty, KeyType::None);

    // Someone deleted one of those picked: fewer removed than asked.
    let mut z = zset(&[("p", 1.0), ("q", 2.0), ("r", 3.0)]);
    let delete = z.delete_rows(&[0, 1]).unwrap();
    assert_eq!(z.apply(&delete, &removed(1, 1)), Applied::Reload);

    // Nobody else: patched in place.
    let mut z = zset(&[("p", 1.0), ("q", 2.0), ("r", 3.0)]);
    let delete = z.delete_rows(&[0, 1]).unwrap();
    assert_eq!(z.apply(&delete, &removed(2, 1)), Applied::Patched(None));
    assert_eq!((z.row_count(), z.total), (1, 1));
}

#[test]
fn messages_say_what_happened() {
    let delete = Mutation::HashDelete {
        key: key(),
        fields: bytes(&["a", "b"]),
    };
    assert_eq!(delete.done_message(&removed(2, 0)), "Deleted 2 fields");
    assert_eq!(delete.done_message(&removed(1, 5)), "Deleted 1 field");
    assert!(delete.overwriting().is_none());
    let stream = Mutation::StreamAdd {
        key: key(),
        id: "*".into(),
        fields: Vec::new(),
    };
    assert_eq!(
        stream.done_message(&Outcome::StreamId("1-1".into())),
        "Added entry 1-1"
    );
}

proptest! {
    #[test]
    fn ttl_inputs_round_trip(seconds in 1u64..=crate::client::MAX_TTL_SECS) {
        prop_assert_eq!(parse_ttl(&ttl_input(seconds)).unwrap(), Some(seconds));
        prop_assert_eq!(parse_ttl(&seconds.to_string()).unwrap(), Some(seconds));
    }

    #[test]
    fn parse_ttl_never_panics(input in "\\PC{0,30}") {
        let _ = parse_ttl(&input);
    }

    #[test]
    fn scores_round_trip(score in any::<f64>().prop_filter("a number", |s| !s.is_nan())) {
        let parsed = parse_score(&format_score(score)).unwrap();
        prop_assert_eq!(parsed.to_bits(), score.to_bits());
    }

    #[test]
    fn stream_fields_round_trip(
        fields in prop::collection::vec(
            (prop::collection::vec(any::<u8>(), 0..12), prop::collection::vec(any::<u8>(), 0..12)),
            1..6,
        )
    ) {
        let line: Vec<String> = fields
            .iter()
            .flat_map(|(f, v)| [format::quote_arg(f), format::quote_arg(v)])
            .collect();
        let line = line.join(" ");
        // A line starting with `{` would be read as JSON.
        prop_assume!(!line.trim_start().starts_with('{'));
        prop_assert_eq!(parse_stream_fields(&line).unwrap(), fields);
    }

    /// JSON laid out on CRLF lines comes back byte for byte, untouched or
    /// with a line added.
    #[test]
    fn crlf_json_round_trips(items in prop::collection::vec("[a-z0-9 ]{0,8}", 1..6)) {
        let quoted: Vec<String> = items.iter().map(|i| format!("  \"{i}\"")).collect();
        let stored = format!("[\r\n{}\r\n]", quoted.join(",\r\n"));
        let edit = TextEdit::new(stored.clone().into_bytes(), true);
        prop_assert!(edit.editable());
        prop_assert_eq!(edit.to_save(edit.text()).unwrap(), None);
        let added = edit.text().replacen("[\n", "[\n  \"new\",\n", 1);
        let expected = stored.replacen("[\r\n", "[\r\n  \"new\",\r\n", 1);
        prop_assert_eq!(edit.to_save(&added).unwrap(), Some(expected.into_bytes()));
    }

    #[test]
    fn text_edits_save_nothing_until_changed(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let edit = TextEdit::new(bytes.clone(), true);
        prop_assert_eq!(edit.to_save(edit.text()).unwrap(), None);
        prop_assert!(!edit.is_changed(edit.text()));
        if edit.editable() {
            // What is shown converts back to exactly the stored bytes.
            prop_assert_eq!(edit.view().encode(edit.text()).unwrap(), bytes);
        }
        for format in ValueFormat::ALL {
            let mut edit = edit.clone();
            if edit.set_format(format, None).is_ok() {
                prop_assert_eq!(edit.to_save(edit.text()).unwrap(), None, "{:?}", format);
            }
        }
    }

    /// Any bytes at all can be edited as text (quoted where they are not
    /// text), and an edit saves exactly the bytes it spells.
    #[test]
    fn any_bytes_edit_as_text(
        stored in prop::collection::vec(any::<u8>(), 0..64),
        typed in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut edit = TextEdit::new(stored.clone(), true);
        edit.set_format(ValueFormat::Text, None).unwrap();
        prop_assert!(edit.editable());
        prop_assert_eq!(edit.view().encode(edit.text()).unwrap(), stored.clone());
        let quoted = matches!(format::bytes(&stored), std::borrow::Cow::Owned(_));
        // Plain text is taken as typed; quoted text the way the console reads it.
        let (spelled, typed) = if quoted {
            (format::quote_arg(&typed), typed)
        } else {
            let text = String::from_utf8_lossy(&typed).into_owned();
            let bytes = text.clone().into_bytes();
            (text, bytes)
        };
        let expected = (typed != stored).then(|| typed.clone());
        prop_assert_eq!(edit.to_save(&spelled).unwrap(), expected);
    }

    /// Gzip and Base64 values edited as text come back as the same kind of
    /// value, holding exactly the text typed.
    #[test]
    fn wrapped_formats_hold_what_was_typed(text in "[a-z0-9 :,{}\\[\\]\"]{0,40}", typed in "[a-zA-Z0-9 ]{1,40}") {
        for format in [ValueFormat::Gzip, ValueFormat::Base64] {
            let Ok(stored) = formats::encode(&text, format) else { continue };
            let mut edit = TextEdit::new(stored.clone(), true);
            if edit.set_format(format, None).is_err() {
                // An empty Base64 text is not Base64.
                prop_assert!(text.is_empty());
                continue;
            }
            if let Some(saved) = edit.to_save(&typed).unwrap() {
                prop_assert_eq!(formats::decode(&saved, format).unwrap(), typed.clone());
            }
        }
    }
}

#[test]
fn removing_the_last_elements_deletes_the_key() {
    let gone = removed(2, 0);
    let left = removed(1, 1);
    for mutation in [
        Mutation::HashDelete {
            key: key(),
            fields: bytes(&["a", "b"]),
        },
        Mutation::ListRemoveAt {
            key: key(),
            elements: vec![(0, b"a".to_vec())],
        },
        Mutation::SetRemove {
            key: key(),
            members: bytes(&["a"]),
        },
        Mutation::ZSetRemove {
            key: key(),
            members: bytes(&["a"]),
        },
    ] {
        assert!(mutation.deleted_key(&gone), "{mutation:?}");
        assert!(!mutation.deleted_key(&left), "{mutation:?}");
    }
    // Redis keeps an empty stream; other changes never empty a key.
    let stream = Mutation::StreamDelete {
        key: key(),
        ids: vec!["1-1".into()],
    };
    assert!(!stream.deleted_key(&gone));
    let rename = Mutation::Rename {
        from: key(),
        to: Key::from(&b"k2"[..]),
    };
    assert!(!rename.deleted_key(&Outcome::Done));
    // The same as applying it to the loaded value says.
    let mut value = hash(&[("a", "1"), ("b", "2")]);
    let delete = Mutation::HashDelete {
        key: key(),
        fields: bytes(&["a", "b"]),
    };
    assert_eq!(value.apply(&delete, &gone), Applied::KeyGone);
}
