//! MessagePack shown as JSON and written back from JSON.
//!
//! Hand-rolled rather than through a MessagePack crate so that map order is
//! kept both ways (JSON objects come out in stored order, and JSON typed back
//! is written in typed order) and so the work is capped for huge values.
//! Every integer, string, array and map is written in its shortest form, as
//! common encoders do; a value stored any other way (or holding binary,
//! extension types, `float32`s or keys that are not strings) does not come
//! back byte for byte, which [`super::round_trips`] tells the editor.

use std::fmt::{self, Write as _};

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

use super::json;

/// Deeper nesting than this is refused, so a hostile value cannot overflow
/// the stack.
const MAX_DEPTH: usize = 256;

/// Whether `bytes` starts like a MessagePack map or array and is exactly
/// one valid value. Scalars are not claimed: too much text and binary
/// happens to parse as a lone MessagePack number or string.
pub(crate) fn is_container(bytes: &[u8]) -> bool {
    let Some(&first) = bytes.first() else {
        return false;
    };
    let container = matches!(first, 0x80..=0x9f | 0xdc..=0xdf);
    container && Reader::new(bytes).skip_all().is_ok()
}

/// The value as compact JSON, failing if the JSON would grow past `limit`
/// bytes. The check runs as it is written, inside strings too: escaping
/// makes a control byte six bytes of JSON, binary a third longer.
pub(crate) fn to_json(bytes: &[u8], limit: usize) -> Result<String> {
    let mut reader = Reader::new(bytes);
    let mut out = String::with_capacity(bytes.len().min(limit) * 2);
    reader.value_json(&mut out, 0, limit)?;
    if reader.pos != bytes.len() {
        bail!(
            "not valid MessagePack: {} bytes left after the value",
            bytes.len() - reader.pos
        );
    }
    check_size(&out, limit)?;
    Ok(out)
}

fn check_size(out: &str, limit: usize) -> Result<()> {
    if out.len() > limit {
        return Err(too_large(limit));
    }
    Ok(())
}

fn too_large(limit: usize) -> anyhow::Error {
    anyhow!(
        "too large to show: more than {} as JSON",
        crate::format::size(limit as u64)
    )
}

/// JSON text as MessagePack: objects in the order typed (duplicate keys
/// kept), integers as integers, other numbers as `float64`. An integer
/// MessagePack cannot hold (past the 64-bit ones) is refused: serde_json
/// would read it as a float, and store other digits than were typed.
pub(crate) fn from_json(json: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(json.len());
    let mut deserializer = serde_json::Deserializer::from_str(json);
    Encoder(&mut out)
        .deserialize(&mut deserializer)
        .map_err(|err| anyhow!("not valid JSON: {err}"))?;
    deserializer
        .end()
        .map_err(|err| anyhow!("not valid JSON: {err}"))?;
    if let Some(big) =
        json::integers(json).find(|n| n.parse::<i64>().is_err() && n.parse::<u64>().is_err())
    {
        bail!(
            "{big} is too big for a MessagePack integer, which holds {} to {}; write it as a string, or as a decimal (such as {big}.0) for a float",
            i64::MIN,
            u64::MAX
        );
    }
    Ok(out)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| anyhow!("not valid MessagePack: the value ends early"))?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn uint(&mut self, width: usize) -> Result<u64> {
        Ok(self
            .take(width)?
            .iter()
            .fold(0u64, |n, &b| (n << 8) | u64::from(b)))
    }

    /// A length that cannot be bigger than what is left: every element
    /// takes at least one byte, so a huge count in a short value fails
    /// here rather than in an allocation.
    fn len(&mut self, width: usize) -> Result<usize> {
        let n = self.uint(width)?;
        let left = (self.bytes.len() - self.pos) as u64;
        if n > left {
            bail!("not valid MessagePack: a length of {n} with {left} bytes left");
        }
        Ok(n as usize)
    }

    fn skip_all(&mut self) -> Result<()> {
        self.skip(0)?;
        if self.pos != self.bytes.len() {
            bail!("trailing bytes");
        }
        Ok(())
    }

    fn skip(&mut self, depth: usize) -> Result<()> {
        // Writing JSON nowhere would do, but this does not allocate.
        match self.header(depth)? {
            Header::Scalar(_) => Ok(()),
            Header::Array(n) => (0..n).try_for_each(|_| self.skip(depth + 1)),
            Header::Map(n) => (0..n * 2).try_for_each(|_| self.skip(depth + 1)),
        }
    }

    /// Reads one header; scalars are read whole.
    fn header(&mut self, depth: usize) -> Result<Header<'a>> {
        if depth > MAX_DEPTH {
            bail!("not valid MessagePack: nested more than {MAX_DEPTH} levels deep");
        }
        let b = self.u8()?;
        Ok(match b {
            0x00..=0x7f => Header::Scalar(Scalar::Uint(u64::from(b))),
            0x80..=0x8f => Header::Map(usize::from(b & 0x0f)),
            0x90..=0x9f => Header::Array(usize::from(b & 0x0f)),
            0xa0..=0xbf => Header::Scalar(Scalar::Str(self.take(usize::from(b & 0x1f))?)),
            0xc0 => Header::Scalar(Scalar::Nil),
            0xc1 => bail!("not valid MessagePack: 0xc1 is never used"),
            0xc2 => Header::Scalar(Scalar::Bool(false)),
            0xc3 => Header::Scalar(Scalar::Bool(true)),
            0xc4..=0xc6 => {
                let n = self.len(1 << (b - 0xc4))?;
                Header::Scalar(Scalar::Bin(self.take(n)?))
            }
            0xc7..=0xc9 => {
                let n = self.len(1 << (b - 0xc7))?;
                let kind = self.u8()? as i8;
                Header::Scalar(Scalar::Ext(kind, self.take(n)?))
            }
            0xca => {
                let bits = self.uint(4)? as u32;
                Header::Scalar(Scalar::Float(f64::from(f32::from_bits(bits))))
            }
            0xcb => Header::Scalar(Scalar::Float(f64::from_bits(self.uint(8)?))),
            0xcc..=0xcf => Header::Scalar(Scalar::Uint(self.uint(1 << (b - 0xcc))?)),
            0xd0 => Header::Scalar(Scalar::Int(i64::from(self.u8()? as i8))),
            0xd1 => Header::Scalar(Scalar::Int(i64::from(self.uint(2)? as u16 as i16))),
            0xd2 => Header::Scalar(Scalar::Int(i64::from(self.uint(4)? as u32 as i32))),
            0xd3 => Header::Scalar(Scalar::Int(self.uint(8)? as i64)),
            0xd4..=0xd8 => {
                let kind = self.u8()? as i8;
                Header::Scalar(Scalar::Ext(kind, self.take(1 << (b - 0xd4))?))
            }
            0xd9..=0xdb => {
                let n = self.len(1 << (b - 0xd9))?;
                Header::Scalar(Scalar::Str(self.take(n)?))
            }
            0xdc | 0xdd => Header::Array(self.len(if b == 0xdc { 2 } else { 4 })?),
            0xde | 0xdf => Header::Map(self.len(if b == 0xde { 2 } else { 4 })?),
            0xe0..=0xff => Header::Scalar(Scalar::Int(i64::from(b as i8))),
        })
    }

    fn value_json(&mut self, out: &mut String, depth: usize, limit: usize) -> Result<()> {
        check_size(out, limit)?;
        match self.header(depth)? {
            Header::Scalar(scalar) => scalar.write_json(out, limit)?,
            Header::Array(n) => {
                out.push('[');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    self.value_json(out, depth + 1, limit)?;
                }
                out.push(']');
            }
            Header::Map(n) => {
                out.push('{');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    // JSON keys are strings: anything else is written as
                    // the string of its JSON.
                    match self.header(depth + 1)? {
                        Header::Scalar(Scalar::Str(s)) => json_string(out, s, limit)?,
                        Header::Scalar(other) => {
                            let mut key = String::new();
                            other.write_json(&mut key, limit)?;
                            json_string(out, key.as_bytes(), limit)?;
                        }
                        Header::Array(_) | Header::Map(_) => {
                            bail!("a map key that is an array or a map cannot be shown as JSON")
                        }
                    }
                    out.push(':');
                    self.value_json(out, depth + 1, limit)?;
                }
                out.push('}');
            }
        }
        Ok(())
    }
}

enum Header<'a> {
    Scalar(Scalar<'a>),
    Array(usize),
    Map(usize),
}

enum Scalar<'a> {
    Nil,
    Bool(bool),
    Uint(u64),
    Int(i64),
    Float(f64),
    Str(&'a [u8]),
    Bin(&'a [u8]),
    Ext(i8, &'a [u8]),
}

impl Scalar<'_> {
    /// Fails, having written part of it, once `out` would pass `limit`.
    fn write_json(&self, out: &mut String, limit: usize) -> Result<()> {
        match *self {
            Self::Nil => out.push_str("null"),
            Self::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            Self::Uint(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Int(n) => {
                let _ = write!(out, "{n}");
            }
            // JSON has no NaN or infinity.
            Self::Float(f) if !f.is_finite() => out.push_str("null"),
            Self::Float(f) => match serde_json::to_string(&f) {
                Ok(text) => out.push_str(&text),
                Err(_) => out.push_str("null"),
            },
            Self::Str(s) => json_string(out, s, limit)?,
            // Binary and extension data as Base64 text, as `msgpack2json -b`
            // writes it; its length is known before encoding.
            Self::Bin(b) => {
                check_size_after(out, base64_len(b.len()) + 2, limit)?;
                let data = base64::engine::general_purpose::STANDARD.encode(b);
                json_string(out, data.as_bytes(), limit)?;
            }
            Self::Ext(kind, data) => {
                check_size_after(out, base64_len(data.len()) + 12, limit)?;
                let data = base64::engine::general_purpose::STANDARD.encode(data);
                json_string(out, format!("ext {kind}: {data}").as_bytes(), limit)?;
            }
        }
        check_size(out, limit)
    }
}

fn base64_len(n: usize) -> usize {
    n.div_ceil(3).saturating_mul(4)
}

/// Fails if writing `more` bytes would take `out` past `limit`.
fn check_size_after(out: &str, more: usize, limit: usize) -> Result<()> {
    if out.len().saturating_add(more) > limit {
        return Err(too_large(limit));
    }
    Ok(())
}

/// `bytes` as a JSON string, escaped as serde_json escapes (invalid UTF-8
/// as U+FFFD, as `String::from_utf8_lossy` does), failing as soon as `out`
/// passes `limit`: escaping can make it six times longer than `bytes`.
fn json_string(out: &mut String, bytes: &[u8], limit: usize) -> Result<()> {
    out.push('"');
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                c if c < ' ' => {
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                c => out.push(c),
            }
            check_size(out, limit)?;
        }
        if !chunk.invalid().is_empty() {
            out.push(char::REPLACEMENT_CHARACTER);
        }
    }
    out.push('"');
    check_size(out, limit)
}

/// Writes MessagePack while serde_json reads the JSON, so objects keep
/// the order (and duplicates) they were typed in.
struct Encoder<'a>(&'a mut Vec<u8>);

impl<'de> DeserializeSeed<'de> for Encoder<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Encoder<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.0.push(0xc0);
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<(), E> {
        self.0.push(if v { 0xc3 } else { 0xc2 });
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<(), E> {
        write_uint(self.0, v);
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<(), E> {
        match u64::try_from(v) {
            Ok(v) => write_uint(self.0, v),
            Err(_) => write_negative(self.0, v),
        }
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<(), E> {
        self.0.push(0xcb);
        self.0.extend_from_slice(&v.to_bits().to_be_bytes());
        Ok(())
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<(), E> {
        let n = v.len();
        match n {
            0..=31 => self.0.push(0xa0 | n as u8),
            32..=0xff => self.0.extend_from_slice(&[0xd9, n as u8]),
            0x100..=0xffff => {
                self.0.push(0xda);
                self.0.extend_from_slice(&(n as u16).to_be_bytes());
            }
            _ => {
                self.0.push(0xdb);
                self.0.extend_from_slice(&len32(n)?.to_be_bytes());
            }
        }
        self.0.extend_from_slice(v.as_bytes());
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        // The count comes first, so the elements are written aside.
        let mut items = Vec::new();
        let mut n = 0usize;
        while seq.next_element_seed(Encoder(&mut items))?.is_some() {
            n += 1;
        }
        write_header(self.0, n, [0x90, 0xdc, 0xdd])?;
        self.0.extend_from_slice(&items);
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut entries = Vec::new();
        let mut n = 0usize;
        while map.next_key_seed(Encoder(&mut entries))?.is_some() {
            map.next_value_seed(Encoder(&mut entries))?;
            n += 1;
        }
        write_header(self.0, n, [0x80, 0xde, 0xdf])?;
        self.0.extend_from_slice(&entries);
        Ok(())
    }
}

fn len32<E: de::Error>(n: usize) -> Result<u32, E> {
    u32::try_from(n).map_err(|_| E::custom("too long for MessagePack"))
}

/// An array or map header: `fix | n` up to 15, then 16 and 32 bits.
fn write_header<E: de::Error>(
    out: &mut Vec<u8>,
    n: usize,
    [fix, h16, h32]: [u8; 3],
) -> Result<(), E> {
    match n {
        0..=15 => out.push(fix | n as u8),
        16..=0xffff => {
            out.push(h16);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            out.push(h32);
            out.extend_from_slice(&len32(n)?.to_be_bytes());
        }
    }
    Ok(())
}

fn write_uint(out: &mut Vec<u8>, v: u64) {
    match v {
        0..=0x7f => out.push(v as u8),
        0x80..=0xff => out.extend_from_slice(&[0xcc, v as u8]),
        0x100..=0xffff => {
            out.push(0xcd);
            out.extend_from_slice(&(v as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xce);
            out.extend_from_slice(&(v as u32).to_be_bytes());
        }
        _ => {
            out.push(0xcf);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

fn write_negative(out: &mut Vec<u8>, v: i64) {
    match v {
        -32..=-1 => out.push(v as u8),
        -128..=-33 => out.extend_from_slice(&[0xd0, v as u8]),
        -32768..=-129 => {
            out.push(0xd1);
            out.extend_from_slice(&(v as i16).to_be_bytes());
        }
        -2_147_483_648..=-32769 => {
            out.push(0xd2);
            out.extend_from_slice(&(v as i32).to_be_bytes());
        }
        _ => {
            out.push(0xd3);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}
