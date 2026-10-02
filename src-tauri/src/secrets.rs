//! Encrypted API-key storage (`secrets.bin`) for the Speechek shell.
//!
//! The key list lives in one container beside `settings.json`: the four ASCII
//! bytes `SPK1` followed by a `CryptProtectData` blob. The blob belongs to the
//! current Windows user; there is no machine scope, no entropy, no password
//! and no environment fallback, and a container that is not exactly what this
//! module wrote is an error rather than an empty list. A store that does not
//! exist yet stays distinct from one that exists but cannot be used
//! ([`SecretStoreKind`]).
//!
//! Key material is held in [`Zeroizing`] buffers. A key leaves this module only
//! through [`SecretKey::as_str`], which the provider calls exactly where the
//! value has to be copied into a request header; there is no `Debug`,
//! `Display`, `Serialize` or safe `Clone` on the secret types, and no
//! diagnostic ever quotes a key. Copies made by the network stack, the Tauri
//! IPC layer or the operating system are outside this module's reach.
//!
//! The key list rules: one key per line (`\r\n`, `\n` or `\r`), ECMAScript
//! `String.prototype.trim` on every line, blank lines skipped, duplicates
//! collapsed to their first occurrence, and an interior whitespace character or
//! NUL byte refused with the physical line number a text editor would show.
//!
//! Decoding never lets `serde_json` hold key material. The deserializer keeps
//! the strings it unescapes in a scratch buffer of its own that it does not
//! clear, so every escaped string is decoded here into a [`Zeroizing`] buffer
//! first and replaced by an escape-free reference token; a string that still
//! carries an escape is refused rather than routed through that buffer.

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use zeroize::{Zeroize, Zeroizing};

use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::Deserialize;

use crate::settings::{is_ts_whitespace, ts_trim};

/// Name of the encrypted container written beside the settings file.
const SECRETS_FILE_NAME: &str = "secrets.bin";

/// Container magic: exactly four ASCII bytes in front of the DPAPI blob.
const CONTAINER_MAGIC: &[u8; 4] = b"SPK1";

/// Payload version this module writes and the only one it accepts.
const CONTAINER_VERSION: u64 = 1;

/// Message for a key whose interior holds a whitespace character.
const KEY_WHITESPACE_MESSAGE: &str =
    "ключ содержит пробельный символ; разместите один ключ в строке без пробелов";

/// Message for a key holding a NUL byte, which means a UTF-16 file.
const KEY_NUL_MESSAGE: &str = "ключ содержит нулевой байт; сохраните список как UTF-8, а не UTF-16";

/// Message for an empty entry where a key had to be.
const KEY_EMPTY_MESSAGE: &str = "строка не содержит ключ";

/// Message for a key that appears twice inside the stored container.
const KEY_DUPLICATE_MESSAGE: &str = "список ключей содержит повтор";

/* -------------------------------------------------------------------------- */
/* Secret types                                                               */
/* -------------------------------------------------------------------------- */

/// One API key. The value lives in a [`Zeroizing`] buffer, and `as_str` is the
/// only way out, so a key cannot reach a log line, an IPC payload or a second
/// long-lived copy by accident.
pub struct SecretKey(Zeroizing<String>);

impl SecretKey {
    /// Takes ownership of `value` without copying it. Callers must hand over
    /// the last existing `String`; a copy left behind is plaintext this module
    /// cannot clear.
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The key material, borrowed for the one place that has to use it.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A validated key list: unique keys in first-occurrence order, each paired
/// with the 1-based physical line it was first seen on. Blank lines, CRLF and
/// collapsed duplicates therefore never shift the line a diagnostic reports.
pub struct NormalizedKeys {
    pub(crate) keys: Vec<Arc<SecretKey>>,
    pub(crate) lines: Vec<usize>,
}

impl fmt::Debug for NormalizedKeys {
    /// Counts and line numbers only: the keys themselves never reach a log
    /// line, even though the value is otherwise an ordinary container.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NormalizedKeys")
            .field("keys", &self.keys.len())
            .field("lines", &self.lines)
            .finish()
    }
}

impl NormalizedKeys {
    /// The keys, in round-robin order.
    pub fn keys(&self) -> &[Arc<SecretKey>] {
        &self.keys
    }

    /// The physical line of each key, one entry per key in the same order.
    pub fn lines(&self) -> &[usize] {
        &self.lines
    }

    /// Number of distinct keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

/// A key list that cannot be used, reported by physical line. The message is
/// fixed text and never contains any part of a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyListError {
    line: usize,
    message: &'static str,
}

impl KeyListError {
    fn new(line: usize, message: &'static str) -> Self {
        Self { line, message }
    }

    /// The 1-based physical line the offending key was found on.
    pub fn line(&self) -> usize {
        self.line
    }

    /// Fixed message, safe to show in the settings window.
    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for KeyListError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "строка {}: {}", self.line, self.message)
    }
}

impl std::error::Error for KeyListError {}

/// Splits a key list on `/\r\n|\n|\r/`: CRLF counts as one break, so the 1-based
/// line number in a diagnostic points at the line a text editor shows.
fn split_key_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\n' || byte == b'\r' {
            lines.push(&text[start..index]);
            if byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
            start = index + 1;
        }
        index += 1;
    }
    lines.push(&text[start..]);
    lines
}

/// Splits a key list (one key per line, CRLF/LF/CR) into unique keys in
/// first-occurrence order, keeping the 1-based physical line of each first
/// occurrence. Blank lines are skipped; a key holding interior whitespace or a
/// NUL byte is refused with its line number. No key value is echoed, logged or
/// returned anywhere but in the resulting list.
pub fn normalize_key_text(text: &str) -> Result<NormalizedKeys, KeyListError> {
    let mut keys: Vec<Arc<SecretKey>> = Vec::new();
    let mut lines: Vec<usize> = Vec::new();
    for (index, line) in split_key_lines(text).into_iter().enumerate() {
        let key = ts_trim(line);
        if key.is_empty() {
            continue;
        }
        if key.chars().any(is_ts_whitespace) {
            return Err(KeyListError::new(index + 1, KEY_WHITESPACE_MESSAGE));
        }
        if key.contains('\0') {
            return Err(KeyListError::new(index + 1, KEY_NUL_MESSAGE));
        }
        if !keys.iter().any(|existing| existing.as_str() == key) {
            keys.push(Arc::new(SecretKey::new(key.to_owned())));
            lines.push(index + 1);
        }
    }
    Ok(NormalizedKeys { keys, lines })
}

/// The round-robin key ring shared by the native backend. `next_key` hands out
/// `Arc` clones so one request keeps its key alive without copying the string;
/// the cursor is atomic, and a ring is shared as `Arc<KeyRing>`.
pub struct KeyRing {
    keys: Vec<Arc<SecretKey>>,
    cursor: AtomicUsize,
}

impl fmt::Debug for KeyRing {
    /// The size and the rotation position are safe to log; the keys are not,
    /// so they are left out even though a ring is otherwise a plain container.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyRing")
            .field("keys", &self.keys.len())
            .field("cursor", &self.cursor.load(Ordering::Relaxed))
            .finish()
    }
}

impl KeyRing {
    /// Wraps an already-normalized list. `normalize_key_text` and
    /// [`load_secret_keys`] are the sources of such a list; `encode_secret_keys`
    /// re-checks the rules before anything reaches the disk.
    pub(crate) fn new(keys: Vec<Arc<SecretKey>>) -> Self {
        Self {
            keys,
            cursor: AtomicUsize::new(0),
        }
    }

    /// A ring with no keys: the state to run with while the user has saved
    /// none, so the launcher reports a missing key instead of failing to start.
    pub const fn empty() -> Self {
        Self {
            keys: Vec::new(),
            cursor: AtomicUsize::new(0),
        }
    }

    /// Every key, in rotation order.
    pub fn keys(&self) -> &[Arc<SecretKey>] {
        &self.keys
    }

    /// Whether the ring holds no key.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The next key in rotation, or `None` when the ring is empty. A request
    /// that received a key keeps it even if the ring is later replaced.
    pub fn next_key(&self) -> Option<Arc<SecretKey>> {
        if self.keys.is_empty() {
            return None;
        }
        let index = self.cursor.fetch_add(1, Ordering::Relaxed) % self.keys.len();
        Some(Arc::clone(&self.keys[index]))
    }
}

/// Whether two key lists hold the same values in the same order. A Save that
/// rewrote an identical list uses this to keep the existing ring and its
/// cursor instead of restarting the rotation.
pub fn list_equal(left: &[Arc<SecretKey>], right: &[Arc<SecretKey>]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(left, right)| left.as_str() == right.as_str())
}

/* -------------------------------------------------------------------------- */
/* Store errors                                                               */
/* -------------------------------------------------------------------------- */

/// Why a secret store could not be used. `Missing` is the normal first-run
/// state, `Unavailable` means the file exists but this process cannot use it,
/// `Corrupt` means the container is not one this module wrote (foreign header,
/// truncation, failed decryption) and `Invalid` means a well-formed container
/// whose key list breaks the format rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretStoreKind {
    Missing,
    Unavailable,
    Corrupt,
    Invalid,
}

/// A secret-store failure whose `Display` text names the path and the problem,
/// never a key value or any byte read from the file.
#[derive(Clone, Debug)]
pub struct SecretStoreError {
    kind: SecretStoreKind,
    text: String,
}

impl SecretStoreError {
    fn new(kind: SecretStoreKind, text: String) -> Self {
        Self { kind, text }
    }

    /// Whether no store exists yet, which is not a failure to report as one.
    pub fn is_missing(&self) -> bool {
        self.kind == SecretStoreKind::Missing
    }

    /// The store has never been written.
    fn missing(path: &Path) -> Self {
        Self::new(
            SecretStoreKind::Missing,
            format!("{}: файл ключей ещё не создан.", path.display()),
        )
    }

    /// The store exists but cannot be read or used by this process.
    fn unavailable(path: &Path, reason: &str) -> Self {
        Self::new(
            SecretStoreKind::Unavailable,
            format!("{}: {reason}.", path.display()),
        )
    }

    /// The store is not a valid Speechek container.
    fn corrupt(path: &Path, reason: &str) -> Self {
        Self::new(
            SecretStoreKind::Corrupt,
            format!("{}: {reason}.", path.display()),
        )
    }

    /// The container decrypted, but its key list breaks the format rules.
    fn invalid(path: &Path, line: usize, reason: &str) -> Self {
        Self::new(
            SecretStoreKind::Invalid,
            format!("{}: строка {line}: {reason}.", path.display()),
        )
    }

    /// The same list failure for a caller that supplied the list in memory and
    /// has no file to name.
    fn invalid_at(line: usize, reason: &str) -> Self {
        Self::new(
            SecretStoreKind::Invalid,
            format!("строка {line}: {reason}."),
        )
    }

    fn from_io(path: &Path, error: &io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::missing(path)
        } else {
            Self::unavailable(path, "файл ключей недоступен для чтения")
        }
    }
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl std::error::Error for SecretStoreError {}

/* -------------------------------------------------------------------------- */
/* Store files                                                                */
/* -------------------------------------------------------------------------- */

/// The container beside the resolved settings file: Production and Test use
/// their own profile locations, and Development uses its executable directory.
/// The caller supplies the path; this function never chooses a profile.
pub fn secrets_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name(SECRETS_FILE_NAME)
}

/// Reads the container exactly as it is on disk. A caller about to replace the
/// file keeps these bytes as its undo record; they are ciphertext, so they
/// need no `Zeroizing` buffer. A file that does not exist is `Missing`;
/// anything else that prevents the read is `Unavailable`.
pub fn read_secret_container(path: &Path) -> Result<Vec<u8>, SecretStoreError> {
    if path.is_dir() {
        return Err(SecretStoreError::unavailable(
            path,
            "это каталог, а не файл",
        ));
    }
    fs::read(path).map_err(|error| SecretStoreError::from_io(path, &error))
}

/// Loads and decrypts the key store for the current Windows user and wraps it
/// in a fresh ring with its cursor at zero. A missing file is `Missing`, a
/// foreign or truncated or undecryptable container is `Corrupt`, and a valid
/// container holding an empty list yields an empty ring - which is a real
/// state, because the user cleared the list on purpose.
pub fn load_secret_keys(path: &Path) -> Result<Arc<KeyRing>, SecretStoreError> {
    let container = read_secret_container(path)?;
    Ok(Arc::new(KeyRing::new(decode_container(path, &container)?)))
}

/// Encrypts a key list into a complete `SPK1` container for the current
/// Windows user. The list is re-validated so a caller cannot write a container
/// that `load_secret_keys` would refuse; the plaintext JSON buffer is zeroed as
/// soon as DPAPI has consumed it.
pub fn encode_secret_keys(keys: &[Arc<SecretKey>]) -> Result<Vec<u8>, SecretStoreError> {
    for (index, key) in keys.iter().enumerate() {
        let line = index + 1;
        let value = key.as_str();
        if value.is_empty() {
            return Err(SecretStoreError::invalid_at(line, KEY_EMPTY_MESSAGE));
        }
        if value.chars().any(is_ts_whitespace) {
            return Err(SecretStoreError::invalid_at(line, KEY_WHITESPACE_MESSAGE));
        }
        if value.contains('\0') {
            return Err(SecretStoreError::invalid_at(line, KEY_NUL_MESSAGE));
        }
        if keys[..index]
            .iter()
            .any(|existing| existing.as_str() == value)
        {
            return Err(SecretStoreError::invalid_at(line, KEY_DUPLICATE_MESSAGE));
        }
    }

    // Worst case every key byte becomes a six-byte `\u00xx` escape, so the
    // buffer is sized once and never grows: a reallocation would leave a
    // plaintext copy in freed memory that `Zeroizing` cannot reach.
    let capacity = keys
        .iter()
        .fold(32usize, |total, key| total + key.as_str().len() * 6 + 3);
    let mut json = Zeroizing::new(String::with_capacity(capacity));
    let _ = write!(json, "{{\"version\":{CONTAINER_VERSION},\"keys\":[");
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            json.push(',');
        }
        push_json_string(&mut json, key.as_str());
    }
    json.push_str("]}");

    let blob = protect(json.as_bytes())?;
    let mut container = Vec::with_capacity(CONTAINER_MAGIC.len() + blob.len());
    container.extend_from_slice(CONTAINER_MAGIC);
    container.extend_from_slice(&blob);
    Ok(container)
}

/* -------------------------------------------------------------------------- */
/* Container payload                                                          */
/* -------------------------------------------------------------------------- */

/// Splits the header from the blob, decrypts it, repairs the escaped strings
/// and deserializes the document through the typed visitor below. A foreign
/// header, a truncated blob, a payload that is not UTF-8 text or a document that
/// breaks the format is damage, never an empty list.
fn decode_container(
    path: &Path,
    container: &[u8],
) -> Result<Vec<Arc<SecretKey>>, SecretStoreError> {
    if container.len() < CONTAINER_MAGIC.len()
        || &container[..CONTAINER_MAGIC.len()] != CONTAINER_MAGIC
    {
        return Err(SecretStoreError::corrupt(
            path,
            "неизвестный формат контейнера: ожидался заголовок SPK1",
        ));
    }
    let plaintext = unprotect(path, &container[CONTAINER_MAGIC.len()..])?;
    let text = str::from_utf8(&plaintext).map_err(|_| {
        SecretStoreError::corrupt(path, "расшифрованное содержимое не является текстом UTF-8")
    })?;

    // The repaired document - a copy of the plaintext, cleared on drop - is
    // what `serde_json` reads: no string it receives carries an escape, so its
    // own never-cleared scratch buffer never holds a key.
    let (repaired, repaired_values) = repair_escaped_strings(path, text)?;
    let mut deserializer = serde_json::Deserializer::from_str(&repaired);
    let document = (&mut deserializer)
        .deserialize_map(DocumentVisitor {
            table: &repaired_values,
        })
        .map_err(|error| SecretStoreError::corrupt(path, describe_json_failure(&error)))?;
    deserializer
        .end()
        .map_err(|_| SecretStoreError::corrupt(path, "после документа есть лишние байты"))?;
    materialize_keys(path, repaired_values, document)
}

/// Applies the key-list rules to the resolved strings and moves each one into a
/// [`SecretKey`]: unique keys in document order, with a bad entry refused by its
/// 1-based position. No value is echoed, logged or returned.
fn materialize_keys(
    path: &Path,
    repaired_values: Vec<Zeroizing<String>>,
    values: Vec<DocumentString<'_>>,
) -> Result<Vec<Arc<SecretKey>>, SecretStoreError> {
    // Each repaired string is used by exactly one reference; the slot is left
    // empty afterwards so the same string can never be handed out twice.
    let mut repaired_values: Vec<Option<Zeroizing<String>>> =
        repaired_values.into_iter().map(Some).collect();
    let mut keys: Vec<Arc<SecretKey>> = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        let line = index + 1;
        let mut key = match document_token(&value.text) {
            DocumentToken::Literal(text) => Zeroizing::new(text.to_owned()),
            DocumentToken::Repaired(reference) => {
                match repaired_values.get_mut(reference.wrapping_sub(1)) {
                    Some(slot) => slot.take().ok_or_else(|| {
                        SecretStoreError::corrupt(path, "ссылка на строку контейнера повторяется")
                    })?,
                    None => {
                        return Err(SecretStoreError::corrupt(
                            path,
                            "ссылка на строку вне документа",
                        ))
                    }
                }
            }
        };
        let text = key.as_str();
        if text.is_empty() {
            return Err(SecretStoreError::invalid(path, line, KEY_EMPTY_MESSAGE));
        }
        if text.chars().any(is_ts_whitespace) {
            return Err(SecretStoreError::invalid(
                path,
                line,
                KEY_WHITESPACE_MESSAGE,
            ));
        }
        if text.contains('\0') {
            return Err(SecretStoreError::invalid(path, line, KEY_NUL_MESSAGE));
        }
        if keys.iter().any(|existing| existing.as_str() == text) {
            return Err(SecretStoreError::invalid(path, line, KEY_DUPLICATE_MESSAGE));
        }
        // `mem::take` moves the value out, leaving a buffer that owns nothing.
        keys.push(Arc::new(SecretKey::new(std::mem::take(&mut *key))));
    }
    Ok(keys)
}

/// Maps a `serde_json` failure to fixed text. serde's own message can quote the
/// value it refused - including a key - so only the failure class is used.
fn describe_json_failure(error: &serde_json::Error) -> &'static str {
    match error.classify() {
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
            "контейнер повреждён"
        }
        serde_json::error::Category::Data => "содержимое контейнера не соответствует формату",
        serde_json::error::Category::Io => "контейнер не удалось прочитать",
    }
}

/* -------------------------------------------------------------------------- */
/* Repaired document strings                                                  */
/* -------------------------------------------------------------------------- */

/// Rewrites the document so `serde_json` never has to unescape a string: the
/// deserializer keeps the strings it does decode in a scratch buffer of its own
/// that it never clears, so every string holding an escape sequence is decoded
/// here into a [`Zeroizing`] buffer and replaced by a reference token. What the
/// deserializer then sees is escape-free text - and a string that still carries
/// an escape is refused by the visitor rather than accepted through that buffer.
///
/// Three shapes keep a reference unambiguous: a decoded string becomes `{n}`, a
/// literal string that looks like a reference or already starts with `~` gains
/// one leading `~`, and everything else is copied byte for byte.
fn repair_escaped_strings(
    path: &Path,
    text: &str,
) -> Result<(Zeroizing<String>, Vec<Zeroizing<String>>), SecretStoreError> {
    let bytes = text.as_bytes();
    // Every escaped string grows by at most its reference token minus its
    // (never shorter than two byte) body plus the digits of the reference, and
    // those strings cost at least four bytes each, so five times the input plus
    // a fixed headroom is a proven upper bound: the buffer is allocated once and
    // never reallocates, because a reallocation would leave a plaintext copy
    // behind in freed memory that `Zeroizing` cannot reach.
    let mut repaired = Zeroizing::new(String::with_capacity(
        text.len().saturating_mul(5).saturating_add(64),
    ));
    let mut repaired_values: Vec<Zeroizing<String>> = Vec::new();
    let mut cursor = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let end = string_end(path, text, index + 1)?;
        let body = &text[index + 1..end];
        repaired.push_str(&text[cursor..index]);
        repaired.push('"');
        if body.contains('\\') {
            let decoded = decode_json_string(path, body)?;
            if decoded.contains('\0') {
                return Err(SecretStoreError::corrupt(
                    path,
                    "строка контейнера содержит нулевой байт",
                ));
            }
            repaired_values.push(decoded);
            let _ = write!(repaired, "{{{}}}", repaired_values.len());
        } else {
            if body.starts_with('~') || is_placeholder_shape(body) {
                repaired.push('~');
            }
            repaired.push_str(body);
        }
        repaired.push('"');
        index = end + 1;
        cursor = index;
    }
    repaired.push_str(&text[cursor..]);
    Ok((repaired, repaired_values))
}

/// The index of the quote closing the string that starts at `start`, honouring
/// escape sequences so an escaped quote does not end it early.
fn string_end(path: &Path, text: &str, start: usize) -> Result<usize, SecretStoreError> {
    let bytes = text.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Ok(index),
            _ => index += 1,
        }
    }
    Err(SecretStoreError::corrupt(
        path,
        "строка контейнера не закрыта",
    ))
}

/// Decodes one JSON string body into a buffer that is cleared on every path,
/// including a failure. The result can never be longer than the body, so the
/// buffer is allocated once and never grows.
fn decode_json_string(path: &Path, body: &str) -> Result<Zeroizing<String>, SecretStoreError> {
    const ESCAPE: &str = "escape-последовательность в строке контейнера не распознана";
    const CONTROL: &str = "строка контейнера содержит служебный символ";
    const SURROGATE: &str = "строка контейнера содержит незавершённую суррогатную пару";

    let mut decoded = Zeroizing::new(String::with_capacity(body.len()));
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                let escape = characters
                    .next()
                    .ok_or_else(|| SecretStoreError::corrupt(path, ESCAPE))?;
                match escape {
                    '"' => decoded.push('"'),
                    '\\' => decoded.push('\\'),
                    '/' => decoded.push('/'),
                    'b' => decoded.push('\u{8}'),
                    'f' => decoded.push('\u{c}'),
                    'n' => decoded.push('\n'),
                    'r' => decoded.push('\r'),
                    't' => decoded.push('\t'),
                    'u' => {
                        let unit = read_hex4(path, &mut characters)?;
                        let character = if (0xd800..=0xdbff).contains(&unit) {
                            if characters.next() != Some('\\') || characters.next() != Some('u') {
                                return Err(SecretStoreError::corrupt(path, SURROGATE));
                            }
                            let low = read_hex4(path, &mut characters)?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err(SecretStoreError::corrupt(path, SURROGATE));
                            }
                            let combined =
                                0x1_0000 + ((unit as u32 - 0xd800) << 10) + (low as u32 - 0xdc00);
                            char::from_u32(combined)
                                .ok_or_else(|| SecretStoreError::corrupt(path, SURROGATE))?
                        } else if (0xdc00..=0xdfff).contains(&unit) {
                            return Err(SecretStoreError::corrupt(path, SURROGATE));
                        } else {
                            char::from_u32(unit as u32)
                                .ok_or_else(|| SecretStoreError::corrupt(path, SURROGATE))?
                        };
                        decoded.push(character);
                    }
                    _ => return Err(SecretStoreError::corrupt(path, ESCAPE)),
                }
            }
            character if (character as u32) < 0x20 => {
                return Err(SecretStoreError::corrupt(path, CONTROL));
            }
            character => decoded.push(character),
        }
    }
    Ok(decoded)
}

/// Four hexadecimal digits of one `\u` escape.
fn read_hex4(path: &Path, characters: &mut str::Chars<'_>) -> Result<u16, SecretStoreError> {
    let mut value: u16 = 0;
    for _ in 0..4 {
        let digit = characters
            .next()
            .and_then(|character| character.to_digit(16))
            .ok_or_else(|| {
                SecretStoreError::corrupt(
                    path,
                    "escape-последовательность в строке контейнера не распознана",
                )
            })?;
        value = (value << 4) | digit as u16;
    }
    Ok(value)
}

/// Whether a literal string has the shape the repair pass uses for a reference:
/// a brace, at least one digit, and digits only.
fn is_placeholder_shape(text: &str) -> bool {
    let Some(digits) = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return false;
    };
    let mut bytes = digits.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_digit())
        && bytes.all(|byte| byte.is_ascii_digit())
}

/// What one token of the repaired document means: a string the repair pass
/// decoded, or the literal text the document held.
enum DocumentToken<'a> {
    Repaired(usize),
    Literal(&'a str),
}

/// Reads one token of the repaired document. A leading `~` marks literal text,
/// a valid `{n}` is a reference to repaired string `n`, and anything else is the
/// literal text itself.
fn document_token(text: &str) -> DocumentToken<'_> {
    if let Some(literal) = text.strip_prefix('~') {
        return DocumentToken::Literal(literal);
    }
    if is_placeholder_shape(text) {
        let digits = &text[1..text.len() - 1];
        if let Ok(reference) = digits.parse::<usize>() {
            if reference > 0 {
                return DocumentToken::Repaired(reference);
            }
        }
    }
    DocumentToken::Literal(text)
}

/* -------------------------------------------------------------------------- */
/* Typed document                                                              */
/* -------------------------------------------------------------------------- */

/// One string taken from the repaired document: either the literal text the
/// document held, or a reference to a string the repair pass decoded.
struct DocumentString<'de> {
    text: Cow<'de, str>,
}

impl<'de> Deserialize<'de> for DocumentString<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StringVisitor;

        impl<'de> Visitor<'de> for StringVisitor {
            type Value = DocumentString<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("строка")
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<DocumentString<'de>, E>
            where
                E: de::Error,
            {
                Ok(DocumentString {
                    text: Cow::Borrowed(value),
                })
            }

            /// The repaired document holds no escape sequences, so serde
            /// reaching for its scratch buffer means a string still carried
            /// one: that is refused instead of being copied there.
            fn visit_str<E>(self, _value: &str) -> Result<DocumentString<'de>, E>
            where
                E: de::Error,
            {
                Err(de::Error::custom("escaped container string"))
            }
        }

        deserializer.deserialize_str(StringVisitor)
    }
}

/// Reads the document `{"version":1,"keys":[...]}` from the repaired text. The
/// version must be the integer this module writes, no member may repeat or be
/// unknown, and the key strings come back as [`DocumentString`] tokens that
/// [`materialize_keys`] resolves against the repaired values.
struct DocumentVisitor<'a> {
    table: &'a [Zeroizing<String>],
}

impl<'de> Visitor<'de> for DocumentVisitor<'_> {
    type Value = Vec<DocumentString<'de>>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("объект с полями version и keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut version_seen = false;
        let mut keys: Option<Vec<DocumentString<'de>>> = None;
        while let Some(field) = map.next_key::<DocumentString<'de>>()? {
            let name: &str = match document_token(&field.text) {
                DocumentToken::Literal(text) => text,
                DocumentToken::Repaired(reference) => self
                    .table
                    .get(reference.wrapping_sub(1))
                    .map(|value| value.as_str())
                    .ok_or_else(|| de::Error::custom("string reference out of range"))?,
            };
            match name {
                "version" => {
                    if version_seen {
                        return Err(de::Error::duplicate_field("version"));
                    }
                    // Only the exact integer this module writes: `1.0`, `1e0`
                    // and `"1"` are refused by the `u64` visitor.
                    if map.next_value::<u64>()? != CONTAINER_VERSION {
                        return Err(de::Error::custom("unsupported container version"));
                    }
                    version_seen = true;
                }
                "keys" => {
                    if keys.is_some() {
                        return Err(de::Error::duplicate_field("keys"));
                    }
                    keys = Some(map.next_value::<Vec<DocumentString<'de>>>()?);
                }
                _ => return Err(de::Error::custom("unexpected container field")),
            }
        }
        if !version_seen {
            return Err(de::Error::missing_field("version"));
        }
        keys.ok_or_else(|| de::Error::missing_field("keys"))
    }
}

/// Writes one JSON string, escaping exactly what the format requires; a key
/// never travels through a serializer that could hold onto it.
fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

/* -------------------------------------------------------------------------- */
/* DPAPI                                                                      */
/* -------------------------------------------------------------------------- */

/// Owns a DPAPI output blob: the memory is cleared with [`Zeroize`] and then
/// released with `LocalFree`, on every path including a later error.
struct OutputBlob(CRYPT_INTEGER_BLOB);

impl OutputBlob {
    fn as_slice(&self) -> &[u8] {
        if self.0.pbData.is_null() || self.0.cbData == 0 {
            return &[];
        }
        // SAFETY: DPAPI reports the length of its own allocation in `cbData`,
        // and `self` owns that allocation until it is freed in `Drop`.
        unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }
    }
}

impl Drop for OutputBlob {
    fn drop(&mut self) {
        if self.0.pbData.is_null() {
            return;
        }
        // SAFETY: the buffer and its reported length are the ones DPAPI
        // allocated for this value, still alive and untouched since then, so
        // clearing it and returning it to `LocalFree` happens exactly once.
        unsafe {
            std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
            let _ = LocalFree(Some(HLOCAL(self.0.pbData as *mut std::ffi::c_void)));
        }
    }
}

/// Encrypts for the current Windows user only: no entropy, no password, no
/// machine scope, and no UI prompt even when the key material has to be
/// re-generated.
fn protect(plaintext: &[u8]) -> Result<Vec<u8>, SecretStoreError> {
    let length = u32::try_from(plaintext.len()).map_err(|_| {
        SecretStoreError::new(
            SecretStoreKind::Invalid,
            "список ключей слишком велик для контейнера.".to_owned(),
        )
    })?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: length,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` borrows the plaintext for the duration of the call, the
    // description, entropy, reserved and prompt arguments are absent, and
    // `output` is a live local DPAPI fills in with a `LocalFree` allocation
    // that `OutputBlob` takes over immediately afterwards.
    unsafe {
        CryptProtectData(
            &input,
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| {
            SecretStoreError::new(
                SecretStoreKind::Unavailable,
                "не удалось зашифровать список ключей для текущего пользователя Windows."
                    .to_owned(),
            )
        })?;
    }
    let output = OutputBlob(output);
    Ok(output.as_slice().to_vec())
}

/// Decrypts a blob written for the current Windows user. The DPAPI buffer is
/// cleared before it is released, and the plaintext is copied into a
/// [`Zeroizing`] buffer so it is cleared again when the caller is done with it.
fn unprotect(path: &Path, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretStoreError> {
    if ciphertext.is_empty() {
        return Err(SecretStoreError::corrupt(path, "контейнер пуст"));
    }
    let length = u32::try_from(ciphertext.len())
        .map_err(|_| SecretStoreError::corrupt(path, "контейнер повреждён"))?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: length,
        pbData: ciphertext.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` borrows the ciphertext read from the container for the
    // duration of the call, the description, entropy, reserved and prompt
    // arguments are absent, and `output` is a live local DPAPI fills in with a
    // `LocalFree` allocation that `OutputBlob` takes over immediately after.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| {
            SecretStoreError::corrupt(
                path,
                "контейнер не расшифрован для текущего пользователя Windows или повреждён",
            )
        })?;
    }
    let output = OutputBlob(output);
    Ok(Zeroizing::new(output.as_slice().to_vec()))
}
