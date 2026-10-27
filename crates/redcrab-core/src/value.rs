use std::sync::Arc;

use redis::Value;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KeyType {
    String,
    List,
    Set,
    ZSet,
    Hash,
    Stream,
    Json,
    /// Any other module type, by the name `TYPE` reports.
    Other(Arc<str>),
    /// The key no longer exists.
    None,
}

impl KeyType {
    pub fn parse(name: &str) -> Self {
        match name {
            "string" => Self::String,
            "list" => Self::List,
            "set" => Self::Set,
            "zset" => Self::ZSet,
            "hash" => Self::Hash,
            "stream" => Self::Stream,
            "ReJSON-RL" => Self::Json,
            "none" => Self::None,
            other => Self::Other(other.into()),
        }
    }

    /// Short label for badges.
    pub fn label(&self) -> &str {
        match self {
            Self::String => "STR",
            Self::List => "LIST",
            Self::Set => "SET",
            Self::ZSet => "ZSET",
            Self::Hash => "HASH",
            Self::Stream => "STREAM",
            Self::Json => "JSON",
            Self::Other(name) => name,
            Self::None => "NONE",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyMeta {
    pub ty: KeyType,
    /// Milliseconds until expiry; `-1` means no expiry, `-2` means missing.
    pub ttl_ms: i64,
}

/// Where the next page of a collection starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cursor {
    Start,
    /// HSCAN / SSCAN cursor.
    Scan(u64),
    /// LRANGE / ZRANGE offset.
    Offset(u64),
    /// Exclusive stream ID for the next XRANGE.
    StreamAfter(String),
}

#[derive(Clone, Debug)]
pub struct ValuePage {
    pub data: ValueData,
    /// Total number of elements (bytes for strings), when the server reports it.
    pub total: u64,
    /// `None` once the last page has been read.
    pub next: Option<Cursor>,
}

#[derive(Clone, Debug)]
pub enum ValueData {
    String(Vec<u8>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    ZSet(Vec<(Vec<u8>, f64)>),
    Stream(Vec<StreamEntry>),
    Json(String),
    Unsupported(Arc<str>),
    Missing,
}

impl ValueData {
    pub fn len(&self) -> usize {
        match self {
            Self::String(_) | Self::Json(_) => 1,
            Self::Hash(v) => v.len(),
            Self::List(v) | Self::Set(v) => v.len(),
            Self::ZSet(v) => v.len(),
            Self::Stream(v) => v.len(),
            Self::Unsupported(_) | Self::Missing => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends a following page of the same type.
    pub fn extend(&mut self, other: ValueData) {
        match (self, other) {
            (Self::Hash(a), Self::Hash(b)) => a.extend(b),
            (Self::List(a), Self::List(b)) | (Self::Set(a), Self::Set(b)) => a.extend(b),
            (Self::ZSet(a), Self::ZSet(b)) => a.extend(b),
            (Self::Stream(a), Self::Stream(b)) => a.extend(b),
            (this, other) => *this = other,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamEntry {
    pub id: String,
    pub fields: Vec<(Vec<u8>, Vec<u8>)>,
}

// Hand-rolled decoding so RESP2 and RESP3 replies take the same path.

pub(crate) fn into_bytes(v: Value) -> Vec<u8> {
    match v {
        Value::BulkString(b) => b,
        Value::SimpleString(s) => s.into_bytes(),
        Value::VerbatimString { text, .. } => text.into_bytes(),
        Value::Okay => b"OK".to_vec(),
        Value::Int(i) => i.to_string().into_bytes(),
        Value::Double(d) => d.to_string().into_bytes(),
        Value::Boolean(b) => (if b { "1" } else { "0" }).into(),
        _ => Vec::new(),
    }
}

pub(crate) fn into_string(v: Value) -> String {
    match String::from_utf8(into_bytes(v)) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

pub(crate) fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::BulkString(b) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        Value::SimpleString(s) => s.parse().unwrap_or(0),
        Value::Double(d) => *d as i64,
        _ => 0,
    }
}

pub(crate) fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Double(d) => *d,
        Value::Int(i) => *i as f64,
        Value::BulkString(b) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
        Value::SimpleString(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Flattens arrays, sets and maps (and nested pairs, as RESP3 sends for
/// `WITHSCORES`) into a single list of scalars.
pub(crate) fn flatten(v: Value) -> Vec<Value> {
    fn walk(v: Value, out: &mut Vec<Value>) {
        match v {
            Value::Array(items) | Value::Set(items) => items.into_iter().for_each(|i| walk(i, out)),
            Value::Map(pairs) => pairs.into_iter().for_each(|(k, v)| {
                walk(k, out);
                walk(v, out);
            }),
            Value::Attribute { data, .. } => walk(*data, out),
            Value::Nil => {}
            other => out.push(other),
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

pub(crate) fn pairs(v: Value) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut items = flatten(v).into_iter();
    let mut out = Vec::with_capacity(items.len() / 2);
    while let (Some(k), Some(v)) = (items.next(), items.next()) {
        out.push((into_bytes(k), into_bytes(v)));
    }
    out
}

/// Splits a `[cursor, items]` reply from HSCAN / SSCAN / ZSCAN.
pub(crate) fn scan_reply(v: Value) -> (u64, Value) {
    match v {
        Value::Array(mut parts) if parts.len() == 2 => {
            let items = parts.pop().unwrap_or(Value::Nil);
            let cursor = as_i64(&parts[0]) as u64;
            (cursor, items)
        }
        _ => (0, Value::Nil),
    }
}

pub(crate) fn stream_entries(v: Value) -> Vec<StreamEntry> {
    let Value::Array(entries) = v else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| match entry {
            Value::Array(mut parts) if parts.len() == 2 => {
                let fields = pairs(parts.pop()?);
                let id = into_string(parts.pop()?);
                Some(StreamEntry { id, fields })
            }
            _ => None,
        })
        .collect()
}
