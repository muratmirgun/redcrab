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
                c if c.is_control() => {
                    let _ = write!(out, "\\x{:02x}", c as u32);
                }
                c => out.push(c),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out
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
        Value::ServerError(err) => {
            let _ = write!(out, "(error) {err:?}");
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

/// Splits a console line into arguments, honouring `"..."` (with escapes)
/// and `'...'` quoting like `redis-cli`.
pub fn split_args(line: &str) -> Result<Vec<String>, &'static str> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            return Ok(args);
        };
        let mut arg = String::new();
        match first {
            '"' => {
                chars.next();
                loop {
                    match chars.next().ok_or("unbalanced quotes")? {
                        '"' => break,
                        '\\' => match chars.next().ok_or("unbalanced quotes")? {
                            'n' => arg.push('\n'),
                            'r' => arg.push('\r'),
                            't' => arg.push('\t'),
                            c => arg.push(c),
                        },
                        c => arg.push(c),
                    }
                }
            }
            '\'' => {
                chars.next();
                loop {
                    match chars.next().ok_or("unbalanced quotes")? {
                        '\'' => break,
                        c => arg.push(c),
                    }
                }
            }
            _ => {
                while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                    arg.push(c);
                }
            }
        }
        args.push(arg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_args() {
        assert_eq!(
            split_args(r#"SET "a b" 'c d' e\x "x\"y""#).unwrap(),
            ["SET", "a b", "c d", "e\\x", "x\"y"]
        );
        assert!(split_args(r#"GET "oops"#).is_err());
    }

    #[test]
    fn escapes_binary() {
        assert_eq!(bytes(b"hi\x00\xff"), "hi\\x00\\xff");
        assert_eq!(bytes_inline(b"a\nb"), "a\\nb");
        assert_eq!(bytes(b"a\nb"), "a\nb");
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(ttl(-1), None);
        assert_eq!(ttl(3_725_000).unwrap(), "1h 2m");
    }
}
