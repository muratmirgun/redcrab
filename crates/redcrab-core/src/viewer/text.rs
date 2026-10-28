//! A value's bytes as text in a chosen format, the rules for editing that
//! text, and the way back to bytes.

use anyhow::{Context as _, Result, anyhow, bail, ensure};

use crate::{
    format,
    formats::{
        self, Base64Guess, Base64Style, DECODE_LIMIT, HEX_LIMIT, ValueFormat, json, msgpack,
    },
};

/// How the text shown maps back to the stored bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layer {
    /// The text is the bytes (JSON maybe indented). Anything goes back.
    Plain,
    /// The text is the bytes, and must stay valid JSON.
    Json,
    /// A hex dump: nothing goes back.
    Hex,
    /// The text is the decoded bytes, encoded again in the same style.
    Base64(Base64Style),
    /// The text is the decompressed bytes.
    Gzip,
    /// The text is JSON, written back as MessagePack.
    MessagePack,
}

/// A value (or one element of a collection) as text.
#[derive(Clone, Debug, PartialEq)]
pub struct TextValue {
    /// What to show: JSON indented, text as it is, anything else quoted
    /// and escaped like `redis-cli` writes it.
    pub text: String,
    /// The text is JSON, worth highlighting as such.
    pub is_json: bool,
    /// The JSON was indented for display; an edit is saved back on one
    /// line, spaced the way it was stored.
    pub reformatted_json: bool,
    /// Editing is allowed and `text` converts back to exactly the bytes it
    /// shows (indented JSON, quoted bytes, a decoded format), so saving
    /// cannot change anything the user did not touch.
    pub editable: bool,
    /// The format `text` is in.
    pub format: ValueFormat,
    /// A note for above the text: why it cannot be edited although the
    /// caller allowed it, or how it is written.
    pub notice: Option<String>,
    /// How the stored JSON was spaced, when it was indented for display.
    spacing: Option<json::Spacing>,
    /// The text is quoted with escapes (see [`format::quote_arg`]): the
    /// bytes are not plain text.
    quoted: bool,
    /// The stored line breaks are all CRLF, shown as LF (an editor keeps
    /// no `\r`); saving writes CRLF back.
    crlf: bool,
    layer: Layer,
}

const QUOTED: &str = "Not plain text, so it is shown in double quotes with escapes, the way redis-cli writes it (\\xHH for a byte, \\n, \\\", \\\\). Edit it the same way.";
const NOT_TEXT: &str =
    "This value holds bytes that are not text, so it is shown escaped and cannot be edited here.";
const JSON_READ_ONLY: &str = "This JSON holds characters that would not stay as they are in the editor (a \\r that is not part of a CRLF line break, say), so it is read-only here. Switch to Text to edit it with escapes.";
const HEX_READ_ONLY: &str = "A hex dump is read-only. Switch to Text to edit the bytes.";
const MSGPACK_INEXACT: &str = "This MessagePack does not convert to JSON and back byte for byte (it holds binary or extension values, 32-bit floats, keys that are not strings, or numbers stored longer than needed), so it is read-only here.";

impl TextValue {
    /// The bytes as text, or as indented JSON if they are a JSON object or
    /// array, for reading. `editable` is whether the caller allows editing
    /// (a whole value on a writable connection); the result is editable
    /// only if the value is also plain text that shows without escapes.
    ///
    /// JSON on one line, compact or with a space after each `,` and `:` (as
    /// Python writes it), is indented for display, since saving restores
    /// exactly that spacing. JSON laid out any other way (indented already,
    /// say) is shown as stored, so that saving keeps its layout. Edited text
    /// that is no longer JSON is saved as typed.
    pub fn new(bytes: &[u8], editable: bool) -> Self {
        if let Some(json) = std::str::from_utf8(bytes)
            .ok()
            .filter(|s| json::is_document(s))
        {
            return Self::json_document(json, editable, Layer::Plain);
        }
        let lossless = format::as_text(bytes).is_some();
        Self {
            text: format::bytes(bytes).into_owned(),
            is_json: false,
            reformatted_json: false,
            editable: editable && lossless,
            format: ValueFormat::Text,
            notice: (editable && !lossless).then(|| NOT_TEXT.to_string()),
            spacing: None,
            quoted: false,
            crlf: false,
            layer: Layer::Plain,
        }
    }

    /// Valid JSON as stored, or indented (see [`Self::new`]). CRLF line
    /// breaks show as LF and are written back as CRLF (JSON written on
    /// Windows, say); any other character the editor would not keep as it
    /// is (a lone `\r`) makes it read-only.
    fn json_document(json: &str, editable: bool, layer: Layer) -> Self {
        if let Some(lf) = crlf_as_lf(json) {
            return Self {
                crlf: true,
                ..Self::json_view(&lf, true, editable, ValueFormat::Json, layer)
            };
        }
        let lossless = format::as_text(json.as_bytes()).is_some();
        Self {
            notice: (editable && !lossless).then(|| JSON_READ_ONLY.to_string()),
            ..Self::json_view(
                json,
                lossless,
                editable && lossless,
                ValueFormat::Json,
                layer,
            )
        }
    }

    /// JSON text shown indented when that can be undone exactly.
    fn json_view(
        json: &str,
        lossless: bool,
        editable: bool,
        format: ValueFormat,
        layer: Layer,
    ) -> Self {
        let spacing = json::one_line_spacing(json);
        // Text that cannot be shown as stored (a `\r` in it) is indented
        // too; it is not editable anyway.
        let (text, reformatted_json) = if spacing.is_none() && lossless {
            (json.to_string(), false)
        } else {
            let pretty = json::pretty(json);
            let reformatted = pretty != json;
            (pretty, reformatted)
        };
        Self {
            text,
            is_json: true,
            reformatted_json,
            editable,
            format,
            notice: None,
            spacing,
            quoted: false,
            crlf: false,
            layer,
        }
    }

    /// Bytes as editable text: as they are if they are plain text (JSON
    /// documents indented if `indent_json`), else quoted with escapes.
    fn text_view(
        bytes: &[u8],
        editable: bool,
        indent_json: bool,
        format: ValueFormat,
        layer: Layer,
    ) -> Self {
        let text = match format::as_text(bytes) {
            Some(text) => text,
            None => {
                return Self {
                    text: format::quote_arg(bytes),
                    is_json: false,
                    reformatted_json: false,
                    editable,
                    format,
                    notice: Some(QUOTED.to_string()),
                    spacing: None,
                    quoted: true,
                    crlf: false,
                    layer,
                };
            }
        };
        if indent_json && json::is_document(text) {
            return Self::json_view(text, true, editable, format, layer);
        }
        Self {
            text: text.to_string(),
            is_json: false,
            reformatted_json: false,
            editable,
            format,
            notice: None,
            spacing: None,
            quoted: false,
            crlf: false,
            layer,
        }
    }

    /// `bytes` shown in `format`. Fails if they are not in that format (not
    /// JSON, not Base64, ...). `editable` as for [`Self::new`]; whether the
    /// result is editable also depends on the format (a hex dump never is)
    /// and on whether its text converts back to exactly these bytes.
    ///
    /// Text shows the bytes as they are, or, if they are not plain text,
    /// quoted and escaped like `redis-cli` (`"\xff\x00"`), which is how
    /// bytes that are not text are edited. Base64 and gzip show what they
    /// decode to the same way, JSON documents indented. What a decoding
    /// format would show is capped ([`formats::text_limit`]): a small
    /// value can decode into far more text than an editor should take.
    pub fn decode(bytes: &[u8], format: ValueFormat, editable: bool) -> Result<Self> {
        let limit = formats::text_limit(bytes);
        let view = Self::decode_uncapped(bytes, format, editable, limit)?;
        if matches!(
            format,
            ValueFormat::Base64 | ValueFormat::Gzip | ValueFormat::MessagePack
        ) {
            formats::check_shown(&view.text, limit)?;
        }
        Ok(view)
    }

    /// [`Self::decode`] before the check of the text's length (gzip and
    /// MessagePack stop decoding at `limit` already).
    fn decode_uncapped(
        bytes: &[u8],
        format: ValueFormat,
        editable: bool,
        limit: usize,
    ) -> Result<Self> {
        Ok(match format {
            ValueFormat::Text => Self::text_view(bytes, editable, false, format, Layer::Plain),
            ValueFormat::Json => {
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| anyhow!("not valid JSON: the value is not UTF-8 text"))?;
                json::check(text).map_err(|err| anyhow!("not valid JSON: {err}"))?;
                Self::json_document(text, editable, Layer::Json)
            }
            ValueFormat::Hex => Self {
                text: formats::hex_dump(bytes, HEX_LIMIT),
                is_json: false,
                reformatted_json: false,
                editable: false,
                format,
                notice: editable.then(|| HEX_READ_ONLY.to_string()),
                spacing: None,
                quoted: false,
                crlf: false,
                layer: Layer::Hex,
            },
            ValueFormat::Base64 => {
                let (inner, style) = Base64Style::decode(bytes).context("not valid Base64")?;
                let view = Self::text_view(&inner, editable, true, format, Layer::Base64(style));
                let guess = view
                    .editable
                    .then(|| base64_guess(Base64Style::guessed(bytes), style))
                    .flatten();
                Self {
                    notice: join_notices(view.notice.clone(), guess),
                    ..view
                }
            }
            ValueFormat::Gzip => {
                let inner = formats::gunzip(bytes, limit)?;
                Self::text_view(&inner, editable, true, format, Layer::Gzip)
            }
            ValueFormat::MessagePack => {
                let json = msgpack::to_json(bytes, limit)?;
                let exact = msgpack::from_json(&json).is_ok_and(|again| again == bytes);
                Self {
                    notice: (editable && !exact).then(|| MSGPACK_INEXACT.to_string()),
                    ..Self::json_view(&json, true, editable && exact, format, Layer::MessagePack)
                }
            }
        })
    }

    /// The bytes to write for the edited text, before any decoding format
    /// is applied again: JSON that was indented for display goes back on
    /// one line, spaced as it was stored, if it still parses; anything else
    /// is written as typed, with CRLF line breaks if the value had them.
    /// (Quoted text is read by [`Self::encode`].)
    pub fn bytes_to_save(&self, edited: &str) -> Vec<u8> {
        let bytes = match self.spacing {
            Some(spacing) if self.reformatted_json && json::is_document(edited) => {
                json::spaced(&json::compact(edited), spacing).into_bytes()
            }
            _ => edited.as_bytes().to_vec(),
        };
        if self.crlf { lf_as_crlf(&bytes) } else { bytes }
    }

    /// The bytes to store for the edited text in this format: unquoted,
    /// then Base64 encoded again in the style it was stored in (guessed
    /// where the stored text does not show it, which the notice says),
    /// gzipped, or written as MessagePack. Fails if the text does not fit
    /// the format (JSON that no longer parses, unbalanced quotes) or the
    /// value is not editable.
    pub fn encode(&self, edited: &str) -> Result<Vec<u8>> {
        if !self.editable {
            bail!(
                "{}",
                self.notice
                    .as_deref()
                    .unwrap_or("this value cannot be edited")
            );
        }
        let inner = if self.quoted {
            unquote(edited)?
        } else {
            self.bytes_to_save(edited)
        };
        let text = || std::str::from_utf8(&inner).context("not valid UTF-8");
        Ok(match self.layer {
            Layer::Plain => inner,
            Layer::Json => {
                json::check(text()?).map_err(|err| anyhow!("not valid JSON: {err}"))?;
                inner
            }
            Layer::Hex => bail!(HEX_READ_ONLY),
            Layer::Base64(style) => style.encode(&inner).into_bytes(),
            Layer::Gzip => formats::gzip(&inner),
            Layer::MessagePack => msgpack::from_json(text()?)?,
        })
    }

    /// Whether two stored values hold the same thing in this format: the
    /// same bytes, or for gzip the same decompressed bytes (compressors
    /// differ; the content is what matters).
    pub fn same(&self, a: &[u8], b: &[u8]) -> bool {
        match self.layer {
            Layer::Gzip => match (
                formats::gunzip(a, DECODE_LIMIT),
                formats::gunzip(b, DECODE_LIMIT),
            ) {
                (Ok(a), Ok(b)) => a == b,
                _ => a == b,
            },
            _ => a == b,
        }
    }
}

/// How an edit of Base64 is written, when the stored text does not show
/// it (see [`Base64Style::guessed`]): the app reading the value may expect
/// the other way, and the user is the one who can know.
fn base64_guess(guess: Base64Guess, style: Base64Style) -> Option<String> {
    let alphabet = if style.url_safe {
        "the URL-safe alphabet (- _)"
    } else {
        "the standard alphabet (+ /)"
    };
    let padding = if style.padded {
        "padded with ="
    } else {
        "without = padding"
    };
    let (how, unknown) = match (guess.alphabet, guess.padding) {
        (true, true) => (
            format!("in {alphabet}, {padding}"),
            "which alphabet or padding it uses",
        ),
        (true, false) => (format!("in {alphabet}"), "which alphabet it uses"),
        (false, true) => (padding.to_string(), "whether it is padded"),
        (false, false) => return None,
    };
    Some(format!(
        "Edits are saved as Base64 {how}; the stored value does not show {unknown}."
    ))
}

/// `text` with its CRLF line breaks as LF, if every line break in it is
/// CRLF, there is one, and nothing else in it would show escaped: such text
/// (JSON written on Windows, say) can be edited as it shows, and written
/// back with [`lf_as_crlf`] exactly as it was.
fn crlf_as_lf(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut breaks = 0;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\r' if bytes.get(i + 1) != Some(&b'\n') => return None,
            b'\n' if i == 0 || bytes[i - 1] != b'\r' => return None,
            b'\n' => breaks += 1,
            _ => {}
        }
    }
    if breaks == 0 {
        return None;
    }
    let lf = text.replace("\r\n", "\n");
    format::as_text(lf.as_bytes()).is_some().then_some(lf)
}

/// Every line break as CRLF (one that is already stays as it is).
fn lf_as_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    let mut previous = 0;
    for &b in bytes {
        if b == b'\n' && previous != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        previous = b;
    }
    out
}

fn join_notices(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a} {b}")),
        (a, b) => a.or(b),
    }
}

/// Quoted text back to its bytes, the way the console reads an argument.
fn unquote(edited: &str) -> Result<Vec<u8>> {
    let mut args = format::split_args(edited)
        .map_err(|err| anyhow!("{err}: write the value in double quotes, \\xHH for a byte"))?;
    ensure!(
        args.len() == 1,
        "write the value as one argument in double quotes, \\xHH for a byte (\"\" if empty)"
    );
    Ok(args.pop().unwrap_or_default())
}

/// Editing one value's bytes as text: the stored bytes, the format they are
/// shown in, and the way from the edited text to the bytes to write.
///
/// The text itself lives in the front end's editor; the model takes it as
/// it is whenever it needs it (to switch format, to save), so typing costs
/// nothing here.
#[derive(Clone, Debug)]
pub struct TextEdit {
    /// The value as loaded.
    stored: Vec<u8>,
    /// What [`Self::view`] was decoded from: `stored`, or an unsaved draft
    /// carried over a change of format.
    source: Vec<u8>,
    allowed: bool,
    formats: &'static [ValueFormat],
    view: TextValue,
}

impl TextEdit {
    /// `stored` shown in the format it looks to be in ([`formats::detect`]).
    /// `allowed` is whether editing is allowed at all (a whole value on a
    /// writable connection).
    ///
    /// A value that looks like gzip or MessagePack but does not decode (a
    /// broken one, or one that decodes into too much text) is shown as
    /// Text, as editable as Text always is, with a notice saying why.
    pub fn new(stored: Vec<u8>, allowed: bool) -> Self {
        let detected = formats::detect(&stored);
        let view = TextValue::decode(&stored, detected, allowed).unwrap_or_else(|err| {
            let text =
                TextValue::text_view(&stored, allowed, false, ValueFormat::Text, Layer::Plain);
            let why = format!(
                "Shown as stored: it looks like {} but does not decode ({err:#}).",
                detected.label()
            );
            TextValue {
                notice: join_notices(Some(why), text.notice.clone()),
                ..text
            }
        });
        Self {
            source: stored.clone(),
            stored,
            allowed,
            formats: &ValueFormat::ALL,
            view,
        }
    }

    /// A document of the JSON module: shown and saved as JSON only.
    pub fn json_document(json: String, allowed: bool) -> Self {
        let stored = json.into_bytes();
        let view = TextValue::decode(&stored, ValueFormat::Json, allowed)
            .unwrap_or_else(|_| TextValue::new(&stored, false));
        Self {
            source: stored.clone(),
            stored,
            allowed,
            formats: &[ValueFormat::Json],
            view,
        }
    }

    /// The text to show, and its rules.
    pub fn view(&self) -> &TextValue {
        &self.view
    }

    pub fn text(&self) -> &str {
        &self.view.text
    }

    pub fn format(&self) -> ValueFormat {
        self.view.format
    }

    /// The formats a front end offers for this value.
    pub fn formats(&self) -> &'static [ValueFormat] {
        self.formats
    }

    pub fn editable(&self) -> bool {
        self.view.editable
    }

    /// The value as loaded.
    pub fn stored(&self) -> &[u8] {
        &self.stored
    }

    /// Shows the value in `format`. `edited` is the editor's text: if it
    /// differs from what was shown, the draft is carried over (turned into
    /// bytes in the current format, then shown in the new one), so
    /// switching formats never loses typing. Fails, changing nothing, if
    /// the draft does not fit the current format or the bytes are not in
    /// the new one.
    pub fn set_format(&mut self, format: ValueFormat, edited: Option<&str>) -> Result<()> {
        if !self.formats.contains(&format) {
            bail!("this value cannot be shown as {}", format.label());
        }
        let source = match edited {
            Some(edited) if self.view.editable && edited != self.view.text => {
                self.view.encode(edited)?
            }
            _ => self.source.clone(),
        };
        let view = TextValue::decode(&source, format, self.allowed)
            .with_context(|| format!("cannot show this value as {}", format.label()))?;
        self.view = view;
        self.source = source;
        Ok(())
    }

    /// Whether saving `edited` would change the stored value.
    pub fn is_changed(&self, edited: &str) -> bool {
        match self.bytes_for(edited) {
            Ok(bytes) => !self.view.same(&bytes, &self.stored),
            Err(_) => true,
        }
    }

    /// The bytes to write for `edited`, or `None` if they would not change
    /// the value. Fails with the reason if the text does not fit the format
    /// or the value cannot be edited.
    pub fn to_save(&self, edited: &str) -> Result<Option<Vec<u8>>> {
        let bytes = self.bytes_for(edited)?;
        Ok((!self.view.same(&bytes, &self.stored)).then_some(bytes))
    }

    /// Text as shown stands for the bytes it was decoded from (which also
    /// covers a read-only view nothing was typed into); edited text is
    /// encoded, which fails for a read-only view.
    fn bytes_for(&self, edited: &str) -> Result<Vec<u8>> {
        if edited == self.view.text {
            Ok(self.source.clone())
        } else {
            self.view.encode(edited)
        }
    }
}
