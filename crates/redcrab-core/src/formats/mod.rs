//! The formats a value can be shown in: as text, JSON, a hex dump, or
//! decoded from Base64, MessagePack or gzip. [`detect`] picks one for a
//! value, [`decode`] turns stored bytes into text to show, and [`encode`]
//! turns edited text back into bytes to store.
//!
//! Decoding is capped (see [`DECODE_LIMIT`], [`TEXT_LIMIT`], [`HEX_LIMIT`]),
//! so a huge or hostile value (a gzip bomb, say) costs bounded time and
//! memory, and never becomes more text than an editor can take.

pub(crate) mod json;
pub(crate) mod msgpack;

use std::{fmt::Write as _, io::Read as _};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{
    Engine as _,
    engine::{GeneralPurpose, general_purpose},
};
use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};

use crate::format;

/// The most bytes a value is decoded into (inflated gzip, MessagePack as
/// JSON) for anything but showing it (see [`TEXT_LIMIT`]). Beyond it,
/// decoding fails rather than eating memory.
pub const DECODE_LIMIT: usize = 16 << 20;
/// The most text a value decoded from Base64, gzip or MessagePack is shown
/// as, unless the stored value is longer itself ([`text_limit`]). A small
/// value can decode into far more: gzip compresses some content a thousand
/// to one, bytes that are not text show as four characters each, and the
/// text goes into an editor. Past it, decoding to show fails.
pub const TEXT_LIMIT: usize = 4 << 20;
/// The bytes a hex dump shows; the rest is summarized on a last line.
pub const HEX_LIMIT: usize = 64 << 10;
/// Values bigger than this are only told apart by their first bytes.
const DETECT_LIMIT: usize = DECODE_LIMIT;

/// How a value's bytes are shown and edited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ValueFormat {
    /// The bytes as text, escaped like `redis-cli` where they are not.
    #[default]
    Text,
    /// JSON, indented for reading.
    Json,
    /// An offset / hex / ASCII dump. Read-only.
    Hex,
    /// Base64 text, shown decoded.
    Base64,
    /// MessagePack, shown as JSON.
    MessagePack,
    /// gzip, shown decompressed.
    Gzip,
}

impl ValueFormat {
    pub const ALL: [Self; 6] = [
        Self::Text,
        Self::Json,
        Self::Hex,
        Self::Base64,
        Self::MessagePack,
        Self::Gzip,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Json => "JSON",
            Self::Hex => "Hex",
            Self::Base64 => "Base64",
            Self::MessagePack => "MessagePack",
            Self::Gzip => "Gzip",
        }
    }

    /// Whether text in this format can be turned back into bytes. A hex
    /// dump cannot; to edit bytes that are not text, use Base64.
    pub fn is_editable(self) -> bool {
        self != Self::Hex
    }

    /// Whether the text shown is JSON, worth highlighting as such.
    pub fn shows_json(self) -> bool {
        matches!(self, Self::Json | Self::MessagePack)
    }
}

/// The format a value most likely is in. Order matters: gzip has a magic
/// number; JSON documents and Base64 are text; other UTF-8 is text (shown
/// escaped where it has control characters); MessagePack maps and arrays
/// are told apart from other binary by parsing; any other binary is shown
/// as hex.
///
/// Base64 is only claimed when the text decodes to something meaningful
/// (gzip, JSON, or for longer values mostly-ASCII text), since plenty of
/// plain words and IDs are valid Base64 too.
pub fn detect(bytes: &[u8]) -> ValueFormat {
    if bytes.is_empty() {
        return ValueFormat::Text;
    }
    if is_gzip(bytes) {
        return ValueFormat::Gzip;
    }
    if bytes.len() > DETECT_LIMIT {
        // Too big to parse just to pick a format.
        return match std::str::from_utf8(&bytes[..1024]) {
            Ok(_) => ValueFormat::Text,
            Err(err) if err.error_len().is_none() => ValueFormat::Text,
            Err(_) => ValueFormat::Hex,
        };
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        if !is_printable(text) {
            // Text with a few control characters (terminal colors, a NUL)
            // reads better escaped than as a hex dump.
            return ValueFormat::Text;
        }
        if json::is_document(text) {
            return ValueFormat::Json;
        }
        if looks_like_base64(text) {
            return ValueFormat::Base64;
        }
        return ValueFormat::Text;
    }
    if msgpack::is_container(bytes) {
        return ValueFormat::MessagePack;
    }
    ValueFormat::Hex
}

/// Text without control characters other than line breaks and tabs.
fn is_printable(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x1f, 0x8b, 0x08])
}

/// Base64 that decodes to gzip or a JSON document, or, if it is long
/// enough not to by chance, to text that is mostly ASCII.
///
/// Short words and IDs are valid Base64 all the time, and about one in a
/// hundred 8-character tokens decodes to "text" (a couple of CJK or other
/// non-ASCII characters): such values would open decoded into gibberish,
/// and an edit typed there would be saved Base64-encoded.
fn looks_like_base64(text: &str) -> bool {
    if text.len() < 8 {
        return false;
    }
    let Some((inner, _)) = Base64Style::decode(text.as_bytes()) else {
        return false;
    };
    if is_gzip(&inner) {
        return true;
    }
    let Ok(decoded) = std::str::from_utf8(&inner) else {
        return false;
    };
    if json::is_document(decoded) {
        return true;
    }
    // Letters only, it is far likelier a word: Base64 of text almost
    // always has a digit, `+`, `/` or `=` in it somewhere.
    text.len() >= BASE64_TEXT_MIN
        && !text.bytes().all(|b| b.is_ascii_alphabetic())
        && !decoded.trim().is_empty()
        && is_mostly_ascii(decoded)
}

/// The shortest Base64 claimed for decoding to plain text: twelve bytes of
/// it. Random 16-character tokens pass the checks about once in 10,000;
/// 8-character ones did about once in 100.
const BASE64_TEXT_MIN: usize = 16;

/// Printable text, at least four in five characters of it ASCII (so
/// accented letters pass, binary that happens to be UTF-8 does not).
fn is_mostly_ascii(text: &str) -> bool {
    if !is_printable(text) {
        return false;
    }
    let (mut ascii, mut all) = (0usize, 0usize);
    for c in text.chars() {
        all += 1;
        if c.is_ascii() {
            ascii += 1;
        }
    }
    ascii * 10 >= all * 8
}

/// The most text `stored` is shown as when decoded: [`TEXT_LIMIT`], or its
/// own length if that is more, so that decoding never refuses what would
/// not make the text longer than the value.
pub fn text_limit(stored: &[u8]) -> usize {
    TEXT_LIMIT.max(stored.len())
}

/// Fails if `text`, decoded to be shown, is longer than `limit`.
pub(crate) fn check_shown(text: &str, limit: usize) -> Result<()> {
    if text.len() > limit {
        bail!(
            "decoded, it is more than {} of text, too much to show",
            format::size(limit as u64)
        );
    }
    Ok(())
}

/// `bytes` as text in `format`, for display. Fails when the bytes are not
/// in that format (not JSON, not Base64, ...), or when they decode into
/// more text than [`text_limit`].
pub fn decode(bytes: &[u8], format: ValueFormat) -> Result<String> {
    let limit = text_limit(bytes);
    let text = match format {
        ValueFormat::Text => return Ok(format::bytes(bytes).into_owned()),
        ValueFormat::Json => {
            let text =
                std::str::from_utf8(bytes).map_err(|_| anyhow!("not valid JSON: not UTF-8"))?;
            json::check(text).map_err(|err| anyhow!("not valid JSON: {err}"))?;
            return Ok(json::pretty(text));
        }
        ValueFormat::Hex => return Ok(hex_dump(bytes, HEX_LIMIT)),
        ValueFormat::Base64 => {
            let (inner, _) = Base64Style::decode(bytes).context("not valid Base64")?;
            inner_text(&inner)
        }
        ValueFormat::MessagePack => json::pretty(&msgpack::to_json(bytes, limit)?),
        ValueFormat::Gzip => inner_text(&gunzip(bytes, limit)?),
    };
    check_shown(&text, limit)?;
    Ok(text)
}

/// Decoded content shown as text: JSON indented, anything else escaped.
fn inner_text(inner: &[u8]) -> String {
    match std::str::from_utf8(inner) {
        Ok(text) if json::is_document(text) => json::pretty(text),
        _ => format::bytes(inner).into_owned(),
    }
}

/// The bytes to store for `text` typed in `format`: as typed for text, JSON
/// on one line, Base64 (standard, padded), MessagePack from JSON, or the
/// text gzipped. A hex dump cannot be edited.
pub fn encode(text: &str, format: ValueFormat) -> Result<Vec<u8>> {
    Ok(match format {
        ValueFormat::Text => text.as_bytes().to_vec(),
        ValueFormat::Json => {
            json::check(text).map_err(|err| anyhow!("not valid JSON: {err}"))?;
            json::compact(text).into_bytes()
        }
        ValueFormat::Hex => bail!("a hex dump cannot be edited; use Base64 to edit bytes"),
        ValueFormat::Base64 => Base64Style::default().encode(text.as_bytes()).into_bytes(),
        ValueFormat::MessagePack => msgpack::from_json(text)?,
        ValueFormat::Gzip => gzip(text.as_bytes()),
    })
}

/// Which Base64 a value was written in, so that it is written back the
/// same way, as far as the stored text shows it (see
/// [`Base64Style::guessed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base64Style {
    /// `-` and `_` instead of `+` and `/`.
    pub url_safe: bool,
    /// `=` up to a multiple of four characters.
    pub padded: bool,
}

impl Base64Style {
    const ALL: [Self; 4] = [
        Self {
            url_safe: false,
            padded: true,
        },
        Self {
            url_safe: false,
            padded: false,
        },
        Self {
            url_safe: true,
            padded: true,
        },
        Self {
            url_safe: true,
            padded: false,
        },
    ];

    fn engine(self) -> &'static GeneralPurpose {
        match (self.url_safe, self.padded) {
            (false, true) => &general_purpose::STANDARD,
            (false, false) => &general_purpose::STANDARD_NO_PAD,
            (true, true) => &general_purpose::URL_SAFE,
            (true, false) => &general_purpose::URL_SAFE_NO_PAD,
        }
    }

    /// The decoded bytes and the style they were in. Only canonical
    /// Base64 is accepted (no whitespace, no stray bits), so encoding the
    /// result in the same style gives back exactly `text`.
    ///
    /// Text that fits several styles is taken as standard, then as padded:
    /// the most common encoders write that, and a padded encoder always
    /// gives a length that is a multiple of four, an unpadded one only a
    /// third of the time. [`Self::guessed`] says when this was a guess.
    pub fn decode(text: &[u8]) -> Option<(Vec<u8>, Self)> {
        if text.is_empty() {
            return None;
        }
        Self::ALL
            .into_iter()
            .find_map(|style| Some((style.engine().decode(text).ok()?, style)))
    }

    pub fn encode(self, bytes: &[u8]) -> String {
        self.engine().encode(bytes)
    }

    /// What Base64 `text` does not show about its style, so that
    /// [`Self::decode`] guessed it: the alphabet, unless it has one of
    /// `+ / - _`; the padding, unless it has `=` or a length that is not a
    /// multiple of four. An edit that needs the part guessed (a `+` or
    /// `/`, padding) is written in the guess, which the app that reads the
    /// value may not take.
    pub fn guessed(text: &[u8]) -> Base64Guess {
        Base64Guess {
            alphabet: !text.iter().any(|b| matches!(b, b'+' | b'/' | b'-' | b'_')),
            padding: !text.contains(&b'=') && text.len().is_multiple_of(4),
        }
    }
}

/// See [`Base64Style::guessed`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Base64Guess {
    /// Standard or URL-safe.
    pub alphabet: bool,
    /// Padded or not.
    pub padding: bool,
}

impl Default for Base64Style {
    fn default() -> Self {
        Self::ALL[0]
    }
}

/// Decompresses gzip (every member of it), failing beyond `limit` bytes.
pub fn gunzip(bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
    if !is_gzip(bytes) {
        bail!("not gzip data");
    }
    let mut out = Vec::new();
    MultiGzDecoder::new(bytes)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|err| anyhow!("not valid gzip data: {err}"))?;
    if out.len() > limit {
        bail!(
            "decompresses to more than {}, too large to show",
            format::size(limit as u64)
        );
    }
    Ok(out)
}

/// gzip at the default level. The header carries no time or name, so the
/// same bytes always compress the same.
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = GzEncoder::new(
        Vec::with_capacity(bytes.len() / 2 + 32),
        Compression::default(),
    );
    // Writing to a Vec cannot fail.
    let _ = encoder.write_all(bytes);
    encoder.finish().unwrap_or_default()
}

/// `hexdump -C` style: offset, sixteen bytes in hex in two groups of
/// eight, and the same bytes as ASCII, `.` for the rest. Shows the first
/// `limit` bytes, then says how many more there are.
pub fn hex_dump(bytes: &[u8], limit: usize) -> String {
    let shown = &bytes[..bytes.len().min(limit)];
    let mut out = String::with_capacity(shown.len() / 16 * 79 + 80);
    for (line, chunk) in shown.chunks(16).enumerate() {
        if line > 0 {
            out.push('\n');
        }
        let _ = write!(out, "{:08x}  ", line * 16);
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        out.push('|');
        out.extend(chunk.iter().map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        }));
        out.push('|');
    }
    if bytes.len() > shown.len() {
        let _ = write!(
            out,
            "\n… {} more bytes not shown",
            format::count((bytes.len() - shown.len()) as u64)
        );
    }
    out
}

#[cfg(test)]
mod tests;
