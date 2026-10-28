//! The value view every front end shows: a key's value loaded page by page,
//! the table a collection is shown as, the text of a value in a chosen
//! format with the rules for editing it, and the edits themselves (see
//! [`Mutation`]).
//!
//! The model holds data only; a front end awaits the futures it hands out
//! ([`load`], [`Loaded::load_more`], [`Mutation::send`]) on its own
//! executor.

mod edit;
mod text;

use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, Result};

pub use self::{
    edit::{
        AddRow, Applied, Mutation, NameField, NewKeyType, Outcome, RowEdit, RowSource, change_ttl,
        format_score, new_key, parse_score, parse_stream_fields, parse_ttl, rename_key, set_ttl,
        ttl_input, ttl_prefill,
    },
    text::{TextEdit, TextValue},
};
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
    /// The server ([`Client::epoch`]) a [`Cursor::Scan`] in `next` came
    /// from; `None` when that is not known (it changed while the page was
    /// loaded).
    epoch: Option<u64>,
}

/// Why [`Loaded::load_more`] cannot go on with an HSCAN or SSCAN cursor.
const SERVER_CHANGED: &str = "the server changed since this value was loaded (a Sentinel failover), so the next page cannot be found where the last one ended; reload the value";

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
        let epoch = client.epoch();
        let (page, memory) = futures::join!(
            client.load(key.clone(), meta.ty.clone(), Cursor::Start, PAGE),
            client.memory_usage(key.clone()),
        );
        let page = page?;
        let mut loaded = Loaded::new(key, meta, page, memory.ok().flatten());
        // A cursor from a page that may have come from either server
        // cannot be followed on either.
        loaded.epoch = (client.epoch() == epoch).then_some(epoch);
        Ok(loaded)
    }
}

/// A string or JSON value copied out of a [`Loaded`] value, to make its
/// [`TextEdit`] from ([`Loaded::text_source`]).
#[derive(Clone, Debug)]
pub enum TextSource {
    String { bytes: Vec<u8>, editable: bool },
    Json { json: String, writable: bool },
}

impl TextSource {
    /// Detects the format and decodes the text (inflating gzip, turning
    /// MessagePack into JSON, pretty-printing JSON): what takes a while
    /// for a big value.
    pub fn into_edit(self) -> TextEdit {
        match self {
            Self::String { bytes, editable } => TextEdit::new(bytes, editable),
            Self::Json { json, writable } => TextEdit::json_document(json, writable),
        }
    }
}

/// The key and kind a string or JSON value's text is saved to
/// ([`Loaded::text_target`]).
#[derive(Clone, Debug)]
pub struct TextTarget {
    key: Key,
    json: bool,
}

impl TextTarget {
    /// [`Loaded::save_text`]: encoding `edited` back into the value's
    /// format (gzip, MessagePack, ...) takes a while for a big value.
    pub fn save(&self, edit: &TextEdit, edited: &str) -> Result<Option<Mutation>> {
        let Some(bytes) = edit.to_save(edited)? else {
            return Ok(None);
        };
        let key = self.key.clone();
        Ok(Some(if self.json {
            Mutation::SetJson {
                key,
                json: String::from_utf8(bytes).context("JSON must be UTF-8 text")?,
            }
        } else {
            Mutation::SetString { key, value: bytes }
        }))
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
            epoch: Some(0),
        }
    }

    pub fn has_more(&self) -> bool {
        self.next.is_some()
    }

    /// Fetches the next page, or `None` once everything is loaded.
    ///
    /// A hash or set pages with an HSCAN / SSCAN cursor, which only means
    /// something on the server that handed it out. After a Sentinel
    /// failover (before or during the request) the page fails with an
    /// error asking to reload, instead of continuing at an arbitrary point
    /// of the new master's table.
    pub fn load_more(
        &self,
        client: &Client,
    ) -> Option<impl Future<Output = Result<MorePage>> + Send + 'static> {
        let from = self.next.clone()?;
        let client = client.clone();
        let (key, ty, load) = (self.key.clone(), self.meta.ty.clone(), self.id);
        // Lists, sorted sets and streams page by index or ID, which hold on
        // any server with the same data.
        let epoch = matches!(from, Cursor::Scan(_)).then_some(self.epoch);
        Some(async move {
            let current = || Some(client.epoch());
            if epoch.is_some_and(|epoch| epoch != current()) {
                anyhow::bail!(SERVER_CHANGED);
            }
            let page = client.load(key, ty, from.clone(), PAGE).await?;
            if epoch.is_some_and(|epoch| epoch != current()) {
                anyhow::bail!(SERVER_CHANGED);
            }
            Ok(MorePage { load, from, page })
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

    /// The text of a string or JSON value, in the format it looks to be in.
    /// See [`Self::text_edit`].
    pub fn text(&self, writable: bool) -> Option<TextValue> {
        self.text_edit(writable).map(|edit| edit.view().clone())
    }

    /// Editing a string or JSON value. Only a whole value (not the preview
    /// of a big string) on a `writable` connection can be edited; a JSON
    /// document of the JSON module stays JSON (saving it as a string would
    /// change the key's type).
    pub fn text_edit(&self, writable: bool) -> Option<TextEdit> {
        self.text_source(writable).map(TextSource::into_edit)
    }

    /// The cheap half of [`Self::text_edit`]: the value copied out, for
    /// [`TextSource::into_edit`] to decode away from the UI thread.
    pub fn text_source(&self, writable: bool) -> Option<TextSource> {
        match &self.data {
            ValueData::String(bytes) => Some(TextSource::String {
                bytes: bytes.clone(),
                editable: writable && !self.is_preview(),
            }),
            ValueData::Json(json) => Some(TextSource::Json {
                json: json.clone(),
                writable,
            }),
            _ => None,
        }
    }

    /// The change that saves `edited` (the editor's text) for a string or
    /// JSON value, or `None` if it would change nothing. Fails with the
    /// reason if the text does not fit its format.
    pub fn save_text(&self, edit: &TextEdit, edited: &str) -> Result<Option<Mutation>> {
        self.text_target().save(edit, edited)
    }

    /// What [`Self::save_text`] needs of the value, for a front end that
    /// encodes the text away from its UI thread.
    pub fn text_target(&self) -> TextTarget {
        TextTarget {
            key: self.key.clone(),
            json: matches!(self.data, ValueData::Json(_)),
        }
    }

    /// Tells loads apart: a reload (or another key) gets another id, a
    /// change patched into this value keeps it.
    pub fn load_id(&self) -> u64 {
        self.id
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

/// What a click on a row of a collection does; see [`click_row`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowClick {
    /// Open the row in the detail pane, in place of the one open.
    Open(usize),
    /// Leave the detail pane as it is.
    Keep,
    /// The open row has unsaved edits: ask before opening this one (and
    /// then call [`click_row`] again with `dirty` false).
    AskFirst(usize),
}

/// A click on row `ix`: a plain one picks only that row (for deleting) and
/// opens it in the detail pane; with ⌘ (`add`) it adds the row to the rows
/// picked, or takes it out, and opens nothing. The row `open` in the pane,
/// if it has unsaved edits (`dirty`), is never replaced without asking,
/// and a click on it keeps what was typed.
pub fn click_row(
    picked: &mut BTreeSet<usize>,
    open: Option<usize>,
    dirty: bool,
    ix: usize,
    add: bool,
) -> RowClick {
    if add {
        if !picked.remove(&ix) {
            picked.insert(ix);
        }
        return RowClick::Keep;
    }
    if open != Some(ix) && dirty {
        return RowClick::AskFirst(ix);
    }
    picked.clear();
    picked.insert(ix);
    if open == Some(ix) {
        RowClick::Keep
    } else {
        RowClick::Open(ix)
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

#[cfg(test)]
mod edit_tests;

#[cfg(test)]
mod tests;
