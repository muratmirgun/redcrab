//! `Client::load` for every type, page by page: exact multiples of the page
//! size, SCAN cursors, stream IDs, offsets past the end, the string preview
//! cap, and values that change between pages.
//!
//! `scripts/test-env.sh up`, then `REDCRAB_IT=1 cargo test`. Uses
//! `standalone` (6400), database 0, and only keys under each test's prefix.

#[macro_use]
mod common;

use std::collections::HashSet;

use common::*;
use redcrab_core::{Client, Cursor, Key, KeyType, ValueData, ValuePage};

/// The preview cap of a string value.
const PREVIEW: usize = 1 << 20;

fn load(client: &Client, key: &Key, ty: &KeyType, cursor: Cursor, count: usize) -> ValuePage {
    block_on(client.load(key.clone(), ty.clone(), cursor, count)).unwrap()
}

/// Every page of a value, from the start until `next` is `None`.
fn pages(client: &Client, key: &Key, ty: KeyType, count: usize) -> Vec<ValuePage> {
    let mut pages = Vec::new();
    let mut cursor = Cursor::Start;
    loop {
        let page = load(client, key, &ty, cursor, count);
        let next = page.next.clone();
        pages.push(page);
        match next {
            Some(next) => cursor = next,
            None => return pages,
        }
        assert!(pages.len() < 10_000, "paging does not end");
    }
}

fn sizes(pages: &[ValuePage]) -> Vec<usize> {
    pages.iter().map(|page| page.data.len()).collect()
}

/// RPUSH, SADD, ... `items` in chunks small enough for one command each.
fn push(con: &mut redis::Connection, command: &str, key: &Key, items: &[Vec<u8>]) {
    for chunk in items.chunks(1000) {
        redis::cmd(command)
            .arg(&**key)
            .arg(chunk)
            .query::<()>(con)
            .unwrap();
    }
}

fn items(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("item {i:05}").into_bytes())
        .collect()
}

#[test]
fn strings_up_to_the_preview_cap() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let pattern = |len: usize| -> Vec<u8> { (0..len).map(|i| (i % 251) as u8).collect() };
    let cases: [(&str, Vec<u8>); 6] = [
        ("small", b"hello".to_vec()),
        ("empty", Vec::new()),
        ("binary", b"\x00\xff\r\n\x80".to_vec()),
        ("cap", pattern(PREVIEW)),
        ("cap+1", pattern(PREVIEW + 1)),
        ("3mb", pattern(3 * PREVIEW)),
    ];
    for (name, value) in &cases {
        redis::cmd("SET")
            .arg(&*t.bkey(name))
            .arg(value)
            .query::<()>(&mut con)
            .unwrap();
    }
    let client = t.connect(standalone());
    for (name, value) in &cases {
        let page = load(&client, &t.bkey(name), &KeyType::String, Cursor::Start, 1);
        let ValueData::String(bytes) = &page.data else {
            panic!("{name}: {:?}", page.data);
        };
        assert_eq!(page.total, value.len() as u64, "{name}");
        assert_eq!(bytes[..], value[..value.len().min(PREVIEW)], "{name}");
        assert_eq!(page.next, None, "{name}");
    }
}

#[test]
fn hashes_page_by_cursor() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    // Small hashes are listpacks, which HSCAN returns whole; big ones are
    // hash tables, which it walks with a cursor.
    for n in [10, 500, 1203] {
        let fields: Vec<(Vec<u8>, Vec<u8>)> = (0..n)
            .map(|i| {
                (
                    [format!("f{i}").as_bytes(), b"\xff"].concat(),
                    format!("v{i}").into_bytes(),
                )
            })
            .collect();
        let key = t.bkey(format!("h{n}"));
        for chunk in fields.chunks(500) {
            redis::cmd("HSET")
                .arg(&*key)
                .arg(chunk)
                .query::<()>(&mut con)
                .unwrap();
        }
        let client = t.connect(standalone());
        let pages = pages(&client, &key, KeyType::Hash, 500);
        let mut seen = Vec::new();
        for page in &pages {
            assert_eq!(page.total, n as u64);
            let ValueData::Hash(pairs) = &page.data else {
                panic!("{:?}", page.data);
            };
            seen.extend(pairs.iter().cloned());
        }
        assert_eq!(seen.len(), n, "{n} fields, pages {:?}", sizes(&pages));
        let seen: HashSet<_> = seen.into_iter().collect();
        assert_eq!(seen, fields.into_iter().collect(), "{n} fields");
        if n <= 500 {
            assert_eq!(pages.len(), 1, "{n} fields");
        }
    }
}

#[test]
fn sets_page_by_cursor() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let ints: Vec<Vec<u8>> = (0..100)
        .map(|i: i32| (i * 7).to_string().into_bytes())
        .collect();
    let strings = items(1203);
    for (name, members) in [("ints", &ints), ("strings", &strings)] {
        let key = t.bkey(name);
        push(&mut con, "SADD", &key, members);
        let client = t.connect(standalone());
        let pages = pages(&client, &key, KeyType::Set, 500);
        let mut seen = Vec::new();
        for page in &pages {
            assert_eq!(page.total, members.len() as u64);
            let ValueData::Set(page) = &page.data else {
                panic!("{:?}", page.data);
            };
            seen.extend(page.iter().cloned());
        }
        assert_eq!(seen.len(), members.len(), "{name}: {:?}", sizes(&pages));
        let seen: HashSet<_> = seen.into_iter().collect();
        assert_eq!(seen, members.iter().cloned().collect(), "{name}");
    }
}

#[test]
fn lists_page_by_offset() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let client = t.connect(standalone());
    for (n, expected) in [
        (1, vec![1]),
        (499, vec![499]),
        (500, vec![500]),
        (501, vec![500, 1]),
        (1000, vec![500, 500]),
        (1203, vec![500, 500, 203]),
    ] {
        let key = t.bkey(format!("list{n}"));
        let all = items(n);
        push(&mut con, "RPUSH", &key, &all);
        let pages = pages(&client, &key, KeyType::List, 500);
        assert_eq!(sizes(&pages), expected, "{n} items");
        let mut seen = Vec::new();
        for page in pages {
            assert_eq!(page.total, n as u64);
            let ValueData::List(items) = page.data else {
                panic!("{:?}", page.data);
            };
            seen.extend(items);
        }
        assert_eq!(seen, all, "{n} items, in order");
    }

    // An offset past the end is an empty last page.
    let key = t.bkey("list1203");
    let page = load(&client, &key, &KeyType::List, Cursor::Offset(5000), 500);
    assert!(page.data.is_empty());
    assert_eq!((page.total, page.next), (1203, None));
    // A page is at least one element.
    let page = load(&client, &key, &KeyType::List, Cursor::Start, 0);
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.next, Some(Cursor::Offset(1)));
}

#[test]
fn sorted_sets_page_by_rank_with_their_scores() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("scores");
    redis::cmd("ZADD")
        .arg(&*key)
        .arg(&[
            ("+inf", "top"),
            ("-inf", "bottom"),
            ("0.1", "tenth"),
            ("1e300", "huge"),
            ("-1.5", "negative"),
            ("2", "b"),
            ("2", "a"),
        ])
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let page = load(&client, &key, &KeyType::ZSet, Cursor::Start, 500);
    let ValueData::ZSet(members) = page.data else {
        panic!("{:?}", page.data);
    };
    let members: Vec<(String, f64)> = members
        .into_iter()
        .map(|(m, s)| (String::from_utf8(m).unwrap(), s))
        .collect();
    let expected = [
        ("bottom", f64::NEG_INFINITY),
        ("negative", -1.5),
        ("tenth", 0.1),
        ("a", 2.0),
        ("b", 2.0),
        ("huge", 1e300),
        ("top", f64::INFINITY),
    ];
    let expected: Vec<(String, f64)> = expected.iter().map(|(m, s)| (m.to_string(), *s)).collect();
    assert_eq!(members, expected);

    // Pages split on rank, exact multiples included.
    for (n, expected) in [
        (500, vec![500]),
        (1000, vec![500, 500]),
        (1001, vec![500, 500, 1]),
    ] {
        let key = t.bkey(format!("z{n}"));
        let pairs: Vec<(usize, String)> = (0..n).map(|i| (i, format!("m{i:05}"))).collect();
        for chunk in pairs.chunks(500) {
            redis::cmd("ZADD")
                .arg(&*key)
                .arg(chunk)
                .query::<()>(&mut con)
                .unwrap();
        }
        let pages = pages(&client, &key, KeyType::ZSet, 500);
        assert_eq!(sizes(&pages), expected, "{n} members");
        let scores: Vec<f64> = pages
            .into_iter()
            .flat_map(|page| match page.data {
                ValueData::ZSet(members) => members,
                other => panic!("{other:?}"),
            })
            .map(|(_, score)| score)
            .collect();
        assert_eq!(scores, (0..n).map(|i| i as f64).collect::<Vec<_>>());
    }
}

/// `XADD` of `n` entries with IDs that share milliseconds, so paging has to
/// follow the sequence part too.
fn seed_stream(con: &mut redis::Connection, key: &Key, n: usize) -> Vec<String> {
    let ids: Vec<String> = (0..n)
        .map(|i| format!("{}-{}", 1000 + i / 7, i % 7))
        .collect();
    let mut pipe = redis::pipe();
    for (i, id) in ids.iter().enumerate() {
        pipe.cmd("XADD")
            .arg(&**key)
            .arg(id)
            .arg("n")
            .arg(i)
            .arg(b"bin\xff")
            .arg(b"\x00")
            .ignore();
    }
    pipe.query::<()>(con).unwrap();
    ids
}

fn stream_ids(pages: &[ValuePage]) -> Vec<String> {
    pages
        .iter()
        .flat_map(|page| match &page.data {
            ValueData::Stream(entries) => entries.iter().map(|e| e.id.clone()),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn streams_page_by_id() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let client = t.connect(standalone());
    for (n, expected) in [
        (1, vec![1]),
        (500, vec![500]),
        (501, vec![500, 1]),
        (1000, vec![500, 500]),
        (1203, vec![500, 500, 203]),
    ] {
        let key = t.bkey(format!("s{n}"));
        let ids = seed_stream(&mut con, &key, n);
        let pages = pages(&client, &key, KeyType::Stream, 500);
        // A length that is a multiple of the page size ends without an
        // empty page.
        assert_eq!(sizes(&pages), expected, "{n} entries");
        assert_eq!(stream_ids(&pages), ids, "{n} entries");
        assert!(pages.iter().all(|page| page.total == n as u64));
        let ValueData::Stream(entries) = &pages[0].data else {
            unreachable!()
        };
        assert_eq!(
            entries[0].fields,
            [
                (b"n".to_vec(), b"0".to_vec()),
                (b"bin\xff".to_vec(), b"\x00".to_vec())
            ]
        );
    }
}

#[test]
fn stream_pages_follow_ids_when_entries_come_and_go() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("s");
    let ids = seed_stream(&mut con, &key, 600);
    let client = t.connect(standalone());

    let first = load(&client, &key, &KeyType::Stream, Cursor::Start, 500);
    assert_eq!(first.next, Some(Cursor::StreamAfter(ids[499].clone())));
    // Entries before the cursor go away and new ones arrive.
    redis::cmd("XDEL")
        .arg(&*key)
        .arg(&ids[..100])
        .query::<()>(&mut con)
        .unwrap();
    for i in 0..10 {
        redis::cmd("XADD")
            .arg(&*key)
            .arg(format!("5000-{i}"))
            .arg("new")
            .arg(i)
            .query::<()>(&mut con)
            .unwrap();
    }
    let second = load(&client, &key, &KeyType::Stream, first.next.unwrap(), 500);
    let mut expected: Vec<String> = ids[500..].to_vec();
    expected.extend((0..10).map(|i| format!("5000-{i}")));
    assert_eq!(stream_ids(std::slice::from_ref(&second)), expected);
    assert_eq!((second.total, second.next), (510, None));
}

#[test]
fn json_documents() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    let key = t.bkey("doc");
    let doc = r#"{"a":[1,2,{"b":"ü"}],"n":null,"big":12345678901234567890}"#;
    redis::cmd("JSON.SET")
        .arg(&*key)
        .arg("$")
        .arg(doc)
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let page = load(&client, &key, &KeyType::Json, Cursor::Start, 500);
    let ValueData::Json(json) = &page.data else {
        panic!("{:?}", page.data);
    };
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(parsed["a"][2]["b"], "ü");
    assert!(parsed["n"].is_null());
    assert_eq!(page.total, json.len() as u64);
    assert_eq!(page.next, None);
}

#[test]
fn other_types_and_missing_keys() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("BF.ADD")
        .arg(t.key("bloom"))
        .arg("x")
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    let bloom = KeyType::Other("MBbloom--".into());
    let page = load(&client, &t.bkey("bloom"), &bloom, Cursor::Start, 500);
    assert!(matches!(&page.data, ValueData::Unsupported(name) if &**name == "MBbloom--"));
    assert_eq!((page.total, page.next), (0, None));

    let page = load(
        &client,
        &t.bkey("missing"),
        &KeyType::None,
        Cursor::Start,
        500,
    );
    assert!(matches!(page.data, ValueData::Missing));
}

#[test]
fn loading_with_the_wrong_type_fails() {
    let Some(t) = it!() else { return };
    let mut con = t.seed0();
    redis::cmd("SET")
        .arg(t.key("s"))
        .arg("v")
        .query::<()>(&mut con)
        .unwrap();
    let client = t.connect(standalone());
    for ty in [KeyType::List, KeyType::Hash, KeyType::Stream, KeyType::Json] {
        let err = block_on(client.load(t.bkey("s"), ty.clone(), Cursor::Start, 10)).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("WRONGTYPE") || err.contains("wrong Redis type"),
            "{ty:?}: {err}"
        );
    }
}
