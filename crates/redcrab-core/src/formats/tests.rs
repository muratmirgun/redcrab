use proptest::prelude::*;

use super::*;

fn gz(text: &str) -> Vec<u8> {
    gzip(text.as_bytes())
}

#[test]
fn detects_formats() {
    let cases: [(&[u8], ValueFormat); 16] = [
        (b"", ValueFormat::Text),
        (b"hello world", ValueFormat::Text),
        ("grüße 🦀".as_bytes(), ValueFormat::Text),
        // Control characters in valid UTF-8 are still text, shown escaped.
        (b"a\x00b", ValueFormat::Text),
        (b"\x1b[31mred\x1b[0m", ValueFormat::Text),
        (br#"{"a":1}"#, ValueFormat::Json),
        (b"  [1, 2]\n", ValueFormat::Json),
        // JSON scalars are text: "123" is a number as much as a string.
        (b"123", ValueFormat::Text),
        (br#""quoted""#, ValueFormat::Text),
        (b"{not json", ValueFormat::Text),
        // Base64 of text, padded and not.
        (b"aGVsbG8gd29ybGQ=", ValueFormat::Base64),
        (b"aGVsbG8gd29ybGQgYWdhaW4", ValueFormat::Base64),
        // Plain words are often valid Base64 that decodes to noise.
        (b"password", ValueFormat::Text),
        (b"deadbeefcafebabe", ValueFormat::Text),
        (b"\x81\xa1a\x01", ValueFormat::MessagePack),
        (b"\xff\xfe\x00\x01binary", ValueFormat::Hex),
    ];
    for (bytes, expected) in cases {
        assert_eq!(
            detect(bytes),
            expected,
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
    assert_eq!(detect(&gz("compressed")), ValueFormat::Gzip);
    let b64_gzip = Base64Style::default().encode(&gz("x"));
    assert_eq!(detect(b64_gzip.as_bytes()), ValueFormat::Base64);
    // A lone MessagePack scalar is not claimed: too much binary parses as one.
    assert_eq!(detect(b"\xcc\xff"), ValueFormat::Hex);
    // An array with bytes left over is not MessagePack.
    assert_eq!(detect(b"\x91\x01\x02"), ValueFormat::Hex);
}

/// JSON can spell integers MessagePack cannot hold; serde_json reads those
/// as floats, which would store other digits than were typed.
#[test]
fn messagepack_refuses_integers_it_cannot_hold() {
    for big in [
        r#"{"id":123456789012345678901234567890}"#,
        "[18446744073709551616]",
        "[-9223372036854775809]",
    ] {
        let err = msgpack::from_json(big).unwrap_err().to_string();
        assert!(err.contains("too big"), "{big}: {err}");
        assert!(encode(big, ValueFormat::MessagePack).is_err());
    }
    for fits in [
        "[18446744073709551615, -9223372036854775808, -0, 0]",
        // Floats are floats; numbers in strings are strings.
        "[1e30, 123456789012345678901234567890.5]",
        r#"{"id":"123456789012345678901234567890"}"#,
        r#"["a \" 99999999999999999999999 \\", 1]"#,
    ] {
        assert!(msgpack::from_json(fits).is_ok(), "{fits}");
    }
    // Invalid JSON says so first.
    let err = msgpack::from_json("[99999999999999999999999,").unwrap_err();
    assert!(err.to_string().starts_with("not valid JSON"), "{err}");
}

#[test]
fn base64_says_what_its_text_does_not_show() {
    let guess = |text: &str| {
        let guess = Base64Style::guessed(text.as_bytes());
        (guess.alphabet, guess.padding)
    };
    assert_eq!(guess("eyJ1c2VyIjoiYm9iIiwibiI6MTJ9"), (true, true));
    assert_eq!(guess("aGVsbG8gd29ybGQ="), (true, false));
    assert_eq!(guess("aGVsbG8gd29ybGQgYWdhaW4"), (true, false));
    assert_eq!(guess("fn5-IGhp"), (false, true));
    assert_eq!(guess("fn5-IGhpIQ"), (false, false));
    assert_eq!(guess("+/8="), (false, false));
}

/// Words and IDs that are valid Base64 decoding to "text" (CJK, private
/// use characters, punctuation soup) by chance are shown as they are.
/// Before, about one in a hundred 8-character IDs opened decoded, and an
/// edit typed there was saved Base64-encoded.
#[test]
fn base64_is_not_claimed_for_words_and_ids() {
    for word in [
        "annotate",
        "anteater",
        "ambulate",
        "airfreighter",
        "acerbity",
        "agrology",
        "PENDING1",
        "w4QtZMul",
        "MSNPKV4u",
        "16d476a7",
        "a1JIdC8kLno6",
        "676c5a2164e1",
        // Long, but letters only: far likelier a word.
        "circumstantially",
        "encyclopedically",
    ] {
        assert_eq!(detect(word.as_bytes()), ValueFormat::Text, "{word}");
    }
    // What Base64 is still found as: text long enough, accents and all,
    // JSON and gzip.
    for text in [
        "hello world",
        "Größe: 3 Stück, café au lait",
        r#"{"user":"bob","n":12}"#,
        "[1,2]",
    ] {
        let encoded = Base64Style::default().encode(text.as_bytes());
        assert_eq!(detect(encoded.as_bytes()), ValueFormat::Base64, "{text}");
    }
}

#[test]
fn decodes_each_format() {
    // One control character escapes the line breaks too, like redis-cli.
    assert_eq!(
        decode(b"a\nb\x00", ValueFormat::Text).unwrap(),
        "a\\nb\\x00"
    );
    assert_eq!(decode(b"a\nb", ValueFormat::Text).unwrap(), "a\nb");
    assert_eq!(
        decode(br#"{"a":[1,{}]}"#, ValueFormat::Json).unwrap(),
        "{\n  \"a\": [\n    1,\n    {}\n  ]\n}"
    );
    // Any JSON value, not just documents.
    assert_eq!(decode(b"12.50", ValueFormat::Json).unwrap(), "12.50");
    let err = decode(b"{oops", ValueFormat::Json).unwrap_err().to_string();
    assert!(err.starts_with("not valid JSON:"), "{err}");
    assert!(decode(b"\xff", ValueFormat::Json).is_err());

    assert_eq!(decode(b"aGk=", ValueFormat::Base64).unwrap(), "hi");
    assert_eq!(
        decode(b"eyJhIjoxfQ==", ValueFormat::Base64).unwrap(),
        "{\n  \"a\": 1\n}"
    );
    assert_eq!(decode(b"AP8=", ValueFormat::Base64).unwrap(), "\\x00\\xff");
    assert!(decode(b"not base64!", ValueFormat::Base64).is_err());
    // Not canonical: stray bits, missing padding where the style wants it.
    assert!(decode(b"aGl=", ValueFormat::Base64).is_err());
    assert!(decode(b"", ValueFormat::Base64).is_err());

    assert_eq!(
        decode(&gz("inner text"), ValueFormat::Gzip).unwrap(),
        "inner text"
    );
    assert_eq!(
        decode(&gz(r#"[1,2]"#), ValueFormat::Gzip).unwrap(),
        "[\n  1,\n  2\n]"
    );
    assert!(decode(b"plain", ValueFormat::Gzip).is_err());
    // Truncated gzip.
    let truncated = &gz("some text that compresses")[..12];
    assert!(decode(truncated, ValueFormat::Gzip).is_err());

    // {"a": [1, -1, 1.5, "x", null, true]}
    let msgpack = b"\x81\xa1a\x96\x01\xff\xcb\x3f\xf8\x00\x00\x00\x00\x00\x00\xa1x\xc0\xc3";
    assert_eq!(
        decode(msgpack, ValueFormat::MessagePack).unwrap(),
        "{\n  \"a\": [\n    1,\n    -1,\n    1.5,\n    \"x\",\n    null,\n    true\n  ]\n}"
    );
    assert!(decode(b"\x92\x01", ValueFormat::MessagePack).is_err());
    assert!(decode(b"\xc1", ValueFormat::MessagePack).is_err());
}

#[test]
fn encodes_each_editable_format() {
    assert_eq!(encode("a b", ValueFormat::Text).unwrap(), b"a b");
    assert_eq!(
        encode("{\n  \"a\": 1\n}", ValueFormat::Json).unwrap(),
        br#"{"a":1}"#
    );
    assert!(encode("{", ValueFormat::Json).is_err());
    assert_eq!(encode("hi", ValueFormat::Base64).unwrap(), b"aGk=");
    assert_eq!(
        gunzip(&encode("zip me", ValueFormat::Gzip).unwrap(), 100).unwrap(),
        b"zip me"
    );
    assert_eq!(
        encode(
            r#"{"a":[1,-1,1.5,"x",null,true]}"#,
            ValueFormat::MessagePack
        )
        .unwrap(),
        b"\x81\xa1a\x96\x01\xff\xcb\x3f\xf8\x00\x00\x00\x00\x00\x00\xa1x\xc0\xc3"
    );
    assert!(encode("not json", ValueFormat::MessagePack).is_err());
    assert!(encode("00 01", ValueFormat::Hex).is_err());
    assert!(!ValueFormat::Hex.is_editable());
    assert!(ValueFormat::ALL.iter().filter(|f| f.is_editable()).count() == 5);
}

#[test]
fn messagepack_keeps_order_and_duplicates() {
    let json = r#"{"z":1,"a":2,"z":3}"#;
    let packed = msgpack::from_json(json).unwrap();
    assert_eq!(msgpack::to_json(&packed, DECODE_LIMIT).unwrap(), json);
}

#[test]
fn messagepack_writes_the_shortest_forms() {
    let cases: [(&str, &[u8]); 14] = [
        ("0", b"\x00"),
        ("127", b"\x7f"),
        ("128", b"\xcc\x80"),
        ("256", b"\xcd\x01\x00"),
        ("65536", b"\xce\x00\x01\x00\x00"),
        ("4294967296", b"\xcf\x00\x00\x00\x01\x00\x00\x00\x00"),
        ("-1", b"\xff"),
        ("-32", b"\xe0"),
        ("-33", b"\xd0\xdf"),
        ("-129", b"\xd1\xff\x7f"),
        ("-32769", b"\xd2\xff\xff\x7f\xff"),
        ("-2147483649", b"\xd3\xff\xff\xff\xff\x7f\xff\xff\xff"),
        (
            "18446744073709551615",
            b"\xcf\xff\xff\xff\xff\xff\xff\xff\xff",
        ),
        ("[]", b"\x90"),
    ];
    for (json, expected) in cases {
        assert_eq!(msgpack::from_json(json).unwrap(), expected, "{json}");
        assert_eq!(msgpack::to_json(expected, 100).unwrap(), json, "{json}");
    }
    let long = "x".repeat(40);
    assert_eq!(
        &msgpack::from_json(&format!("{long:?}")).unwrap()[..2],
        b"\xd9\x28"
    );
    let many = format!("[{}]", vec!["1"; 16].join(","));
    assert_eq!(&msgpack::from_json(&many).unwrap()[..3], b"\xdc\x00\x10");
}

#[test]
fn messagepack_shows_what_json_cannot_hold() {
    // bin 8, ext, float32, int key, NaN.
    assert_eq!(
        msgpack::to_json(b"\xc4\x02\x00\xff", 100).unwrap(),
        r#""AP8=""#
    );
    assert_eq!(
        msgpack::to_json(b"\xd4\x05\x01", 100).unwrap(),
        r#""ext 5: AQ==""#
    );
    assert_eq!(
        msgpack::to_json(b"\xca\x3f\xc0\x00\x00", 100).unwrap(),
        "1.5"
    );
    assert_eq!(
        msgpack::to_json(b"\x81\x01\xa1x", 100).unwrap(),
        r#"{"1":"x"}"#
    );
    assert_eq!(
        msgpack::to_json(b"\xcb\x7f\xf8\x00\x00\x00\x00\x00\x00", 100).unwrap(),
        "null"
    );
    // A map key that is a map.
    assert!(msgpack::to_json(b"\x81\x80\x01", 100).is_err());
    // Too big to show.
    let many = msgpack::from_json(&format!("[{}]", vec!["1"; 101].join(","))).unwrap();
    assert!(msgpack::to_json(&many, 50).is_err());
    assert!(msgpack::to_json(&many, 1000).is_ok());
}

#[test]
fn messagepack_refuses_hostile_input() {
    // A huge count with nothing behind it.
    assert!(msgpack::to_json(b"\xdd\xff\xff\xff\xff", DECODE_LIMIT).is_err());
    assert!(msgpack::to_json(b"\xdb\xff\xff\xff\xff", DECODE_LIMIT).is_err());
    // Nesting deeper than the stack should go.
    let deep = vec![0x91u8; 10_000];
    assert!(msgpack::to_json(&deep, DECODE_LIMIT).is_err());
    assert!(!msgpack::is_container(&deep));

    // One big string or binary value: escaped, a control byte is six bytes
    // of JSON (\u0001), binary is Base64. The cap holds inside it.
    let scalar = |header: u8, len: usize, byte: u8| {
        let mut value = vec![0x91, header];
        value.extend_from_slice(&(len as u32).to_be_bytes());
        value.resize(value.len() + len, byte);
        value
    };
    for big in [scalar(0xdb, 3 << 20, 0x01), scalar(0xc6, 13 << 20, 0)] {
        assert!(msgpack::is_container(&big));
        let err = msgpack::to_json(&big, DECODE_LIMIT).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err:#}");
    }
    // Right at the limit is fine: ["\u0001" x 10].
    let small = scalar(0xdb, 10, 0x01);
    assert_eq!(msgpack::to_json(&small, 64).unwrap().len(), 64);
    assert!(msgpack::to_json(&small, 63).is_err());
}

#[test]
fn gzip_is_capped() {
    let bomb = gzip(&vec![b'a'; 100_000]);
    assert!(bomb.len() < 1000);
    let err = gunzip(&bomb, 10_000).unwrap_err().to_string();
    assert!(err.contains("too large"), "{err}");
    assert_eq!(gunzip(&bomb, 100_000).unwrap().len(), 100_000);
    // Several members are read as one.
    let mut two = gzip(b"one ");
    two.extend(gzip(b"two"));
    assert_eq!(gunzip(&two, 100).unwrap(), b"one two");
    // Compressing is deterministic.
    assert_eq!(gzip(b"same"), gzip(b"same"));
}

#[test]
fn hex_dumps_like_hexdump() {
    assert_eq!(hex_dump(b"", 100), "");
    assert_eq!(
        hex_dump(b"Hello, world!\n\x00\xff", 100),
        "00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 0a 00 ff  |Hello, world!...|"
    );
    assert_eq!(
        hex_dump(b"0123456789abcdefXY", 100),
        "00000000  30 31 32 33 34 35 36 37  38 39 61 62 63 64 65 66  |0123456789abcdef|\n\
         00000010  58 59                                             |XY|"
    );
    let dump = hex_dump(&[0u8; 100], 32);
    assert_eq!(dump.lines().count(), 3);
    assert!(dump.ends_with("… 68 more bytes not shown"), "{dump}");
}

#[test]
fn base64_keeps_its_style() {
    for (text, url_safe, padded) in [
        ("+/8=", false, true),
        ("+/8", false, false),
        ("-_8=", true, true),
        ("-_8", true, false),
    ] {
        let (bytes, style) = Base64Style::decode(text.as_bytes()).unwrap();
        assert_eq!(bytes, [0xfb, 0xff]);
        assert_eq!(style, Base64Style { url_safe, padded }, "{text}");
        assert_eq!(style.encode(&bytes), text);
    }
}

#[test]
fn json_helpers_move_whitespace_only() {
    let stored = r#"{"a" :[1, 2.50],"s":"x , y : {z}"}"#;
    assert_eq!(json::compact(stored), r#"{"a":[1,2.50],"s":"x , y : {z}"}"#);
    assert_eq!(json::compact(&json::pretty(stored)), json::compact(stored));
    assert!(json::is_document(" [1]"));
    assert!(!json::is_document("1"));
    assert!(json::is_valid("1"));
}

/// Arbitrary JSON values, nested a few levels.
fn json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(serde_json::Value::from),
        any::<u64>().prop_map(serde_json::Value::from),
        (-1e15f64..1e15).prop_map(serde_json::Value::from),
        "\\PC{0,40}".prop_map(serde_json::Value::String),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..20).prop_map(serde_json::Value::Array),
            prop::collection::vec(("\\PC{0,10}", inner), 0..20)
                .prop_map(|pairs| serde_json::Value::Object(pairs.into_iter().collect())),
        ]
    })
}

proptest! {
    #[test]
    fn detect_and_decode_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let format = detect(&bytes);
        // What is detected decodes, except MessagePack with a map key that
        // is an array or a map: valid MessagePack that JSON cannot show,
        // which the editor shows as stored with a notice saying why.
        if let Err(err) = decode(&bytes, format) {
            prop_assert!(
                format == ValueFormat::MessagePack && err.to_string().contains("a map key that is"),
                "{format:?}: {err:#}"
            );
        }
        for format in ValueFormat::ALL {
            let _ = decode(&bytes, format);
        }
        let _ = msgpack::is_container(&bytes);
    }

    #[test]
    fn text_round_trips(text in "\\PC{0,60}") {
        let bytes = encode(&text, ValueFormat::Text).unwrap();
        prop_assert_eq!(decode(&bytes, ValueFormat::Text).unwrap(), text);
    }

    #[test]
    fn base64_round_trips(bytes in prop::collection::vec(any::<u8>(), 1..100), url_safe: bool, padded: bool) {
        let style = Base64Style { url_safe, padded };
        let text = style.encode(&bytes);
        let (decoded, _) = Base64Style::decode(text.as_bytes()).unwrap();
        prop_assert_eq!(&decoded, &bytes);
        // Encoding in the detected style gives the same text back.
        let (_, detected) = Base64Style::decode(text.as_bytes()).unwrap();
        prop_assert_eq!(detected.encode(&decoded), text);
    }

    #[test]
    fn base64_text_round_trips(text in "[a-zA-Z0-9 .,!?ü🦀]{1,60}") {
        let stored = encode(&text, ValueFormat::Base64).unwrap();
        prop_assert_eq!(decode(&stored, ValueFormat::Base64).unwrap(), text);
    }

    /// Whatever the bytes, the JSON is never longer than the limit.
    #[test]
    fn messagepack_json_stays_within_its_limit(
        bytes in prop::collection::vec(any::<u8>(), 0..200),
        limit in 0usize..400,
    ) {
        if let Ok(json) = msgpack::to_json(&bytes, limit) {
            prop_assert!(json.len() <= limit, "{} > {limit}", json.len());
        }
    }

    /// Integers are stored exactly, or refused: never as a float.
    #[test]
    fn messagepack_integers_are_exact(n in any::<i128>()) {
        let packed = msgpack::from_json(&format!("[{n}]"));
        if i64::try_from(n).is_ok() || u64::try_from(n).is_ok() {
            let json = msgpack::to_json(&packed.unwrap(), 100).unwrap();
            prop_assert_eq!(json, format!("[{n}]"));
        } else {
            prop_assert!(packed.is_err());
        }
    }

    /// Strings come out escaped exactly as serde_json escapes them.
    #[test]
    fn messagepack_strings_escape_like_serde_json(text in any::<String>()) {
        let quoted = serde_json::to_string(&text).unwrap();
        let packed = msgpack::from_json(&quoted).unwrap();
        prop_assert_eq!(msgpack::to_json(&packed, DECODE_LIMIT).unwrap(), quoted);
    }

    /// Short IDs are not Base64 unless they decode to gzip or JSON, which
    /// takes far more than chance.
    #[test]
    fn short_ids_are_not_base64(id in "[a-zA-Z0-9]{8,15}") {
        prop_assert_eq!(detect(id.as_bytes()), ValueFormat::Text);
    }

    #[test]
    fn gzip_round_trips(bytes in prop::collection::vec(any::<u8>(), 0..500)) {
        prop_assert_eq!(gunzip(&gzip(&bytes), 1000).unwrap(), bytes);
    }

    #[test]
    fn gzip_text_round_trips(text in "\\PC{0,80}") {
        let stored = encode(&text, ValueFormat::Gzip).unwrap();
        prop_assert_eq!(detect(&stored), ValueFormat::Gzip);
        let shown = decode(&stored, ValueFormat::Gzip).unwrap();
        // Text that is a JSON document is shown indented.
        if json::is_document(&text) {
            prop_assert_eq!(json::compact(&shown), json::compact(&text));
        } else {
            prop_assert_eq!(shown, text);
        }
    }

    #[test]
    fn messagepack_round_trips(value in json_value()) {
        let json = serde_json::to_string(&value).unwrap();
        let packed = msgpack::from_json(&json).unwrap();
        let back = msgpack::to_json(&packed, DECODE_LIMIT).unwrap();
        // Same value, and the same bytes again from the JSON shown.
        let reparsed: serde_json::Value = serde_json::from_str(&back).unwrap();
        prop_assert_eq!(reparsed, value);
        prop_assert_eq!(msgpack::from_json(&back).unwrap(), packed.clone());
        // Containers are detected as MessagePack.
        if packed[0] >= 0x80 && packed[0] <= 0x9f || (0xdc..=0xdf).contains(&packed[0]) {
            prop_assert_eq!(detect(&packed), ValueFormat::MessagePack);
        }
    }

    #[test]
    fn json_round_trips(value in json_value()) {
        let compact = serde_json::to_string(&value).unwrap();
        let shown = decode(compact.as_bytes(), ValueFormat::Json).unwrap();
        prop_assert_eq!(encode(&shown, ValueFormat::Json).unwrap(), compact.into_bytes());
    }

    #[test]
    fn hex_dump_lines(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let dump = hex_dump(&bytes, 1 << 20);
        prop_assert_eq!(dump.lines().count(), bytes.len().div_ceil(16));
        for (i, line) in dump.lines().enumerate() {
            let offset = format!("{:08x}  ", i * 16);
            prop_assert!(line.starts_with(&offset), "{}", line);
            // The hex part reads back as the bytes.
            let hex: Vec<u8> = line[10..58]
                .split_whitespace()
                .map(|h| u8::from_str_radix(h, 16).unwrap())
                .collect();
            prop_assert_eq!(&hex[..], &bytes[i * 16..(i * 16 + 16).min(bytes.len())]);
        }
    }
}
