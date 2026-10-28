//! JSON laid out for reading and back, by moving whitespace only: key
//! order, number spelling and string escapes stay exactly as stored, so a
//! value shown indented can be saved back on one line unchanged.

/// How JSON on one line is spaced: whether a space follows each `,` and
/// each `:` between tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Spacing {
    comma: bool,
    colon: bool,
}

/// The spacings recognised, the most common first: compact (most
/// serializers) and Python's `json.dumps` default. A document without
/// colons or commas fits several, and gets the first.
const SPACINGS: [Spacing; 4] = [
    Spacing {
        comma: false,
        colon: false,
    },
    Spacing {
        comma: true,
        colon: true,
    },
    Spacing {
        comma: true,
        colon: false,
    },
    Spacing {
        comma: false,
        colon: true,
    },
];

/// The spacing of `json` if it is all on one line, spaced consistently.
pub(crate) fn one_line_spacing(json: &str) -> Option<Spacing> {
    let compact = compact(json);
    SPACINGS
        .into_iter()
        .find(|&spacing| spaced(&compact, spacing) == json)
}

/// Compact JSON with a space after each `,` and/or `:` between tokens.
pub(crate) fn spaced(compact: &str, spacing: Spacing) -> String {
    let bytes = compact.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 4);
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let end = string_end(bytes, i);
                out.extend_from_slice(&bytes[i..end]);
                i = end;
                continue;
            }
            b',' if spacing.comma => out.extend_from_slice(b", "),
            b':' if spacing.colon => out.extend_from_slice(b": "),
            b => out.push(b),
        }
        i += 1;
    }
    into_string(out)
}

/// An object or array that parses as JSON.
pub(crate) fn is_document(s: &str) -> bool {
    s.trim_start().starts_with(['{', '[']) && is_valid(s)
}

/// Any JSON value: a document, a string, a number, `true`, `null`, ...
pub(crate) fn is_valid(s: &str) -> bool {
    check(s).is_ok()
}

/// Why `s` is not JSON, as serde_json says it ("expected `,` or `}` at
/// line 3 column 5").
pub(crate) fn check(s: &str) -> Result<(), serde_json::Error> {
    serde_json::from_str::<serde::de::IgnoredAny>(s).map(drop)
}

/// Indents valid JSON like `serde_json`'s pretty printer, but by moving
/// whitespace only: key order, number spelling and string escapes stay
/// exactly as stored, so saving it back on one line changes nothing else.
pub(crate) fn pretty(json: &str) -> String {
    let bytes = json.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 2);
    let mut depth = 0usize;
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        out.resize(out.len() + depth * 2, b' ');
    };
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'"' => {
                let end = string_end(bytes, i);
                out.extend_from_slice(&bytes[i..end]);
                i = end;
            }
            open @ (b'{' | b'[') => {
                let close = if open == b'{' { b'}' } else { b']' };
                let next = skip_whitespace(bytes, i + 1);
                if bytes.get(next) == Some(&close) {
                    out.extend_from_slice(&[open, close]);
                    i = next + 1;
                } else {
                    out.push(open);
                    depth += 1;
                    newline(&mut out, depth);
                    i += 1;
                }
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(close);
                i += 1;
            }
            b',' => {
                out.push(b',');
                newline(&mut out, depth);
                i += 1;
            }
            b':' => {
                out.extend_from_slice(b": ");
                i += 1;
            }
            _ => {
                let start = i;
                while i < bytes.len() && !is_structural(bytes[i]) {
                    i += 1;
                }
                out.extend_from_slice(&bytes[start..i]);
            }
        }
    }
    into_string(out)
}

/// Drops the whitespace between JSON tokens.
pub(crate) fn compact(json: &str) -> String {
    let bytes = json.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'"' => {
                let end = string_end(bytes, i);
                out.extend_from_slice(&bytes[i..end]);
                i = end;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    into_string(out)
}

/// The integers in valid JSON `json` (numbers without a fraction or an
/// exponent), as written, strings skipped.
pub(crate) fn integers(json: &str) -> impl Iterator<Item = &str> {
    let bytes = json.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            match bytes[i] {
                b'"' => i = string_end(bytes, i),
                b'-' | b'0'..=b'9' => {
                    let start = i;
                    while i < bytes.len()
                        && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                    {
                        i += 1;
                    }
                    let number = &json[start..i];
                    if !number.contains(['.', 'e', 'E']) {
                        return Some(number);
                    }
                }
                _ => i += 1,
            }
        }
        None
    })
}

/// The index just past the string that starts with the quote at `start`.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

fn skip_whitespace(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

fn is_structural(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b'{' | b'}' | b'[' | b']' | b'"'
    )
}

/// Only ASCII is ever added or removed, so valid UTF-8 stays valid; the
/// fallback is for input that was not valid to begin with.
fn into_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}
