//! Text helpers shared by every front end.

use std::{borrow::Cow, fmt::Write as _};

use redis::Value;

/// Shows bytes as text, escaping anything that is not printable UTF-8 the way
/// `redis-cli` does (`\x00`, `\n`, ...).
pub fn bytes(b: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(b) {
        Ok(s) if !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') => Cow::Borrowed(s),
        _ => Cow::Owned(escape(b)),
    }
}

/// Like [`bytes`], but also escapes newlines and tabs so the result fits on
/// one line.
pub fn bytes_inline(b: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(b) {
        Ok(s) if !s.chars().any(char::is_control) => Cow::Borrowed(s),
        _ => Cow::Owned(escape(b)),
    }
}

fn escape(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => escape_bytes(&mut out, c),
                c => out.push(c),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out
}

/// `\xHH` for each byte of `c`: a control character past ASCII (U+0085)
/// takes two bytes, and `\x85` would read back as a different, single one.
fn escape_bytes(out: &mut String, c: char) {
    for byte in c.encode_utf8(&mut [0; 4]).bytes() {
        let _ = write!(out, "\\x{byte:02x}");
    }
}

/// `1.2 KB`, `3.4 MB`, ...
pub fn size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Compact TTL such as `45s`, `12m`, `3h 20m`, `5d`. `None` for keys without
/// an expiry.
pub fn ttl(ms: i64) -> Option<String> {
    if ms < 0 {
        return None;
    }
    let s = ms / 1000;
    Some(match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        3600..86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    })
}

/// Integer with thousands separators: `1,234,567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Renders a reply the way `redis-cli` prints it.
pub fn reply(v: &Value) -> String {
    let mut out = String::new();
    write_reply(&mut out, v, 0);
    out
}

fn write_reply(out: &mut String, v: &Value, indent: usize) {
    match v {
        Value::Nil => out.push_str("(nil)"),
        Value::Int(i) => {
            let _ = write!(out, "(integer) {i}");
        }
        Value::BulkString(b) => {
            let _ = write!(out, "\"{}\"", bytes_inline(b));
        }
        Value::SimpleString(s) => out.push_str(s),
        Value::Okay => out.push_str("OK"),
        Value::Double(d) => {
            let _ = write!(out, "(double) {d}");
        }
        Value::Boolean(b) => {
            let _ = write!(out, "({})", if *b { "true" } else { "false" });
        }
        Value::VerbatimString { text, .. } => out.push_str(text),
        Value::Array(items) | Value::Set(items) => write_items(out, items.iter(), indent),
        Value::Map(pairs) => {
            if pairs.is_empty() {
                out.push_str("(empty map)");
            }
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                    out.push_str(&" ".repeat(indent));
                }
                let _ = write!(out, "{}) ", i + 1);
                write_reply(out, k, indent + 3);
                out.push_str(" => ");
                write_reply(out, v, indent + 3);
            }
        }
        Value::Attribute { data, .. } => write_reply(out, data, indent),
        // An error inside a reply, such as one command's in an EXEC.
        Value::ServerError(err) => {
            let _ = write!(out, "(error) {}", err.code());
            if let Some(detail) = err.details() {
                let _ = write!(out, " {detail}");
            }
        }
        other => {
            let _ = write!(out, "{other:?}");
        }
    }
}

fn write_items<'a>(
    out: &mut String,
    items: impl ExactSizeIterator<Item = &'a Value>,
    indent: usize,
) {
    if items.len() == 0 {
        out.push_str("(empty array)");
        return;
    }
    let width = items.len().to_string().len();
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(&" ".repeat(indent));
        }
        let _ = write!(out, "{:>width$}) ", i + 1);
        write_reply(out, item, indent + width + 2);
    }
}

/// Splits a console line into arguments the way `redis-cli` does
/// (`sdssplitargs`). Whitespace separates arguments. Inside `"..."` the
/// escapes `\n \r \t \b \a` and `\xHH` (any byte, so binary keys can be
/// typed) work, and a backslash takes the next character as it is (`\"`,
/// `\\`). Inside `'...'` everything is literal but `\'`. A quote can start
/// in the middle of an argument (`key"with space"`); a closing quote ends
/// the argument and must be followed by whitespace or the end of the line.
pub fn split_args(line: &str) -> Result<Vec<Vec<u8>>, &'static str> {
    let chars: Vec<char> = line.chars().collect();
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        if i == chars.len() {
            return Ok(args);
        }
        let mut arg = Vec::new();
        while let Some(&c) = chars.get(i) {
            i += 1;
            match c {
                c if c.is_whitespace() => break,
                '"' | '\'' => {
                    i = quoted(&chars, i, c, &mut arg)?;
                    if chars.get(i).is_some_and(|c| !c.is_whitespace()) {
                        return Err("a closing quote must be followed by a space");
                    }
                    break;
                }
                c => push_char(&mut arg, c),
            }
        }
        args.push(arg);
    }
}

/// Reads the quoted part of an argument from `i`, just past the opening
/// `quote`, into `arg`. Returns where it ends: just past the closing quote.
fn quoted(
    chars: &[char],
    mut i: usize,
    quote: char,
    arg: &mut Vec<u8>,
) -> Result<usize, &'static str> {
    let hex = |i: usize| chars.get(i).and_then(|c| c.to_digit(16));
    loop {
        let c = *chars.get(i).ok_or("unbalanced quotes")?;
        i += 1;
        match c {
            c if c == quote => return Ok(i),
            '\\' if quote == '"' => {
                let escaped = *chars.get(i).ok_or("unbalanced quotes")?;
                i += 1;
                match escaped {
                    'x' => match (hex(i), hex(i + 1)) {
                        (Some(high), Some(low)) => {
                            arg.push((high << 4 | low) as u8);
                            i += 2;
                        }
                        // Not a byte: the `x` itself, as in redis-cli.
                        _ => arg.push(b'x'),
                    },
                    'n' => arg.push(b'\n'),
                    'r' => arg.push(b'\r'),
                    't' => arg.push(b'\t'),
                    'b' => arg.push(0x08),
                    'a' => arg.push(0x07),
                    c => push_char(arg, c),
                }
            }
            '\\' if quote == '\'' && chars.get(i) == Some(&'\'') => {
                arg.push(b'\'');
                i += 1;
            }
            c => push_char(arg, c),
        }
    }
}

fn push_char(arg: &mut Vec<u8>, c: char) {
    arg.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
}

/// One argument written so that [`split_args`] (and `redis-cli`) reads it
/// back as exactly these bytes: as it is when it is text without spaces or
/// quotes, else in double quotes, escaped.
pub fn quote_arg(arg: &[u8]) -> String {
    let plain = std::str::from_utf8(arg).ok().filter(|s| {
        !s.is_empty()
            && !s
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\'')
    });
    if let Some(plain) = plain {
        return plain.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for chunk in arg.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => escape_bytes(&mut out, c),
                c => out.push(c),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out.push('"');
    out
}

/// A key name for the clipboard: the name itself when it is text, so that
/// it pastes anywhere; else (bytes that are not text, control characters)
/// quoted the way the console and `redis-cli` read it back, since a
/// clipboard holds text.
pub fn copyable(key: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(key) {
        Ok(s) if !s.chars().any(char::is_control) => Cow::Borrowed(s),
        _ => Cow::Owned(quote_arg(key)),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn split(line: &str) -> Vec<Vec<u8>> {
        split_args(line).unwrap_or_else(|err| panic!("{line}: {err}"))
    }

    #[test]
    fn splits_quoted_args() {
        let expected: [&[u8]; 5] = [b"SET", b"a b", b"c d", b"e\\x", b"x\"y"];
        assert_eq!(split(r#"SET "a b" 'c d' e\x "x\"y""#), expected);
        assert!(split_args(r#"GET "oops"#).is_err());
        assert!(split_args("GET 'oops").is_err());
        assert!(split_args(r#"GET "oops\"#).is_err());
        assert!(split("  ").is_empty());
        assert_eq!(split(r#"GET """#), [&b"GET"[..], b""]);
    }

    #[test]
    fn splits_like_redis_cli() {
        // Bytes as `\xHH`, the way the app shows binary keys.
        assert_eq!(
            split(r#"GET "bin:\xff\x00\x7F""#),
            [&b"GET"[..], b"bin:\xff\x00\x7f"]
        );
        // Not two hex digits: just an `x`.
        assert_eq!(split(r#""\xg1" "\x1""#), [&b"xg1"[..], b"x1"]);
        assert_eq!(
            split(r#""\a\b\n\r\t\\\"\q""#),
            [&b"\x07\x08\n\r\t\\\"q"[..]]
        );
        // Single quotes are literal but for `\'`.
        assert_eq!(split(r"'a\x41\n\'b'"), [&br"a\x41\n'b"[..]]);
        // Outside quotes a backslash is literal too.
        assert_eq!(split(r"C:\tmp\x41"), [&br"C:\tmp\x41"[..]]);
        // A quote can start mid-argument; a closing one ends it.
        assert_eq!(split(r#"key"with space" x"#), [&b"keywith space"[..], b"x"]);
        assert_eq!(split("a'b c'\td"), [&b"ab c"[..], b"d"]);
        assert!(split_args(r#""a"b"#).is_err());
        assert!(split_args(r#"'a'"b""#).is_err());
        // Text other than ASCII passes through as UTF-8.
        assert_eq!(
            split("SET ü \"🦀\""),
            [&b"SET"[..], "ü".as_bytes(), "🦀".as_bytes()]
        );
    }

    #[test]
    fn quotes_arguments_so_they_read_back() {
        assert_eq!(quote_arg(b"user:1"), "user:1");
        assert_eq!(quote_arg(br"C:\tmp"), r"C:\tmp");
        assert_eq!(quote_arg(b"a b"), r#""a b""#);
        assert_eq!(quote_arg(b""), r#""""#);
        assert_eq!(quote_arg(b"bin:\xff\x00"), r#""bin:\xff\x00""#);
        assert_eq!(quote_arg(br#"say "hi"\n"#), r#""say \"hi\"\\n""#);
        assert_eq!(quote_arg("\u{85}".as_bytes()), r#""\xc2\x85""#);
        for key in [
            &b"bin:\x00\x01\xff\xfe"[..],
            b"bin:\x80",
            b"bin:\xc3",
            b"it's",
            b"tab\there",
            br"\x41",
            "ü 🦀".as_bytes(),
        ] {
            assert_eq!(split(&quote_arg(key)), [key], "{key:?}");
        }
        assert_eq!(copyable(b"user:1"), "user:1");
        assert_eq!(copyable(b"with space"), "with space");
        assert_eq!(copyable(b"bin:\xff"), r#""bin:\xff""#);
        assert_eq!(copyable(b"line\nbreak"), r#""line\nbreak""#);
    }

    proptest! {
        #[test]
        fn any_bytes_survive_quoting(arg in proptest::collection::vec(any::<u8>(), 0..40)) {
            prop_assert_eq!(split_args(&quote_arg(&arg)), Ok(vec![arg]));
        }

        #[test]
        fn any_text_survives_quoting(arg in "\\PC{0,20}|[ \"'\\\\\\t\\n]{0,6}") {
            let arg = arg.into_bytes();
            prop_assert_eq!(split_args(&quote_arg(&arg)), Ok(vec![arg]));
        }
    }

    #[test]
    fn escapes_binary() {
        assert_eq!(bytes(b"hi\x00\xff"), "hi\\x00\\xff");
        assert_eq!(bytes_inline(b"a\nb"), "a\\nb");
        assert_eq!(bytes(b"a\nb"), "a\nb");
        // Escaped per byte, so that it reads back as the same bytes.
        assert_eq!(bytes("a\u{85}".as_bytes()), "a\\xc2\\x85");
    }

    #[test]
    fn formats_errors_inside_replies() {
        // EXEC of SET, INCR on a non-integer, and INCR.
        let exec = redis::parse_redis_value(b"*3\r\n+OK\r\n-ERR value is not an integer\r\n:2\r\n")
            .unwrap();
        assert_eq!(
            reply(&exec),
            "1) OK\n2) (error) ERR value is not an integer\n3) (integer) 2"
        );
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(ttl(-1), None);
        assert_eq!(ttl(3_725_000).unwrap(), "1h 2m");
    }
}
