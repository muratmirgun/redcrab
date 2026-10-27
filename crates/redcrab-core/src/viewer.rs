//! The value view every front end shows: a key's value loaded page by page,
//! the table a collection is shown as, and the text of a string or JSON
//! value with the rules for editing it.
//!
//! The model holds data only; a front end awaits the futures it hands out
//! ([`load`], [`Loaded::load_more`]) on its own executor.

use std::{
    borrow::Cow,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, Result};

use crate::{
    client::{Client, Key},
    format,
    value::{Cursor, KeyMeta, KeyType, ValueData, ValuePage},
};

/// Elements fetched per page of a collection.
pub const PAGE: usize = 500;
/// Bytes of a value shown in a table cell; the whole value is in
/// [`Loaded::row_value`].
pub const CELL_PREVIEW: usize = 256;

/// Tells loads apart, so a page fetched for one load is never appended to
/// another (say, a reload of the same key).
static LOADS: AtomicU64 = AtomicU64::new(0);

/// A key's value as far as it has been loaded.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub key: Key,
    /// Type and TTL, read fresh when the value was loaded.
    pub meta: KeyMeta,
    pub data: ValueData,
    /// Elements in the whole value (bytes for a string).
    pub total: u64,
    /// Where the next page starts; `None` once everything is loaded.
    pub next: Option<Cursor>,
    /// `MEMORY USAGE`, if the server allows it.
    pub memory: Option<u64>,
    id: u64,
}

/// Reads the type and TTL of `key` fresh (the key list's may be stale),
/// then its first page and its memory usage at the same time.
pub fn load(client: &Client, key: Key) -> impl Future<Output = Result<Loaded>> + Send + 'static {
    let client = client.clone();
    async move {
        let meta = client
            .key_meta(vec![key.clone()])
            .await?
            .pop()
            .context("empty reply")?;
        let (page, memory) = futures::join!(
            client.load(key.clone(), meta.ty.clone(), Cursor::Start, PAGE),
            client.memory_usage(key.clone()),
        );
        let page = page?;
        Ok(Loaded::new(key, meta, page, memory.ok().flatten()))
    }
}

/// A following page of a [`Loaded`] value, for [`Loaded::apply_page`].
pub struct MorePage {
    load: u64,
    from: Cursor,
    page: ValuePage,
}

/// The table a collection is shown as.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Columns {
    /// Header of the first column; `None` when rows have one column (sets).
    pub first: Option<&'static str>,
    pub second: &'static str,
    /// The share of the width the first column takes when its content is as
    /// long as the second's (hash fields); `None` for a narrow fixed column
    /// (indexes, scores, stream IDs).
    pub first_share: Option<f32>,
}

pub fn columns(ty: &KeyType) -> Columns {
    let (first, second, first_share) = match ty {
        KeyType::Hash => (Some("Field"), "Value", Some(0.35)),
        KeyType::List => (Some("#"), "Value", None),
        KeyType::Set => (None, "Member", None),
        KeyType::ZSet => (Some("Score"), "Member", None),
        KeyType::Stream => (Some("ID"), "Fields", None),
        _ => (None, "", None),
    };
    Columns {
        first,
        second,
        first_share,
    }
}

impl Loaded {
    pub fn new(key: Key, meta: KeyMeta, page: ValuePage, memory: Option<u64>) -> Self {
        Self {
            key,
            meta,
            data: page.data,
            total: page.total,
            next: page.next,
            memory,
            id: LOADS.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn has_more(&self) -> bool {
        self.next.is_some()
    }

    /// Fetches the next page, or `None` once everything is loaded.
    pub fn load_more(
        &self,
        client: &Client,
    ) -> Option<impl Future<Output = Result<MorePage>> + Send + 'static> {
        let from = self.next.clone()?;
        let request = client.load(self.key.clone(), self.meta.ty.clone(), from.clone(), PAGE);
        let load = self.id;
        Some(async move {
            Ok(MorePage {
                load,
                from,
                page: request.await?,
            })
        })
    }

    /// Appends a page from [`Self::load_more`]. A page that does not continue
    /// this value where it ends (it belongs to another load, or arrived
    /// twice) is dropped and `false` returned.
    pub fn apply_page(&mut self, more: MorePage) -> bool {
        if more.load != self.id || self.next.as_ref() != Some(&more.from) {
            return false;
        }
        self.data.extend(more.page.data);
        self.next = more.page.next;
        self.total = more.page.total;
        true
    }

    pub fn columns(&self) -> Columns {
        columns(&self.meta.ty)
    }

    /// Rows of the collection table; 0 for values shown as text.
    pub fn row_count(&self) -> usize {
        match &self.data {
            ValueData::Hash(_)
            | ValueData::List(_)
            | ValueData::Set(_)
            | ValueData::ZSet(_)
            | ValueData::Stream(_) => self.data.len(),
            _ => 0,
        }
    }

    /// The (optional first column, second column) text of row `ix`, each on
    /// one line and cut to about [`CELL_PREVIEW`] bytes, so drawing a row
    /// stays cheap however big its value is.
    pub fn row_cells(&self, ix: usize) -> (Option<String>, String) {
        let cells = match &self.data {
            ValueData::Hash(pairs) => pairs.get(ix).map(|(k, v)| (Some(cell(k)), cell(v))),
            ValueData::List(items) => items.get(ix).map(|v| (Some(ix.to_string()), cell(v))),
            ValueData::Set(items) => items.get(ix).map(|v| (None, cell(v))),
            ValueData::ZSet(members) => members
                .get(ix)
                .map(|(m, score)| (Some(score.to_string()), cell(m))),
            ValueData::Stream(entries) => entries.get(ix).map(|entry| {
                let mut fields = String::new();
                for (k, v) in &entry.fields {
                    if fields.len() > CELL_PREVIEW {
                        break;
                    }
                    if !fields.is_empty() {
                        fields.push_str("  ");
                    }
                    fields.push_str(&cell(k));
                    fields.push('=');
                    fields.push_str(&cell(v));
                }
                (Some(entry.id.clone()), fields)
            }),
            _ => None,
        };
        cells.unwrap_or_default()
    }

    /// The whole value of row `ix`, for a detail pane. A stream entry comes
    /// as a JSON object of its fields, in their order.
    pub fn row_value(&self, ix: usize) -> Option<Vec<u8>> {
        Some(match &self.data {
            ValueData::Hash(pairs) => pairs.get(ix)?.1.clone(),
            ValueData::List(items) | ValueData::Set(items) => items.get(ix)?.clone(),
            ValueData::ZSet(members) => members.get(ix)?.0.clone(),
            ValueData::Stream(entries) => {
                let mut json = String::from("{");
                for (i, (k, v)) in entries.get(ix)?.fields.iter().enumerate() {
                    if i > 0 {
                        json.push(',');
                    }
                    json_string(&mut json, &format::bytes(k));
                    json.push(':');
                    json_string(&mut json, &format::bytes(v));
                }
                json.push('}');
                json.into_bytes()
            }
            _ => return None,
        })
    }

    /// [`Self::row_value`] as read-only text.
    pub fn row_text(&self, ix: usize) -> Option<TextValue> {
        self.row_value(ix)
            .map(|bytes| TextValue::new(&bytes, false))
    }

    /// The text of a string or JSON value. Only a whole string value on a
    /// `writable` connection can be edited; JSON documents are read-only
    /// (saving them as a string would change the key's type).
    pub fn text(&self, writable: bool) -> Option<TextValue> {
        match &self.data {
            ValueData::String(bytes) => Some(TextValue::new(bytes, writable && !self.is_preview())),
            ValueData::Json(json) => Some(TextValue::new(json.as_bytes(), false)),
            _ => None,
        }
    }

    /// Whether only the beginning of a big string was loaded.
    pub fn is_preview(&self) -> bool {
        matches!(&self.data, ValueData::String(b) if (b.len() as u64) < self.total)
    }

    /// Explains a preview: "Showing the first 1.0 MB of 5.0 MB. …".
    pub fn preview_notice(&self) -> Option<String> {
        match &self.data {
            ValueData::String(b) if self.is_preview() => Some(format!(
                "Showing the first {} of {}. Large values are read-only.",
                format::size(b.len() as u64),
                format::size(self.total)
            )),
            _ => None,
        }
    }

    /// Size for strings and JSON ("1.2 KB"), element count for collections.
    pub fn summary(&self) -> Option<String> {
        match self.meta.ty {
            KeyType::String | KeyType::Json => Some(format::size(self.total)),
            KeyType::Other(_) | KeyType::None => None,
            _ => Some(format!("{} items", format::count(self.total))),
        }
    }

    pub fn ttl_text(&self) -> String {
        format::ttl(self.meta.ttl_ms).unwrap_or_else(|| "No expiry".into())
    }

    /// "Showing 500 of 20,000".
    pub fn footer(&self) -> String {
        format!(
            "Showing {} of {}",
            format::count(self.row_count() as u64),
            format::count(self.total)
        )
    }
}

fn cell(bytes: &[u8]) -> String {
    let end = bytes.len().min(CELL_PREVIEW);
    format::bytes_inline(&bytes[..end]).into_owned()
}

fn json_string(out: &mut String, s: &str) {
    match serde_json::to_string(s) {
        Ok(quoted) => out.push_str(&quoted),
        Err(_) => out.push_str("\"\""),
    }
}

/// A string or JSON value as text.
#[derive(Clone, Debug, PartialEq)]
pub struct TextValue {
    /// What to show: JSON indented, anything else escaped like `redis-cli`.
    pub text: String,
    /// Valid JSON (an object or array), worth highlighting as such.
    pub is_json: bool,
    /// The JSON was indented for display; an edit is saved back on one
    /// line, spaced the way it was stored.
    pub reformatted_json: bool,
    /// Editing is allowed and `text` is the value's bytes, or (for indented
    /// JSON) the same tokens laid out differently, so saving cannot change
    /// anything the user did not touch.
    pub editable: bool,
    /// How the stored JSON was spaced, when it was indented for display.
    spacing: Option<Spacing>,
}

impl TextValue {
    /// `editable` is whether the caller allows editing (a whole value on a
    /// writable connection); the result is editable only if the value is
    /// also plain text that shows without escapes.
    ///
    /// JSON on one line, compact or with a space after each `,` and `:` (as
    /// Python writes it), is indented for display, since saving restores
    /// exactly that spacing. JSON laid out any other way (indented already,
    /// say) is shown as stored, so that saving keeps its layout.
    pub fn new(bytes: &[u8], editable: bool) -> Self {
        let lossless = matches!(format::bytes(bytes), Cow::Borrowed(_));
        let editable = editable && lossless;
        let Some(json) = std::str::from_utf8(bytes)
            .ok()
            .filter(|s| is_json_document(s))
        else {
            return Self {
                text: format::bytes(bytes).into_owned(),
                is_json: false,
                reformatted_json: false,
                editable,
                spacing: None,
            };
        };
        let spacing = one_line_spacing(json);
        // Text that cannot be shown as stored (a `\r` in it) is indented
        // too; it is not editable anyway.
        if spacing.is_none() && lossless {
            return Self {
                text: json.to_string(),
                is_json: true,
                reformatted_json: false,
                editable,
                spacing: None,
            };
        }
        let pretty = pretty_json(json);
        Self {
            reformatted_json: pretty != json,
            is_json: true,
            text: pretty,
            editable,
            spacing,
        }
    }

    /// The bytes to write for the edited text: JSON that was indented for
    /// display goes back on one line, spaced as it was stored, if it still
    /// parses; anything else is written as typed.
    pub fn bytes_to_save(&self, edited: &str) -> Vec<u8> {
        match self.spacing {
            Some(spacing) if self.reformatted_json && is_json_document(edited) => {
                spaced_json(&compact_json(edited), spacing).into_bytes()
            }
            _ => edited.as_bytes().to_vec(),
        }
    }
}

/// How JSON on one line is spaced: whether a space follows each `,` and
/// each `:` between tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Spacing {
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
fn one_line_spacing(json: &str) -> Option<Spacing> {
    let compact = compact_json(json);
    SPACINGS
        .into_iter()
        .find(|&spacing| spaced_json(&compact, spacing) == json)
}

/// Compact JSON with a space after each `,` and/or `:` between tokens.
fn spaced_json(compact: &str, spacing: Spacing) -> String {
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
fn is_json_document(s: &str) -> bool {
    s.trim_start().starts_with(['{', '['])
        && serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
}

/// Indents valid JSON like `serde_json`'s pretty printer, but by moving
/// whitespace only: key order, number spelling and string escapes stay
/// exactly as stored, so saving it back on one line changes nothing else.
fn pretty_json(json: &str) -> String {
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
fn compact_json(json: &str) -> String {
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

#[cfg(test)]
mod tests;
