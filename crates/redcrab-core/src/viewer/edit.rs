//! Edits: the changes a front end sends ([`Mutation`]), the models that
//! build them from what the user typed and check it first ([`RowEdit`],
//! [`AddRow`], [`new_key`], [`parse_ttl`], ...), and how a loaded value takes
//! a change in without being loaded again ([`Loaded::apply`]).

use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeSet, HashSet},
    fmt,
};

use anyhow::{Result, anyhow, bail, ensure};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};

use super::{Loaded, TextEdit};
use crate::{
    client::{Client, Key, ListEnd, MAX_TTL_SECS, NewValue, Removed, check_stream_id, score_arg},
    format,
    formats::json,
    value::{Cursor, KeyType, StreamEntry, ValueData},
};

/// One change to a key, ready to send with [`Mutation::send`].
#[derive(Clone, Debug, PartialEq)]
pub enum Mutation {
    SetString {
        key: Key,
        value: Vec<u8>,
    },
    SetJson {
        key: Key,
        json: String,
    },
    /// Sets a field, creating it if it is new (the add dialog); refused
    /// for one that exists unless `overwrite`.
    HashSet {
        key: Key,
        field: Vec<u8>,
        value: Vec<u8>,
        overwrite: bool,
    },
    HashDelete {
        key: Key,
        fields: Vec<Vec<u8>>,
    },
    /// Changes the existing field `from`: renames it to `to` (the same name
    /// keeps it, for a new value only) and/or gives it a new value, keeping
    /// its TTL. Refused if `from` is gone, so an edit never brings back a
    /// field deleted or expired since it was loaded.
    HashRenameField {
        key: Key,
        from: Vec<u8>,
        to: Vec<u8>,
        /// The new value; `None` keeps the old one.
        value: Option<Vec<u8>>,
        overwrite: bool,
    },
    ListPush {
        key: Key,
        values: Vec<Vec<u8>>,
        end: ListEnd,
    },
    ListSet {
        key: Key,
        index: i64,
        expected: Vec<u8>,
        value: Vec<u8>,
    },
    ListRemoveAt {
        key: Key,
        /// Index and the element expected there.
        elements: Vec<(i64, Vec<u8>)>,
    },
    SetAdd {
        key: Key,
        members: Vec<Vec<u8>>,
    },
    SetRemove {
        key: Key,
        members: Vec<Vec<u8>>,
    },
    SetReplace {
        key: Key,
        from: Vec<u8>,
        to: Vec<u8>,
        overwrite: bool,
    },
    ZSetAdd {
        key: Key,
        member: Vec<u8>,
        score: f64,
        overwrite: bool,
    },
    ZSetRename {
        key: Key,
        from: Vec<u8>,
        to: Vec<u8>,
        /// The new score; `None` keeps the old one.
        score: Option<f64>,
        overwrite: bool,
    },
    ZSetRemove {
        key: Key,
        members: Vec<Vec<u8>>,
    },
    StreamAdd {
        key: Key,
        id: String,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
    },
    StreamDelete {
        key: Key,
        ids: Vec<String>,
    },
    CreateKey {
        key: Key,
        value: NewValue,
        /// Seconds.
        ttl: Option<u64>,
    },
    Rename {
        from: Key,
        to: Key,
    },
    SetTtl {
        key: Key,
        /// `None` removes the TTL.
        seconds: Option<u64>,
    },
}

/// What the server answered to a [`Mutation`], as far as a view cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Elements added (for a list push, the new length).
    Count(u64),
    /// Elements removed, and how many are left.
    Removed(Removed),
    /// Whether the element written is new.
    Created(bool),
    /// The ID of a new stream entry.
    StreamId(String),
}

/// What [`Loaded::apply`] did with a change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The loaded value shows the change; the row of the element changed
    /// or added, if there is one, is worth selecting.
    Patched(Option<usize>),
    /// The change cannot be shown without loading the value again (it
    /// lands on a page not loaded yet, say).
    Reload,
    /// The key is gone: its last element was removed (streams excepted,
    /// which Redis keeps empty).
    KeyGone,
}

impl Mutation {
    /// The key changed (the old name, for a rename).
    pub fn key(&self) -> &Key {
        match self {
            Self::SetString { key, .. }
            | Self::SetJson { key, .. }
            | Self::HashSet { key, .. }
            | Self::HashDelete { key, .. }
            | Self::HashRenameField { key, .. }
            | Self::ListPush { key, .. }
            | Self::ListSet { key, .. }
            | Self::ListRemoveAt { key, .. }
            | Self::SetAdd { key, .. }
            | Self::SetRemove { key, .. }
            | Self::SetReplace { key, .. }
            | Self::ZSetAdd { key, .. }
            | Self::ZSetRename { key, .. }
            | Self::ZSetRemove { key, .. }
            | Self::StreamAdd { key, .. }
            | Self::StreamDelete { key, .. }
            | Self::CreateKey { key, .. }
            | Self::SetTtl { key, .. } => key,
            Self::Rename { from, .. } => from,
        }
    }

    /// Sends the change. Refusals come back as
    /// [`EditError`](crate::EditError)s, a read-only connection's as a
    /// [`ReadOnlyError`](crate::ReadOnlyError).
    pub fn send(self, client: &Client) -> impl Future<Output = Result<Outcome>> + Send + 'static {
        let client = client.clone();
        async move {
            Ok(match self {
                Self::SetString { key, value } => {
                    client.set_string(key, value).await?;
                    Outcome::Done
                }
                Self::SetJson { key, json } => {
                    client.set_json(key, json).await?;
                    Outcome::Done
                }
                Self::HashSet {
                    key,
                    field,
                    value,
                    overwrite,
                } => Outcome::Created(client.hash_set(key, field, value, overwrite).await?),
                Self::HashDelete { key, fields } => {
                    Outcome::Removed(client.hash_delete(key, fields).await?)
                }
                Self::HashRenameField {
                    key,
                    from,
                    to,
                    value,
                    overwrite,
                } => {
                    client
                        .hash_rename_field(key, from, to, value, overwrite)
                        .await?;
                    Outcome::Done
                }
                Self::ListPush { key, values, end } => {
                    Outcome::Count(client.list_push(key, values, end).await?)
                }
                Self::ListSet {
                    key,
                    index,
                    expected,
                    value,
                } => {
                    client.list_set(key, index, expected, value).await?;
                    Outcome::Done
                }
                Self::ListRemoveAt { key, elements } => {
                    Outcome::Removed(client.list_remove_at(key, elements).await?)
                }
                Self::SetAdd { key, members } => {
                    Outcome::Count(client.set_add(key, members).await?)
                }
                Self::SetRemove { key, members } => {
                    Outcome::Removed(client.set_remove(key, members).await?)
                }
                Self::SetReplace {
                    key,
                    from,
                    to,
                    overwrite,
                } => Outcome::Created(client.set_replace(key, from, to, overwrite).await?),
                Self::ZSetAdd {
                    key,
                    member,
                    score,
                    overwrite,
                } => Outcome::Created(client.zset_add(key, member, score, overwrite).await?),
                Self::ZSetRename {
                    key,
                    from,
                    to,
                    score,
                    overwrite,
                } => {
                    client
                        .zset_rename_member(key, from, to, score, overwrite)
                        .await?;
                    Outcome::Done
                }
                Self::ZSetRemove { key, members } => {
                    Outcome::Removed(client.zset_remove(key, members).await?)
                }
                Self::StreamAdd { key, id, fields } => {
                    Outcome::StreamId(client.stream_add(key, id, fields).await?)
                }
                Self::StreamDelete { key, ids } => {
                    Outcome::Removed(client.stream_delete(key, ids).await?)
                }
                Self::CreateKey { key, value, ttl } => {
                    client.create_key(key, value, ttl).await?;
                    Outcome::Done
                }
                Self::Rename { from, to } => {
                    client.rename(from, to).await?;
                    Outcome::Done
                }
                Self::SetTtl { key, seconds } => {
                    client.set_ttl(key, seconds).await?;
                    Outcome::Done
                }
            })
        }
    }

    /// The same change, allowed to replace an element that exists. For
    /// after an [`EditError::ElementExists`](crate::EditError), once the
    /// user agreed; `None` for changes that never ask.
    pub fn overwriting(&self) -> Option<Self> {
        let mut again = self.clone();
        match &mut again {
            Self::HashSet { overwrite, .. }
            | Self::HashRenameField { overwrite, .. }
            | Self::SetReplace { overwrite, .. }
            | Self::ZSetAdd { overwrite, .. }
            | Self::ZSetRename { overwrite, .. } => {
                *overwrite = true;
                Some(again)
            }
            _ => None,
        }
    }

    /// The question to ask before [`Self::overwriting`]: "A field named
    /// `name` already exists. Replace it?"
    pub fn overwrite_question(&self) -> Option<String> {
        let (what, name) = match self {
            Self::HashSet { field, .. } => ("A field", field),
            Self::HashRenameField { to, .. } => ("A field", to),
            Self::SetReplace { to, .. } => ("The member", to),
            Self::ZSetAdd { member, .. } => ("The member", member),
            Self::ZSetRename { to, .. } => ("The member", to),
            _ => return None,
        };
        let name = format::bytes_inline(&name[..name.len().min(80)]).into_owned();
        Some(match self {
            Self::HashSet { .. } | Self::HashRenameField { .. } => {
                format!("{what} named \u{201c}{name}\u{201d} already exists. Replace its value?")
            }
            Self::SetReplace { .. } => format!(
                "{what} \u{201c}{name}\u{201d} already exists. Remove the old member and keep the existing one?"
            ),
            _ => format!("{what} \u{201c}{name}\u{201d} already exists. Replace its score?"),
        })
    }

    /// What to tell the user once the change is done.
    pub fn done_message(&self, outcome: &Outcome) -> String {
        let count = match outcome {
            Outcome::Count(n) | Outcome::Removed(Removed { count: n, .. }) => *n,
            _ => 0,
        };
        let plural = |n: u64, one: &str, many: &str| {
            format!("{} {}", format::count(n), if n == 1 { one } else { many })
        };
        match self {
            Self::SetString { .. } | Self::SetJson { .. } => "Value saved".into(),
            Self::HashSet { .. } if *outcome == Outcome::Created(true) => "Field added".into(),
            Self::HashSet { .. } => "Field saved".into(),
            Self::HashRenameField { .. } => "Field saved".into(),
            Self::HashDelete { .. } => format!("Deleted {}", plural(count, "field", "fields")),
            Self::ListPush { values, .. } => {
                format!(
                    "Added {}",
                    plural(values.len() as u64, "element", "elements")
                )
            }
            Self::ListSet { .. } => "Element saved".into(),
            Self::ListRemoveAt { .. } => {
                format!("Deleted {}", plural(count, "element", "elements"))
            }
            Self::SetAdd { .. } if count == 0 => "Already a member".into(),
            Self::SetAdd { .. } => format!("Added {}", plural(count, "member", "members")),
            Self::SetRemove { .. } | Self::ZSetRemove { .. } => {
                format!("Deleted {}", plural(count, "member", "members"))
            }
            Self::SetReplace { .. } | Self::ZSetRename { .. } => "Member saved".into(),
            Self::ZSetAdd { .. } if *outcome == Outcome::Created(false) => "Score saved".into(),
            Self::ZSetAdd { .. } => "Member added".into(),
            Self::StreamAdd { .. } => match outcome {
                Outcome::StreamId(id) => format!("Added entry {id}"),
                _ => "Entry added".into(),
            },
            Self::StreamDelete { .. } => format!("Deleted {}", plural(count, "entry", "entries")),
            Self::CreateKey { key, .. } => format!("Created {}", format::bytes_inline(key)),
            Self::Rename { to, .. } => format!("Renamed to {}", format::bytes_inline(to)),
            Self::SetTtl {
                seconds: Some(s), ..
            } => format!("Expires in {}", ttl_input(*s)),
            Self::SetTtl { seconds: None, .. } => "Expiry removed".into(),
        }
    }
}

// Parsing what the user typed.

/// A TTL as typed: seconds (`90`), or amounts with units (`1h 30m`,
/// `2d`, `45s`, `1w`; also `sec`, `min`, `hours`, ...). Empty, `none`,
/// `never` or `persist` mean no expiry (`None`). A TTL of 0 is refused: it
/// would delete the key.
pub fn parse_ttl(input: &str) -> Result<Option<u64>> {
    let input = input.trim();
    let lower = input.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "" | "none" | "never" | "persist" | "no expiry" | "-1"
    ) {
        return Ok(None);
    }
    let bad = || {
        anyhow!("{input:?} is not a duration: use seconds (3600) or units such as 1h 30m, 2d, 45s")
    };
    let mut total = 0u64;
    let mut rest = lower.as_str();
    loop {
        rest = rest.trim_start_matches([' ', ',', '\t']);
        if rest.is_empty() {
            break;
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(bad());
        }
        let amount: u64 = rest[..digits]
            .parse()
            .map_err(|_| anyhow!("{input:?} is too long a TTL"))?;
        rest = rest[digits..].trim_start();
        let letters = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        let unit = match &rest[..letters] {
            "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 604_800,
            _ => return Err(bad()),
        };
        rest = &rest[letters..];
        total = amount
            .checked_mul(unit)
            .and_then(|s| total.checked_add(s))
            .ok_or_else(|| anyhow!("{input:?} is too long a TTL"))?;
    }
    ensure!(
        total > 0,
        "a TTL of 0 would delete the key at once; delete it instead"
    );
    ensure!(total <= MAX_TTL_SECS, "{input:?} is too long a TTL");
    Ok(Some(total))
}

/// Seconds as [`parse_ttl`] reads them: `1d 2h 3m 4s`, zero parts left out.
pub fn ttl_input(seconds: u64) -> String {
    let parts = [
        (seconds / 86_400, "d"),
        (seconds % 86_400 / 3600, "h"),
        (seconds % 3600 / 60, "m"),
        (seconds % 60, "s"),
    ];
    let text: Vec<String> = parts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect();
    if text.is_empty() {
        "0s".into()
    } else {
        text.join(" ")
    }
}

/// A sorted set score as typed: any number, `inf`, `-inf`. NaN is refused.
pub fn parse_score(input: &str) -> Result<f64> {
    let s = input.trim();
    let score: f64 = s.parse().map_err(|_| {
        anyhow!("{s:?} is not a number; a score is a number such as 1, -2.5, 1e6 or inf")
    })?;
    ensure!(!score.is_nan(), "a score must be a number, not NaN");
    Ok(score)
}

/// A score as text that [`parse_score`] reads back exactly.
pub fn format_score(score: f64) -> String {
    match score_arg(score) {
        Ok(text) => text.trim_start_matches('+').to_string(),
        Err(_) => "nan".into(),
    }
}

/// A stream entry's fields as typed: a JSON object (`{"name": "Ada",
/// "age": 36}`, in that order; values that are not strings as their JSON),
/// or `field value` pairs split like the console splits a line, so values
/// with spaces go in quotes: `name "Ada Lovelace" age 36`.
pub fn parse_stream_fields(input: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let fields = if input.trim_start().starts_with('{') {
        serde_json::from_str::<Fields>(input)
            .map_err(|err| anyhow!("not a JSON object of fields: {err}"))?
            .0
    } else {
        let args = format::split_args(input).map_err(|err| anyhow!("{err}"))?;
        ensure!(
            args.len() % 2 == 0,
            "fields come in pairs: field value [field value ...]"
        );
        let mut args = args.into_iter();
        let mut pairs = Vec::new();
        while let (Some(field), Some(value)) = (args.next(), args.next()) {
            pairs.push((field, value));
        }
        pairs
    };
    ensure!(
        !fields.is_empty(),
        "a stream entry needs at least one field"
    );
    Ok(fields)
}

/// A JSON object's members in order, duplicates kept.
struct Fields(Vec<(Vec<u8>, Vec<u8>)>);

impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> Visitor<'de> for FieldsVisitor {
            type Value = Fields;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
                let mut fields = Vec::new();
                while let Some((field, value)) = map.next_entry::<String, serde_json::Value>()? {
                    let value = match value {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    };
                    fields.push((field.into_bytes(), value.into_bytes()));
                }
                Ok(Fields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

/// The change for a new TTL typed for `key` (see [`parse_ttl`]).
pub fn set_ttl(key: &Key, input: &str) -> Result<Mutation> {
    Ok(Mutation::SetTtl {
        key: key.clone(),
        seconds: parse_ttl(input)?,
    })
}

/// What a TTL input starts with for a key that has `ttl_ms` left (negative:
/// no expiry): the TTL in whole seconds, rounded up, as [`parse_ttl`] reads
/// it, or nothing.
pub fn ttl_prefill(ttl_ms: i64) -> String {
    if ttl_ms > 0 {
        ttl_input((ttl_ms as u64).div_ceil(1000))
    } else {
        String::new()
    }
}

/// The change for a TTL typed for `key` into an input that started as
/// `prefilled` ([`ttl_prefill`]), or `None` if it still reads the same.
/// The TTL shown was read when the value was loaded; sending it back
/// unchanged would not keep the expiry but push it back to that (rounded
/// up) amount from now.
pub fn change_ttl(key: &Key, prefilled: &str, input: &str) -> Result<Option<Mutation>> {
    if input.trim() == prefilled.trim() {
        return Ok(None);
    }
    set_ttl(key, input).map(Some)
}

/// The change for a new name typed for `key`, or `None` if it is the same
/// name. The name is taken as typed, as UTF-8.
pub fn rename_key(key: &Key, input: &str) -> Result<Option<Mutation>> {
    ensure!(!input.is_empty(), "a key needs a name");
    if input.as_bytes() == &**key {
        return Ok(None);
    }
    Ok(Some(Mutation::Rename {
        from: key.clone(),
        to: Key::from(input.as_bytes()),
    }))
}

// A row of a collection, opened for editing.

/// The second input a row edit has besides its value: a hash field's
/// name, or a sorted set member's score.
#[derive(Clone, Debug, PartialEq)]
pub struct NameField {
    /// "Field" or "Score".
    pub label: &'static str,
    /// Always one line, for a one-line input.
    pub text: String,
    /// A field name that is not plain text on one line (shown escaped)
    /// cannot be renamed; its value can still be edited.
    pub editable: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Target {
    Hash { field: Vec<u8> },
    List { index: i64, element: Vec<u8> },
    Set { member: Vec<u8> },
    ZSet { member: Vec<u8>, score: f64 },
    Stream,
}

/// One row of a collection opened for editing: a hash field (name and
/// value), a list element, a set member, a sorted set member and its
/// score. Stream entries cannot be changed, only added and deleted.
///
/// It holds no key: [`Loaded::save_row`] takes the key from the loaded
/// value, so a row open while the key is renamed saves under the new name.
#[derive(Clone, Debug)]
pub struct RowEdit {
    row: usize,
    target: Target,
    value: TextEdit,
    name: Option<NameField>,
}

impl RowEdit {
    /// The row's index in the loaded value.
    pub fn row(&self) -> usize {
        self.row
    }

    /// The row's main bytes: the hash value, list element, or member.
    pub fn value(&self) -> &TextEdit {
        &self.value
    }

    /// For switching the format the value is shown in.
    pub fn value_mut(&mut self) -> &mut TextEdit {
        &mut self.value
    }

    pub fn name(&self) -> Option<&NameField> {
        self.name.as_ref()
    }

    /// Whether anything of the row can be changed.
    pub fn editable(&self) -> bool {
        self.value.editable() || self.name.as_ref().is_some_and(|name| name.editable)
    }

    /// The change to `key` that saves the row as edited; see
    /// [`Loaded::save_row`].
    fn save(&self, key: &Key, value: &str, name: Option<&str>) -> Result<Option<Mutation>> {
        // Untouched text stands for the stored bytes, editable or not.
        let new_value = self.value.to_save(value)?;
        // The name input's text, if it was changed.
        let new_name = match (&self.name, name) {
            (Some(field), Some(text)) if text != field.text => {
                ensure!(
                    field.editable,
                    "the {} cannot be changed here",
                    field.label.to_lowercase()
                );
                Some(text)
            }
            _ => None,
        };
        let key = key.clone();
        Ok(match &self.target {
            Target::Hash { field } => match (new_name, new_value) {
                (None, None) => None,
                // A field "renamed" to itself: the change is refused if the
                // field is gone (deleted, or expired on its own) rather than
                // creating it again, without its TTL.
                (None, Some(value)) => Some(Mutation::HashRenameField {
                    key,
                    from: field.clone(),
                    to: field.clone(),
                    value: Some(value),
                    overwrite: false,
                }),
                (Some(to), value) => Some(Mutation::HashRenameField {
                    key,
                    from: field.clone(),
                    to: to.as_bytes().to_vec(),
                    value,
                    overwrite: false,
                }),
            },
            Target::List { index, element } => new_value.map(|value| Mutation::ListSet {
                key,
                index: *index,
                expected: element.clone(),
                value,
            }),
            Target::Set { member } => new_value.map(|to| Mutation::SetReplace {
                key,
                from: member.clone(),
                to,
                overwrite: false,
            }),
            Target::ZSet { member, score } => {
                let new_score = new_name.map(parse_score).transpose()?;
                let new_score = new_score.filter(|s| s.to_bits() != score.to_bits());
                match (new_value, new_score) {
                    (None, None) => None,
                    (to, score) => Some(Mutation::ZSetRename {
                        key,
                        from: member.clone(),
                        to: to.unwrap_or_else(|| member.clone()),
                        score,
                        overwrite: false,
                    }),
                }
            }
            Target::Stream => {
                ensure!(new_value.is_none(), "stream entries cannot be changed");
                None
            }
        })
    }
}

// Adding to a collection.

/// The add dialog of a collection: the inputs it has, and the change they
/// make.
#[derive(Clone, Debug, PartialEq)]
pub struct AddRow {
    key: Key,
    ty: KeyType,
}

impl AddRow {
    /// For collections only.
    pub fn new(key: Key, ty: &KeyType) -> Option<Self> {
        matches!(
            ty,
            KeyType::Hash | KeyType::List | KeyType::Set | KeyType::ZSet | KeyType::Stream
        )
        .then(|| Self {
            key,
            ty: ty.clone(),
        })
    }

    pub fn title(&self) -> &'static str {
        match self.ty {
            KeyType::Hash => "Add field",
            KeyType::List => "Add element",
            KeyType::Stream => "Add entry",
            _ => "Add member",
        }
    }

    /// The label of the first input, if there is one: "Field", "Score" or
    /// "Entry ID".
    pub fn name_label(&self) -> Option<&'static str> {
        match self.ty {
            KeyType::Hash => Some("Field"),
            KeyType::ZSet => Some("Score"),
            KeyType::Stream => Some("Entry ID"),
            _ => None,
        }
    }

    pub fn name_placeholder(&self) -> &'static str {
        match self.ty {
            KeyType::Hash => "name",
            KeyType::ZSet => "1.5, -3, inf",
            _ => "* for a new ID, or e.g. 1700000000000-0",
        }
    }

    pub fn value_label(&self) -> &'static str {
        match self.ty {
            KeyType::Hash => "Value",
            KeyType::List => "Element",
            KeyType::Stream => "Fields",
            _ => "Member",
        }
    }

    pub fn value_placeholder(&self) -> &'static str {
        match self.ty {
            KeyType::Stream => "field value field value …  or  {\"field\": \"value\"}",
            _ => "",
        }
    }

    /// Lists take an element at either end.
    pub fn has_ends(&self) -> bool {
        self.ty == KeyType::List
    }

    /// The change adding what was typed. An existing hash field or member
    /// is refused by the server ([`EditError::ElementExists`]
    /// (crate::EditError)); see [`Mutation::overwriting`].
    pub fn build(&self, name: &str, value: &str, end: ListEnd) -> Result<Mutation> {
        let key = self.key.clone();
        let value = value.as_bytes().to_vec();
        Ok(match self.ty {
            KeyType::Hash => Mutation::HashSet {
                key,
                field: name.as_bytes().to_vec(),
                value,
                overwrite: false,
            },
            KeyType::List => Mutation::ListPush {
                key,
                values: vec![value],
                end,
            },
            KeyType::Set => Mutation::SetAdd {
                key,
                members: vec![value],
            },
            KeyType::ZSet => Mutation::ZSetAdd {
                key,
                member: value,
                score: parse_score(name)?,
                overwrite: false,
            },
            KeyType::Stream => {
                let id = match name.trim() {
                    "" => "*".to_string(),
                    id => id.to_string(),
                };
                check_stream_id(&id)?;
                Mutation::StreamAdd {
                    key,
                    id,
                    fields: parse_stream_fields(std::str::from_utf8(&value).unwrap_or_default())?,
                }
            }
            _ => bail!("values of this type have no rows to add"),
        })
    }
}

// New keys.

/// The types a new key can have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NewKeyType {
    #[default]
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    /// A document of the JSON module.
    Json,
}

impl NewKeyType {
    pub const ALL: [Self; 7] = [
        Self::String,
        Self::Hash,
        Self::List,
        Self::Set,
        Self::ZSet,
        Self::Stream,
        Self::Json,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Hash => "Hash",
            Self::List => "List",
            Self::Set => "Set",
            Self::ZSet => "Sorted set",
            Self::Stream => "Stream",
            Self::Json => "JSON",
        }
    }

    /// The label of the input besides the value, if there is one.
    pub fn name_label(self) -> Option<&'static str> {
        match self {
            Self::Hash => Some("Field"),
            Self::ZSet => Some("Score"),
            Self::Stream => Some("Entry ID"),
            _ => None,
        }
    }

    pub fn name_placeholder(self) -> &'static str {
        match self {
            Self::Hash => "name",
            Self::ZSet => "1.5, -3, inf",
            _ => "* for a new ID",
        }
    }

    pub fn value_label(self) -> &'static str {
        match self {
            Self::String => "Value",
            Self::Hash => "Value",
            Self::List => "First element",
            Self::Set | Self::ZSet => "First member",
            Self::Stream => "Fields",
            Self::Json => "Document",
        }
    }

    pub fn value_placeholder(self) -> &'static str {
        match self {
            Self::Stream => "field value field value …  or  {\"field\": \"value\"}",
            Self::Json => "{\"name\": \"value\"}",
            _ => "",
        }
    }
}

/// The change creating a key from the new key form: its name, type, the
/// first value (and field, score or entry ID, see
/// [`NewKeyType::name_label`]) and a TTL as typed (see [`parse_ttl`]).
/// An existing key is never overwritten
/// ([`EditError::KeyExists`](crate::EditError)).
pub fn new_key(
    ty: NewKeyType,
    name: &str,
    field: &str,
    value: &str,
    ttl: &str,
) -> Result<Mutation> {
    ensure!(!name.is_empty(), "a key needs a name");
    let ttl = parse_ttl(ttl)?;
    let bytes = || value.as_bytes().to_vec();
    let value = match ty {
        NewKeyType::String => NewValue::String(bytes()),
        NewKeyType::Hash => NewValue::Hash(vec![(field.as_bytes().to_vec(), bytes())]),
        NewKeyType::List => NewValue::List(vec![bytes()]),
        NewKeyType::Set => NewValue::Set(vec![bytes()]),
        NewKeyType::ZSet => NewValue::ZSet(vec![(bytes(), parse_score(field)?)]),
        NewKeyType::Stream => {
            let id = match field.trim() {
                "" => "*".to_string(),
                id => id.to_string(),
            };
            check_stream_id(&id)?;
            NewValue::Stream {
                id,
                fields: parse_stream_fields(value)?,
            }
        }
        NewKeyType::Json => {
            json::check(value).map_err(|err| anyhow!("not valid JSON: {err}"))?;
            NewValue::Json(value.to_string())
        }
    };
    Ok(Mutation::CreateKey {
        key: Key::from(name.as_bytes()),
        value,
        ttl,
    })
}

// The loaded value and its changes.

impl Loaded {
    /// Row `ix` of a collection, opened for editing; `writable` is whether
    /// the connection allows changes.
    pub fn row_edit(&self, ix: usize, writable: bool) -> Option<RowEdit> {
        let (target, bytes, name) = match &self.data {
            ValueData::Hash(pairs) => {
                let (field, value) = pairs.get(ix)?;
                // The name is edited in a one-line input, which drops line
                // breaks: a name with one would come back changed, and a
                // value saved under it would rename the field. Such names,
                // like bytes that are not text, are shown escaped and kept.
                let (text, editable) = match format::bytes(field) {
                    Cow::Borrowed(text) if !text.contains('\n') => (text.to_string(), writable),
                    _ => (format::bytes_inline(field).into_owned(), false),
                };
                let name = NameField {
                    label: "Field",
                    text,
                    editable,
                };
                (
                    Target::Hash {
                        field: field.clone(),
                    },
                    value.clone(),
                    Some(name),
                )
            }
            ValueData::List(items) => {
                let element = items.get(ix)?;
                let index = i64::try_from(ix).ok()?;
                (
                    Target::List {
                        index,
                        element: element.clone(),
                    },
                    element.clone(),
                    None,
                )
            }
            ValueData::Set(items) => {
                let member = items.get(ix)?;
                (
                    Target::Set {
                        member: member.clone(),
                    },
                    member.clone(),
                    None,
                )
            }
            ValueData::ZSet(members) => {
                let (member, score) = members.get(ix)?;
                let name = NameField {
                    label: "Score",
                    text: format_score(*score),
                    editable: writable,
                };
                (
                    Target::ZSet {
                        member: member.clone(),
                        score: *score,
                    },
                    member.clone(),
                    Some(name),
                )
            }
            ValueData::Stream(_) => (Target::Stream, self.row_value(ix)?, None),
            _ => return None,
        };
        let allowed = writable && target != Target::Stream;
        Some(RowEdit {
            row: ix,
            target,
            value: TextEdit::new(bytes, allowed),
            name,
        })
    }

    /// The change that saves row `edit` (from [`Self::row_edit`]) as edited,
    /// or `None` if nothing changed: `value` is the value editor's text,
    /// `name` the name input's (`None` if it was not typed into). Fails
    /// with the reason if the input does not fit (invalid JSON in a JSON
    /// view, a score that is not a number, ...).
    ///
    /// The change is for this value's key as it is now, so a rename while
    /// the row was open does not send the edit to the old name.
    pub fn save_row(
        &self,
        edit: &RowEdit,
        value: &str,
        name: Option<&str>,
    ) -> Result<Option<Mutation>> {
        edit.save(&self.key, value, name)
    }

    /// The add dialog for this value, if it is a collection.
    pub fn add_row(&self) -> Option<AddRow> {
        AddRow::new(self.key.clone(), &self.meta.ty)
    }

    /// The change deleting rows `rows` of a collection (by index into the
    /// loaded rows); `None` if there is nothing to delete. List elements
    /// are only removed if they still are what was loaded.
    pub fn delete_rows(&self, rows: &[usize]) -> Option<Mutation> {
        let rows: BTreeSet<usize> = rows.iter().copied().collect();
        let key = self.key.clone();
        let mutation = match &self.data {
            ValueData::Hash(pairs) => Mutation::HashDelete {
                key,
                fields: rows
                    .iter()
                    .filter_map(|&ix| Some(pairs.get(ix)?.0.clone()))
                    .collect(),
            },
            ValueData::List(items) => Mutation::ListRemoveAt {
                key,
                elements: rows
                    .iter()
                    .filter_map(|&ix| Some((i64::try_from(ix).ok()?, items.get(ix)?.clone())))
                    .collect(),
            },
            ValueData::Set(items) => Mutation::SetRemove {
                key,
                members: rows
                    .iter()
                    .filter_map(|&ix| items.get(ix).cloned())
                    .collect(),
            },
            ValueData::ZSet(members) => Mutation::ZSetRemove {
                key,
                members: rows
                    .iter()
                    .filter_map(|&ix| Some(members.get(ix)?.0.clone()))
                    .collect(),
            },
            ValueData::Stream(entries) => Mutation::StreamDelete {
                key,
                ids: rows
                    .iter()
                    .filter_map(|&ix| Some(entries.get(ix)?.id.clone()))
                    .collect(),
            },
            _ => return None,
        };
        let empty = match &mutation {
            Mutation::HashDelete { fields: v, .. }
            | Mutation::SetRemove { members: v, .. }
            | Mutation::ZSetRemove { members: v, .. } => v.is_empty(),
            Mutation::ListRemoveAt { elements, .. } => elements.is_empty(),
            Mutation::StreamDelete { ids, .. } => ids.is_empty(),
            _ => false,
        };
        (!empty).then_some(mutation)
    }

    /// Takes a change that was sent successfully into the loaded value, so
    /// the view shows it without loading the value again (and keeps its
    /// scroll position and loaded pages). Changes that cannot be placed
    /// (an element that lands on a page not loaded yet) ask for a reload.
    pub fn apply(&mut self, mutation: &Mutation, outcome: &Outcome) -> Applied {
        if let Mutation::Rename { from, to } = mutation {
            if *from != self.key {
                return Applied::Reload;
            }
            self.key = to.clone();
            return Applied::Patched(None);
        }
        if *mutation.key() != self.key {
            return Applied::Reload;
        }
        let complete = self.next.is_none();
        let count = match outcome {
            Outcome::Count(n) => Some(*n),
            _ => None,
        };
        let created = *outcome == Outcome::Created(true);
        match (mutation, &mut self.data) {
            (Mutation::SetTtl { seconds, .. }, _) => {
                self.meta.ttl_ms = seconds.map_or(-1, |s| {
                    i64::try_from(s.saturating_mul(1000)).unwrap_or(i64::MAX)
                });
                Applied::Patched(None)
            }
            (Mutation::SetString { value, .. }, ValueData::String(bytes)) => {
                bytes.clone_from(value);
                self.total = value.len() as u64;
                Applied::Patched(None)
            }
            (Mutation::SetJson { json, .. }, ValueData::Json(doc)) => {
                doc.clone_from(json);
                self.total = json.len() as u64;
                Applied::Patched(None)
            }
            (Mutation::HashSet { field, value, .. }, ValueData::Hash(pairs)) => {
                if let Some(ix) = pairs.iter().position(|(f, _)| f == field) {
                    pairs[ix].1.clone_from(value);
                    return Applied::Patched(Some(ix));
                }
                if created {
                    self.total += 1;
                }
                // A field not loaded may come with a later page (HSCAN may
                // or may not return one added meanwhile).
                if !complete {
                    return Applied::Reload;
                }
                pairs.push((field.clone(), value.clone()));
                Applied::Patched(Some(pairs.len() - 1))
            }
            (
                Mutation::HashRenameField {
                    from, to, value, ..
                },
                ValueData::Hash(pairs),
            ) => {
                let Some(ix) = pairs.iter().position(|(f, _)| f == from) else {
                    return Applied::Reload;
                };
                let other = pairs.iter().position(|(f, _)| f == to).filter(|&o| o != ix);
                if from != to && other.is_none() && !complete {
                    // A name not loaded: HSCAN may well return it with a
                    // later page (as a new field, or the one replaced
                    // there), and it would show twice.
                    return Applied::Reload;
                }
                pairs[ix].0.clone_from(to);
                if let Some(value) = value {
                    pairs[ix].1.clone_from(value);
                }
                let mut ix = ix;
                if let Some(other) = other {
                    pairs.remove(other);
                    self.total = self.total.saturating_sub(1);
                    if other < ix {
                        ix -= 1;
                    }
                }
                Applied::Patched(Some(ix))
            }
            (Mutation::HashDelete { fields, .. }, ValueData::Hash(pairs)) => {
                let gone: HashSet<&[u8]> = fields.iter().map(Vec::as_slice).collect();
                pairs.retain(|(f, _)| !gone.contains(f.as_slice()));
                self.removed(outcome)
            }
            (Mutation::ListPush { values, end, .. }, ValueData::List(items)) => {
                if let Some(len) = count {
                    self.total = len;
                }
                match end {
                    ListEnd::Head => {
                        items.splice(0..0, values.iter().cloned());
                        if let Some(Cursor::Offset(offset)) = &mut self.next {
                            *offset += values.len() as u64;
                        }
                        Applied::Patched(Some(0))
                    }
                    ListEnd::Tail if complete => {
                        items.extend(values.iter().cloned());
                        Applied::Patched(Some(items.len() - 1))
                    }
                    // Lands on a page not loaded yet.
                    ListEnd::Tail => Applied::Patched(None),
                }
            }
            (Mutation::ListSet { index, value, .. }, ValueData::List(items)) => {
                let ix = usize::try_from(*index).ok();
                match ix.and_then(|ix| items.get_mut(ix)) {
                    Some(item) => {
                        item.clone_from(value);
                        Applied::Patched(ix)
                    }
                    None => Applied::Reload,
                }
            }
            (Mutation::ListRemoveAt { elements, .. }, ValueData::List(items)) => {
                let gone: BTreeSet<usize> = elements
                    .iter()
                    .filter_map(|(ix, _)| usize::try_from(*ix).ok())
                    .collect();
                let before = items.len();
                let mut ix = 0;
                items.retain(|_| {
                    let keep = !gone.contains(&ix);
                    ix += 1;
                    keep
                });
                let removed = (before - items.len()) as u64;
                if let Some(Cursor::Offset(offset)) = &mut self.next {
                    *offset = offset.saturating_sub(removed);
                }
                self.removed(outcome)
            }
            (Mutation::SetAdd { members, .. }, ValueData::Set(items)) => {
                let added = count.unwrap_or(0);
                if added == 0 {
                    return Applied::Patched(None);
                }
                if !complete {
                    self.total += added;
                    return Applied::Reload;
                }
                let have: HashSet<Vec<u8>> = items.iter().cloned().collect();
                for member in members {
                    if !have.contains(member) {
                        items.push(member.clone());
                    }
                }
                self.total += added;
                Applied::Patched(Some(items.len() - 1))
            }
            (Mutation::SetRemove { members, .. }, ValueData::Set(items)) => {
                let gone: HashSet<&[u8]> = members.iter().map(Vec::as_slice).collect();
                items.retain(|m| !gone.contains(m.as_slice()));
                self.removed(outcome)
            }
            (Mutation::SetReplace { from, to, .. }, ValueData::Set(items)) => {
                let Some(ix) = items.iter().position(|m| m == from) else {
                    return Applied::Reload;
                };
                if created {
                    if !complete {
                        // SSCAN may return the new member with a later
                        // page, and it would show twice.
                        return Applied::Reload;
                    }
                    items[ix].clone_from(to);
                    return Applied::Patched(Some(ix));
                }
                // `to` was a member already: `from` just went.
                items.remove(ix);
                self.total = self.total.saturating_sub(1);
                Applied::Patched(items.iter().position(|m| m == to))
            }
            (Mutation::ZSetAdd { member, score, .. }, ValueData::ZSet(members)) => {
                if !complete {
                    if created {
                        self.total += 1;
                    }
                    return Applied::Reload;
                }
                members.retain(|(m, _)| m != member);
                Self::zset_insert(members, member.clone(), *score);
                self.total = members.len() as u64;
                Applied::Patched(members.iter().position(|(m, _)| m == member))
            }
            (
                Mutation::ZSetRename {
                    from, to, score, ..
                },
                ValueData::ZSet(members),
            ) => {
                if !complete {
                    return Applied::Reload;
                }
                let Some(ix) = members.iter().position(|(m, _)| m == from) else {
                    return Applied::Reload;
                };
                let (_, old) = members.remove(ix);
                members.retain(|(m, _)| m != to);
                Self::zset_insert(members, to.clone(), score.unwrap_or(old));
                self.total = members.len() as u64;
                Applied::Patched(members.iter().position(|(m, _)| m == to))
            }
            (Mutation::ZSetRemove { members: gone, .. }, ValueData::ZSet(members)) => {
                let gone: HashSet<&[u8]> = gone.iter().map(Vec::as_slice).collect();
                let before = members.len();
                members.retain(|(m, _)| !gone.contains(m.as_slice()));
                let removed = (before - members.len()) as u64;
                if let Some(Cursor::Offset(offset)) = &mut self.next {
                    *offset = offset.saturating_sub(removed);
                }
                self.removed(outcome)
            }
            (Mutation::StreamAdd { fields, .. }, ValueData::Stream(entries)) => {
                self.total += 1;
                match outcome {
                    Outcome::StreamId(id) if complete => {
                        entries.push(StreamEntry {
                            id: id.clone(),
                            fields: fields.clone(),
                        });
                        Applied::Patched(Some(entries.len() - 1))
                    }
                    // On a page not loaded yet.
                    _ => Applied::Patched(None),
                }
            }
            (Mutation::StreamDelete { ids, .. }, ValueData::Stream(entries)) => {
                let gone: HashSet<&str> = ids.iter().map(String::as_str).collect();
                entries.retain(|e| !gone.contains(e.id.as_str()));
                self.removed(outcome)
            }
            _ => Applied::Reload,
        }
    }

    /// After elements were removed (and taken out of the loaded rows), by
    /// what the server said in the same step: how many it removed, and how
    /// many are left. With none left a hash, list, set or sorted set is
    /// gone (Redis deletes it), while a stream stays, empty. Left other
    /// than what the total loaded here comes to, the value was changed by
    /// someone else meanwhile, and is loaded again.
    fn removed(&mut self, outcome: &Outcome) -> Applied {
        let Outcome::Removed(Removed { count, left }) = *outcome else {
            return Applied::Reload;
        };
        let expected = self.total.saturating_sub(count);
        self.total = left;
        if left == 0 && self.meta.ty != KeyType::Stream {
            self.data = ValueData::Missing;
            self.meta.ty = KeyType::None;
            return Applied::KeyGone;
        }
        if left == expected {
            Applied::Patched(None)
        } else {
            Applied::Reload
        }
    }

    /// Inserts where Redis orders it: by score, then by member bytes.
    fn zset_insert(members: &mut Vec<(Vec<u8>, f64)>, member: Vec<u8>, score: f64) {
        let at = members.partition_point(|(m, s)| {
            s.partial_cmp(&score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| m.as_slice().cmp(&member))
                == Ordering::Less
        });
        members.insert(at, (member, score));
    }
}
