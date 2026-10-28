//! The edit models end to end: a value is loaded, edited the way a front
//! end edits it (text in a format, a row, the add and new key forms, the
//! TTL and rename inputs), the change is sent, and the loaded value takes
//! it in. After every change, the patched value must match a fresh load.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400), database 0, and only keys under each test's prefix.

#[macro_use]
mod common;

use common::*;
use redcrab_core::{
    Client, EditError, Key, KeyType, ListEnd, ValueData,
    formats::{self, ValueFormat},
    viewer::{self, Applied, Loaded, Mutation, NewKeyType},
};
use redis::Connection;

fn load(client: &Client, key: &Key) -> Loaded {
    let mut loaded = block_on(viewer::load(client, key.clone())).unwrap();
    while let Some(more) = loaded.load_more(client) {
        assert!(loaded.apply_page(block_on(more).unwrap()));
    }
    loaded
}

fn get(con: &mut Connection, key: &[u8]) -> Vec<u8> {
    redis::cmd("GET").arg(key).query(con).unwrap()
}

/// Sends `mutation`, applies it to `loaded`, and checks the result against
/// a fresh load (rows in any order for hashes and sets, whose order is the
/// server's).
fn send_and_apply(client: &Client, loaded: &mut Loaded, mutation: Mutation) -> Applied {
    let outcome = block_on(mutation.clone().send(client)).unwrap();
    let applied = loaded.apply(&mutation, &outcome);
    let fresh = load(client, &loaded.key);
    match &applied {
        Applied::KeyGone => assert_eq!(fresh.meta.ty, KeyType::None),
        Applied::Reload => {}
        Applied::Patched(_) => {
            assert_eq!(loaded.total, fresh.total, "{mutation:?}");
            assert_eq!(rows(loaded), rows(&fresh), "{mutation:?}");
            assert_eq!(loaded.meta.ty, fresh.meta.ty);
        }
    }
    applied
}

fn rows(loaded: &Loaded) -> Vec<(Option<String>, String)> {
    let mut rows: Vec<_> = (0..loaded.row_count())
        .map(|ix| loaded.row_cells(ix))
        .collect();
    if matches!(loaded.meta.ty, KeyType::Hash | KeyType::Set) {
        rows.sort();
    }
    rows
}

#[test]
fn strings_are_edited_in_their_format() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let msgpack = formats::encode(r#"{"user":"ada","n":[1,2]}"#, ValueFormat::MessagePack).unwrap();
    redis::pipe()
        .cmd("SET")
        .arg(t.key("gz"))
        .arg(formats::gzip(br#"{"a":1}"#))
        .arg("EX")
        .arg(100)
        .ignore()
        .cmd("SET")
        .arg(t.key("mp"))
        .arg(&msgpack)
        .ignore()
        .cmd("SET")
        .arg(t.key("b64"))
        .arg("aGVsbG8gd29ybGQ=")
        .ignore()
        .cmd("SET")
        .arg(t.key("bin"))
        .arg(b"\xff\x00\x01")
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let save = |key: &str, edit: &viewer::TextEdit, text: &str| {
        let mut loaded = load(&client, &t.bkey(key));
        let mutation = loaded.save_text(edit, text).unwrap().expect("a change");
        assert_eq!(
            send_and_apply(&client, &mut loaded, mutation),
            Applied::Patched(None)
        );
    };

    // gzip: shown as the JSON inside, saved gzipped, TTL kept.
    let loaded = load(&client, &t.bkey("gz"));
    let edit = loaded.text_edit(true).unwrap();
    assert_eq!(
        (edit.format(), edit.text()),
        (ValueFormat::Gzip, "{\n  \"a\": 1\n}")
    );
    assert_eq!(loaded.save_text(&edit, edit.text()).unwrap(), None);
    save("gz", &edit, "{\n  \"a\": 2\n}");
    let stored = get(&mut con, &t.bkey("gz"));
    assert_eq!(formats::gunzip(&stored, 100).unwrap(), br#"{"a":2}"#);
    let ttl: i64 = redis::cmd("PTTL").arg(t.key("gz")).query(&mut con).unwrap();
    assert!(ttl > 90_000);

    // MessagePack: shown as JSON, saved as MessagePack in the same order.
    let loaded = load(&client, &t.bkey("mp"));
    let edit = loaded.text_edit(true).unwrap();
    assert_eq!(edit.format(), ValueFormat::MessagePack);
    assert!(edit.editable());
    save("mp", &edit, &edit.text().replace("ada", "grace"));
    assert_eq!(
        get(&mut con, &t.bkey("mp")),
        formats::encode(r#"{"user":"grace","n":[1,2]}"#, ValueFormat::MessagePack).unwrap()
    );

    // Base64 text: shown decoded, saved encoded again.
    let loaded = load(&client, &t.bkey("b64"));
    let edit = loaded.text_edit(true).unwrap();
    assert_eq!(
        (edit.format(), edit.text()),
        (ValueFormat::Base64, "hello world")
    );
    save("b64", &edit, "hello there");
    assert_eq!(get(&mut con, &t.bkey("b64")), b"aGVsbG8gdGhlcmU=");

    // Binary: a hex dump to read, quoted text to edit.
    let loaded = load(&client, &t.bkey("bin"));
    let mut edit = loaded.text_edit(true).unwrap();
    assert_eq!(edit.format(), ValueFormat::Hex);
    assert!(edit.text().starts_with("00000000  ff 00 01"));
    edit.set_format(ValueFormat::Text, None).unwrap();
    assert_eq!(edit.text(), r#""\xff\x00\x01""#);
    save("bin", &edit, r#""\xff\x00\x02 ok""#);
    assert_eq!(get(&mut con, &t.bkey("bin")), b"\xff\x00\x02 ok");
}

/// A few KB of gzip that inflate into megabytes of NULs: loaded whole (it
/// is small), shown as stored rather than as the text it decodes to.
#[test]
fn small_values_that_decode_into_too_much_text_show_as_stored() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let bomb = formats::gzip(&vec![0u8; 5 << 20]);
    redis::cmd("SET")
        .arg(t.key("gz"))
        .arg(&bomb)
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let loaded = load(&client, &t.bkey("gz"));
    assert!(!loaded.is_preview());
    let edit = loaded.text_edit(true).unwrap();
    assert_eq!(edit.format(), ValueFormat::Text);
    assert!(edit.text().len() <= 4 * bomb.len() + 2);
    assert!(edit.editable());
}

#[test]
fn json_documents_are_edited_as_json() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("JSON.SET")
        .arg(t.key("doc"))
        .arg("$")
        .arg(r#"{"a":[1,2]}"#)
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let mut loaded = load(&client, &t.bkey("doc"));
    let edit = loaded.text_edit(true).unwrap();
    assert!(edit.editable());
    assert!(loaded.save_text(&edit, "{\"a\": ").is_err(), "invalid JSON");
    let mutation = loaded
        .save_text(&edit, &edit.text().replace('2', "3"))
        .unwrap()
        .unwrap();
    send_and_apply(&client, &mut loaded, mutation);
    let doc: String = redis::cmd("JSON.GET")
        .arg(t.key("doc"))
        .query(&mut con)
        .unwrap();
    assert_eq!(doc, r#"{"a":[1,3]}"#);
}

#[test]
fn rows_of_every_collection() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::pipe()
        .cmd("HSET")
        .arg(t.key("h"))
        .arg(&[("a", "1"), ("b", "2")])
        .ignore()
        .cmd("RPUSH")
        .arg(t.key("l"))
        .arg(&["x", "y", "x"])
        .ignore()
        .cmd("SADD")
        .arg(t.key("s"))
        .arg(&["m", "n"])
        .ignore()
        .cmd("ZADD")
        .arg(t.key("z"))
        .arg(&[(1, "p"), (2, "q")])
        .ignore()
        .cmd("XADD")
        .arg(t.key("x"))
        .arg("1-1")
        .arg("f")
        .arg("v")
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let edit_row = |loaded: &Loaded, ix: usize, value: &str, name: Option<&str>| {
        let edit = loaded.row_edit(ix, true).unwrap();
        loaded
            .save_row(&edit, value, name)
            .unwrap()
            .expect("a change")
    };

    // Hash: a value, then a rename onto an existing field, which asks first.
    let mut hash = load(&client, &t.bkey("h"));
    let ix = (0..2)
        .find(|&ix| hash.row_cells(ix).0.as_deref() == Some("a"))
        .unwrap();
    let change = edit_row(&hash, ix, "10", Some("a"));
    assert_eq!(
        send_and_apply(&client, &mut hash, change),
        Applied::Patched(Some(ix))
    );
    let rename = edit_row(&hash, ix, "10", Some("b"));
    let err = block_on(rename.clone().send(&client)).unwrap_err();
    assert_eq!(err.downcast_ref(), Some(&EditError::ElementExists));
    assert!(rename.overwrite_question().is_some());
    send_and_apply(&client, &mut hash, rename.overwriting().unwrap());
    assert_eq!(rows(&hash), [(Some("b".into()), "10".into())]);
    let add = hash
        .add_row()
        .unwrap()
        .build("c", "3", ListEnd::Tail)
        .unwrap();
    assert!(matches!(
        send_and_apply(&client, &mut hash, add),
        Applied::Patched(Some(_))
    ));
    let delete = hash.delete_rows(&[0, 1]).unwrap();
    assert_eq!(send_and_apply(&client, &mut hash, delete), Applied::KeyGone);

    // List: set, add at both ends, delete.
    let mut list = load(&client, &t.bkey("l"));
    let change = edit_row(&list, 2, "z", None);
    send_and_apply(&client, &mut list, change);
    for end in [ListEnd::Head, ListEnd::Tail] {
        let add = list.add_row().unwrap().build("", "new", end).unwrap();
        assert!(matches!(
            send_and_apply(&client, &mut list, add),
            Applied::Patched(Some(_))
        ));
    }
    let delete = list.delete_rows(&[0, 4]).unwrap();
    send_and_apply(&client, &mut list, delete);
    assert_eq!(
        rows(&list).into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        ["x", "y", "z"]
    );

    // Set: replace a member, add one.
    let mut set = load(&client, &t.bkey("s"));
    let ix = (0..2).find(|&ix| set.row_cells(ix).1 == "m").unwrap();
    let change = edit_row(&set, ix, "o", None);
    send_and_apply(&client, &mut set, change);
    let add = set
        .add_row()
        .unwrap()
        .build("", "p", ListEnd::Tail)
        .unwrap();
    send_and_apply(&client, &mut set, add);
    assert_eq!(set.total, 3);

    // Sorted set: a score moves the member; a rename keeps the score.
    let mut zset = load(&client, &t.bkey("z"));
    let change = edit_row(&zset, 0, "p", Some("5"));
    assert_eq!(
        send_and_apply(&client, &mut zset, change),
        Applied::Patched(Some(1))
    );
    let change = edit_row(&zset, 1, "p2", Some("5"));
    send_and_apply(&client, &mut zset, change);
    assert_eq!(rows(&zset)[1], (Some("5".into()), "p2".into()));
    let add = zset
        .add_row()
        .unwrap()
        .build("-1", "first", ListEnd::Tail)
        .unwrap();
    assert_eq!(
        send_and_apply(&client, &mut zset, add),
        Applied::Patched(Some(0))
    );

    // Stream: entries are added and deleted, never changed.
    let mut stream = load(&client, &t.bkey("x"));
    assert!(!stream.row_edit(0, true).unwrap().editable());
    let add = stream
        .add_row()
        .unwrap()
        .build("", r#"{"name": "Ada", "age": 36}"#, ListEnd::Tail)
        .unwrap();
    assert_eq!(
        send_and_apply(&client, &mut stream, add),
        Applied::Patched(Some(1))
    );
    assert_eq!(stream.row_cells(1).1, "name=Ada  age=36");
    let delete = stream.delete_rows(&[0]).unwrap();
    send_and_apply(&client, &mut stream, delete);
    assert_eq!(stream.total, 1);
}

/// A field name with a line break, edited in a one-line input (which drops
/// line breaks, as the GUI's does): saving only the value keeps the field,
/// and leaves the field named like it without the break alone.
#[test]
fn hash_field_names_with_line_breaks_keep_their_name() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("HSET")
        .arg(t.key("h"))
        .arg(&[("line1\nline2", "v"), ("line1line2", "keep me")])
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let mut hash = load(&client, &t.bkey("h"));
    let ix = (0..2)
        .find(|&ix| hash.row_cells(ix).0.as_deref() == Some("line1\\nline2"))
        .unwrap();
    let edit = hash.row_edit(ix, true).unwrap();
    let field = edit.name().unwrap();
    assert!(!field.editable);
    let in_input = field.text.replace(['\n', '\r'], "");
    let change = hash
        .save_row(&edit, "v2", Some(&in_input))
        .unwrap()
        .unwrap();
    assert_eq!(
        send_and_apply(&client, &mut hash, change),
        Applied::Patched(Some(ix))
    );
    let mut now: Vec<(String, String)> = redis::cmd("HGETALL")
        .arg(t.key("h"))
        .query(&mut con)
        .unwrap();
    now.sort();
    assert_eq!(
        now,
        [
            ("line1\nline2".to_string(), "v2".to_string()),
            ("line1line2".to_string(), "keep me".to_string())
        ]
    );
}

/// A row left open while its key is renamed saves under the new name, even
/// once a key with the old name exists again.
#[test]
fn rows_open_across_a_rename_save_under_the_new_name() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("HSET")
        .arg(t.key("user:1"))
        .arg("f")
        .arg("v")
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let mut hash = load(&client, &t.bkey("user:1"));
    let edit = hash.row_edit(0, true).unwrap();
    let rename = viewer::rename_key(&hash.key, &t.key("user:1:bak"))
        .unwrap()
        .unwrap();
    assert_eq!(
        send_and_apply(&client, &mut hash, rename),
        Applied::Patched(None)
    );
    // The application writes the old name again.
    redis::cmd("HSET")
        .arg(t.key("user:1"))
        .arg("f")
        .arg("other")
        .query::<()>(&mut con)
        .unwrap();
    let change = hash.save_row(&edit, "edited", None).unwrap().unwrap();
    assert_eq!(change.key(), &t.bkey("user:1:bak"));
    send_and_apply(&client, &mut hash, change);
    let hget = |con: &mut Connection, key: &str| -> String {
        redis::cmd("HGET").arg(key).arg("f").query(con).unwrap()
    };
    assert_eq!(hget(&mut con, &t.key("user:1:bak")), "edited");
    assert_eq!(hget(&mut con, &t.key("user:1")), "other");
}

/// Whether a delete emptied the key comes from the server: a stream stays
/// once its last entry is deleted, and a set someone added to since it was
/// loaded is not gone when every loaded member is.
#[test]
fn deleting_every_loaded_row() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::pipe()
        .cmd("XADD")
        .arg(t.key("x"))
        .arg("1-1")
        .arg("f")
        .arg("v")
        .ignore()
        .cmd("EXPIRE")
        .arg(t.key("x"))
        .arg(500)
        .ignore()
        .cmd("SADD")
        .arg(t.key("s"))
        .arg(&["a", "b"])
        .ignore()
        .cmd("HSET")
        .arg(t.key("h"))
        .arg(&[("a", "1"), ("b", "2"), ("c", "3")])
        .ignore()
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());

    let mut stream = load(&client, &t.bkey("x"));
    let delete = stream.delete_rows(&[0]).unwrap();
    assert_eq!(
        send_and_apply(&client, &mut stream, delete),
        Applied::Patched(None)
    );
    assert_eq!((stream.meta.ty.clone(), stream.total), (KeyType::Stream, 0));
    let ttl: i64 = redis::cmd("PTTL").arg(t.key("x")).query(&mut con).unwrap();
    assert!(ttl > 400_000);

    let mut set = load(&client, &t.bkey("s"));
    redis::cmd("SADD")
        .arg(t.key("s"))
        .arg("c")
        .query::<()>(&mut con)
        .unwrap();
    let delete = set.delete_rows(&[0, 1]).unwrap();
    assert_eq!(send_and_apply(&client, &mut set, delete), Applied::Reload);
    let fresh = load(&client, &t.bkey("s"));
    assert_eq!(rows(&fresh), [(None, "c".into())]);

    // The other way round: someone deleted a field, and deleting the two
    // loaded ones that are left empties the key.
    let mut hash = load(&client, &t.bkey("h"));
    redis::cmd("HDEL")
        .arg(t.key("h"))
        .arg("c")
        .query::<()>(&mut con)
        .unwrap();
    let ix: Vec<usize> = (0..3)
        .filter(|&ix| hash.row_cells(ix).0.as_deref() != Some("c"))
        .collect();
    let delete = hash.delete_rows(&ix).unwrap();
    assert_eq!(send_and_apply(&client, &mut hash, delete), Applied::KeyGone);
}

/// A hash field's new value is only written while the field exists: one
/// deleted by someone else, or expired on its own, since it was loaded is
/// not brought back (without its TTL). One that is there keeps its TTL.
#[test]
fn hash_value_edits_never_bring_a_field_back() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("HSET")
        .arg(t.key("h"))
        .arg(&[("token", "secret"), ("revoked", "x"), ("kept", "1")])
        .query::<()>(&mut con)
        .unwrap();
    let field_ttl = |con: &mut Connection, field: &str, ms: i64| {
        redis::cmd("HPEXPIRE")
            .arg(t.key("h"))
            .arg(ms)
            .arg("FIELDS")
            .arg(1)
            .arg(field)
            .query::<()>(con)
            .unwrap();
    };
    field_ttl(&mut con, "token", 300);
    field_ttl(&mut con, "kept", 600_000);
    let client = t.connect(standalone());
    let hash = load(&client, &t.bkey("h"));
    let row = |name: &str| {
        let ix = (0..hash.row_count())
            .find(|&ix| hash.row_cells(ix).0.as_deref() == Some(name))
            .unwrap();
        hash.row_edit(ix, true).unwrap()
    };
    let (token, revoked, kept) = (row("token"), row("revoked"), row("kept"));

    // Deleted by someone else.
    redis::cmd("HDEL")
        .arg(t.key("h"))
        .arg("revoked")
        .query::<()>(&mut con)
        .unwrap();
    let change = hash.save_row(&revoked, "y", None).unwrap().unwrap();
    let err = block_on(change.send(&client)).unwrap_err();
    assert_eq!(err.downcast_ref(), Some(&EditError::ElementMissing));

    // Expired while it was open.
    std::thread::sleep(std::time::Duration::from_millis(600));
    let change = hash.save_row(&token, "secret2", None).unwrap().unwrap();
    let err = block_on(change.send(&client)).unwrap_err();
    assert_eq!(err.downcast_ref(), Some(&EditError::ElementMissing));
    let hget = |con: &mut Connection, field: &str| -> Option<String> {
        redis::cmd("HGET")
            .arg(t.key("h"))
            .arg(field)
            .query(con)
            .unwrap()
    };
    assert_eq!(hget(&mut con, "token"), None);
    assert_eq!(hget(&mut con, "revoked"), None);

    // Still there: the value changes, the TTL stays.
    let change = hash.save_row(&kept, "2", None).unwrap().unwrap();
    block_on(change.send(&client)).unwrap();
    assert_eq!(hget(&mut con, "kept").as_deref(), Some("2"));
    let ttl: Vec<i64> = redis::cmd("HPTTL")
        .arg(t.key("h"))
        .arg("FIELDS")
        .arg(1)
        .arg("kept")
        .query(&mut con)
        .unwrap();
    assert!(ttl[0] > 500_000, "{ttl:?}");
}

/// A list element changed by someone else after loading: the edit is
/// refused, and nothing is overwritten.
#[test]
fn stale_list_edits_are_refused() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("RPUSH")
        .arg(t.key("l"))
        .arg(&["a", "b", "c"])
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let loaded = load(&client, &t.bkey("l"));
    redis::cmd("LPUSH")
        .arg(t.key("l"))
        .arg("pushed")
        .query::<()>(&mut con)
        .unwrap();
    let edit = loaded.row_edit(1, true).unwrap();
    let change = loaded.save_row(&edit, "B", None).unwrap().unwrap();
    let err = block_on(change.send(&client)).unwrap_err();
    assert_eq!(err.downcast_ref(), Some(&EditError::Changed));
    let delete = loaded.delete_rows(&[2]).unwrap();
    let err = block_on(delete.send(&client)).unwrap_err();
    assert_eq!(err.downcast_ref(), Some(&EditError::Changed));
    let now: Vec<String> = redis::cmd("LRANGE")
        .arg(t.key("l"))
        .arg(0)
        .arg(-1)
        .query(&mut con)
        .unwrap();
    assert_eq!(now, ["pushed", "a", "b", "c"]);
}

/// Values not loaded to the end: changes that land on a later page ask for
/// a reload instead of showing rows twice.
#[test]
fn partly_loaded_values_reload_when_they_must() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let fields: Vec<(String, u32)> = (0..1500).map(|i| (format!("f{i:04}"), i)).collect();
    redis::cmd("HSET")
        .arg(t.key("h"))
        .arg(&fields)
        .query::<()>(&mut con)
        .unwrap();
    let list: Vec<u32> = (0..1500).collect();
    redis::cmd("RPUSH")
        .arg(t.key("l"))
        .arg(&list)
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());

    let mut hash = block_on(viewer::load(&client, t.bkey("h"))).unwrap();
    assert!(hash.has_more());
    let add = hash
        .add_row()
        .unwrap()
        .build("new", "v", ListEnd::Tail)
        .unwrap();
    let outcome = block_on(add.clone().send(&client)).unwrap();
    assert_eq!(hash.apply(&add, &outcome), Applied::Reload);

    // A field renamed, or a member replaced, to a name HSCAN or SSCAN may
    // return again with a later page.
    let members: Vec<String> = (0..1500).map(|i| format!("m{i:04}")).collect();
    redis::cmd("SADD")
        .arg(t.key("s"))
        .arg(&members)
        .query::<()>(&mut con)
        .unwrap();
    let mut hash = block_on(viewer::load(&client, t.bkey("h"))).unwrap();
    let mut set = block_on(viewer::load(&client, t.bkey("s"))).unwrap();
    for (value, name) in [(&mut hash, Some("renamed")), (&mut set, None)] {
        assert!(value.has_more());
        let edit = value.row_edit(0, true).unwrap();
        let change = value
            .save_row(&edit, "renamed", name)
            .unwrap()
            .expect("a change");
        let outcome = block_on(change.clone().send(&client)).unwrap();
        assert_eq!(value.apply(&change, &outcome), Applied::Reload);
        let fresh = load(&client, &value.key);
        assert_eq!(fresh.row_count() as u64, fresh.total);
    }

    let mut list = block_on(viewer::load(&client, t.bkey("l"))).unwrap();
    let rows = list.row_count();
    let add = list
        .add_row()
        .unwrap()
        .build("", "tail", ListEnd::Tail)
        .unwrap();
    let outcome = block_on(add.clone().send(&client)).unwrap();
    // On a later page: only the total changes.
    assert_eq!(list.apply(&add, &outcome), Applied::Patched(None));
    assert_eq!((list.row_count(), list.total), (rows, 1501));
    // Removing loaded rows shifts where the next page starts.
    let delete = list.delete_rows(&[0, 1]).unwrap();
    let outcome = block_on(delete.clone().send(&client)).unwrap();
    list.apply(&delete, &outcome);
    while let Some(more) = list.load_more(&client) {
        assert!(list.apply_page(block_on(more).unwrap()));
    }
    let all: Vec<String> = (0..list.row_count())
        .map(|ix| list.row_cells(ix).1)
        .collect();
    let expected: Vec<String> = (2..1500)
        .map(|i| i.to_string())
        .chain(["tail".into()])
        .collect();
    assert_eq!(all, expected, "no element skipped or shown twice");
}

#[test]
fn new_keys_ttls_and_renames() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let client = t.connect(standalone());

    for (ty, field, value) in [
        (NewKeyType::String, "", "hello"),
        (NewKeyType::Hash, "name", "Ada"),
        (NewKeyType::List, "", "first"),
        (NewKeyType::Set, "", "member"),
        (NewKeyType::ZSet, "1.5", "member"),
        (NewKeyType::Stream, "", "name Ada"),
        (NewKeyType::Json, "", r#"{"a": 1}"#),
    ] {
        let name = t.key(&format!("new:{}", ty.label()));
        let create = viewer::new_key(ty, &name, field, value, "1h").unwrap();
        block_on(create.clone().send(&client)).unwrap();
        let loaded = load(&client, &Key::from(name.as_bytes()));
        assert!(loaded.meta.ttl_ms > 3_500_000, "{ty:?}");
        assert!(
            loaded.row_count() == 1 || loaded.text(false).is_some(),
            "{ty:?}"
        );
        let err = block_on(create.send(&client)).unwrap_err();
        assert_eq!(err.downcast_ref(), Some(&EditError::KeyExists), "{ty:?}");
    }

    let key = t.bkey("new:String");
    let mut loaded = load(&client, &key);
    let ttl = viewer::set_ttl(&key, "90s").unwrap();
    send_and_apply(&client, &mut loaded, ttl);
    assert!((89_000..=90_000).contains(&loaded.meta.ttl_ms));
    let persist = viewer::set_ttl(&key, "").unwrap();
    send_and_apply(&client, &mut loaded, persist);
    assert_eq!(loaded.ttl_text(), "No expiry");

    assert_eq!(
        viewer::rename_key(&key, &t.key("new:String")).unwrap(),
        None
    );
    let rename = viewer::rename_key(&key, &t.key("renamed 🦀"))
        .unwrap()
        .unwrap();
    send_and_apply(&client, &mut loaded, rename);
    assert_eq!(&*loaded.key, t.key("renamed 🦀").as_bytes());
    assert_eq!(get(&mut con, &loaded.key), b"hello");
    let taken = viewer::rename_key(&loaded.key, &t.key("new:Hash"))
        .unwrap()
        .unwrap();
    let err = block_on(taken.send(&client)).unwrap_err();
    assert_eq!(err.downcast_ref(), Some(&EditError::KeyExists));
    assert!(matches!(
        load(&client, &t.bkey("new:Hash")).data,
        ValueData::Hash(_)
    ));
}
