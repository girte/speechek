//! Redaction of the full Development evidence.
//!
//! Two things could otherwise reach an archived record: a configured API key
//! spelled in one of the encodings its carriers use, and a credential field of
//! a JSON document the record carries. Both are found semantically.
//!
//! Key matching never guesses a spelling. A body is reduced to *one* decoded
//! view: each source byte is appended and any escape that then ends the buffer
//! is collapsed, repeatedly, so `%2573`, `\u0073` and `%5Cu0073` all settle on
//! the same letters in a single pass - a doubly encoded key needs no second
//! view. Every collapse strictly shortens the buffer, so the whole body costs
//! O(its length), whatever depth it was nested to. A key is searched for in the
//! body as it stands and in that view; a match in the view is replaced in the
//! original span it came from, and every other byte of the body - escapes
//! included - is copied verbatim, so the text around a finding is byte-exact.
//!
//! Decoding reads exactly the two spellings a carrier of this data produces:
//! the escapes JSON text uses (`\"`, `\\`, `\/`, `\b`, `\f`, `\n`, `\r`, `\t`
//! and `\uXXXX`, a surrogate pair being two `\u` escapes) and percent encoding
//! in either hexadecimal case. `+`, `%uXXXX`, `\xHH` and base64 are not decoded:
//! no carrier here writes them, and each would turn ordinary text into a match.
//!
//! Credential fields are found by *parsing*, never by scanning prose: a member
//! whose name is `authorization`, `proxy-authorization`, `x-goog-api-key`,
//! `api-key`, `access_token`, `refresh_token`, `password` or `secret` has its
//! value replaced, in a body that is a JSON document and in the JSON-looking
//! strings such a document carries. No keyword prefilter runs before the parse,
//! which is what used to let an `api_key` spelled in escapes through. Only JSON
//! is parsed, so a transcript that says the word "password" keeps saying it.
//!
//! Nothing here replaces a whole body. A body that holds no key and no
//! credential field is handed back byte for byte - including its formatting,
//! which is preserved for every body except a JSON text that itself held a
//! credential field and is therefore re-rendered.

use std::ops::Range;
use std::sync::Arc;

use serde_json::Value;

use crate::secrets::SecretKey;

/// What one replaced span becomes. The same marker the production boundary
/// uses, so a reader of the archive cannot mistake one for the other.
const REDACTED: &str = "[redacted]";

/// The credential field names a JSON document may spell, normalized: case, `-`
/// and `_` do not count, so each entry covers every spelling of its field.
const CREDENTIAL_NAMES: [&[u8]; 8] = [
    b"authorization",
    b"proxyauthorization",
    b"xgoogapikey",
    b"apikey",
    b"accesstoken",
    b"refreshtoken",
    b"password",
    b"secret",
];

/// Raw evidence that a JSON document could name a credential field: one of
/// these fragments is spelled as it stands, or a `\u` escape hides letters (no
/// other JSON escape can spell one). A document that shows neither cannot name
/// a field this module recognizes and is not parsed.
const CREDENTIAL_FRAGMENTS: [&[u8]; 5] = [
    b"authorization",
    b"api",
    b"token",
    b"password",
    b"secret",
];

/* -------------------------------------------------------------------------- */
/* The boundary                                                               */
/* -------------------------------------------------------------------------- */

/// Never let a configured key or a recognized credential field reach a
/// Development record, and keep every other byte of the body where it was. The
/// keys stay borrowed handles: this function reads them and replaces nothing
/// but the spans it finds.
pub(crate) fn diagnostic_text(message: &str, keys: &[Arc<SecretKey>]) -> String {
    redact_owned(message.to_owned(), keys)
}

/// The same boundary for a body that is a `serde_json` value. The caller keeps
/// its own copy - this one only reads it - so the body is rendered once and
/// cleaned as text: the rendering is what the writer would have written anyway,
/// and no member of the caller's value is copied.
pub(crate) fn diagnostic_json(value: &Value, keys: &[Arc<SecretKey>]) -> String {
    diagnostic_text(&value.to_string(), keys)
}

/// Redacts one owned value in place: every string it holds, every member name
/// that is a key, and every recognized credential field, however deeply nested.
/// A credential-named member is replaced whatever its value holds, so nothing
/// under that name survives; numbers, flags and unrelated members are left
/// alone, and a string that needs nothing keeps its own buffer.
pub(crate) fn diagnostic_redact_value(value: &mut Value, keys: &[Arc<SecretKey>]) {
    match value {
        Value::String(text) => *text = redact_owned(std::mem::take(text), keys),
        Value::Array(items) => {
            for item in items {
                diagnostic_redact_value(item, keys);
            }
        }
        Value::Object(members) => {
            for (name, nested) in members.iter_mut() {
                if credential_name(name) {
                    *nested = Value::String(REDACTED.to_owned());
                } else {
                    diagnostic_redact_value(nested, keys);
                }
            }
            if members.keys().any(|name| needs_key_scan(name, keys)) {
                let original = std::mem::take(members);
                for (name, nested) in original {
                    let name = redact_owned(name, keys);
                    members.insert(name, nested);
                }
            }
        }
        _ => {}
    }
}

/// The boundary for a string the caller already owns: a string that holds
/// nothing this module redacts is handed straight back.
fn redact_owned(text: String, keys: &[Arc<SecretKey>]) -> String {
    if let Some(stripped) = strip_credential_fields(&text) {
        return redact_keys(&stripped, keys);
    }
    if !needs_key_scan(&text, keys) {
        return text;
    }
    redact_keys(&text, keys)
}

/* -------------------------------------------------------------------------- */
/* Credential fields                                                          */
/* -------------------------------------------------------------------------- */

/// The body as a JSON document with every recognized credential field's value
/// replaced, or `None` when the body is not a JSON document, holds no such
/// field, or cannot be re-rendered. The parse is what finds a field: no keyword
/// prefilter runs before it, so a name spelled in escapes is decoded here and
/// recognized like any other.
fn strip_credential_fields(text: &str) -> Option<String> {
    if !could_name_credentials(text) {
        return None;
    }
    let mut document: Value = serde_json::from_str(text).ok()?;
    if !strip_fields(&mut document) {
        return None;
    }
    Some(document.to_string())
}

/// Whether a body is a JSON document that could name a credential field.
fn could_name_credentials(text: &str) -> bool {
    if !text.trim_start().starts_with(['{', '[']) {
        return false;
    }
    let bytes = text.as_bytes();
    CREDENTIAL_FRAGMENTS
        .iter()
        .any(|fragment| contains_ignore_case(bytes, fragment))
        || bytes.windows(2).any(|pair| pair == b"\\u".as_slice())
}

/// Replaces every recognized credential field's value inside one parsed
/// document, however deeply it is nested. `true` when anything changed, which
/// is the only case the caller re-renders the document for.
fn strip_fields(value: &mut Value) -> bool {
    match value {
        Value::Object(members) => {
            let mut changed = false;
            for (name, nested) in members.iter_mut() {
                if credential_name(name) {
                    *nested = Value::String(REDACTED.to_owned());
                    changed = true;
                } else if strip_fields(nested) {
                    changed = true;
                }
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |changed, item| strip_fields(item) || changed),
        Value::String(text) => match strip_credential_fields(text) {
            Some(stripped) => {
                *text = stripped;
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// Whether one member name is a recognized credential field.
fn credential_name(name: &str) -> bool {
    let normalized = name.bytes().filter(|byte| !matches!(*byte, b'-' | b'_'));
    CREDENTIAL_NAMES.iter().any(|known| {
        normalized
            .clone()
            .map(|byte| byte.to_ascii_lowercase())
            .eq(known.iter().copied())
    })
}

/* -------------------------------------------------------------------------- */
/* Keys                                                                       */
/* -------------------------------------------------------------------------- */

/// The body with every spelling of a configured key replaced by the marker. A
/// body that holds none keeps its own text. [`decoded_view`] returns `None`
/// before allocating anything when the body spells no `%` or `\`, so a plain
/// body is searched as it stands and never gets a copy or an atom map.
fn redact_keys(text: &str, keys: &[Arc<SecretKey>]) -> String {
    let needles = needles(keys);
    if needles.is_empty() {
        return text.to_owned();
    }
    let mut spans = Vec::new();
    key_spans(text.as_bytes(), &needles, &mut spans);
    if let Some(view) = decoded_view(text) {
        let mut found = Vec::new();
        key_spans(&view.bytes, &needles, &mut found);
        spans.extend(found.into_iter().map(|span| map_back(span, &view.atoms)));
    }
    if spans.is_empty() {
        text.to_owned()
    } else {
        rebuild(text, spans)
    }
}

/// Whether a body could spell a configured key: the ring holds a key at all,
/// and the body either carries the key as it stands or a byte that could begin
/// one of its escape spellings. A body that shows neither is handed back
/// without a copy, and an empty key list makes every body such a body.
fn needs_key_scan(text: &str, keys: &[Arc<SecretKey>]) -> bool {
    if !keys.iter().any(|key| !key.as_str().is_empty()) {
        return false;
    }
    text.as_bytes().iter().any(|byte| matches!(*byte, b'%' | b'\\'))
        || keys
            .iter()
            .any(|key| !key.as_str().is_empty() && text.contains(key.as_str()))
}

/// The configured keys as borrowed byte needles. An empty key is skipped: it
/// would mark every position of every body.
fn needles<'a>(keys: &'a [Arc<SecretKey>]) -> Vec<&'a [u8]> {
    keys.iter()
        .map(|key| key.as_str().as_bytes())
        .filter(|needle| !needle.is_empty())
        .collect()
}

/// Every occurrence of every key in one view, non-overlapping per key.
fn key_spans(bytes: &[u8], needles: &[&[u8]], out: &mut Vec<Range<usize>>) {
    for needle in needles {
        let needle: &[u8] = needle;
        let mut at = 0;
        while at + needle.len() <= bytes.len() {
            if bytes[at] == needle[0] && &bytes[at..at + needle.len()] == needle {
                out.push(at..at + needle.len());
                at += needle.len();
            } else {
                at += 1;
            }
        }
    }
}

fn contains_ignore_case(bytes: &[u8], needle: &[u8]) -> bool {
    needle.len() <= bytes.len()
        && bytes
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

/* -------------------------------------------------------------------------- */
/* The decoded view                                                           */
/* -------------------------------------------------------------------------- */

/// One decoded view of a body, with the compact record of where each of its
/// bytes came from. `atoms` covers the whole of `bytes` in order, and adjacent
/// ordinary source bytes share one atom, so the record costs a few entries per
/// escape a body really spells rather than one per byte.
struct Tail {
    bytes: Vec<u8>,
    atoms: Vec<Atom>,
}

/// One covered run of a view: `out` in the view's bytes, `src` in the body. A
/// decoded atom need not be as long as its source (`%73` is three bytes of
/// source and one of view), which is why the record exists.
struct Atom {
    out: Range<usize>,
    src: Range<usize>,
    decoded: bool,
}

impl Tail {
    /// Appends one source byte, extending the run of ordinary bytes it belongs
    /// to while that run is still the end of the view.
    fn push(&mut self, byte: u8, at: usize) {
        let end = self.bytes.len();
        self.bytes.push(byte);
        if let Some(atom) = self.atoms.last_mut() {
            if !atom.decoded && atom.out.end == end && atom.src.end == at {
                atom.out.end = end + 1;
                atom.src.end = at + 1;
                return;
            }
        }
        self.atoms.push(Atom {
            out: end..end + 1,
            src: at..at + 1,
            decoded: false,
        });
    }

    /// Collapses the escape that now ends the view, if there is one, and says
    /// whether it did. Every collapse shortens the view, so a body nested to
    /// any depth settles after a number of collapses bounded by its length.
    fn collapse(&mut self) -> bool {
        let Some(escape) = suffix_escape(&self.bytes) else {
            return false;
        };
        let end = self.bytes.len();
        let (src, first, start) = self.sources(end - escape.consumed, end);
        self.bytes.truncate(start);
        self.bytes.extend_from_slice(&escape.out[..escape.len]);
        self.atoms.truncate(first);
        self.atoms.push(Atom {
            out: start..start + escape.len,
            src,
            decoded: true,
        });
        true
    }

    /// The body span the view's bytes `start..end` came from, the index at which
    /// the merged atoms are dropped, and the view position the merged region
    /// begins at. A run of ordinary bytes the escape begins inside is split, so
    /// the merged region starts at an atom boundary; a decoded atom is never
    /// split, because a collapse can only begin at `%` or `\`, which no byte of
    /// a multi-byte decoded atom can hold.
    fn sources(&mut self, start: usize, end: usize) -> (Range<usize>, usize, usize) {
        let mut start = start;
        let mut first = self.atoms.partition_point(|atom| atom.out.end <= start);
        if self.atoms[first].out.start < start {
            if self.atoms[first].decoded {
                start = self.atoms[first].out.start;
            } else {
                let atom = &mut self.atoms[first];
                let keep = start - atom.out.start;
                atom.out.end = start;
                atom.src.end = atom.src.start + keep;
                first += 1;
            }
        }
        let mut from = usize::MAX;
        let mut to = 0;
        for atom in &self.atoms[first..] {
            if atom.decoded {
                from = from.min(atom.src.start);
                to = to.max(atom.src.end);
            } else {
                let head = start.max(atom.out.start) - atom.out.start;
                let tail = end.min(atom.out.end) - atom.out.start;
                from = from.min(atom.src.start + head);
                to = to.max(atom.src.start + tail);
            }
        }
        if from == usize::MAX {
            // The escape began inside the run that was just split, so its whole
            // source is that run's remainder.
            from = self.atoms[first - 1].src.end;
            to = from + (end - start);
        }
        (from..to, first, start)
    }
}

/// The view a body decodes to, or `None` when it spells no escape at all and
/// the body itself is the whole view.
fn decoded_view(text: &str) -> Option<Tail> {
    if !text
        .as_bytes()
        .iter()
        .any(|byte| matches!(*byte, b'%' | b'\\'))
    {
        return None;
    }
    let mut tail = Tail {
        bytes: Vec::with_capacity(text.len()),
        atoms: Vec::new(),
    };
    for (at, byte) in text.bytes().enumerate() {
        tail.push(byte, at);
        while tail.collapse() {}
    }
    Some(tail)
}

/// The escape that ends a view, if the bytes there spell one. The longest
/// spelling is tried first, so a surrogate pair is decoded as one code point.
fn suffix_escape(bytes: &[u8]) -> Option<Escape> {
    let end = bytes.len();
    if end >= 12
        && bytes[end - 12] == b'\\'
        && bytes[end - 11] == b'u'
        && bytes[end - 6] == b'\\'
        && bytes[end - 5] == b'u'
    {
        if let (Some(high), Some(low)) = (hex4_at(bytes, end - 10), hex4_at(bytes, end - 4)) {
            if let Some(pair) = Escape::pair(high, low) {
                return Some(pair);
            }
        }
    }
    if end >= 6 && bytes[end - 6] == b'\\' && bytes[end - 5] == b'u' {
        if let Some(code) = hex4_at(bytes, end - 4) {
            if let Some(escape) = Escape::code(code) {
                return Some(escape);
            }
        }
    }
    if end >= 3 && bytes[end - 3] == b'%' {
        if let (Some(high), Some(low)) = (hex(bytes[end - 2]), hex(bytes[end - 1])) {
            return Some(Escape::byte(high << 4 | low, 3));
        }
    }
    if end >= 2 && bytes[end - 2] == b'\\' {
        return simple_escape(bytes[end - 1]);
    }
    None
}

/// One of the single-byte escapes JSON text uses, if `byte` is one.
fn simple_escape(byte: u8) -> Option<Escape> {
    match byte {
        b'"' => Some(Escape::byte(b'"', 2)),
        b'\\' => Some(Escape::byte(b'\\', 2)),
        b'/' => Some(Escape::byte(b'/', 2)),
        b'b' => Some(Escape::byte(0x08, 2)),
        b'f' => Some(Escape::byte(0x0c, 2)),
        b'n' => Some(Escape::byte(b'\n', 2)),
        b'r' => Some(Escape::byte(b'\r', 2)),
        b't' => Some(Escape::byte(b'\t', 2)),
        _ => None,
    }
}

/// One decoded escape: the bytes it stands for and how many bytes of the view
/// spell it. It is always shorter than its spelling, which is what makes a
/// collapse terminate.
struct Escape {
    consumed: usize,
    out: [u8; 4],
    len: usize,
}

impl Escape {
    fn byte(byte: u8, consumed: usize) -> Self {
        Self {
            consumed,
            out: [byte, 0, 0, 0],
            len: 1,
        }
    }

    /// One code point spelled as `\uXXXX`. A lone surrogate is no text a body
    /// can hold, so it is left to be read as the bytes it is spelled with.
    fn code(code: u16) -> Option<Self> {
        if (0xD800..=0xDFFF).contains(&code) {
            return None;
        }
        let mut out = [0u8; 4];
        let len = char::from_u32(u32::from(code))?.encode_utf8(&mut out).len();
        Some(Self {
            consumed: 6,
            out,
            len,
        })
    }

    /// A surrogate pair, spelled as the two `\u` escapes that carry it.
    fn pair(high: u16, low: u16) -> Option<Self> {
        if !(0xD800..=0xDBFF).contains(&high) || !(0xDC00..=0xDFFF).contains(&low) {
            return None;
        }
        let code = 0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
        let mut out = [0u8; 4];
        let len = char::from_u32(code)?.encode_utf8(&mut out).len();
        Some(Self {
            consumed: 12,
            out,
            len,
        })
    }
}

fn hex4_at(bytes: &[u8], at: usize) -> Option<u16> {
    let mut value = 0u16;
    for offset in 0..4 {
        value = value
            .checked_mul(16)?
            .checked_add(u16::from(hex(*bytes.get(at + offset)?)?))?;
    }
    Some(value)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The body span a finding in the view covers. A decoded atom a finding only
/// partly covers is taken whole: half of an escape is not a span a body can be
/// cleaned at.
fn map_back(range: Range<usize>, atoms: &[Atom]) -> Range<usize> {
    let first = atoms.partition_point(|atom| atom.out.end <= range.start);
    let mut from = usize::MAX;
    let mut to = 0;
    for atom in &atoms[first..] {
        if atom.out.start >= range.end {
            break;
        }
        if atom.decoded {
            from = from.min(atom.src.start);
            to = to.max(atom.src.end);
        } else {
            let head = range.start.max(atom.out.start) - atom.out.start;
            let tail = range.end.min(atom.out.end) - atom.out.start;
            from = from.min(atom.src.start + head);
            to = to.max(atom.src.start + tail);
        }
    }
    if from < to {
        from..to
    } else {
        range.start..range.start
    }
}

/* -------------------------------------------------------------------------- */
/* The rebuilt body                                                           */
/* -------------------------------------------------------------------------- */

/// The body with every finding replaced by the marker and every other byte
/// copied out of the body itself.
fn rebuild(text: &str, mut spans: Vec<Range<usize>>) -> String {
    align(text, &mut spans);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for span in spans {
        out.push_str(&text[at..span.start]);
        out.push_str(REDACTED);
        at = span.end;
    }
    out.push_str(&text[at..]);
    out
}

/// Widens the spans to character boundaries, drops the empty ones and merges
/// the ones that touch, so a replacement can never split a character and one
/// span of text is never marked twice.
fn align(text: &str, spans: &mut Vec<Range<usize>>) {
    let bytes = text.as_bytes();
    for span in spans.iter_mut() {
        if span.start >= span.end {
            continue;
        }
        while span.start > 0 && is_continuation(bytes[span.start]) {
            span.start -= 1;
        }
        while span.end < bytes.len() && is_continuation(bytes[span.end]) {
            span.end += 1;
        }
    }
    spans.retain(|span| span.start < span.end);
    spans.sort_by_key(|span| (span.start, span.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    *spans = merged;
}

fn is_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/* -------------------------------------------------------------------------- */
/* Tests                                                                      */
/* -------------------------------------------------------------------------- */

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::{diagnostic_json, diagnostic_redact_value, diagnostic_text};
    use crate::secrets::SecretKey;

    /// The key the review used, spelled in every form a carrier produces.
    const KEY: &str = "synthetic-old-key";

    fn ring() -> Vec<Arc<SecretKey>> {
        vec![Arc::new(SecretKey::new(KEY.to_owned()))]
    }

    /// `text` with every byte percent-encoded `layers` times over.
    fn encode(text: &str, layers: usize) -> String {
        let mut body = text.to_owned();
        for _ in 0..layers {
            body = body.bytes().map(|byte| format!("%{byte:02X}")).collect();
        }
        body
    }

    #[test]
    fn a_plain_key_is_replaced_where_it_stands() {
        let body = diagnostic_text("recognition failed for key synthetic-old-key.", &ring());
        assert_eq!(body, "recognition failed for key [redacted].");
    }

    #[test]
    fn a_query_string_keeps_its_other_parameters() {
        let body = format!("https://x/?key={KEY}&mode=smart");
        assert_eq!(
            diagnostic_text(&body, &ring()),
            "https://x/?key=[redacted]&mode=smart"
        );
    }

    /// The clause the review found: one letter percent encoded - and the same
    /// key fully encoded with both hexadecimal cases mixed.
    #[test]
    fn a_percent_encoded_key_is_replaced() {
        let body = format!("prefix ?key=%73{} suffix", &KEY[1..]);
        assert_eq!(diagnostic_text(&body, &ring()), "prefix ?key=[redacted] suffix");
        let mixed: String = KEY
            .bytes()
            .enumerate()
            .map(|(at, byte)| {
                if at % 2 == 0 {
                    format!("%{byte:02X}")
                } else {
                    format!("%{byte:02x}")
                }
            })
            .collect();
        assert_eq!(diagnostic_text(&mixed, &ring()), "[redacted]");
    }

    /// The other clause: a JSON string literal and the escaped spelling of one.
    #[test]
    fn a_json_escaped_key_is_replaced() {
        let escaped = format!("\\u0073{}", &KEY[1..]);
        assert_eq!(
            diagnostic_json(&json!({ "text": escaped }), &ring()),
            r#"{"text":"[redacted]"}"#
        );
        let twice = format!("{{\"text\":\"\\\\u0073{}\"}}", &KEY[1..]);
        assert_eq!(diagnostic_text(&twice, &ring()), r#"{"text":"[redacted]"}"#);
    }

    #[test]
    fn a_doubly_encoded_key_is_replaced() {
        let body = format!("prefix {} suffix", encode(KEY, 2));
        assert_eq!(diagnostic_text(&body, &ring()), "prefix [redacted] suffix");
    }

    /// Nesting has no depth that needs a second view or a bound: the same pass
    /// decodes an arbitrarily deep chain of percent encodings.
    #[test]
    fn a_deeply_nested_encoding_is_still_replaced() {
        for layers in [3, 5, 7] {
            let body = format!("prefix {} suffix", encode(KEY, layers));
            assert_eq!(diagnostic_text(&body, &ring()), "prefix [redacted] suffix");
        }
    }

    /// A percent-encoded backslash that then spells a `\u` escape: the collapse
    /// of one spelling feeds the next in the same pass.
    #[test]
    fn a_percent_encoded_escape_is_still_decoded() {
        let body = format!("prefix %5Cu0073{} suffix", &KEY[1..]);
        assert_eq!(diagnostic_text(&body, &ring()), "prefix [redacted] suffix");
    }

    /// A key beyond the basic plane, spelled the way JSON text carries it.
    #[test]
    fn a_surrogate_pair_escape_is_still_decoded() {
        let key = "🎤synthetic-old-key";
        let body = format!("prefix \\uD83C\\uDFA4{} suffix", &key[4..]);
        assert_eq!(
            diagnostic_text(&body, &[Arc::new(SecretKey::new(key.to_owned()))]),
            "prefix [redacted] suffix"
        );
    }

    /// The other review finding: a credential name spelled in escapes used to
    /// escape the keyword prefilter and keep a foreign credential. No key list
    /// is involved - the field is what makes it a credential.
    #[test]
    fn an_escaped_credential_name_is_still_a_field() {
        let body = diagnostic_text(
            r#"{"\u0061\u0070\u0069\u005f\u006b\u0065\u0079":"foreign-credential"}"#,
            &[],
        );
        assert!(!body.contains("foreign-credential"), "{body}");
        assert!(body.contains("[redacted]"), "{body}");
    }

    #[test]
    fn a_nested_report_string_carries_no_foreign_credential() {
        let body = diagnostic_text(
            r#"{"results":{"smart":"{\"\u0061\u0070\u0069\u005f\u006b\u0065\u0079\":\"foreign-credential\"}"},"errors":{},"errorDetails":{}}"#,
            &[],
        );
        assert!(!body.contains("foreign-credential"), "{body}");
        assert!(body.contains("[redacted]"), "{body}");
        assert!(body.contains("results"), "{body}");
    }

    #[test]
    fn a_credential_field_is_replaced_and_its_neighbours_kept() {
        let body = diagnostic_text(r#"{"api_key":"foreign-credential","mode":"smart"}"#, &[]);
        assert_eq!(body, r#"{"api_key":"[redacted]","mode":"smart"}"#);
    }

    #[test]
    fn a_credential_field_holding_a_document_is_replaced_whole() {
        let body = diagnostic_json(&json!({ "password": { "hint": "x", "value": "y" } }), &[]);
        assert!(!body.contains("hint"), "{body}");
        assert!(!body.contains('y'), "{body}");
    }

    #[test]
    fn a_replaced_json_pair_stays_valid_json() {
        let body = diagnostic_json(&json!({ "api_key": "AIza-not-a-configured-key" }), &[]);
        assert_eq!(body, r#"{"api_key":"[redacted]"}"#);
        assert!(serde_json::from_str::<Value>(&body).is_ok());
    }

    /// Prose is not a JSON document, so a credential word inside it is left
    /// where the user said it: only a parsed field is a field.
    #[test]
    fn prose_naming_a_credential_word_is_untouched() {
        for body in [
            "we typed the password: hunter2 into the field",
            "Authorization: Bearer 8I57HZZ7TG7T",
            "the secret: is safe with us",
        ] {
            assert_eq!(diagnostic_text(body, &[]), body);
            assert_eq!(diagnostic_text(body, &ring()), body);
        }
    }

    /// The shape the renderer endpoint redacts in place: the remembered key
    /// goes, the report's own maps stay exactly as they were.
    #[test]
    fn a_stored_result_loses_a_remembered_key_in_every_spelling() {
        let raw = r#"retired"secret\key/+=?&%"#;
        let escaped = r#"retired\"secret\\key/+=?&%"#;
        let percent = "retired%22secret%5Ckey%2F%2B%3D%3F%26%25";
        let mut report = json!({
            "results": { "live": format!("raw {raw} escaped {escaped} percent {percent}") },
            "errors": {},
            "errorDetails": {},
        });
        diagnostic_redact_value(&mut report, &[Arc::new(SecretKey::new(raw.to_owned()))]);
        let stored = report["results"]["live"].as_str().expect("the result string");
        for form in [raw, escaped, percent] {
            assert!(!stored.contains(form), "{form} survived in {stored}");
        }
        assert_eq!(report["errors"], json!({}));
        assert_eq!(report["errorDetails"], json!({}));
    }

    #[test]
    fn a_credential_name_in_a_value_is_replaced_in_place() {
        let mut value = json!({
            "result": {
                "results": { "smart": format!("{{\"api_key\":\"{KEY}\"}}") },
                "errors": {},
                "errorDetails": {},
            },
        });
        diagnostic_redact_value(&mut value, &ring());
        let smart = value["result"]["results"]["smart"]
            .as_str()
            .expect("the result");
        assert!(!smart.contains(KEY), "{smart}");
        assert!(value["result"]["results"].is_object());
    }

    /// Ordinary text keeps every byte, even the ones the view decodes while it
    /// is only looking for a key: nothing outside a finding is ever rewritten.
    #[test]
    fn unrelated_text_survives_byte_for_byte() {
        let body = "Übertragung: «émoji🎤» 100% \\ Pfad\\C:\\temp druckfertig";
        assert_eq!(diagnostic_text(body, &ring()), body);
        assert_eq!(diagnostic_text(body, &[]), body);
    }

    /// A JSON document that names no credential field is not touched at all:
    /// no re-rendering, no reformatting, whatever whitespace it was written
    /// with.
    #[test]
    fn a_document_without_a_credential_field_is_untouched() {
        let body = r#"{ "a": 1,  "b": 2 }"#;
        assert_eq!(diagnostic_text(body, &[]), body);
        assert_eq!(diagnostic_text(body, &ring()), body);
        assert_eq!(
            diagnostic_json(&json!({ "a": 1, "b": 2 }), &[]),
            r#"{"a":1,"b":2}"#
        );
    }

    #[test]
    fn an_empty_key_list_changes_nothing_prose_or_document() {
        let value = json!({ "text": "the whole dictation", "level": 3 });
        assert_eq!(diagnostic_json(&value, &[]), value.to_string());
    }
}
