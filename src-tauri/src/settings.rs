//! Configuration storage for the Speechek shell: the editable settings
//! document, the launcher hotkey grammar and the atomic writer that both the
//! settings file and the DPAPI container (`secrets.rs`) are published through.
//!
//! `settings.json` belongs to the user. Whole-line `//` comments are allowed,
//! unknown properties and their formatting survive a value change byte for
//! byte, and a replacement is written as a same-directory temporary file that
//! is moved over the original, so a reader never sees a partial document. The
//! API keys live in `secrets.bin` beside this file; the runtime never reads
//! paths to plaintext key files from settings. The obsolete root properties
//! `compare_all` and `port` are ignored on read and removed on the next save;
//! `port` takes the comment block the shipped template wrote with it.
//!
//! No diagnostic ever echoes a value read from a configuration file - only
//! paths, line numbers, field names and property names.
//!
//! Storage depends on the build flavor (`crate::profile`): the distributed
//! shell always uses `%APPDATA%\Speechek\settings.json`, the test flavor
//! accepts an absolute `SPEECHEK_CONFIG_PATH` and otherwise uses
//! `%APPDATA%\Speechek-Test\settings.json`, and the development flavor keeps
//! `settings.json` beside the executable it runs from, so a portable build
//! carries its own profile. A release build reads no path from the
//! environment. The settings window saves through [`patch_settings_document`]
//! and [`atomic_replace`].

use crate::dictation_language;
use crate::i18n::{self, Language, MessageId, UiMessage};
use crate::profile::{self, BuildFlavor};
use crate::secrets::{KeyRing, SecretKey};
use parking_lot::{Mutex, RwLock};
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::ops::Range;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::OwnedHandle;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::{Path, PathBuf};
use std::process;
use std::ptr;
use std::str;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows::Win32::Security::{
    AddAccessAllowedAce, CreateWellKnownSid, EqualSid, GetAce, GetLengthSid,
    GetSecurityDescriptorControl, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
    IsValidSid, SetSecurityDescriptorControl, SetSecurityDescriptorDacl, TokenUser,
    WinLocalSystemSid, ACE_HEADER, ACL, ACL_REVISION, DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, MoveFileExW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    OPEN_EXISTING,
};

/// The Windows pieces only the private archive uses: the directory creation
/// that applies its own descriptor, and the attribute call that tells a
/// directory from a file and a link from a plain entry. Gated with their only
/// callers, so a build without `dev-diagnostics` has no unused import.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The settings file every flavor resolves: inside the flavor's `%APPDATA%`
/// directory for production and test, and beside the executable for the
/// development flavor.
const SETTINGS_FILE_NAME: &str = "settings.json";

/// Annotated first-run template, embedded at build time so the executable needs
/// no file beside it.
const SETTINGS_EXAMPLE: &str = include_str!("../../config/settings.example.json");

/// The six comment lines the shipped template wrote in front of the `port`
/// property it also shipped, without their line endings: a document saved with
/// CRLF matches them, and the migration removes the document's own bytes. The
/// lines are the released template's own, not a translation of them, because
/// only an unchanged block is one this migration may take out of a user's file.
const OBSOLETE_PORT_COMMENTS: [&str; 6] = [
    "  // port: порт внутреннего сервера Speechek на 127.0.0.1, целое число от 1",
    "  // до 65535. Значение ниже — production-умолчание 4173; debug-профили",
    "  // используют 4174 (Development) и 4175 (Test). Новое значение применяется",
    "  // после перезапуска приложения. Если при запуске порт занят, Speechek",
    "  // временно откроется на свободном порту и покажет это в окне настроек, не",
    "  // перезаписывая сохранённое значение.",
];

/// The byte order mark a UTF-8 document may start with. [`decode_utf8_bytes`]
/// drops it from the content it hands back; [`read_settings_text`] and
/// [`patch_settings_document`] keep it, so a document that starts with one is
/// written back exactly as the user saved it.
pub(crate) const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";

/// The ACE type marking an access-allowed entry; `ACCESS_ALLOWED_ACE_TYPE` in
/// the Windows headers, and the constant is not exported by the `windows`
/// modules this build compiles.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// `FILE_ALL_ACCESS` as the raw mask an ACE carries: the same access the
/// documented `FILE_ALL_ACCESS` right stands for.
const ACCESS_FULL: u32 = 0x001F_01FF;

/// Access rights the writer itself asks for. The typed constants
/// (`GENERIC_WRITE`, `READ_CONTROL`) live behind helper functions this build
/// does not compile, so their documented values are used directly.
const ACCESS_GENERIC_WRITE: u32 = 0x4000_0000;
const ACCESS_READ_CONTROL: u32 = 0x0002_0000;

/// The two rights the archive's own checks add to [`ACCESS_READ_CONTROL`]:
/// `GENERIC_READ`, so the handle a manifest is created through can be read and
/// patched as well as written, and `FILE_READ_ATTRIBUTES`, which is what the
/// call that recognizes a link needs. Both are the documented values of rights
/// whose typed constants this build does not compile, and both are gated with
/// their only callers.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
const ACCESS_GENERIC_READ: u32 = 0x8000_0000;
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
const ACCESS_READ_ATTRIBUTES: u32 = 0x0000_0080;

/// Windows error codes translated into this module's own wording; the system's
/// localized text is never quoted, because it is not a settings value but is
/// not ours to show either.
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_SHARING_VIOLATION: u32 = 32;
const ERROR_LOCK_VIOLATION: u32 = 33;
const ERROR_FILE_EXISTS: u32 = 80;
const ERROR_DISK_FULL: u32 = 112;
const ERROR_ALREADY_EXISTS: u32 = 183;

/// Size of [`SidBuffer`] in machine words: enough for the widest SID Windows
/// allows (68 bytes) and for the `TOKEN_USER` record a SID is read out of.
const SID_BUFFER_WORDS: usize = 32;

/// Modifiers the Tauri global-shortcut syntax accepts. Anything else is refused
/// by [`validate_hotkey`], so a hotkey that could never be registered does not
/// reach the launcher.
const HOTKEY_MODIFIERS: &[&str] = &[
    "alt",
    "cmd",
    "cmdorctrl",
    "command",
    "commandorcontrol",
    "control",
    "ctrl",
    "meta",
    "shift",
    "super",
];

/// Named keys that translate into a Tauri global shortcut.
const HOTKEY_NAMED_KEYS: &[&str] = &[
    "arrowdown",
    "arrowleft",
    "arrowright",
    "arrowup",
    "backspace",
    "delete",
    "end",
    "enter",
    "escape",
    "home",
    "insert",
    "pagedown",
    "pageup",
    "space",
    "tab",
];

/// Launcher settings. `/api/settings` publishes `hotkey` and `mode` and nothing
/// else; the settings window reads this same struct, `mute_during_recording`,
/// `input_device`, `language`, `custom_vocabulary` and `dictation_languages`
/// included.
///
/// `mute_during_recording`, `input_device`, `language`, `custom_vocabulary` and
/// `dictation_languages` are the managed values a document may omit: a file
/// written before each existed has no such property, and it reads as the
/// documented default there - `false`, the system default input device, the
/// language this profile would otherwise start in, an empty vocabulary and no
/// language hints (Auto-detect) - not as a guess about a value the user chose.
/// The patch writes the properties into the document on the next change, so the
/// values a page chose survive the save rather than being read back as missing
/// and refused.
///
/// `input_device` is a `cpal::DeviceId` in its `Display`/`FromStr` form, or
/// `None` for the system default input device; the device is looked up when a
/// recording opens, so a stored id whose device is not connected keeps its
/// stored value and is warned about instead of being silently replaced.
///
/// The loopback port is not part of this struct: Windows assigns a free one at
/// every start, so there is nothing to store and nothing to configure.
/// `compare_all` and `port` are no longer managed values either: a document may
/// still carry either root property from when it was managed, and both are
/// ignored when the file is read and removed by the next
/// [`patch_settings_document`] - `port` together with the comment block the
/// shipped template wrote in front of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub hotkey: String,
    pub mode: String,
    pub mute_during_recording: bool,
    /// The selected input device id, or `None` for the system default device.
    pub input_device: Option<String>,
    /// The interface language, in its lowercase wire spelling. A document that
    /// does not carry the property reads as the language this profile would
    /// start in.
    #[serde(default = "crate::i18n::detect_default_language")]
    pub language: Language,
    /// Words and phrases recognition is biased toward, in the order the user
    /// listed them: trimmed, non-empty, without line breaks, without exact
    /// repeats, at most [`MAX_CUSTOM_VOCABULARY`]. Empty means no vocabulary.
    #[serde(default)]
    pub custom_vocabulary: Arc<[String]>,
    /// Dictation-language hints for Gemini, as BCP-47 codes from
    /// [`crate::dictation_language::DICTATION_LANGUAGES`] in the order the user chose
    /// them: at most two, no repeats. Empty means automatic detection.
    #[serde(default)]
    pub dictation_languages: Arc<[String]>,
}
/// One coherent configuration revision. Existing dictations retain their Arc
/// while Save publishes a replacement for subsequent requests.
pub struct RuntimeSnapshot {
    pub settings: Settings,
    pub keys: Arc<KeyRing>,
    pub revision: u64,
}

pub struct SharedRuntime {
    current: RwLock<Arc<RuntimeSnapshot>>,
    pinned: Mutex<Option<(u64, Arc<RuntimeSnapshot>)>>,
}

impl SharedRuntime {
    pub fn new(initial: Arc<RuntimeSnapshot>) -> Self {
        Self {
            current: RwLock::new(initial),
            pinned: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        Arc::clone(&self.current.read())
    }

    pub fn publish(&self, next: Arc<RuntimeSnapshot>) {
        *self.current.write() = next;
    }

    pub fn pin(&self, generation: u64, snapshot: Arc<RuntimeSnapshot>) {
        *self.pinned.lock() = Some((generation, snapshot));
    }

    pub fn retire(&self, generation: u64) {
        let mut pinned = self.pinned.lock();
        if pinned.as_ref().is_some_and(|(id, _)| *id == generation) {
            *pinned = None;
        }
    }

    /// Untagged laboratory requests observe current settings; a tagged
    /// dictation never silently falls forward to a newer revision.
    pub fn resolve(&self, generation: Option<u64>) -> Option<Arc<RuntimeSnapshot>> {
        match generation {
            None => Some(self.snapshot()),
            Some(id) => self
                .pinned
                .lock()
                .as_ref()
                .filter(|(pinned, _)| *pinned == id)
                .map(|(_, snapshot)| Arc::clone(snapshot)),
        }
    }

    /// Admission and rotation are serialized with retirement. Cancellation
    /// before this point consumes no turn of the pinned key ring.
    pub fn pinned_key(&self, generation: u64) -> Option<Option<Arc<SecretKey>>> {
        let pinned = self.pinned.lock();
        pinned
            .as_ref()
            .filter(|(id, _)| *id == generation)
            .map(|(_, snapshot)| snapshot.keys.next_key())
    }
}

/// A settings or storage failure whose `Display` text names the path and the
/// problem, never a value read from a configuration file. The statement itself
/// is semantic: the running shell renders it in the current language, and the
/// English diagnostic is rendered from the same descriptor.
#[derive(Clone, Debug)]
pub struct ConfigError {
    /// The reason and its raw arguments - paths, property names, line numbers,
    /// environment variable names - never a value read from a file. The catalog
    /// holds every word.
    pub ui: UiMessage,
    /// Set only when the destination file had already been replaced by the time
    /// this failure was reported: the previous bytes are gone then, so a caller
    /// that keeps a backup has to restore it.
    replaced: bool,
}

impl ConfigError {
    pub(crate) fn new(ui: UiMessage) -> Self {
        Self {
            ui,
            replaced: false,
        }
    }

    /// Whether the file this failure is about was already replaced. Only
    /// [`atomic_replace`] ever sets it, and only after the replacement move
    /// itself succeeded, so a caller can tell "nothing was written" from "the
    /// new bytes are in place but something about them is wrong".
    pub(crate) fn replaced(&self) -> bool {
        self.replaced
    }

    /// The same failure, marked as one reported after the destination had been
    /// replaced.
    fn after_replace(self) -> Self {
        Self {
            ui: self.ui,
            replaced: true,
        }
    }
}

impl fmt::Display for ConfigError {
    /// The English diagnostic, rendered from the descriptor so the catalog
    /// stays the only copy of the wording.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&i18n::render(Language::En, &self.ui))
    }
}

impl Error for ConfigError {}

/// A path as a diagnostic argument: raw text, substituted as-is.
fn path_arg(path: &Path) -> serde_json::Value {
    serde_json::Value::String(path.display().to_string())
}

/// A plain string argument (a property name, a directory name).
fn text_arg(value: &str) -> serde_json::Value {
    serde_json::Value::String(value.to_owned())
}

/// An authored reason as the argument of another message: a nested descriptor
/// renders in the same language as the message that names it.
fn message_arg(message: UiMessage) -> serde_json::Value {
    serde_json::to_value(message).unwrap_or(serde_json::Value::Null)
}

/// A configuration problem named by the file it belongs to: `{path}: {problem}`.
fn path_problem(path: &Path, problem: UiMessage) -> ConfigError {
    ConfigError::new(
        UiMessage::new(MessageId::ConfigPathProblem)
            .with_arg("path", path_arg(path))
            .with_arg("problem", message_arg(problem)),
    )
}

/// A configuration problem about one path with no other argument.
fn path_only(path: &Path, key: MessageId) -> ConfigError {
    ConfigError::new(UiMessage::new(key).with_arg("path", path_arg(path)))
}

/// The access list of `path` could not be read, with the OS reason.
fn acl_read_failed(path: &Path, reason: UiMessage) -> ConfigError {
    ConfigError::new(
        UiMessage::new(MessageId::ConfigAclReadFailed)
            .with_arg("path", path_arg(path))
            .with_arg("reason", message_arg(reason)),
    )
}

/* -------------------------------------------------------------------------- */
/* Settings document                                                          */
/* -------------------------------------------------------------------------- */

/// Reads and validates the settings file: `hotkey` and `mode` are both
/// required, so a half-written file stops the launcher instead of running with
/// a value nobody chose. `mute_during_recording` and `input_device` may be
/// absent - the documented defaults of a file written before they existed
/// (`false`, the system default input device) - but a value that is present has
/// to be a JSON boolean or `null` / a non-empty device id string. Obsolete root
/// `compare_all` and `port` properties are ignored here and removed by the next
/// patch. Whole-line `//` comments are allowed so the file can be annotated;
/// inline comments and trailing commas are not.
///
/// The hotkey is read as a non-empty string and is *not* checked against the
/// shortcut grammar here: [`validate_hotkey`] does that when the launcher
/// registers it, so a chord Windows refuses still starts the application and
/// can be corrected in the settings window. Other root properties are ignored
/// and preserved.
pub fn load_settings(config_path: &Path) -> Result<Settings, ConfigError> {
    read_settings_document(config_path, MessageId::SettingsFileRole)
}

/// [`load_settings`] with the role of the file as an argument, so a diagnostic
/// names the file correctly; the settings window reads the document it is about
/// to patch through this. The text is read and released here: only the parsed
/// settings are returned.
pub(crate) fn read_settings_document(
    config_path: &Path,
    label: MessageId,
) -> Result<Settings, ConfigError> {
    let text = read_settings_text(config_path, label)?;
    parse_settings_document(&text, config_path, label)
}

/// Reads a settings document as the text an editor works on: the file's own
/// characters, with a leading byte order mark kept, because a document that is
/// patched and written back has to stay the file the user saved. Strict UTF-8
/// and the NUL check are [`decode_utf8_bytes`]'s.
pub(crate) fn read_settings_text(config_path: &Path, label: MessageId) -> Result<String, ConfigError> {
    if config_path.is_dir() {
        return Err(ConfigError::new(
            UiMessage::new(MessageId::ConfigFileIsDirectory)
                .with_arg("path", path_arg(config_path))
                .with_arg("label", message_arg(UiMessage::new(label))),
        ));
    }
    let bytes = fs::read(config_path).map_err(|error| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigFileReadFailed)
                .with_arg("path", path_arg(config_path))
                .with_arg("label", message_arg(UiMessage::new(label)))
                .with_arg("reason", message_arg(read_reason(io_code(&error)))),
        )
    })?;
    let text = decode_utf8_bytes(config_path, label, &bytes)?.to_owned();
    if bytes.starts_with(UTF8_BOM) {
        let mut with_mark = String::with_capacity(text.len() + UTF8_BOM.len());
        with_mark.push('\u{feff}');
        with_mark.push_str(&text);
        Ok(with_mark)
    } else {
        Ok(text)
    }
}

/// Parses a settings document that is already in hand: the text is borrowed and
/// may come from a file read or from the document the settings window is
/// editing, and `config_path` is used for its wording only. A leading byte order
/// mark is part of the document and is skipped here, as are whole-line `//`
/// comments. A document that does not name a language reads as
/// [`crate::i18n::detect_default_language`].
pub(crate) fn parse_settings_document(
    text: &str,
    config_path: &Path,
    label: MessageId,
) -> Result<Settings, ConfigError> {
    parse_settings_document_with_language(
        text,
        config_path,
        label,
        crate::i18n::detect_default_language(),
    )
}

/// [`parse_settings_document`] with the language a document that omits the
/// property reads as passed in, so the precedence rule can be exercised without
/// the process environment or the registry; the runtime always goes through the
/// detected default.
pub(crate) fn parse_settings_document_with_language(
    text: &str,
    config_path: &Path,
    label: MessageId,
    default_language: Language,
) -> Result<Settings, ConfigError> {
    let text = strip_comment_lines(document_body(text));
    let label_arg = || message_arg(UiMessage::new(label));

    // This parse proves the document is one JSON object; the properties are
    // then read from the text again, so a repeated root property is refused
    // instead of silently collapsing to its last occurrence.
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(_)) => {}
        Ok(_) => {
            return Err(ConfigError::new(
                UiMessage::new(MessageId::ConfigFileNotJsonObject)
                    .with_arg("path", path_arg(config_path))
                    .with_arg("label", label_arg()),
            ))
        }
        Err(_) => {
            return Err(ConfigError::new(
                UiMessage::new(MessageId::ConfigFileInvalidJson)
                    .with_arg("path", path_arg(config_path))
                    .with_arg("label", label_arg()),
            ))
        }
    }

    let members = root_members(&text).ok_or_else(|| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigFileUnreadableObject)
                .with_arg("path", path_arg(config_path))
                .with_arg("label", label_arg()),
        )
    })?;
    settings_from_members(&members, &text, Some(config_path), default_language)
}

/// The interface language a syntactically valid, annotated settings document
/// names, when it names exactly one and that one is supported. It deliberately
/// reads nothing else: a document whose other managed values are missing or
/// malformed still answers this question, because the startup dialog has to
/// pick its language before the document is validated. `None` means the
/// document is not a JSON object, does not name a language, or names one more
/// than once or unusably.
fn document_language(text: &str) -> Option<Language> {
    let text = strip_comment_lines(document_body(text));
    if !matches!(serde_json::from_str::<Value>(&text), Ok(Value::Object(_))) {
        return None;
    }
    let members = root_members(&text)?;
    let member = single_member(&members, "language").ok()??;
    member_language(member, &text)
}

/// The interface language for the startup dialog, before the runtime and its
/// configuration exist: the sole valid `language` of the document at
/// `config_path`, or the detected default when there is no such document, it
/// cannot be read, or its language is missing, repeated, malformed or
/// unsupported. The rest of the document is neither validated nor repaired.
pub fn startup_language(config_path: Option<&Path>) -> Language {
    config_path
        .and_then(|path| read_settings_text(path, MessageId::SettingsFileRole).ok())
        .and_then(|text| document_language(&text))
        .unwrap_or_else(crate::i18n::detect_default_language)
}

/// Replaces the launcher values in `raw` and removes the obsolete root
/// `compare_all` and `port` properties, preserving comments, byte order mark,
/// line endings, property order and every unknown property byte for byte.
///
/// Only managed values are edited; an obsolete property leaves with its
/// separator comma, and `port` takes the comment block the released template
/// wrote with it. A required property that is missing or repeated is an error
/// rather than a guessed default, while `mute_during_recording`,
/// `input_device`, `language`, `custom_vocabulary` and `dictation_languages`
/// are inserted together when missing, in that fixed order in front of the
/// first property using its indentation and line ending. The result is read
/// back and compared with `settings` before it is returned.
///
/// The result is the whole document: a leading byte order mark in `raw` is kept
/// in front of it, so the caller writes [`patch_settings_document`]'s text as
/// the bytes of the file without adding anything - the text
/// [`read_settings_text`] returns is the text to patch, and a document that has
/// no mark never gains one.
pub fn patch_settings_document(raw: &str, settings: &Settings) -> Result<String, ConfigError> {
    // The values the patch is about to write are checked here, in the wording
    // the loader uses, so a caller cannot write a value the next read would
    // refuse.
    let invalid = |name: &str| -> ConfigError {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigPropertyInvalid)
                .with_arg("name", text_arg(name))
                .with_arg("requirement", message_arg(requirement(name))),
        )
    };
    let hotkey = ts_trim(&settings.hotkey);
    if hotkey.is_empty() {
        return Err(invalid("hotkey"));
    }
    let mode = ts_trim(&settings.mode);
    if !matches!(mode, "live" | "smart" | "verbatim") {
        return Err(invalid("mode"));
    }
    if settings
        .input_device
        .as_deref()
        .is_some_and(|device| usable_device_id(device).is_none())
    {
        return Err(invalid("input_device"));
    }
    if !is_canonical_vocabulary(&settings.custom_vocabulary) {
        return Err(invalid("custom_vocabulary"));
    }
    if !dictation_language::is_valid_list(&settings.dictation_languages) {
        return Err(invalid("dictation_languages"));
    }

    // A document handed over by a caller that kept the original bytes may still
    // start with a byte order mark; the body is worked on and the mark is put
    // back in front of the result, so a document with one keeps it and a
    // document without one never gains it.
    let (bom, body) = match raw.strip_prefix('\u{feff}') {
        Some(body) => ("\u{feff}", body),
        None => ("", raw),
    };

    let masked = masked_comments(body);
    let members = root_members(&masked)
        .ok_or_else(|| ConfigError::new(UiMessage::new(MessageId::ConfigDocumentNotObject)))?;

    // The values already in the document are read through the same rule the
    // loader uses, so a device that could never be used refuses the write: it is
    // repaired in the settings window, not silently rewritten here with a value
    // nobody chose.
    if let Some(member) = single_member(&members, "input_device")? {
        let usable = match member_value(member, &masked) {
            Value::Null => true,
            Value::String(value) => usable_device_id(&value).is_some(),
            _ => false,
        };
        if !usable {
            return Err(invalid("input_device"));
        }
    }
    if let Some(member) = single_member(&members, "language")? {
        if member_language(member, &masked).is_none() {
            return Err(invalid("language"));
        }
    }
    if let Some(member) = single_member(&members, "custom_vocabulary")? {
        if member_vocabulary(member, &masked).is_none() {
            return Err(invalid("custom_vocabulary"));
        }
    }
    if let Some(member) = single_member(&members, "dictation_languages")? {
        if member_dictation_languages(member, &masked).is_none() {
            return Err(invalid("dictation_languages"));
        }
    }

    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for (name, replacement) in [
        ("hotkey", json_string(hotkey)),
        ("mode", json_string(mode)),
    ] {
        let member = managed_member(&members, name)?;
        edits.push((member.value_span.clone(), replacement));
    }
    // The values a document written before them does not carry are inserted
    // together, in this fixed order and as a single edit, so a document missing
    // more than one never gets several zero-width insertions at one offset.
    let mut missing: Vec<(&str, String)> = Vec::new();
    match single_member(&members, "mute_during_recording")? {
        Some(member) => edits.push((
            member.value_span.clone(),
            settings.mute_during_recording.to_string(),
        )),
        None => missing.push((
            "mute_during_recording",
            settings.mute_during_recording.to_string(),
        )),
    }
    let device = match &settings.input_device {
        Some(device) => json_string(device),
        None => "null".to_owned(),
    };
    match single_member(&members, "input_device")? {
        Some(member) => edits.push((member.value_span.clone(), device)),
        None => missing.push(("input_device", device)),
    }
    let language = json_string(language_text(settings.language));
    match single_member(&members, "language")? {
        Some(member) => edits.push((member.value_span.clone(), language)),
        None => missing.push(("language", language)),
    }
    let vocabulary = serde_json::to_string(&*settings.custom_vocabulary)
        .expect("a list of strings always serializes");
    match single_member(&members, "custom_vocabulary")? {
        Some(member) => edits.push((member.value_span.clone(), vocabulary)),
        None => missing.push(("custom_vocabulary", vocabulary)),
    }
    let languages = serde_json::to_string(&*settings.dictation_languages)
        .expect("a list of strings always serializes");
    match single_member(&members, "dictation_languages")? {
        Some(member) => edits.push((member.value_span.clone(), languages)),
        None => missing.push(("dictation_languages", languages)),
    }
    member_insertion_edits(body, &masked, &members, &missing, &mut edits);

    // Obsolete root properties are retired without changing unrelated bytes;
    // `port` takes the comment block the released template wrote with it.
    for (name, comments) in [
        ("compare_all", None),
        ("port", Some(&OBSOLETE_PORT_COMMENTS[..])),
    ] {
        if let Some(member) = single_member(&members, name)? {
            member_removal_edits(&masked, body, member, comments, &mut edits);
        }
    }

    let patched = splice(body, &mut edits);
    verify_patched(&patched, settings)?;
    Ok(format!("{bom}{patched}"))
}

/// Reads a patched document back and refuses a patch that fails to round-trip
/// settings or still carries an obsolete root property - `compare_all` or
/// `port` - so a patch never reports success on a file it failed to migrate.
fn verify_patched(document: &str, settings: &Settings) -> Result<(), ConfigError> {
    let text = strip_comment_lines(document_body(document));
    if !matches!(serde_json::from_str::<Value>(&text), Ok(Value::Object(_))) {
        return Err(ConfigError::new(UiMessage::new(
            MessageId::ConfigPatchedInvalidJson,
        )));
    }
    let members = root_members(&text).ok_or_else(|| {
        ConfigError::new(UiMessage::new(MessageId::ConfigPatchedNotObject))
    })?;
    let written = settings_from_members(&members, &text, None, settings.language)?;
    let expected = Settings {
        hotkey: ts_trim(&settings.hotkey).to_owned(),
        mode: ts_trim(&settings.mode).to_owned(),
        mute_during_recording: settings.mute_during_recording,
        input_device: settings.input_device.clone(),
        language: settings.language,
        custom_vocabulary: Arc::clone(&settings.custom_vocabulary),
        dictation_languages: Arc::clone(&settings.dictation_languages),
    };
    if written != expected {
        return Err(ConfigError::new(UiMessage::new(
            MessageId::ConfigPatchReadbackMismatch,
        )));
    }
    for name in ["compare_all", "port"] {
        if single_member(&members, name)?.is_some() {
            return Err(ConfigError::new(
                UiMessage::new(MessageId::ConfigObsoletePropertyNotRemoved)
                    .with_arg("name", text_arg(name)),
            ));
        }
    }
    Ok(())
}

/// Adds the edits that take a whole property out of a document: the property
/// itself, and exactly one of the comma bytes that separated it from its
/// neighbours - the one after the value when there is one, the one before the
/// name otherwise. The two are separate edits on purpose: a whole-line comment
/// may sit between the comma and the property, and every byte of the user's
/// document except the property and that one comma has to survive. `document`
/// is the masked text, so a comment line cannot hide the comma.
///
/// `block`, when given, is the comment block the released template wrote in
/// front of the property. The property then leaves with the whole block - the
/// six shipped comment lines, its own line and the blank line the block closed
/// with - so a document that shipped with it and the document the shipped
/// template writes today are the same file once the migration is done.
/// `original` holds the document's own bytes, because `document` has its
/// comment lines blanked and the block being looked for is made of comments.
fn member_removal_edits(
    document: &str,
    original: &str,
    member: &RootMember,
    block: Option<&[&str]>,
    edits: &mut Vec<(Range<usize>, String)>,
) {
    let bytes = document.as_bytes();
    let after = skip_json_whitespace(bytes, member.value_span.end);
    let comma_after = bytes.get(after) == Some(&b',');

    if let Some(comments) = block {
        if let Some(start) = shipped_comment_block(original, member, comments) {
            let line = line_end(original, start_of_line(original, member.name_span.start));
            // The blank line the block closed with belongs to the block, not to
            // the property that follows it.
            let end = if line_content(original, line).trim().is_empty() {
                line_end(original, line)
            } else {
                line
            };
            edits.push((start..end, String::new()));
            // A property that is not the last one already leaves with the comma
            // after its value; as the last one, the separator is the comma
            // before the block, even though comment lines sit between them.
            if !comma_after {
                if let Some(comma) = comma_before(bytes, start) {
                    edits.push((comma..comma + 1, String::new()));
                }
            }
            return;
        }
    }

    edits.push((member.name_span.start..member.value_span.end, String::new()));
    if comma_after {
        edits.push((after..after + 1, String::new()));
    } else if let Some(comma) = comma_before(bytes, member.name_span.start) {
        edits.push((comma..comma + 1, String::new()));
    }
}

/// Where the shipped comment block in front of `member` begins, when the
/// document carries it exactly: the lines above the property reproduce
/// `comments` linewise, ignoring only each line's own ending, and the property
/// shares its line with nothing but its value and its separator comma. `None`
/// means the block is not the shipped one - a line was translated, edited or
/// written by the user - or the property is not alone on its line, and the
/// property then leaves without any comment.
fn shipped_comment_block(document: &str, member: &RootMember, comments: &[&str]) -> Option<usize> {
    let start = start_of_line(document, member.name_span.start);
    let tail = document[member.value_span.end..content_end(document, start)].trim();
    if !(tail.is_empty() || tail == ",") {
        return None;
    }
    let mut begin = start;
    for wanted in comments.iter().rev() {
        if begin == 0 {
            return None;
        }
        begin = start_of_line(document, begin - 1);
        if line_content(document, begin) != *wanted {
            return None;
        }
    }
    Some(begin)
}

/// The byte index where the line holding `index` begins.
fn start_of_line(document: &str, index: usize) -> usize {
    document[..index].rfind('\n').map_or(0, |newline| newline + 1)
}

/// The byte index just past the line ending of the line that begins at `start`:
/// where the next line begins, or the end of the document when that line is the
/// last one and carries no ending.
fn line_end(document: &str, start: usize) -> usize {
    match document[start..].find('\n') {
        Some(offset) => start + offset + 1,
        None => document.len(),
    }
}

/// The byte index where the line that begins at `start` ends without its line
/// ending, so `document[start..content_end]` is the line's own text whatever
/// ending it carries.
fn content_end(document: &str, start: usize) -> usize {
    let mut end = line_end(document, start);
    if end > start && document.as_bytes()[end - 1] == b'\n' {
        end -= 1;
        if end > start && document.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
    }
    end
}

/// The line that begins at `start`, without its line ending.
fn line_content(document: &str, start: usize) -> &str {
    &document[start..content_end(document, start)]
}

/// Adds the single edit that gives a document the properties it does not carry
/// yet. Each missing property is written as `"name": value` in the order given,
/// and all of them go into one insertion in front of the first root property,
/// with that property's own indentation and line ending: a document from before
/// `mute_during_recording`, `input_device`, `language`,
/// `custom_vocabulary` or `dictation_languages` existed has no
/// such values to write, and one edit keeps the spans of several zero-width
/// insertions from piling up at the same offset. Every other byte - the
/// comments, the order, the mark - stays where it was, and an object whose
/// first property shares a line
/// with the opening brace gets the insertion separated by a space instead.
///
/// `document` is the text the patch splices, so its own line endings and
/// indentation decide what the inserted lines look like. `masked` is the same
/// text with comment lines blanked - the same length, so the spans from
/// [`root_members`] address both - and is used only to find the opening brace:
/// before the first property the document holds nothing else, and a brace
/// written inside a comment cannot be mistaken for it. The patch refuses a
/// document without the required properties before it gets here, so there is
/// always a first property and always an opening brace.
fn member_insertion_edits(
    document: &str,
    masked: &str,
    members: &[RootMember],
    missing: &[(&str, String)],
    edits: &mut Vec<(Range<usize>, String)>,
) {
    if missing.is_empty() {
        return;
    }
    let Some(first) = members.first() else {
        return;
    };
    let brace = masked[..first.name_span.start]
        .rfind('{')
        .map(|index| index + 1);
    let before = &document[..first.name_span.start];
    // The property's own line: the newline it starts after has to be inside the
    // object, or the insertion would land in front of the opening brace.
    let own_line = match (brace, before.rfind('\n')) {
        (Some(brace), Some(newline)) if newline >= brace => Some((newline, &before[newline + 1..])),
        _ => None,
    };
    let (at, insertion) = match own_line {
        Some((newline, indent)) => {
            let ending = if newline > 0 && document.as_bytes()[newline - 1] == b'\r' {
                "\r\n"
            } else {
                "\n"
            };
            let properties = missing
                .iter()
                .map(|(name, value)| format!("{indent}\"{name}\": {value},{ending}"))
                .collect::<String>();
            (newline + 1, properties)
        }
        None => {
            let at = brace.unwrap_or_else(|| skip_json_whitespace(document.as_bytes(), 0) + 1);
            let properties = missing
                .iter()
                .map(|(name, value)| format!("\"{name}\": {value}, "))
                .collect::<String>();
            (at, properties)
        }
    };
    edits.push((at..at, insertion));
}

/// The single root member named `name`, or the error explaining that the
/// document cannot supply it. `Ok(None)` means the document does not carry the
/// property at all.
fn single_member<'a>(
    members: &'a [RootMember],
    name: &str,
) -> Result<Option<&'a RootMember>, ConfigError> {
    let mut found = members.iter().filter(|member| member.name == name);
    match (found.next(), found.next()) {
        (None, _) => Ok(None),
        (Some(member), None) => Ok(Some(member)),
        (Some(_), Some(_)) => Err(ConfigError::new(
            UiMessage::new(MessageId::ConfigPropertyRepeated).with_arg("name", text_arg(name)),
        )),
    }
}

/// A managed property that must be there exactly once.
fn managed_member<'a>(
    members: &'a [RootMember],
    name: &str,
) -> Result<&'a RootMember, ConfigError> {
    single_member(members, name)?.ok_or_else(|| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigPropertyMissingFromDocument)
                .with_arg("name", text_arg(name)),
        )
    })
}

/// What a managed property has to look like, as the catalog spells it.
fn requirement(name: &str) -> UiMessage {
    let key = match name {
        "hotkey" => MessageId::ConfigHotkeyRequirement,
        "mode" => MessageId::ConfigModeRequirement,
        "input_device" => MessageId::ConfigInputDeviceRequirement,
        "language" => MessageId::ConfigLanguageRequirement,
        "custom_vocabulary" => MessageId::ConfigCustomVocabularyRequirement,
        "dictation_languages" => MessageId::ConfigDictationLanguagesRequirement,
        _ => MessageId::ConfigMuteRequirement,
    };
    UiMessage::new(key)
}

/// The managed properties of a document, each typed and checked.
/// `mute_during_recording`, `input_device`, `language`, `custom_vocabulary` and
/// `dictation_languages` are the ones that may be absent - a document written
/// before them reads as `false`, the system default input device, the default
/// language, an empty vocabulary and no language hints there - while every
/// other missing or malformed property refuses the document. The obsolete root
/// `compare_all` and `port` are not read at all: the patch removes them, so
/// neither ever becomes a value, and a document that carries one of them twice
/// is refused because it could not be migrated unambiguously. `config_path`
/// names the file a problem belongs to when the caller has one; the patch
/// readback describes itself without a path. A value is never quoted.
fn settings_from_members(
    members: &[RootMember],
    document: &str,
    config_path: Option<&Path>,
    default_language: Language,
) -> Result<Settings, ConfigError> {
    // A member-level problem is a sentence of its own; the loader prefixes the
    // file it belongs to, the readback needs no path.
    let named = |problem: UiMessage| -> ConfigError {
        match config_path {
            Some(path) => path_problem(path, problem),
            None => ConfigError::new(problem),
        }
    };
    let member = |name: &str| -> Result<&RootMember, ConfigError> {
        match single_member(members, name) {
            Ok(Some(member)) => Ok(member),
            Ok(None) => Err(named(
                UiMessage::new(MessageId::ConfigPropertyMissing)
                    .with_arg("name", text_arg(name)),
            )),
            Err(error) => Err(named(error.ui)),
        }
    };
    let invalid = |name: &str| -> ConfigError {
        named(
            UiMessage::new(MessageId::ConfigPropertyInvalid)
                .with_arg("name", text_arg(name))
                .with_arg("requirement", message_arg(requirement(name))),
        )
    };

    let hotkey = member("hotkey")?;
    let mode = member("mode")?;

    let hotkey = member_string(hotkey, document)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("hotkey"))?;
    let mode = member_string(mode, document)
        .filter(|value| matches!(value.as_str(), "live" | "smart" | "verbatim"))
        .ok_or_else(|| invalid("mode"))?;

    // An obsolete value is never read as one, but a document that carries it
    // twice is refused: the patch could not migrate it unambiguously.
    for name in ["compare_all", "port"] {
        if let Err(error) = single_member(members, name) {
            return Err(named(error.ui));
        }
    }

    let mute_during_recording = match single_member(members, "mute_during_recording") {
        Ok(Some(mute)) => member_value(mute, document)
            .as_bool()
            .ok_or_else(|| invalid("mute_during_recording"))?,
        // The property is optional and its absence is the documented default,
        // so an annotated file from before the switch still loads.
        Ok(None) => false,
        Err(error) => return Err(named(error.ui)),
    };

    let input_device = match single_member(members, "input_device") {
        Ok(Some(device)) => match member_value(device, document) {
            // `null` is the stored form of the system default input device.
            Value::Null => None,
            Value::String(value) => {
                Some(usable_device_id(&value).ok_or_else(|| invalid("input_device"))?)
            }
            _ => return Err(invalid("input_device")),
        },
        // A document written before the microphone was selectable, and one that
        // names no device at all, both mean the system default device.
        Ok(None) => None,
        Err(error) => return Err(named(error.ui)),
    };
    let language = match single_member(members, "language") {
        Ok(Some(language)) => {
            member_language(language, document).ok_or_else(|| invalid("language"))?
        }
        // A document written before the language was selectable reads as the
        // language this profile starts in.
        Ok(None) => default_language,
        Err(error) => return Err(named(error.ui)),
    };
    let custom_vocabulary = match single_member(members, "custom_vocabulary") {
        // Blank lines, edge whitespace and repeats in a hand-edited list are
        // normalized rather than refused, so they cannot stop the launcher.
        Ok(Some(vocabulary)) => {
            member_vocabulary(vocabulary, document).ok_or_else(|| invalid("custom_vocabulary"))?
        }
        Ok(None) => Arc::default(),
        Err(error) => return Err(named(error.ui)),
    };
    let dictation_languages = match single_member(members, "dictation_languages") {
        // Unlike the vocabulary, a hand-edited list is not normalized: an unknown
        // code, a different spelling or a repeat refuses the document.
        Ok(Some(languages)) => member_dictation_languages(languages, document)
            .ok_or_else(|| invalid("dictation_languages"))?,
        // A document written before the setting existed reads as Auto-detect.
        Ok(None) => Arc::default(),
        Err(error) => return Err(named(error.ui)),
    };

    Ok(Settings {
        hotkey,
        mode,
        mute_during_recording,
        input_device,
        language,
        custom_vocabulary,
        dictation_languages,
    })
}

/// The string value of a member, trimmed the way the loader has always trimmed
/// it; `None` for any other JSON type.
fn member_string(member: &RootMember, document: &str) -> Option<String> {
    member_value(member, document)
        .as_str()
        .map(|value| ts_trim(value).to_owned())
}

/// The interface language a member spells: the lowercase wire value `"en"` or
/// `"ru"`, and `None` for any other JSON value or spelling.
fn member_language(member: &RootMember, document: &str) -> Option<Language> {
    match member_string(member, document)?.as_str() {
        "en" => Some(Language::En),
        "ru" => Some(Language::Ru),
        _ => None,
    }
}

/// The value of a `custom_vocabulary` member as the vocabulary it stores: a JSON
/// array of strings without line breaks, normalized by [`collect_vocabulary`];
/// `None` for any other JSON value, a line break inside a term or a list that
/// is still longer than [`MAX_CUSTOM_VOCABULARY`] after normalization.
fn member_vocabulary(member: &RootMember, document: &str) -> Option<Arc<[String]>> {
    let value = member_value(member, document);
    let items = value.as_array()?;
    let mut terms = Vec::with_capacity(items.len());
    for item in items {
        let term = item.as_str()?;
        if term.contains(['\r', '\n']) {
            return None;
        }
        terms.push(term);
    }
    collect_vocabulary(terms).ok()
}

/// The value of a `dictation_languages` member as the hints it stores: a JSON
/// array of strings that [`dictation_language::is_valid_list`] accepts as
/// written, untrimmed; `None` for any other JSON value or list.
fn member_dictation_languages(member: &RootMember, document: &str) -> Option<Arc<[String]>> {
    let value = member_value(member, document);
    let codes = value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()?;
    dictation_language::is_valid_list(&codes).then(|| codes.into())
}

/// The lowercase wire spelling of a language, the form a document stores.
fn language_text(language: Language) -> &'static str {
    match language {
        Language::En => "en",
        Language::Ru => "ru",
    }
}

/// An input device id as the document stores it: a non-empty string that
/// parses as a [`cpal::DeviceId`] in the form `Display` writes and `FromStr`
/// reads back, with both halves of that form non-empty. Parsing only proves
/// the value could name a device; whether the device is connected is decided
/// when a recording opens, so a stored id whose device is gone still loads and
/// is warned about instead of being replaced.
///
/// The settings window's general form is checked with this same rule before it
/// writes a device, so a choice the next read would refuse cannot be stored.
pub(crate) fn usable_device_id(value: &str) -> Option<String> {
    let parsed = cpal::DeviceId::from_str(value).ok()?;
    (!value.is_empty() && !parsed.1.is_empty()).then(|| value.to_owned())
}

/// The most terms Gemini accepts as a custom vocabulary.
pub const MAX_CUSTOM_VOCABULARY: usize = 1000;

/// The vocabulary `terms` describe: each term trimmed, empty ones dropped and
/// exact repeats (case-sensitive) dropped after their first occurrence, in the
/// order given. A list still longer than [`MAX_CUSTOM_VOCABULARY`] is refused
/// with the number of terms it has, never cut short. Line breaks inside a term
/// are the caller's to refuse.
pub(crate) fn collect_vocabulary<'a>(
    terms: impl IntoIterator<Item = &'a str>,
) -> Result<Arc<[String]>, usize> {
    let mut seen = HashSet::new();
    let kept: Vec<String> = terms
        .into_iter()
        .map(ts_trim)
        .filter(|term| !term.is_empty() && seen.insert(*term))
        .map(str::to_owned)
        .collect();
    if kept.len() > MAX_CUSTOM_VOCABULARY {
        return Err(kept.len());
    }
    Ok(kept.into())
}

/// The vocabulary the settings window's text holds, one term per line.
pub(crate) fn vocabulary_from_text(text: &str) -> Result<Arc<[String]>, usize> {
    collect_vocabulary(text.split(['\r', '\n']))
}

/// Whether `terms` is already in the form [`collect_vocabulary`] produces, so
/// the patch never writes a list the next read would change or refuse.
fn is_canonical_vocabulary(terms: &[String]) -> bool {
    let mut seen = HashSet::new();
    terms.len() <= MAX_CUSTOM_VOCABULARY
        && terms.iter().all(|term| {
            !term.is_empty()
                && ts_trim(term) == term
                && !term.contains(['\r', '\n'])
                && seen.insert(term.as_str())
        })
}

/// The value of a member, parsed from the document text.
fn member_value(member: &RootMember, document: &str) -> Value {
    serde_json::from_str(&document[member.value_span.clone()]).unwrap_or(Value::Null)
}

/// A JSON string literal for the patched document: `serde_json` escapes what
/// has to be escaped, so a hotkey holding a quote cannot break the file.
fn json_string(value: &str) -> String {
    Value::String(value.to_owned()).to_string()
}

/// Applies non-overlapping edits to `document`, in ascending order, keeping
/// every byte the edits do not cover.
fn splice(document: &str, edits: &mut Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|(span, _)| span.start);
    let mut result = String::with_capacity(document.len());
    let mut cursor = 0usize;
    for (span, replacement) in edits.iter() {
        if span.start < cursor || span.end < span.start || span.end > document.len() {
            continue;
        }
        result.push_str(&document[cursor..span.start]);
        result.push_str(replacement);
        cursor = span.end;
    }
    result.push_str(&document[cursor..]);
    result
}

/// The settings document this process uses, resolved from the active flavor
/// alone: the development flavor keeps `settings.json` beside the executable
/// it runs from, the test flavor accepts an absolute `SPEECHEK_CONFIG_PATH`
/// and otherwise keeps `%APPDATA%\Speechek-Test\settings.json`, and the
/// distributed shell always uses `%APPDATA%\Speechek\settings.json` and never
/// reads a path from the environment.
///
/// The override belongs to the test flavor alone: a blank value is no override
/// at all, a relative one is refused with the variable's name, and a valid one
/// names the document even when the profile directory around it exists only
/// for that run. The development flavor never reads it, so a forgotten
/// override cannot move a portable build off the profile beside its
/// executable; the executable itself is read for the development flavor only,
/// and a release build reads neither it nor any variable.
///
/// Nothing here touches the disk: [`create_default_if_missing`] writes the
/// first-run document.
pub fn settings_path() -> Result<PathBuf, ConfigError> {
    // One flavor is compiled in, so exactly one of these selections exists in
    // a given build: a release build reads no environment path at all, and the
    // development flavor resolves its own executable instead.
    #[cfg(not(debug_assertions))]
    let (flavor, config_override, executable) = (
        BuildFlavor::Production,
        None::<std::ffi::OsString>,
        None::<PathBuf>,
    );
    #[cfg(all(debug_assertions, feature = "test-provider"))]
    let (flavor, config_override, executable) = (
        BuildFlavor::Test,
        env::var_os("SPEECHEK_CONFIG_PATH"),
        None::<PathBuf>,
    );
    #[cfg(all(debug_assertions, not(feature = "test-provider")))]
    let (flavor, config_override, executable) = (
        BuildFlavor::Development,
        None::<std::ffi::OsString>,
        Some(env::current_exe().map_err(|_| {
            dev_executable_error(UiMessage::new(MessageId::ConfigDevExecutableUnreadable))
        })?),
    );

    resolve_settings_path(
        flavor,
        config_override.as_deref(),
        env::var_os("APPDATA").as_deref(),
        executable.as_deref(),
    )
}

/// [`settings_path`] with the flavor, both environment values and the
/// executable path as arguments, so the whole precedence can be exercised
/// without touching the process environment or the compiled-in flavor:
/// `config_override` is what the test flavor takes from
/// `SPEECHEK_CONFIG_PATH`, `appdata` what it takes from `APPDATA`, and
/// `executable` what the development flavor takes from the running
/// executable's own path.
///
/// The override belongs to [`BuildFlavor::Test`] alone and the executable to
/// [`BuildFlavor::Development`] alone. Production ignores both entirely - it
/// resolves through `%APPDATA%` and nothing else - so no released build can be
/// moved off the profile it documents, and a debug flavor cannot pick up
/// another flavor's document from an inherited variable.
fn resolve_settings_path(
    flavor: BuildFlavor,
    config_override: Option<&OsStr>,
    appdata: Option<&OsStr>,
    executable: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    match flavor {
        BuildFlavor::Production => appdata_settings_path(flavor, appdata),
        BuildFlavor::Development => development_settings_path(executable),
        BuildFlavor::Test => {
            if let Some(override_path) = config_override {
                let override_text = override_path.to_string_lossy();
                let trimmed = ts_trim(&override_text);
                if !trimmed.is_empty() {
                    let path = PathBuf::from(override_path);
                    if !path.is_absolute() {
                        return Err(ConfigError::new(
                            UiMessage::new(MessageId::ConfigOverrideNotAbsolute)
                                .with_arg("value", text_arg(&override_text)),
                        ));
                    }
                    return Ok(path);
                }
            }
            appdata_settings_path(flavor, appdata)
        }
    }
}

/// The `%APPDATA%` document of a flavor whose profile directory the table
/// names: `%APPDATA%\<directory>\settings.json`. Only the flavors that keep
/// such a profile reach this - the development flavor never does, and its
/// empty table directory is not a path this function has to handle.
fn appdata_settings_path(
    flavor: BuildFlavor,
    appdata: Option<&OsStr>,
) -> Result<PathBuf, ConfigError> {
    let directory = profile::defaults(flavor).directory;
    let appdata = appdata.filter(|value| !value.is_empty()).ok_or_else(|| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigAppdataMissing).with_arg("directory", text_arg(directory)),
        )
    })?;
    Ok(PathBuf::from(appdata).join(directory).join(SETTINGS_FILE_NAME))
}

/// The development flavor's document: `settings.json` in the directory of the
/// executable the process runs from (`src-tauri/target/debug` for the
/// canonical build), so a portable run carries its own profile and never
/// touches `%APPDATA%`.
///
/// The path has to be absolute and to have a directory of its own: a missing
/// executable, a relative path or a bare file name is refused with a
/// diagnostic that says the directory cannot be determined. Nothing falls back
/// to the working directory or to `%APPDATA%`, because either would silently
/// select a document the user never chose.
fn development_settings_path(executable: Option<&Path>) -> Result<PathBuf, ConfigError> {
    let executable = executable.ok_or_else(|| {
        dev_executable_error(UiMessage::new(MessageId::ConfigDevExecutableUnavailable))
    })?;
    if !executable.is_absolute() {
        return Err(dev_executable_error(
            UiMessage::new(MessageId::ConfigDevExecutableNotAbsolute)
                .with_arg("path", path_arg(executable)),
        ));
    }
    let directory = executable
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .ok_or_else(|| {
            dev_executable_error(
                UiMessage::new(MessageId::ConfigDevExecutableNoDirectory)
                    .with_arg("path", path_arg(executable)),
            )
        })?;
    Ok(directory.join(SETTINGS_FILE_NAME))
}

/// The refusal of a development run whose document directory cannot be named:
/// the message names the unusable path, or the missing one, and never falls
/// back to another directory.
fn dev_executable_error(reason: UiMessage) -> ConfigError {
    ConfigError::new(
        UiMessage::new(MessageId::ConfigDevExecutableDirectory)
            .with_arg("reason", message_arg(reason)),
    )
}

/// Creates the settings file for a first run: the flavor's annotated template
/// (embedded at build time, then brought to the active flavor's first-run
/// hotkey and language) as `settings.json`, written with `create_new` so
/// nothing that exists is ever overwritten. Returns `true` when the default
/// file was written, which tells the caller to continue startup with the
/// settings window open on the key section; `false` means the existing file is
/// left exactly as it was found.
///
/// The key container is not created here: `secrets.bin` is written by the first
/// explicit key apply of the settings window, so an installation without keys
/// never has an empty vault to get wrong.
pub fn create_default_if_missing(path: &Path) -> Result<bool, ConfigError> {
    let directory = match path.parent() {
        Some(directory) if !directory.as_os_str().is_empty() => directory,
        _ => {
            return Err(path_problem(
                path,
                UiMessage::new(MessageId::ConfigSettingsPathNoDirectory),
            ))
        }
    };

    if path.exists() {
        return Ok(false);
    }

    fs::create_dir_all(directory).map_err(|error| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigDirectoryCreateFailed)
                .with_arg("path", path_arg(directory))
                .with_arg("reason", message_arg(create_reason(io_code(&error)))),
        )
    })?;

    write_new(
        path,
        first_run_document(profile::ACTIVE, crate::i18n::detect_default_language())?.as_bytes(),
    )?;
    Ok(true)
}

/// The annotated document [`create_default_if_missing`] writes: the production
/// template brought to the flavor's first-run hotkey and to the language this
/// profile starts in.
///
/// The template itself stays the production one: it is the document the
/// distributed shell ships and the file a user reads. The flavor's own values
/// are rewritten through the same parser and patch the settings window uses, so
/// the template's comments, line endings and every other byte survive - and a
/// debug flavor never stores a development or test chord in a file a production
/// build could read. When the template already spells every wanted value, it is
/// borrowed unchanged.
fn first_run_document(
    flavor: BuildFlavor,
    language: Language,
) -> Result<Cow<'static, str>, ConfigError> {
    let defaults = profile::defaults(flavor);
    // The path is for the parser's wording only; nothing is read or written
    // here. The template already carries a language, but passing the wanted one
    // keeps the answer independent of the template's placeholder.
    let template_path = Path::new(SETTINGS_FILE_NAME);
    let mut settings = parse_settings_document_with_language(
        SETTINGS_EXAMPLE,
        template_path,
        MessageId::SettingsTemplateRole,
        language,
    )?;
    if settings.hotkey == defaults.hotkey && settings.language == language {
        return Ok(Cow::Borrowed(SETTINGS_EXAMPLE));
    }
    settings.hotkey = defaults.hotkey.to_owned();
    settings.language = language;
    Ok(Cow::Owned(patch_settings_document(
        SETTINGS_EXAMPLE,
        &settings,
    )?))
}

/* -------------------------------------------------------------------------- */
/* Atomic replacement                                                         */
/* -------------------------------------------------------------------------- */

/// Replaces `path` with `bytes` in one step: the bytes go to a unique temporary
/// file beside the destination, are flushed to disk, and are moved over the
/// destination with `MoveFileExW`. A reader therefore sees the previous file or
/// the complete new one, never a partial write; an interrupted run leaves at
/// most its own temporary file behind, and only a temporary file this call
/// created is ever removed.
///
/// `secret` marks a file that belongs to the current Windows user alone: the
/// temporary file is created with a protected DACL granting full access to that
/// user and to `SYSTEM` and to nobody else, and the DACL is checked on the
/// temporary file and again on the destination. Any other DACL is an error
/// rather than a weaker guarantee. With `secret` clear, an existing destination
/// keeps its own access, because the temporary file is created with the
/// descriptor of the file being replaced; a destination that exists but whose
/// descriptor cannot be read stops the write instead of being broadened.
///
/// A failure leaves the destination exactly as it was, with one exception the
/// error itself reports: [`ConfigError::replaced`] is true only after the move
/// over the destination has already succeeded, so a caller that keeps the
/// previous bytes knows they have to be restored rather than skipped.
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8], secret: bool) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            path_problem(
                path,
                UiMessage::new(MessageId::ConfigReplacementNoDirectory),
            )
        })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            path_problem(path, UiMessage::new(MessageId::ConfigFileNotFileName))
        })?
        .to_string_lossy()
        .into_owned();

    fs::create_dir_all(parent).map_err(|error| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigDirectoryCreateFailed)
                .with_arg("path", path_arg(parent))
                .with_arg("reason", message_arg(create_reason(io_code(&error)))),
        )
    })?;

    // The descriptor is resolved once, before any file is created: a caller
    // that cannot protect a container should hear about it at once.
    let descriptor = if secret {
        StagedDescriptor::private_to_current_user()?
    } else {
        StagedDescriptor::inherited_from(path)?
    };
    let attributes = descriptor.attributes();

    for _ in 0..8 {
        // `CREATE_NEW` with a name unique to this process refuses whatever is
        // already there; a name left behind by an earlier run is skipped for
        // the next one, and nothing that is not this call's own file is ever
        // removed.
        let temporary = parent.join(format!("{file_name}.{}.{}.tmp", process::id(), unique()));

        match write_new_file(&temporary, bytes, Some(&attributes)) {
            Ok(StagedBytes::Written) => {}
            Ok(StagedBytes::NameTaken) => continue,
            Err(error) => return Err(error),
        }
        if let Err(error) = descriptor.verify(&temporary) {
            remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = move_over(&temporary, path) {
            remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = descriptor.verify(path) {
            // The move already happened: the destination holds the new bytes
            // even though its access list is not the one that was asked for, so
            // the failure says so and the caller can restore what was there.
            return Err(error.after_replace());
        }
        return Ok(());
    }

    Err(path_problem(
        path,
        UiMessage::new(MessageId::ConfigReplacementNoTempFile),
    ))
}

/// What a staging write did with the name it was given.
enum StagedBytes {
    /// The bytes are in a new file of this call's own making.
    Written,
    /// Another file already has that name; it was not touched.
    NameTaken,
}

/// A counter shared by every replacement in this process, so two writers never
/// pick the same temporary name.
fn unique() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn remove_file(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Creates `path` with `CREATE_NEW` and writes the bytes through a standard
/// library file, which closes the handle on every path out of this function. A
/// failure after the file exists removes that file again, so an interrupted
/// write never leaves anything but its own name behind.
fn write_new_file(
    path: &Path,
    bytes: &[u8],
    attributes: Option<&SECURITY_ATTRIBUTES>,
) -> Result<StagedBytes, ConfigError> {
    let wide = wide_path(path);
    let created = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            ACCESS_GENERIC_WRITE,
            FILE_SHARE_READ,
            attributes.map(|attributes| attributes as *const SECURITY_ATTRIBUTES),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    let created = match created {
        Ok(handle) => handle,
        Err(error) => {
            return match windows_code(&error) {
                ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS => Ok(StagedBytes::NameTaken),
                code => Err(create_failure(path, code)),
            }
        }
    };

    // SAFETY: `CreateFileW` has just returned this handle and nothing else owns
    // it. The standard library takes it over here and closes it when the file
    // is dropped, so it is never closed by hand.
    let mut file = unsafe { File::from_raw_handle(created.0) };

    let outcome = file
        .write_all(bytes)
        // The bytes have to reach the disk before the destination can name them.
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = outcome {
        remove_file(path);
        return Err(create_failure(path, io_code(&error)));
    }
    Ok(StagedBytes::Written)
}

/// Moves `source` over `destination` in the same directory, waiting for the
/// move itself to reach the disk.
fn move_over(source: &Path, destination: &Path) -> Result<(), ConfigError> {
    let source_wide = wide_path(source);
    let destination_wide = wide_path(destination);
    unsafe {
        MoveFileExW(
            PCWSTR(source_wide.as_ptr()),
            PCWSTR(destination_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigReplaceProblem)
                .with_arg("path", path_arg(destination))
                .with_arg("reason", message_arg(replace_reason(windows_code(&error)))),
        )
    })
}

/// Opens an existing file with just `READ_CONTROL`, which is all the
/// verification of a written file needs.
fn open_for_read_control(path: &Path) -> Result<File, ConfigError> {
    let wide = wide_path(path);
    let opened = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            ACCESS_READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigAclReadFailed)
                .with_arg("path", path_arg(path))
                .with_arg("reason", message_arg(read_reason(Some(windows_code(&error))))),
        )
    })?;

    // SAFETY: the handle was just returned and belongs to this function; the
    // standard library closes it when the file is dropped.
    Ok(unsafe { File::from_raw_handle(opened.0) })
}

/// The descriptor a replacement is created with, and the check the resulting
/// file has to pass.
struct StagedDescriptor {
    /// The descriptor `CreateFileW` applies. It points into `private` or into
    /// the system copy in `inherited`, both of which outlive every use because
    /// each lives on the heap.
    descriptor: PSECURITY_DESCRIPTOR,
    /// The private descriptor of a container, when one was asked for.
    private: Option<Box<PrivateDescriptor>>,
    /// The descriptor copied from the system for an ordinary document; it is
    /// held only so the pointer in `descriptor` keeps pointing at live memory.
    _inherited: Option<OwnedDescriptor>,
}

/// A protected descriptor holding exactly two full-access entries: the current
/// user and `SYSTEM`.
struct PrivateDescriptor {
    descriptor: SECURITY_DESCRIPTOR,
    acl: Vec<u32>,
    user_sid: SidBuffer,
    system_sid: SidBuffer,
}

impl StagedDescriptor {
    /// The attributes to create the temporary file with. They stay valid while
    /// `self` is alive and is not moved, because the raw pointer inside them
    /// points at a heap allocation rather than at a field of `self`.
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        let mut attributes = SECURITY_ATTRIBUTES::default();
        attributes.nLength = core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        attributes.lpSecurityDescriptor = self.descriptor.0;
        // The handle is closed by the standard library, never inherited.
        attributes.bInheritHandle = false.into();
        attributes
    }

    /// Creates the private descriptor a container gets.
    fn private_to_current_user() -> Result<Self, ConfigError> {
        let mut private = Box::new(PrivateDescriptor {
            descriptor: SECURITY_DESCRIPTOR::default(),
            acl: Vec::new(),
            user_sid: SidBuffer::new(),
            system_sid: SidBuffer::new(),
        });
        read_current_user_sid(&mut private.user_sid)?;
        read_local_system_sid(&mut private.system_sid)?;

        // One ACL holding two `ACCESS_ALLOWED_ACE` entries: the header, and for
        // each entry the ACE header and its mask plus the SID itself. A
        // `Vec<u32>` keeps the DWORD alignment the ACL API expects.
        let user_length = unsafe { GetLengthSid(private.user_sid.sid()) } as usize;
        let system_length = unsafe { GetLengthSid(private.system_sid.sid()) } as usize;
        let ace_overhead = core::mem::size_of::<ACE_HEADER>() + core::mem::size_of::<u32>();
        let acl_bytes =
            core::mem::size_of::<ACL>() + 2 * ace_overhead + user_length + system_length;
        private.acl = vec![0u32; acl_bytes.div_ceil(4)];

        let user_sid = private.user_sid.sid();
        let system_sid = private.system_sid.sid();
        let descriptor = &mut private.descriptor as *mut SECURITY_DESCRIPTOR as *mut _;
        // SAFETY: the ACL buffer is zeroed, large enough for both entries and
        // aligned; the SIDs are valid for the lengths reported above; the
        // descriptor is a live local whose DACL is set once and then only read.
        unsafe {
            let acl = private.acl.as_mut_ptr() as *mut ACL;
            InitializeAcl(acl, acl_bytes as u32, ACL_REVISION)
                .map_err(|error| private_descriptor_error(&error))?;
            AddAccessAllowedAce(acl, ACL_REVISION, ACCESS_FULL, user_sid)
                .map_err(|error| private_descriptor_error(&error))?;
            AddAccessAllowedAce(acl, ACL_REVISION, ACCESS_FULL, system_sid)
                .map_err(|error| private_descriptor_error(&error))?;

            InitializeSecurityDescriptor(PSECURITY_DESCRIPTOR(descriptor), 1)
                .map_err(|error| private_descriptor_error(&error))?;
            SetSecurityDescriptorDacl(
                PSECURITY_DESCRIPTOR(descriptor),
                true,
                Some(acl as *const ACL),
                false,
            )
            .map_err(|error| private_descriptor_error(&error))?;
            // Nothing above the container may hand anybody else access.
            SetSecurityDescriptorControl(
                PSECURITY_DESCRIPTOR(descriptor),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            )
            .map_err(|error| private_descriptor_error(&error))?;
        }

        let descriptor = PSECURITY_DESCRIPTOR(&mut private.descriptor as *mut _ as *mut _);
        Ok(Self {
            descriptor,
            private: Some(private),
            _inherited: None,
        })
    }

    /// The descriptor of an ordinary document: the one the destination already
    /// has, so replacing it keeps the access the user gave that file. A
    /// destination that does not exist yet gets the directory's default, which
    /// is what creating it afresh would have produced.
    fn inherited_from(path: &Path) -> Result<Self, ConfigError> {
        let inherited = read_file_descriptor(path)?;
        let descriptor = inherited
            .as_ref()
            .map(|owned| owned.0)
            .unwrap_or(PSECURITY_DESCRIPTOR(ptr::null_mut()));
        Ok(Self {
            descriptor,
            private: None,
            _inherited: inherited,
        })
    }

    /// Checks a file written by this module: a container must be reachable only
    /// by the current user and by `SYSTEM`, and an ordinary document has no
    /// extra promise to keep.
    fn verify(&self, path: &Path) -> Result<(), ConfigError> {
        match &self.private {
            Some(private) => {
                verify_private_descriptor(path, private.user_sid.sid(), private.system_sid.sid())
            }
            None => Ok(()),
        }
    }

    /// The same check through a handle that is already open. The private archive
    /// inspects the entry under a name once and verifies exactly that entry, so
    /// nothing can be swapped in between the two calls.
    #[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
    fn verify_handle(&self, path: &Path, handle: HANDLE) -> Result<(), ConfigError> {
        match &self.private {
            Some(private) => verify_private_descriptor_handle(
                path,
                handle,
                private.user_sid.sid(),
                private.system_sid.sid(),
            ),
            None => Ok(()),
        }
    }
}

/// Reads the security descriptor of the file a replacement is about to write
/// over, or `None` when there is genuinely no such file: then the replacement
/// gets the directory's default access, exactly as creating the file would.
///
/// Anything else is an error rather than a shrug. A destination whose access
/// list cannot be read - a lock that refuses the handle, a directory in the way,
/// a file the user may not even read the permissions of - must not silently be
/// replaced with a broader descriptor, so the write stops before it starts.
fn read_file_descriptor(path: &Path) -> Result<Option<OwnedDescriptor>, ConfigError> {
    let wide = wide_path(path);
    let opened = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            ACCESS_READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    let file = match opened {
        Ok(handle) => {
            // SAFETY: the handle was just returned and belongs to this
            // function; the standard library closes it when the file is
            // dropped.
            unsafe { File::from_raw_handle(handle.0) }
        }
        Err(error) => {
            return match windows_code(&error) {
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
                code => Err(create_failure(path, code)),
            }
        }
    };

    let mut descriptor = PSECURITY_DESCRIPTOR(ptr::null_mut());
    // SAFETY: the handle is open for the length of the call and the descriptor
    // it hands back is released through `OwnedDescriptor`.
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(acl_read_failed(path, security_reason(status.0)));
    }
    Ok(Some(OwnedDescriptor(descriptor)))
}

/// Checks that the entry `path` names is reachable only by `user_sid` and
/// `system_sid`: the entry is opened for the check, and the access list of that
/// very entry is read.
fn verify_private_descriptor(
    path: &Path,
    user_sid: PSID,
    system_sid: PSID,
) -> Result<(), ConfigError> {
    let file = open_for_read_control(path)?;
    verify_private_descriptor_handle(path, HANDLE(file.as_raw_handle()), user_sid, system_sid)
}

/// Checks that an entry that is already open is reachable only by `user_sid`
/// and `system_sid`: its DACL is protected from the directory holding it, is not
/// empty, and holds nothing but full-access entries for those two accounts.
///
/// The caller keeps the handle, so the check is about the entry it opened and
/// never about whatever that name points at by the time the check runs.
fn verify_private_descriptor_handle(
    path: &Path,
    handle: HANDLE,
    user_sid: PSID,
    system_sid: PSID,
) -> Result<(), ConfigError> {
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR(ptr::null_mut());
    // SAFETY: the handle stays open for the whole check, and the descriptor the
    // call allocates is released as soon as it is read out.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(acl_read_failed(path, security_reason(status.0)));
    }
    let descriptor = OwnedDescriptor(descriptor);

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: the descriptor is the allocation the system just handed over.
    unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) }
        .map_err(|error| acl_read_failed(path, security_reason(windows_code(&error))))?;
    if control & SE_DACL_PROTECTED.0 == 0 {
        return Err(path_only(path, MessageId::ConfigAclNotProtected));
    }
    if dacl.is_null() {
        return Err(path_only(path, MessageId::ConfigAclMissing));
    }

    // SAFETY: the DACL and the ACEs inside it belong to the descriptor that is
    // alive here; `GetAce` returns a pointer into that same allocation.
    let entries = unsafe { (*dacl).AceCount } as u32;
    if entries == 0 {
        return Err(path_only(path, MessageId::ConfigAclEmpty));
    }
    for index in 0..entries {
        let mut ace: *mut core::ffi::c_void = ptr::null_mut();
        // SAFETY: the index is inside the DACL and the ACE it points at lives
        // in the same allocation.
        unsafe { GetAce(dacl, index, &mut ace) }
            .map_err(|error| acl_read_failed(path, security_reason(windows_code(&error))))?;
        if ace.is_null() {
            return Err(path_only(path, MessageId::ConfigAclUnreadable));
        }

        // SAFETY: an ACE starts with its header and, for an access-allowed
        // entry, carries the mask and then the SID.
        let (ace_type, mask, sid) = unsafe {
            let header = &*(ace as *const ACE_HEADER);
            let mask_offset = (ace as *const u8).add(core::mem::size_of::<ACE_HEADER>());
            let sid_offset = mask_offset.add(core::mem::size_of::<u32>());
            (
                header.AceType,
                ptr::read_unaligned(mask_offset as *const u32),
                PSID(sid_offset as *mut _),
            )
        };
        if ace_type != ACCESS_ALLOWED_ACE_TYPE {
            return Err(path_only(path, MessageId::ConfigAclForeignAce));
        }
        if mask & ACCESS_FULL != ACCESS_FULL {
            return Err(path_only(path, MessageId::ConfigAclPartial));
        }
        // SAFETY: both SIDs are valid and are only read.
        let known = unsafe { EqualSid(sid, user_sid).is_ok() || EqualSid(sid, system_sid).is_ok() };
        if !known {
            return Err(path_only(path, MessageId::ConfigAclForeignAccount));
        }
    }
    Ok(())
}

/// A security descriptor the system allocated, released with `LocalFree`.
struct OwnedDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for OwnedDescriptor {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: the pointer came out of a Windows API that documents
            // `LocalFree` as the way to release it.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0 .0)));
            }
        }
    }
}

/// A SID in a buffer aligned for the Windows security APIs.
struct SidBuffer {
    words: [usize; SID_BUFFER_WORDS],
}

impl SidBuffer {
    fn new() -> Self {
        Self {
            words: [0; SID_BUFFER_WORDS],
        }
    }

    fn as_output(&mut self) -> PSID {
        PSID(self.words.as_mut_ptr() as *mut _)
    }

    fn sid(&self) -> PSID {
        PSID(self.words.as_ptr() as *mut _)
    }

    fn length(&self) -> u32 {
        core::mem::size_of::<[usize; SID_BUFFER_WORDS]>() as u32
    }

    fn as_bytes_mut(&mut self) -> *mut core::ffi::c_void {
        self.words.as_mut_ptr() as *mut _
    }

    fn as_bytes(&self) -> *const core::ffi::c_void {
        self.words.as_ptr() as *const _
    }
}

/// Reads the SID the current process runs as into `buffer`.
fn read_current_user_sid(buffer: &mut SidBuffer) -> Result<(), ConfigError> {
    let mut staging = SidBuffer::new();
    // SAFETY: the token handle comes from the process token, is closed through
    // the standard library, and the token information call is given a buffer of
    // the size it is told about.
    unsafe {
        let mut token = HANDLE(ptr::null_mut());
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|error| identity_error(MessageId::ConfigIdentityActionToken, &error))?;
        // SAFETY: the handle was just returned and is owned by this function,
        // which closes it through the guard when the check is over.
        let token = OwnedHandle::from_raw_handle(token.0);

        let mut needed = 0u32;
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            Some(staging.as_bytes_mut()),
            staging.length(),
            &mut needed,
        )
        .map_err(|error| identity_error(MessageId::ConfigIdentityActionUser, &error))?;

        let (sid, length) = {
            let record = &*(staging.as_bytes() as *const TOKEN_USER);
            if !IsValidSid(record.User.Sid).as_bool() {
                return Err(identity_reason(UiMessage::new(
                    MessageId::ConfigIdentityInvalidSid,
                )));
            }
            (record.User.Sid, GetLengthSid(record.User.Sid) as usize)
        };
        if length > buffer.words.len() * core::mem::size_of::<usize>() {
            return Err(identity_reason(UiMessage::new(MessageId::ConfigIdentitySidTooLong)));
        }
        // The record lives inside `staging`; the SID is copied to the front of
        // `buffer`, which is a different allocation.
        ptr::copy_nonoverlapping(sid.0 as *const u8, buffer.as_bytes_mut() as *mut u8, length);
    }
    Ok(())
}

/// Reads the SID of `SYSTEM` into `buffer`.
fn read_local_system_sid(buffer: &mut SidBuffer) -> Result<(), ConfigError> {
    let mut length = buffer.length();
    let sid = buffer.as_output();
    // SAFETY: the buffer is as large as the length passed in, and the call
    // writes at most that many bytes.
    unsafe { CreateWellKnownSid(WinLocalSystemSid, None, Some(sid), &mut length) }
        .map_err(|error| identity_error(MessageId::ConfigIdentityActionSystem, &error))
}

/// The failure of building the private descriptor, before any file exists.
fn private_descriptor_error(error: &windows::core::Error) -> ConfigError {
    ConfigError::new(
        UiMessage::new(MessageId::ConfigPrivateDescriptorFailed)
            .with_arg("reason", message_arg(security_reason(windows_code(error)))),
    )
}

/// The failure of reading the identity a private descriptor names.
fn identity_error(action: MessageId, error: &windows::core::Error) -> ConfigError {
    identity_reason(
        UiMessage::new(MessageId::ConfigIdentityFailed)
            .with_arg("action", message_arg(UiMessage::new(action)))
            .with_arg("reason", message_arg(security_reason(windows_code(error)))),
    )
}

/// An identity failure worded exactly as given: the callers know which part of
/// the identity could not be read, and none of them quotes a value.
fn identity_reason(reason: UiMessage) -> ConfigError {
    ConfigError::new(reason)
}

/// The wording for a file that could not be created or written: the path, and
/// the small set of system codes both the standard library and the Windows
/// bindings report.
fn create_failure(path: &Path, code: impl Into<Option<u32>>) -> ConfigError {
    ConfigError::new(
        UiMessage::new(MessageId::ConfigCreateFailed)
            .with_arg("path", path_arg(path))
            .with_arg("reason", message_arg(create_reason(code.into()))),
    )
}

/* -------------------------------------------------------------------------- */
/* Private archive files                                                      */
/* -------------------------------------------------------------------------- */

/// The private archive of a development build: the session directories and
/// files the diagnostics writer keeps under `logs/` beside the development
/// `settings.json`. Everything here is gated with the feature and the profile,
/// so a production or test build has no writer, no directory and no way to
/// reach an archive - and these four functions are the only way in, so the
/// descriptor and the checks that make a container private have one home.
///
/// The four functions share one rule: an entry under a name this module builds
/// must be a plain file or directory of the current user with the private
/// access list, and never a link, because the archive reads and writes through
/// names of its own making and a link is a name whose target somebody else
/// chose. Nothing here follows a link, replaces an existing entry or repairs a
/// wider access list: a path that is not exactly what the archive writes is a
/// refusal, and the caller decides what to do about it.

/// Creates `path` as a directory only the current user and `SYSTEM` can reach,
/// or checks the one that is already there.
///
/// `CreateDirectoryW` applies the descriptor itself, so the directory never
/// exists for a moment with the access its parent would have handed down. A
/// directory that is already there is only checked - never repaired, replaced
/// or removed - because a container that is not private is something the caller
/// has to decide about. A missing parent is an error: nothing outside `path` is
/// created, and nothing is ever created over a name that is in use.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
pub(crate) fn create_private_directory(path: &Path) -> Result<(), ConfigError> {
    let descriptor = StagedDescriptor::private_to_current_user()?;
    match fs::symlink_metadata(path) {
        Ok(_) => verify_private_directory(path, &descriptor),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let wide = wide_path(path);
            let attributes = descriptor.attributes();
            // SAFETY: the path is NUL-terminated and lives for the call, and
            // the descriptor the attributes point at is owned by `descriptor`,
            // which outlives them.
            let created = unsafe {
                CreateDirectoryW(
                    PCWSTR(wide.as_ptr()),
                    Some(&attributes as *const SECURITY_ATTRIBUTES),
                )
            };
            match created {
                Ok(()) => verify_private_directory(path, &descriptor),
                // Somebody created the name between the check and the call:
                // what is there now still has to pass, exactly as if the first
                // check had found it.
                Err(found)
                    if matches!(
                        windows_code(&found),
                        ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS
                    ) =>
                {
                    verify_private_directory(path, &descriptor)
                }
                Err(found) => Err(create_failure(path, windows_code(&found))),
            }
        }
        Err(error) => Err(create_failure(path, io_code(&error))),
    }
}

/// Creates `path` as a brand-new file the current user and `SYSTEM` alone can
/// reach, open for reading, writing and patching.
///
/// `CREATE_NEW` refuses a name that is in use, so the call never touches a file
/// that is already there, and the checks run on the handle this call itself
/// created: what comes back is the entry that was checked, not whatever the
/// name points at a moment later. The handle shares only reads, so a second
/// writer is refused by the system rather than interleaving with this one.
///
/// A file whose own checks fail is left under its name - the caller owns the
/// name it asked for, and a staging name this module cannot vouch for is
/// reconciled by the caller rather than deleted here.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
pub(crate) fn create_private_file(path: &Path) -> Result<File, ConfigError> {
    let descriptor = StagedDescriptor::private_to_current_user()?;
    let wide = wide_path(path);
    let attributes = descriptor.attributes();
    // SAFETY: the path is NUL-terminated and lives for the call, the descriptor
    // the attributes point at is owned by `descriptor`, and the handle that
    // comes back is taken over below, so it is never closed by hand.
    let created = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            ACCESS_GENERIC_READ | ACCESS_GENERIC_WRITE,
            FILE_SHARE_READ,
            Some(&attributes as *const SECURITY_ATTRIBUTES),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    };
    let created = match created {
        Ok(handle) => handle,
        Err(error) => return Err(create_failure(path, windows_code(&error))),
    };

    // SAFETY: `CreateFileW` has just returned this handle and nothing else owns
    // it; the standard library closes it when the file is dropped.
    let file = unsafe { File::from_raw_handle(created.0) };
    let attributes = entry_attributes(path, &file)?;
    reject_link(path, attributes)?;
    descriptor.verify_handle(path, HANDLE(file.as_raw_handle()))?;
    Ok(file)
}

/// Checks the entry `path` names: it is not a link, and its access list is the
/// private one. Both a file and a directory are accepted, because the caller
/// knows which of the two it is publishing.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
pub(crate) fn verify_private_path(path: &Path) -> Result<(), ConfigError> {
    let descriptor = StagedDescriptor::private_to_current_user()?;
    let entry = open_for_inspection(path)?;
    let attributes = entry_attributes(path, &entry)?;
    reject_link(path, attributes)?;
    descriptor.verify_handle(path, HANDLE(entry.as_raw_handle()))
}

/// Publishes `source` under the name `destination`: the move itself is the
/// publish, it never replaces anything, and the destination is checked before
/// this call claims success.
///
/// `MOVEFILE_WRITE_THROUGH` waits for the move to reach the disk, so a crash
/// right after leaves either nothing under the new name or the complete file.
/// A destination that already holds something - a file, a directory or a link -
/// is reported and left exactly as it is, and `source` stays where it was.
///
/// The source is closed before this call: the move needs the name, and a handle
/// that shares writes keeps the system from renaming the file it holds.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
pub(crate) fn publish_private_file(source: &Path, destination: &Path) -> Result<(), ConfigError> {
    let source_wide = wide_path(source);
    let destination_wide = wide_path(destination);
    // SAFETY: both paths are NUL-terminated and live for the call. The move
    // asks to replace nothing, so a name in use is a failure rather than
    // something to overwrite, and no link is followed.
    let moved = unsafe {
        MoveFileExW(
            PCWSTR(source_wide.as_ptr()),
            PCWSTR(destination_wide.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = moved {
        let code = windows_code(&error);
        if matches!(code, ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS) {
            return Err(create_failure(destination, code));
        }
        return Err(ConfigError::new(
            UiMessage::new(MessageId::ConfigReplaceProblem)
                .with_arg("path", path_arg(destination))
                .with_arg("reason", message_arg(replace_reason(code))),
        ));
    }
    verify_private_path(destination)
}

/// Checks the directory the archive keeps its takes in: a plain directory with
/// the private access list. A file or a link under that name is refused, and
/// nothing about a directory that is already there is changed.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn verify_private_directory(path: &Path, descriptor: &StagedDescriptor) -> Result<(), ConfigError> {
    let directory = open_for_inspection(path)?;
    let attributes = entry_attributes(path, &directory)?;
    reject_link(path, attributes)?;
    if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
        return Err(path_problem(
            path,
            UiMessage::new(MessageId::ConfigCreateReasonExists),
        ));
    }
    descriptor.verify_handle(path, HANDLE(directory.as_raw_handle()))
}

/// Opens an existing file or directory for the read-only checks the archive
/// makes. `FILE_FLAG_BACKUP_SEMANTICS` is what lets a directory be opened at
/// all, and `FILE_FLAG_OPEN_REPARSE_POINT` keeps a link from being followed, so
/// every check is about the entry under this name and never about its target.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn open_for_inspection(path: &Path) -> Result<File, ConfigError> {
    let wide = wide_path(path);
    let opened = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            ACCESS_READ_CONTROL | ACCESS_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|error| path_problem(path, read_reason(Some(windows_code(&error)))))?;

    // SAFETY: the handle was just returned and belongs to this function; the
    // standard library closes it when the file is dropped.
    Ok(unsafe { File::from_raw_handle(opened.0) })
}

/// The attributes of an entry that is already open. They are what tells a
/// directory from a file and a link from a plain entry, and reading them from
/// the handle is what keeps both answers about the entry itself.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn entry_attributes(path: &Path, entry: &File) -> Result<u32, ConfigError> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is open for the length of the call, and the structure
    // is a live local the call fills in.
    unsafe { GetFileInformationByHandle(HANDLE(entry.as_raw_handle()), &mut information) }
        .map_err(|error| path_problem(path, read_reason(Some(windows_code(&error)))))?;
    Ok(information.dwFileAttributes)
}

/// Refuses an entry that is a link.
#[cfg(all(debug_assertions, feature = "dev-diagnostics"))]
fn reject_link(path: &Path, attributes: u32) -> Result<(), ConfigError> {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
        return Ok(());
    }
    Err(ConfigError::new(
        UiMessage::new(MessageId::ConfigPathIsLink).with_arg("path", path_arg(path)),
    ))
}

/* -------------------------------------------------------------------------- */
/* Document text                                                              */
/* -------------------------------------------------------------------------- */

/// One root property of a JSON object, with the byte spans of its name and its
/// value inside the document they were read from.
struct RootMember {
    /// The decoded property name: `"h\u006ftkey"` is `hotkey`.
    name: String,
    /// The name with its quotes, exactly as the document spells it.
    name_span: Range<usize>,
    /// The value, without the whitespace around it.
    value_span: Range<usize>,
}

/// The root properties of a JSON object document. `None` means the document is
/// not an object the reader understands - the callers check JSON validity
/// first, so this is the shape check they need.
///
/// Reading the properties from the text rather than from a parsed value is what
/// lets the same name appear twice be an error instead of a silent "the last
/// value wins", and what lets a replacement rewrite the bytes in place.
fn root_members(document: &str) -> Option<Vec<RootMember>> {
    let bytes = document.as_bytes();
    let mut index = skip_json_whitespace(bytes, 0);
    if bytes.get(index) != Some(&b'{') {
        return None;
    }
    index += 1;

    let mut members = Vec::new();
    loop {
        index = skip_json_whitespace(bytes, index);
        match bytes.get(index)? {
            b'}' => return Some(members),
            b'"' => {}
            _ => return None,
        }
        let name_start = index;
        let name_end = skip_json_string(bytes, index)?;
        index = skip_json_whitespace(bytes, name_end);
        if bytes.get(index)? != &b':' {
            return None;
        }
        index = skip_json_whitespace(bytes, index + 1);
        let value_start = index;
        let value_end = skip_json_value(bytes, index)?;
        members.push(RootMember {
            name: serde_json::from_str::<String>(&document[name_start..name_end]).ok()?,
            name_span: name_start..name_end,
            value_span: value_start..value_end,
        });

        index = skip_json_whitespace(bytes, value_end);
        match bytes.get(index)? {
            b',' => index += 1,
            b'}' => return Some(members),
            _ => return None,
        }
    }
}

/// The index just past the whitespace JSON allows.
fn skip_json_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while matches!(bytes.get(index), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        index += 1;
    }
    index
}

/// The index just past the string that starts at `start`, honouring escapes.
fn skip_json_string(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut index = start + 1;
    loop {
        match bytes.get(index)? {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            _ => index += 1,
        }
    }
}

/// The index just past the value that starts at `start`, whatever its type.
fn skip_json_value(bytes: &[u8], start: usize) -> Option<usize> {
    match bytes.get(start)? {
        b'"' => skip_json_string(bytes, start),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut index = start;
            while let Some(byte) = bytes.get(index) {
                match byte {
                    b'{' | b'[' => {
                        depth += 1;
                        index += 1;
                    }
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        index += 1;
                        if depth == 0 {
                            return Some(index);
                        }
                    }
                    b'"' => index = skip_json_string(bytes, index)?,
                    _ => index += 1,
                }
            }
            None
        }
        _ => {
            let mut index = start;
            while let Some(byte) = bytes.get(index) {
                if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                    return Some(index);
                }
                index += 1;
            }
            Some(index)
        }
    }
}

/// The index of the comma that separates the property starting at `start` from
/// the one before it, when there is one.
fn comma_before(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index > 0 {
        index -= 1;
        match bytes.get(index) {
            Some(b' ' | b'\t' | b'\n' | b'\r') => {}
            Some(b',') => return Some(index),
            _ => return None,
        }
    }
    None
}

/// Drops whole-line `//` comments so the settings file can be annotated. Only a
/// line whose first non-blank characters are `//` is removed, so `//` inside a
/// quoted value (a URL, a UNC path) survives; every other line keeps its
/// content and its own line ending.
fn strip_comment_lines(text: &str) -> String {
    text.split('\n')
        .filter(|line| !line.trim_start_matches(is_ts_whitespace).starts_with("//"))
        .collect::<Vec<&str>>()
        .join("\n")
}

/// The same comment removal, but at a band rather than at a line: every byte of
/// a comment line becomes a space and the line ending is kept, so the result has
/// exactly the same length as the input. Offsets computed on the result can
/// therefore be used to splice the original text, which is what
/// [`patch_settings_document`] needs.
fn masked_comments(text: &str) -> String {
    let mut masked = String::with_capacity(text.len());
    let mut start = 0usize;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            push_masked_line(&text[start..index], &mut masked);
            masked.push('\n');
            start = index + 1;
        }
    }
    push_masked_line(&text[start..], &mut masked);
    masked
}

fn push_masked_line(line: &str, masked: &mut String) {
    if line.trim_start_matches(is_ts_whitespace).starts_with("//") {
        // One space per byte keeps the masked text exactly as long as the
        // document, which is what makes its offsets usable on the original.
        for _ in 0..line.len() {
            masked.push(' ');
        }
    } else {
        masked.push_str(line);
    }
}

/// The characters ECMAScript calls WhiteSpace, the set a trim must strip:
/// notably U+00A0 and U+FEFF count, while a lone `char::is_whitespace` would
/// not treat U+FEFF as space. `secrets.rs` normalizes key text with the same
/// two functions, so both files agree on what a line holds.
pub(crate) fn is_ts_whitespace(character: char) -> bool {
    character.is_ascii_whitespace()
        || matches!(
            character,
            '\u{85}' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
}

/// `String.prototype.trim`.
pub(crate) fn ts_trim(value: &str) -> &str {
    value.trim_matches(is_ts_whitespace)
}

/// The body of a document: one leading byte order mark, if the text still
/// carries one, is not part of the JSON the readers parse.
pub(crate) fn document_body(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// The decoder every text file in the shell goes through: one leading UTF-8
/// byte order mark is dropped, malformed UTF-8 and NUL bytes are refused, and
/// the text is borrowed from the caller's buffer, so a caller holding that
/// buffer in a `Zeroizing` allocation never leaves a second, uncleaned copy
/// behind.
pub(crate) fn decode_utf8_bytes<'a>(
    path: &Path,
    label: MessageId,
    bytes: &'a [u8],
) -> Result<&'a str, ConfigError> {
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    let text = str::from_utf8(body).map_err(|_| {
        ConfigError::new(
            UiMessage::new(MessageId::ConfigFileNotUtf8)
                .with_arg("path", path_arg(path))
                .with_arg("label", message_arg(UiMessage::new(label))),
        )
    })?;
    if text.contains('\0') {
        return Err(ConfigError::new(
            UiMessage::new(MessageId::ConfigFileHasNul)
                .with_arg("path", path_arg(path))
                .with_arg("label", message_arg(UiMessage::new(label))),
        ));
    }
    Ok(text)
}

/// A path as the Windows file APIs want it. `OsStr` is passed through without a
/// lossy round trip, because the path is the one the user chose.
fn wide_path(path: &Path) -> Vec<u16> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}

/* -------------------------------------------------------------------------- */
/* Hotkey grammar                                                             */
/* -------------------------------------------------------------------------- */

/// Validates the launcher hotkey as the conservative subset of the Tauri
/// global-shortcut syntax: optional modifiers followed by exactly one key, as
/// in "F2" or "Ctrl+Shift+Space". "Fn" is never reported to applications,
/// "F12" is reserved by the app, a modifiers-only shortcut would never fire,
/// and a bare "Escape" belongs to cancelling the dictation in progress, so all
/// four are rejected here rather than at registration time.
///
/// The loader deliberately does not call this: a settings file whose chord
/// cannot be registered still starts the launcher disabled, so the settings
/// window can repair it instead of the process refusing to run at all.
pub(crate) fn validate_hotkey(hotkey: &str, settings_path: &Path) -> Result<String, ConfigError> {
    let hotkey = ts_trim(hotkey);
    if hotkey.is_empty() {
        return Err(invalid_hotkey(
            settings_path,
            UiMessage::new(MessageId::ConfigHotkeyRequirement),
        ));
    }

    let parts: Vec<&str> = hotkey.split('+').map(ts_trim).collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(invalid_hotkey(
            settings_path,
            UiMessage::new(MessageId::ConfigHotkeyEmptyPart).with_arg("hotkey", text_arg(hotkey)),
        ));
    }

    let last_index = parts.len() - 1;
    for (index, part) in parts.iter().enumerate() {
        let lower = part.to_lowercase();
        let is_last = index == last_index;

        if lower == "fn" {
            return Err(invalid_hotkey(
                settings_path,
                UiMessage::new(MessageId::ConfigHotkeyFn),
            ));
        }
        if lower == "f12" {
            return Err(invalid_hotkey(
                settings_path,
                UiMessage::new(MessageId::ConfigHotkeyF12),
            ));
        }
        if lower == "escape" && last_index == 0 {
            return Err(invalid_hotkey(
                settings_path,
                UiMessage::new(MessageId::ConfigHotkeyEscape),
            ));
        }
        if HOTKEY_MODIFIERS.contains(&lower.as_str()) {
            if is_last {
                return Err(invalid_hotkey(
                    settings_path,
                    UiMessage::new(MessageId::ConfigHotkeyModifiersOnly)
                        .with_arg("hotkey", text_arg(hotkey)),
                ));
            }
            continue;
        }
        if !is_last {
            return Err(invalid_hotkey(
                settings_path,
                UiMessage::new(MessageId::ConfigHotkeyNotModifier)
                    .with_arg("hotkey", text_arg(hotkey))
                    .with_arg("part", text_arg(part)),
            ));
        }
        if !HOTKEY_NAMED_KEYS.contains(&lower.as_str())
            && !is_single_key_character(&lower)
            && !is_function_key(&lower)
        {
            return Err(invalid_hotkey(
                settings_path,
                UiMessage::new(MessageId::ConfigHotkeyUnsupportedKey)
                    .with_arg("part", text_arg(part)),
            ));
        }
    }

    Ok(hotkey.to_owned())
}

/// A launcher-chord problem named by the file whose setting it is.
fn invalid_hotkey(settings_path: &Path, reason: UiMessage) -> ConfigError {
    ConfigError::new(
        UiMessage::new(MessageId::ConfigHotkeyProblem)
            .with_arg("path", path_arg(settings_path))
            .with_arg("reason", message_arg(reason)),
    )
}

/// `/^[a-z0-9]$/` on the lower-cased part: exactly one ASCII lower-case letter
/// or digit.
fn is_single_key_character(lower: &str) -> bool {
    let mut characters = lower.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) => character.is_ascii_lowercase() || character.is_ascii_digit(),
        _ => false,
    }
}

/// `/^f([1-9]|1[0-9]|2[0-4])$/` on the lower-cased part: F1-F9, F10-F19 and
/// F20-F24, without a leading zero.
fn is_function_key(lower: &str) -> bool {
    let Some(number) = lower.strip_prefix('f') else {
        return false;
    };
    if !matches!(number.len(), 1 | 2)
        || number.starts_with('0')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    number
        .parse::<u8>()
        .map(|value| (1..=24).contains(&value))
        .unwrap_or(false)
}

/* -------------------------------------------------------------------------- */
/* Failure wording                                                            */
/* -------------------------------------------------------------------------- */

/// The Windows code behind a standard library error, when there is one.
fn io_code(error: &io::Error) -> Option<u32> {
    error.raw_os_error().map(|code| code as u32)
}

/// The Win32 code behind an error from the Windows bindings. `Error::from_win32`
/// wraps the system code in `HRESULT_FROM_WIN32`, so the wrapped form is
/// unwrapped again; anything else is left as the `HRESULT` it is, which is all a
/// diagnostic needs.
fn windows_code(error: &windows::core::Error) -> u32 {
    let code = error.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        code & 0xFFFF
    } else {
        code
    }
}

/// The reason a read failed, as a descriptor built from its OS error code; the
/// raw error text is never quoted.
fn read_reason(code: Option<u32>) -> UiMessage {
    let key = match code {
        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND) => {
            MessageId::ConfigReadReasonNotFound
        }
        Some(ERROR_ACCESS_DENIED) => MessageId::ConfigReadReasonDenied,
        Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION) => {
            MessageId::ConfigReadReasonLocked
        }
        _ => MessageId::ConfigReadReasonFailed,
    };
    UiMessage::new(key)
}

/// The same codes as seen by a create or a write; Windows reports a locked or
/// protected destination through the same small set.
fn create_reason(code: Option<u32>) -> UiMessage {
    let key = match code {
        Some(ERROR_ACCESS_DENIED) => MessageId::ConfigCreateReasonDenied,
        Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION) => {
            MessageId::ConfigCreateReasonLocked
        }
        Some(ERROR_FILE_EXISTS) | Some(ERROR_ALREADY_EXISTS) => MessageId::ConfigCreateReasonExists,
        Some(ERROR_DISK_FULL) => MessageId::ConfigCreateReasonNoSpace,
        _ => MessageId::ConfigCreateReasonFailed,
    };
    UiMessage::new(key)
}

/// The reason a rename could not replace the destination.
fn replace_reason(code: u32) -> UiMessage {
    let key = match code {
        ERROR_ACCESS_DENIED => MessageId::ConfigReplaceReasonDenied,
        ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => MessageId::ConfigReplaceReasonLocked,
        ERROR_DISK_FULL => MessageId::ConfigReplaceReasonNoSpace,
        _ => MessageId::ConfigReplaceReasonFailed,
    };
    UiMessage::new(key)
}

/// The reason a security call refused, naming the code because this is the one
/// place where the reason matters for diagnosis and the code is not sensitive.
fn security_reason(code: u32) -> UiMessage {
    match code {
        ERROR_ACCESS_DENIED => UiMessage::new(MessageId::SecurityReasonDenied),
        _ => UiMessage::new(MessageId::SecurityReasonRefused).with_arg("code", code.into()),
    }
}

/// Writes a brand-new file, never replacing one that exists; a racing creator
/// is reported instead of overwritten.
fn write_new(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file
            .write_all(contents)
            .map_err(|error| create_failure(path, io_code(&error))),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(path_problem(
            path,
            UiMessage::new(MessageId::ConfigFileExistsUntouched),
        )),
        Err(error) => Err(create_failure(path, io_code(&error))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A byte order mark, CRLF and nested unknown values survive the patch.
    /// The obsolete `compare_all` and `port` properties disappear - each line
    /// keeps only its own indentation - while unrelated root properties and user
    /// comments remain unchanged.
    #[test]
    fn patching_a_marked_commented_document_keeps_every_other_byte() {
        let document = "\u{feff}// Speechek settings\r\n{\r\n  // Этот файл сохранён в UTF-8 с BOM.\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n  \"port\": 4173,\r\n  \"input_device\": \"wasapi:card-one\",\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"compare_all\": false,\r\n\r\n  // Заметки пользователя и вложенный объект.\r\n  \"notes\": {\r\n    \"text\": \"keep this value\",\r\n    \"nested\": [1, 2, { \"deep\": true }]\r\n  },\r\n  \"trailing\": null,\r\n  // Дополнительное поле пользователя.\r\n  \"extra\": \"kept\"\r\n}\r\n";
        let expected = "\u{feff}// Speechek settings\r\n{\r\n  // Этот файл сохранён в UTF-8 с BOM.\r\n  \"hotkey\": \"Ctrl+Shift+F9\",\r\n  \"mode\": \"verbatim\",\r\n  \"mute_during_recording\": true,\r\n  \r\n  \"input_device\": \"wasapi:card-two\",\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [\"Speechek\"],\r\n  \"dictation_languages\": [\"ru-RU\",\"en-US\"],\r\n  \r\n\r\n  // Заметки пользователя и вложенный объект.\r\n  \"notes\": {\r\n    \"text\": \"keep this value\",\r\n    \"nested\": [1, 2, { \"deep\": true }]\r\n  },\r\n  \"trailing\": null,\r\n  // Дополнительное поле пользователя.\r\n  \"extra\": \"kept\"\r\n}\r\n";

        let patched = patch_settings_document(
            document,
            &Settings {
                hotkey: "Ctrl+Shift+F9".to_owned(),
                mode: "verbatim".to_owned(),
                mute_during_recording: true,
                input_device: Some("wasapi:card-two".to_owned()),
                language: Language::En,
                custom_vocabulary: Arc::from(vec!["Speechek".to_owned()]),
                dictation_languages: Arc::from(vec!["ru-RU".to_owned(), "en-US".to_owned()]),
            },
        )
        .expect("the documented patch");

        assert_eq!(patched, expected);
    }

    /// The optional values a document may not carry: a file from before them
    /// still loads as the documented defaults - `false`, the system default
    /// input device, the detected language, an empty vocabulary and no
    /// dictation languages - and the first patch that changes them writes all
    /// five properties into the document in one insertion, in front of the
    /// first property and in that property's indentation and CRLF, in the fixed
    /// order `mute_during_recording`, `input_device`, `language`,
    /// `custom_vocabulary`, `dictation_languages`, instead of readback refusing
    /// values the file does not hold. Later patches replace the same bytes in
    /// place, and every other byte of the document survives, including an
    /// unknown property between them.
    #[test]
    fn a_document_without_the_optional_values_gains_them_in_one_patch() {
        let path = Path::new("settings.json");
        let document = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"notes\": { \"keep\": true }\r\n}\r\n";

        let loaded = parse_settings_document_with_language(
            document,
            path,
            MessageId::SettingsFileRole,
            Language::En,
        )
            .expect("a document from before the optional values");
        assert!(!loaded.mute_during_recording);
        assert_eq!(loaded.input_device, None);
        assert!(loaded.custom_vocabulary.is_empty());
        assert!(loaded.dictation_languages.is_empty());

        let switched_on = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: true,
            input_device: Some("wasapi:card-one".to_owned()),
            language: Language::En,
            custom_vocabulary: Arc::from(vec!["Speechek".to_owned()]),
            dictation_languages: Arc::from(vec!["ru-RU".to_owned()]),
        };
        let on_document = patch_settings_document(document, &switched_on).expect("a patch");
        assert_eq!(
            on_document,
            "{\r\n  \"mute_during_recording\": true,\r\n  \"input_device\": \"wasapi:card-one\",\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [\"Speechek\"],\r\n  \"dictation_languages\": [\"ru-RU\"],\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"notes\": { \"keep\": true }\r\n}\r\n"
        );
        assert_eq!(
            parse_settings_document_with_language(&on_document, path, MessageId::SettingsFileRole, Language::En)
                .expect("the patched document"),
            switched_on
        );

        let switched_off = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            input_device: None,
            language: Language::En,
            custom_vocabulary: Arc::default(),
            dictation_languages: Arc::default(),
        };
        let off_document =
            patch_settings_document(&on_document, &switched_off).expect("a patch");
        assert_eq!(
            off_document,
            "{\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"notes\": { \"keep\": true }\r\n}\r\n"
        );
        assert_eq!(
            parse_settings_document_with_language(&off_document, path, MessageId::SettingsFileRole, Language::En)
                .expect("the patched document"),
            switched_off
        );
    }

    /// `compare_all` is no longer a managed value: a document that still carries
    /// the root property loads as if it were absent, and the next patch removes
    /// it with one separator comma wherever it sits. Every other byte survives.
    /// A repeated obsolete property is refused because it cannot be removed
    /// unambiguously.
    #[test]
    fn an_obsolete_compare_all_is_ignored_on_read_and_removed_by_the_next_patch() {
        let path = Path::new("settings.json");
        let with = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"compare_all\": true,\r\n  \"notes\": 1\r\n}\r\n";
        let without = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"notes\": 1\r\n}\r\n";
        let expected = Settings {
            hotkey: "F2".to_owned(),
            mode: "smart".to_owned(),
            mute_during_recording: false,
            input_device: None,
            language: Language::En,
            custom_vocabulary: Arc::default(),
            dictation_languages: Arc::default(),
        };

        // Reading: the obsolete property is simply not a value, so an old and a
        // current document describe the same settings.
        assert_eq!(
            parse_settings_document_with_language(with, path, MessageId::SettingsFileRole, Language::En)
                .expect("an old document"),
            expected
        );
        assert_eq!(
            parse_settings_document_with_language(without, path, MessageId::SettingsFileRole, Language::En)
                .expect("a current document"),
            expected
        );

        // Writing: a property in the middle loses the comma that follows it; the
        // indentation and line ending of its own line are all that is left.
        let patched = patch_settings_document(with, &expected).expect("a patch");
        assert_eq!(
            patched,
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \r\n  \"notes\": 1\r\n}\r\n"
        );
        assert_eq!(
            parse_settings_document_with_language(&patched, path, MessageId::SettingsFileRole, Language::En)
                .expect("the patched document"),
            expected
        );

        // Writing: as the last property it loses the comma before it instead.
        let last = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"compare_all\": true\r\n}\r\n";
        assert_eq!(
            patch_settings_document(last, &expected).expect("a patch"),
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": []\r\n  \r\n}\r\n"
        );

        // A repeated obsolete property cannot be migrated by the same rule the
        // managed values use, so neither reading nor writing accepts it.
        let repeated = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"smart\",\n  \"mute_during_recording\": false,\n  \"compare_all\": true,\n  \"compare_all\": false\n}\n";
        parse_settings_document_with_language(repeated, path, MessageId::SettingsFileRole, Language::En)
            .expect_err("a repeated obsolete property");
        patch_settings_document(repeated, &expected)
            .expect_err("a repeated obsolete property must refuse the patch");
    }

    /// The shipped template as the version that still managed `port` wrote it:
    /// the block of six comment lines with the property under them, at the place
    /// that version kept them. The lines come from the constant the migration
    /// removes, so the fixture and the removal cannot drift apart, and the
    /// anchor assertion fails if the template stops holding what the block
    /// followed.
    fn with_released_port_block(template: &str) -> String {
        let released = template.replace(
            "  \"mute_during_recording\": false,\n\n  // input_device:",
            &format!(
                "  \"mute_during_recording\": false,\n\n{}\n  \"port\": 4173,\n\n  // input_device:",
                OBSOLETE_PORT_COMMENTS.join("\n")
            ),
        );
        assert_ne!(released, template, "the template holds the port block's anchor");
        released
    }

    /// Migrating the document the released template shipped is byte-for-byte the
    /// template that ships today, so a profile that already saved the released
    /// file ends up with the file a new installation starts from; the obsolete
    /// property is not a value in either document.
    #[test]
    fn a_document_that_shipped_with_port_migrates_to_the_shipped_template() {
        let path = Path::new("settings.example.json");
        let released = with_released_port_block(SETTINGS_EXAMPLE);
        assert!(
            !SETTINGS_EXAMPLE.contains("\"port\""),
            "the shipped template no longer carries the property"
        );

        let before = parse_settings_document_with_language(
            &released,
            path,
            MessageId::SettingsTemplateRole,
            Language::En,
        )
        .expect("the released template");
        let after = parse_settings_document_with_language(
            SETTINGS_EXAMPLE,
            path,
            MessageId::SettingsTemplateRole,
            Language::En,
        )
        .expect("the shipped template");
        assert_eq!(before, after, "the obsolete port is not a value");

        assert_eq!(
            patch_settings_document(&released, &before).expect("a patch"),
            SETTINGS_EXAMPLE,
            "the migrated document is the shipped template exactly"
        );
    }

    /// An obsolete root `port` is ignored on read and removed by the next patch
    /// together with the comment block the released template wrote with it: the
    /// six comment lines, the property's own line and the blank line that closed
    /// the block. The block is matched linewise, so the document's own line
    /// endings - here CRLF - decide nothing.
    #[test]
    fn an_obsolete_port_is_ignored_on_read_and_removed_by_the_next_patch() {
        let path = Path::new("settings.json");
        let block = OBSOLETE_PORT_COMMENTS.join("\r\n");
        let with = format!(
            "{{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n\r\n{block}\r\n  \"port\": 5000,\r\n\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"notes\": \"kept\"\r\n}}\r\n"
        );
        let without = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"notes\": \"kept\"\r\n}\r\n";

        let loaded = parse_settings_document_with_language(
            &with,
            path,
            MessageId::SettingsFileRole,
            Language::En,
        )
        .expect("a document that still carries the port");
        assert_eq!(
            loaded,
            parse_settings_document_with_language(
                without,
                path,
                MessageId::SettingsFileRole,
                Language::En,
            )
            .expect("the same document without the property"),
            "the obsolete port is not a value"
        );
        assert_eq!(
            patch_settings_document(&with, &loaded).expect("a patch"),
            without,
            "the block and the property leave, every other byte stays"
        );

        // As the last property the block still leaves, and the comma that
        // separated the property from the property before the block goes with
        // it, so the document stays readable.
        let last = format!(
            "{{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"notes\": \"kept\",\r\n\r\n{block}\r\n  \"port\": 5000\r\n}}\r\n"
        );
        assert_eq!(
            patch_settings_document(&last, &loaded).expect("a patch"),
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"notes\": \"kept\"\r\n\r\n}\r\n"
        );
    }

    /// A repeated obsolete `port` cannot be migrated unambiguously, so neither
    /// the loader nor the patch accepts it.
    #[test]
    fn a_repeated_obsolete_port_is_refused() {
        let path = Path::new("settings.json");
        let base = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": [],\n  \"dictation_languages\": []\n}\n";
        let settings = parse_settings_document_with_language(
            base,
            path,
            MessageId::SettingsFileRole,
            Language::En,
        )
        .expect("a document with every managed value");
        let repeated = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": [],\n  \"dictation_languages\": [],\n  \"port\": 4173,\n  \"port\": 4174\n}\n";

        let error = parse_settings_document_with_language(
            repeated,
            path,
            MessageId::SettingsFileRole,
            Language::En,
        )
        .expect_err("a repeated obsolete property");
        assert!(
            serde_json::to_string(&error.ui)
                .expect("a message serializes")
                .contains("ConfigPropertyRepeated"),
            "{:?}",
            error.ui
        );

        let error = patch_settings_document(repeated, &settings)
            .expect_err("a repeated obsolete property must refuse the patch");
        assert_eq!(error.ui.key, MessageId::ConfigPropertyRepeated);
    }

    /// Only the released comment block leaves with the property: a comment the
    /// user wrote above the block survives where it was, and a comment written
    /// between the block and the property keeps the whole block, because the
    /// property then no longer stands under the shipped lines.
    #[test]
    fn a_user_comment_around_an_obsolete_port_survives() {
        let path = Path::new("settings.json");
        let block = OBSOLETE_PORT_COMMENTS.join("\n");
        let settings = parse_settings_document_with_language(
            "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": [],\n  \"dictation_languages\": []\n}\n",
            path,
            MessageId::SettingsFileRole,
            Language::En,
        )
        .expect("a document with every managed value");

        let above = format!(
            "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  // моя заметка\n{block}\n  \"port\": 43118,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": [],\n  \"dictation_languages\": []\n}}\n"
        );
        assert_eq!(
            patch_settings_document(&above, &settings).expect("a patch"),
            "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  // моя заметка\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": [],\n  \"dictation_languages\": []\n}\n"
        );

        let inside = format!(
            "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n{block}\n  // моя заметка\n  \"port\": 43118,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": [],\n  \"dictation_languages\": []\n}}\n"
        );
        assert_eq!(
            patch_settings_document(&inside, &settings).expect("a patch"),
            format!(
                "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n{block}\n  // моя заметка\n  \n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": [],\n  \"dictation_languages\": []\n}}\n"
            )
        );
    }

    /// `null` is the stored form of the system default input device - a value
    /// that says "use the system device", not a missing property - and a
    /// well-formed device id is kept as it stands even when no connected device
    /// wears it, because whether the device is there is decided when a recording
    /// opens and not when the file is read. A document that already carries
    /// every managed value is patched back byte for byte.
    #[test]
    fn a_null_device_is_the_system_default_and_a_stored_id_is_kept() {
        let path = Path::new("settings.json");
        let default_device = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": [],\n  \"dictation_languages\": []\n}\n";
        let loaded = parse_settings_document_with_language(
            default_device,
            path,
            MessageId::SettingsFileRole,
            Language::En,
        )
            .expect("a null device");
        assert_eq!(loaded.input_device, None);
        assert_eq!(
            patch_settings_document(default_device, &loaded).expect("a patch"),
            default_device
        );

        let chosen = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": \"wasapi:card-one\",\n  \"language\": \"en\",\n  \"custom_vocabulary\": [\"Speechek\"],\n  \"dictation_languages\": [\"ru-RU\",\"en-US\"]\n}\n";
        let loaded = parse_settings_document_with_language(
            chosen,
            path,
            MessageId::SettingsFileRole,
            Language::En,
        )
        .expect("a stored device id");
        assert_eq!(loaded.input_device.as_deref(), Some("wasapi:card-one"));
        assert_eq!(&*loaded.custom_vocabulary, ["Speechek"]);
        assert_eq!(&*loaded.dictation_languages, ["ru-RU", "en-US"]);
        assert_eq!(
            patch_settings_document(chosen, &loaded).expect("a patch"),
            chosen
        );
    }

    /// An `input_device` that is present has to be usable: a number, a boolean,
    /// an object, an empty string and a string no `cpal::DeviceId` can parse all
    /// refuse the document; a repeated property is refused rather than one of the
    /// two values winning. No message quotes the value it refused, and neither
    /// the loader nor the patch substitutes one.
    #[test]
    fn a_device_that_is_not_usable_is_refused() {
        let path = Path::new("settings.json");
        let document_with = |value: &str| {
            format!(
                "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": {value}\n}}\n"
            )
        };
        let settings = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            input_device: None,
            language: Language::En,
            custom_vocabulary: Arc::default(),
            dictation_languages: Arc::default(),
        };

        for value in ["5", "\"\"", "\"not-a-device-id\"", "\"wasapi:\"", "true", "{}"] {
            let document = document_with(value);
            parse_settings_document_with_language(
                &document,
                path,
                MessageId::SettingsFileRole,
                Language::En,
            )
                .expect_err("an unusable device");
            patch_settings_document(&document, &settings)
                .expect_err("an unusable device must refuse the patch");
        }

        // A repeated property is refused by both paths instead of one of the two
        // values winning.
        let document = format!(
            "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": \"wasapi:card-one\",\n  \"input_device\": \"wasapi:card-two\"\n}}\n"
        );
        parse_settings_document_with_language(
            &document,
            path,
            MessageId::SettingsFileRole,
            Language::En,
        )
            .expect_err("a repeated property");
        patch_settings_document(&document, &settings)
            .expect_err("a repeated property must refuse the patch");
    }

    /// A general change carries every managed value: patching with a new hotkey,
    /// mode and mute switch writes the chosen device too, and the port an
    /// earlier version managed is retired, so an old value is never left behind
    /// under a new configuration while unknown properties keep their bytes.
    #[test]
    fn a_general_change_does_not_leave_an_old_port_or_device_behind() {
        let path = Path::new("settings.json");
        let document = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"port\": 5000,\n  \"input_device\": \"wasapi:old\",\n  \"language\": \"en\",\n  \"custom_vocabulary\": [\"OldTerm\"],\n  \"dictation_languages\": [\"de-DE\"],\n  \"notes\": \"kept\"\n}\n";
        let changed = Settings {
            hotkey: "Ctrl+Shift+F9".to_owned(),
            mode: "verbatim".to_owned(),
            mute_during_recording: true,
            input_device: Some("wasapi:new".to_owned()),
            language: Language::En,
            custom_vocabulary: Arc::from(vec!["NewTerm".to_owned()]),
            dictation_languages: Arc::from(vec!["ru-RU".to_owned(), "en-US".to_owned()]),
        };

        let patched = patch_settings_document(document, &changed).expect("a patch");
        assert_eq!(
            patched,
            "{\n  \"hotkey\": \"Ctrl+Shift+F9\",\n  \"mode\": \"verbatim\",\n  \"mute_during_recording\": true,\n  \n  \"input_device\": \"wasapi:new\",\n  \"language\": \"en\",\n  \"custom_vocabulary\": [\"NewTerm\"],\n  \"dictation_languages\": [\"ru-RU\",\"en-US\"],\n  \"notes\": \"kept\"\n}\n"
        );
        assert!(!patched.contains("5000"), "the old port is gone");
        assert!(!patched.contains("wasapi:old"), "the old device is gone");
        assert!(!patched.contains("OldTerm"), "the old vocabulary is gone");
        assert!(!patched.contains("de-DE"), "the old dictation languages are gone");
        assert_eq!(
            parse_settings_document_with_language(&patched, path, MessageId::SettingsFileRole, Language::En)
                .expect("the patched document"),
            changed
        );
    }

    /// The production template documents the release defaults and already
    /// carries every managed value: it loads as those settings, and patching it
    /// back with exactly those settings - what the settings window would do
    /// without a change - returns the template byte for byte, so the file a new
    /// install starts from is never reformatted.
    #[test]
    fn the_first_run_template_carries_the_documented_defaults() {
        let path = Path::new("settings.example.json");
        let production = profile::defaults(BuildFlavor::Production);
        let loaded = parse_settings_document_with_language(
            SETTINGS_EXAMPLE,
            path,
            MessageId::SettingsTemplateRole,
            Language::En,
        )
            .expect("the embedded template");
        assert_eq!(loaded.hotkey, production.hotkey);
        assert_eq!(loaded.mode, "live");
        assert!(!loaded.mute_during_recording);
        assert_eq!(loaded.input_device, None);
        assert_eq!(loaded.language, Language::En);
        assert!(loaded.custom_vocabulary.is_empty());

        assert_eq!(
            patch_settings_document(SETTINGS_EXAMPLE, &loaded).expect("a patch"),
            SETTINGS_EXAMPLE
        );
    }

    /// The settings window's text becomes one term per line: edges trimmed with
    /// the loader's whitespace rule, blank lines dropped and exact repeats
    /// dropped after their first occurrence, in the order typed. Case is part of
    /// the spelling, so two spellings stay two terms.
    #[test]
    fn vocabulary_text_is_trimmed_deduplicated_and_kept_in_order() {
        let terms = vocabulary_from_text("  Speechek \r\n\r\nKubernetes\nSpeechek\n\u{a0}BigQuery")
            .expect("a short list");
        assert_eq!(&*terms, ["Speechek", "Kubernetes", "BigQuery"]);

        let spellings = vocabulary_from_text("speechek\nSpeechek").expect("a short list");
        assert_eq!(&*spellings, ["speechek", "Speechek"]);
    }

    /// The limit counts the terms that would be sent, after blank lines and
    /// repeats are gone, and a list above it is refused with its real size
    /// instead of being cut short.
    #[test]
    fn the_vocabulary_limit_counts_terms_after_normalization() {
        let distinct: Vec<String> = (0..MAX_CUSTOM_VOCABULARY).map(|n| format!("term{n}")).collect();
        let mut at_limit = distinct.join("\n");
        at_limit.push_str("\n\nterm0\nterm1\nterm2\nterm3\nterm4\n");
        assert_eq!(
            vocabulary_from_text(&at_limit).expect("repeats do not count").len(),
            MAX_CUSTOM_VOCABULARY
        );

        let over = format!("{}\nterm{MAX_CUSTOM_VOCABULARY}", distinct.join("\n"));
        assert_eq!(
            vocabulary_from_text(&over).expect_err("one term too many"),
            MAX_CUSTOM_VOCABULARY + 1
        );
    }

    /// A stored vocabulary is written compactly, without escaping Cyrillic, and
    /// reads back unchanged. A value that is not a list of strings, a term with
    /// a line break and a list over the limit refuse the document and the patch
    /// with the property's own name, while edge whitespace, blank terms and
    /// repeats in a hand-edited file are normalized rather than stopping the
    /// launcher.
    #[test]
    fn a_stored_vocabulary_round_trips_and_a_malformed_one_is_refused() {
        let path = Path::new("settings.json");
        let document_with = |value: &str| {
            format!(
                "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": {value},\n  \"dictation_languages\": []\n}}\n"
            )
        };
        let settings = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            input_device: None,
            language: Language::En,
            custom_vocabulary: Arc::from(vec!["Speechek".to_owned(), "Кубернетес".to_owned()]),
            dictation_languages: Arc::default(),
        };

        let patched = patch_settings_document(&document_with("[]"), &settings).expect("a patch");
        assert_eq!(patched, document_with(r#"["Speechek","Кубернетес"]"#));
        assert_eq!(
            parse_settings_document_with_language(&patched, path, MessageId::SettingsFileRole, Language::En)
                .expect("the patched document"),
            settings
        );

        let over = format!(
            "[{}]",
            (0..=MAX_CUSTOM_VOCABULARY)
                .map(|n| format!("\"term{n}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let names_the_vocabulary = |ui: &UiMessage| {
            let text = serde_json::to_string(ui).expect("a message serializes");
            text.contains("ConfigPropertyInvalid") && text.contains("\"custom_vocabulary\"")
        };
        for value in ["\"Speechek\"", "[1]", r#"["a\nb"]"#, "null", over.as_str()] {
            let document = document_with(value);
            let error = parse_settings_document_with_language(
                &document,
                path,
                MessageId::SettingsFileRole,
                Language::En,
            )
            .expect_err("a malformed vocabulary");
            assert!(names_the_vocabulary(&error.ui), "{value}: {:?}", error.ui);
            let error = patch_settings_document(&document, &settings)
                .expect_err("a malformed stored vocabulary must refuse the patch");
            assert_eq!(error.ui.key, MessageId::ConfigPropertyInvalid);
            assert!(names_the_vocabulary(&error.ui), "{value}: {:?}", error.ui);
        }

        let hand_edited = document_with(r#"[" Speechek ", "Speechek", ""]"#);
        let loaded =
            parse_settings_document_with_language(&hand_edited, path, MessageId::SettingsFileRole, Language::En)
                .expect("a hand-edited list is normalized");
        assert_eq!(&*loaded.custom_vocabulary, ["Speechek"]);

        // The writer never stores a list the next read would change.
        let untrimmed = Settings {
            custom_vocabulary: Arc::from(vec![" Speechek".to_owned()]),
            ..settings
        };
        let error = patch_settings_document(&document_with("[]"), &untrimmed)
            .expect_err("a list that is not normalized");
        assert!(names_the_vocabulary(&error.ui), "{:?}", error.ui);
    }

    /// A document without dictation languages reads as Auto-detect, a stored
    /// list reads in the user's order, and the patch adds the member to a
    /// document from before it existed. Unlike the vocabulary, a hand-edited
    /// list is never normalized: an unknown code, another spelling, a repeat,
    /// a third code or another JSON type refuses the document and the patch.
    #[test]
    fn stored_dictation_languages_round_trip_and_an_invalid_list_is_refused() {
        let path = Path::new("settings.json");
        let base = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"custom_vocabulary\": []";
        let without = format!("{base}\n}}\n");
        let document_with = |value: &str| format!("{base},\n  \"dictation_languages\": {value}\n}}\n");
        let read = |document: &str| {
            parse_settings_document_with_language(document, path, MessageId::SettingsFileRole, Language::En)
        };

        let auto = read(&without).expect("a document without the property");
        assert!(auto.dictation_languages.is_empty(), "a missing property is Auto-detect");
        let stored = read(&document_with(r#"["ru-RU", "en-US"]"#)).expect("a stored list");
        assert_eq!(&*stored.dictation_languages, ["ru-RU", "en-US"]);

        let chosen = Settings {
            dictation_languages: Arc::from(vec!["ru-RU".to_owned(), "en-US".to_owned()]),
            ..auto.clone()
        };
        let patched = patch_settings_document(&without, &chosen).expect("a patch");
        assert_eq!(
            &*read(&patched).expect("the patched document").dictation_languages,
            ["ru-RU", "en-US"]
        );

        let names_the_languages = |ui: &UiMessage| {
            let text = serde_json::to_string(ui).expect("a message serializes");
            text.contains("ConfigPropertyInvalid") && text.contains("\"dictation_languages\"")
        };
        for value in [
            "\"ru-RU\"",
            r#"["xx-XX"]"#,
            r#"["ru-ru"]"#,
            r#"["ru-RU", "ru-RU"]"#,
            r#"["ru-RU", "en-US", "de-DE"]"#,
            "[1]",
            "null",
        ] {
            let document = document_with(value);
            let error = read(&document).expect_err("an invalid list");
            assert!(names_the_languages(&error.ui), "{value}: {:?}", error.ui);
            let error = patch_settings_document(&document, &auto)
                .expect_err("an invalid stored list must refuse the patch");
            assert!(names_the_languages(&error.ui), "{value}: {:?}", error.ui);
        }

        // The writer never stores a list the next read would refuse.
        for codes in [vec!["xx-XX"], vec!["ru-RU", "ru-RU"], vec!["ru-RU", "en-US", "de-DE"]] {
            let invalid = Settings {
                dictation_languages: codes.iter().map(|code| (*code).to_owned()).collect(),
                ..auto.clone()
            };
            let error = patch_settings_document(&without, &invalid)
                .expect_err("an invalid list must not be written");
            assert!(names_the_languages(&error.ui), "{codes:?}: {:?}", error.ui);
        }
    }

    /// Readback refuses a document that still carries an obsolete root property,
    /// so a patch can never report success on a file it failed to migrate even
    /// when the managed values themselves read back correctly.
    #[test]
    fn readback_refuses_a_residual_obsolete_property() {
        let settings = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            input_device: None,
            language: Language::En,
            custom_vocabulary: Arc::default(),
            dictation_languages: Arc::default(),
        };
        for name in ["compare_all", "port"] {
            let document = format!(
                "{{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"{name}\": true\r\n}}\r\n"
            );
            let error = verify_patched(&document, &settings)
                .expect_err("a residual obsolete property must refuse the readback");
            assert_eq!(error.ui.key, MessageId::ConfigObsoletePropertyNotRemoved);
            assert_eq!(
                error
                    .ui
                    .args
                    .as_ref()
                    .and_then(|args| args.get("name"))
                    .and_then(|value| value.as_str()),
                Some(name),
                "the refusal names the property it could not remove"
            );
        }

        // The same document without the property passes, so the refusal is about
        // the residual property and not about the settings it carries.
        verify_patched(
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\"\r\n}\r\n",
            &settings,
        )
        .expect("a migrated document reads back");
    }

    /// A value that is present has to be a JSON boolean: a string, a number, a
    /// null or a repeated property refuses the document rather than being
    /// coerced, and no message quotes the value it refused.
    #[test]
    fn a_mute_switch_that_is_not_a_boolean_is_refused() {
        let path = Path::new("settings.json");
        for value in ["\"true\"", "1", "null", "{}"] {
            let document = format!(
                "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": {value}\n}}\n"
            );
            parse_settings_document_with_language(
                &document,
                path,
                MessageId::SettingsFileRole,
                Language::En,
            )
                .expect_err("a non-boolean switch");
        }

        let duplicated = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"mute_during_recording\": true\n}\n";
        parse_settings_document_with_language(duplicated, path, MessageId::SettingsFileRole, Language::En)
            .expect_err("a repeated switch");

        // The patch reads the document through the same rule, so a repeated
        // property refuses the write instead of one of the two values winning.
        patch_settings_document(duplicated, &Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: true,
            input_device: None,
            language: Language::En,
            custom_vocabulary: Arc::default(),
            dictation_languages: Arc::default(),
        })
        .expect_err("a repeated switch must refuse the patch");
    }

    /// A dictation keeps the configuration it started with while a general
    /// change publishes a new revision: a tagged request must never fall forward
    /// to a mode or a mute switch it did not start with, an unchanged key list
    /// must keep the rotation the take was already using, and retiring another
    /// generation must not touch the one that is still pinned.
    #[test]
    fn a_pinned_take_keeps_its_settings_and_ring_across_a_publish() {
        let ring = Arc::new(KeyRing::new(
            crate::secrets::normalize_key_text("FAKE_KEY_ONE\nFAKE_KEY_TWO")
                .expect("a valid fake list")
                .keys()
                .to_vec(),
        ));
        let pinned = Arc::new(RuntimeSnapshot {
            settings: Settings {
                hotkey: "F2".to_owned(),
                mode: "live".to_owned(),
                mute_during_recording: false,
                input_device: None,
                language: Language::En,
                custom_vocabulary: Arc::default(),
                dictation_languages: Arc::default(),
            },
            keys: Arc::clone(&ring),
            revision: 1,
        });
        let runtime = SharedRuntime::new(Arc::clone(&pinned));
        runtime.pin(9, Arc::clone(&pinned));

        // The take that starts now consumes the first turn of the ring.
        assert_eq!(ring.next_key().expect("a key").as_str(), "FAKE_KEY_ONE");

        // A general change - here a new mode together with the mute switch -
        // publishes a new revision while the list is unchanged, so the running
        // ring is reused rather than restarted, and the take keeps the
        // configuration it started with.
        runtime.publish(Arc::new(RuntimeSnapshot {
            settings: Settings {
                hotkey: "F2".to_owned(),
                mode: "smart".to_owned(),
                mute_during_recording: true,
                input_device: Some("wasapi:card-one".to_owned()),
                language: Language::En,
                custom_vocabulary: Arc::default(),
                dictation_languages: Arc::default(),
            },
            keys: Arc::clone(&ring),
            revision: 2,
        }));

        let pinned_snapshot = runtime.resolve(Some(9)).expect("the pinned take");
        assert_eq!(pinned_snapshot.settings.mode, "live");
        assert!(!pinned_snapshot.settings.mute_during_recording);
        assert_eq!(pinned_snapshot.settings.input_device, None);
        let current = runtime.resolve(None).expect("current");
        assert_eq!(current.settings.mode, "smart");
        assert_eq!(
            current.settings.input_device.as_deref(),
            Some("wasapi:card-one"),
            "the next session sees the published device"
        );
        assert!(
            current.settings.mute_during_recording,
            "the next session sees the published switch"
        );

        // The rotation continues where the take left it instead of restarting.
        assert_eq!(
            runtime
                .resolve(Some(9))
                .expect("the pinned take")
                .keys
                .next_key()
                .expect("a key")
                .as_str(),
            "FAKE_KEY_TWO"
        );

        // Retiring another generation leaves this take alone; retiring its own
        // ends it.
        runtime.retire(8);
        assert!(runtime.resolve(Some(9)).is_some());
        runtime.retire(9);
        assert!(runtime.resolve(Some(9)).is_none());
    }

    /// Isolated tree for the storage regression matrix: the simulated
    /// `%APPDATA%` directory and the directory a development executable runs
    /// from, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "speechek-settings-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&path).expect("a scratch directory");
            Self(path)
        }

        /// The simulated `%APPDATA%` directory.
        fn appdata(&self) -> &Path {
            &self.0
        }

        /// The directory the development build runs from: its `settings.json`
        /// and `secrets.bin` live here.
        fn dev_bin(&self) -> PathBuf {
            self.0.join("dev-bin")
        }

        /// The absolute executable path the development flavor resolves its
        /// document from.
        fn executable(&self) -> PathBuf {
            self.dev_bin().join("speechek.exe")
        }

        fn target(&self) -> PathBuf {
            self.target_of(profile::ACTIVE)
        }

        /// The directory one flavor keeps its document in: the flavor's own
        /// directory under the simulated `%APPDATA%` for production and test,
        /// and the executable's directory for the development flavor, which
        /// never resolves through `%APPDATA%`.
        fn target_of(&self, flavor: BuildFlavor) -> PathBuf {
            match flavor {
                BuildFlavor::Development => self.dev_bin(),
                BuildFlavor::Production | BuildFlavor::Test => {
                    self.0.join(profile::defaults(flavor).directory)
                }
            }
        }

        fn target_settings(&self) -> PathBuf {
            self.target().join(SETTINGS_FILE_NAME)
        }
    }

    /// The `executable` argument the regression matrix passes for the
    /// development flavor, and `None` for the flavors that resolve through
    /// `%APPDATA%` alone.
    fn active_executable(scratch: &Scratch) -> Option<PathBuf> {
        (profile::ACTIVE == BuildFlavor::Development).then(|| scratch.executable())
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Writing the first-run document touches Speechek's own profile only: a
    /// directory another application keeps beside it, and the document in it,
    /// are never read, and an existing Speechek document is left as it is.
    #[test]
    fn the_first_run_document_leaves_unrelated_directories_alone() {
        let scratch = Scratch::new("first-run-siblings");
        let unrelated = scratch.appdata().join("another-application");
        fs::create_dir_all(&unrelated).expect("the unrelated directory");
        fs::write(unrelated.join(SETTINGS_FILE_NAME), b"not a settings document")
            .expect("the unrelated document");

        assert!(create_default_if_missing(&scratch.target_settings())
            .expect("the first-run Speechek profile"));
        assert_eq!(
            load_settings(&scratch.target_settings())
                .expect("the first-run settings")
                .hotkey,
            profile::defaults(profile::ACTIVE).hotkey,
            "the first-run document carries the active flavor's own chord"
        );
        assert!(!create_default_if_missing(&scratch.target_settings())
            .expect("an existing Speechek profile"));
        assert_eq!(
            fs::read(unrelated.join(SETTINGS_FILE_NAME)).expect("the unrelated document"),
            b"not a settings document"
        );
    }

    /// The flavor table is the single source of the identities and hotkeys,
    /// every row is distinct, only the development row names no `%APPDATA%`
    /// directory, and the compiled-in flavor matches the kind of build this
    /// test runs in.
    #[test]
    fn the_flavor_table_holds_each_builds_own_identity() {
        let production = profile::defaults(BuildFlavor::Production);
        assert_eq!(production.identifier, "app.speechek.desktop");
        assert_eq!(production.title, "Speechek");
        assert_eq!(production.directory, "Speechek");
        assert_eq!(production.hotkey, "F2");

        let development = profile::defaults(BuildFlavor::Development);
        let test = profile::defaults(BuildFlavor::Test);
        assert_eq!(development.identifier, "app.speechek.dev");
        assert_eq!(development.title, "Speechek Dev");
        assert_eq!(development.hotkey, "Ctrl+Shift+F9");
        assert_eq!(
            development.directory, "",
            "the development flavor resolves beside its executable, not through %APPDATA%"
        );
        assert_eq!(test.identifier, "app.speechek.test");
        assert_eq!(test.title, "Speechek Test");
        assert_eq!(test.directory, "Speechek-Test");
        assert_eq!(test.hotkey, "Ctrl+Shift+F10");

        for (left, right) in [
            (&production, &development),
            (&production, &test),
            (&development, &test),
        ] {
            assert_ne!(left.identifier, right.identifier);
            assert_ne!(left.title, right.title);
            assert_ne!(left.hotkey, right.hotkey);
        }
        assert!(
            !production.directory.is_empty() && !test.directory.is_empty(),
            "the flavors that resolve through %APPDATA% name their directory"
        );

        #[cfg(not(debug_assertions))]
        assert_eq!(profile::ACTIVE, BuildFlavor::Production);
        #[cfg(all(debug_assertions, feature = "test-provider"))]
        assert_eq!(profile::ACTIVE, BuildFlavor::Test);
        #[cfg(all(debug_assertions, not(feature = "test-provider")))]
        assert_eq!(profile::ACTIVE, BuildFlavor::Development);
    }

    /// The resolver, flavor by flavor: the test flavor takes an absolute
    /// `SPEECHEK_CONFIG_PATH` (even without `%APPDATA%`) and refuses a
    /// relative one, while a blank value is no override at all; its
    /// `%APPDATA%` fallback names its own directory. The development flavor
    /// resolves beside the executable it is given and refuses without a
    /// usable one, never through `%APPDATA%` or the working directory.
    /// Production ignores the override and the executable entirely and still
    /// needs `%APPDATA%`.
    #[test]
    fn the_settings_path_resolver_keeps_the_flavors_apart() {
        let scratch = Scratch::new("resolver");
        let appdata = Some(scratch.appdata().as_os_str());
        let elsewhere = scratch.appdata().join("elsewhere.json");
        let executable = scratch.executable();
        let executable_arg = Some(executable.as_path());
        let dev_document = scratch.dev_bin().join(SETTINGS_FILE_NAME);

        // Each flavor resolves its own document from nothing but its own
        // source: `%APPDATA%` for production and test, the executable for
        // development. A blank override changes nothing for any of them.
        for flavor in [
            BuildFlavor::Production,
            BuildFlavor::Development,
            BuildFlavor::Test,
        ] {
            assert_eq!(
                resolve_settings_path(flavor, None, appdata, executable_arg)
                    .expect("the flavor's document"),
                scratch.target_of(flavor).join(SETTINGS_FILE_NAME),
                "{flavor:?} keeps its own document"
            );
            assert_eq!(
                resolve_settings_path(flavor, Some(OsStr::new("   ")), appdata, executable_arg)
                    .expect("a blank override is no override"),
                scratch.target_of(flavor).join(SETTINGS_FILE_NAME)
            );
        }

        // The development document is beside the executable and follows it
        // alone: an inherited production override and an arbitrary location
        // are ignored, and `%APPDATA%` never takes part.
        assert_eq!(
            resolve_settings_path(
                BuildFlavor::Development,
                Some(elsewhere.as_os_str()),
                appdata,
                executable_arg,
            )
            .expect("the development document"),
            dev_document
        );
        assert_eq!(
            resolve_settings_path(BuildFlavor::Development, None, None, executable_arg)
                .expect("the development document without %APPDATA%"),
            dev_document
        );
        assert_eq!(
            resolve_settings_path(
                BuildFlavor::Development,
                Some(Path::new("relative.json").as_os_str()),
                None,
                executable_arg,
            )
            .expect("a relative override cannot move the development document"),
            dev_document
        );

        // Without a usable executable path the development flavor refuses
        // rather than falling back to a working directory or `%APPDATA%`.
        for unusable in [
            None,
            Some(Path::new("speechek.exe")),
            Some(Path::new("relative/speechek.exe")),
        ] {
            resolve_settings_path(BuildFlavor::Development, None, appdata, unusable)
                .expect_err("an unusable development executable");
        }
        assert!(
            !dev_document.exists(),
            "a refused development resolution creates nothing"
        );

        // The test flavor takes an absolute override with or without
        // `%APPDATA%`, and refuses a relative one with the variable's name.
        assert_eq!(
            resolve_settings_path(BuildFlavor::Test, Some(elsewhere.as_os_str()), appdata, None)
                .expect("the absolute override"),
            elsewhere
        );
        assert_eq!(
            resolve_settings_path(BuildFlavor::Test, Some(elsewhere.as_os_str()), None, None)
                .expect("the override alone is enough"),
            elsewhere
        );
        resolve_settings_path(
            BuildFlavor::Test,
            Some(Path::new("relative.json").as_os_str()),
            appdata,
            None,
        )
        .expect_err("a relative override");

        // The test flavor's `%APPDATA%` fallback and its refusal name its own
        // directory.
        assert_eq!(
            resolve_settings_path(BuildFlavor::Test, None, appdata, None)
                .expect("the test document without an override"),
            scratch.target_of(BuildFlavor::Test).join(SETTINGS_FILE_NAME)
        );
        resolve_settings_path(BuildFlavor::Test, None, None, None)
            .expect_err("no %APPDATA%");

        // Production ignores the override, relative or absolute, and the
        // executable path with it, and still needs `%APPDATA%`.
        for override_path in [
            Path::new("relative.json").as_os_str(),
            elsewhere.as_os_str(),
        ] {
            assert_eq!(
                resolve_settings_path(
                    BuildFlavor::Production,
                    Some(override_path),
                    appdata,
                    executable_arg,
                )
                .expect("production never reads the override"),
                scratch
                    .target_of(BuildFlavor::Production)
                    .join(SETTINGS_FILE_NAME)
            );
        }
        resolve_settings_path(
            BuildFlavor::Production,
            Some(elsewhere.as_os_str()),
            None,
            None,
        )
        .expect_err("production still needs %APPDATA%");
    }

    /// The first run and the resolver agree on one document per flavor: the
    /// resolver names the flavor's own file - beside the executable for the
    /// development flavor, under `%APPDATA%` for production and test - and the
    /// active flavor's first-run document loads with that flavor's own chord.
    #[test]
    fn the_first_run_document_resolves_one_document_per_flavor() {
        let scratch = Scratch::new("first-run-agreement");
        let appdata = Some(scratch.appdata().as_os_str());
        let executable = scratch.executable();
        let executable_arg = Some(executable.as_path());

        for flavor in [
            BuildFlavor::Production,
            BuildFlavor::Development,
            BuildFlavor::Test,
        ] {
            let path = resolve_settings_path(flavor, None, appdata, executable_arg)
                .expect("the flavor's document");
            assert_eq!(path, scratch.target_of(flavor).join(SETTINGS_FILE_NAME));
            assert!(
                !path.exists() && !scratch.target_of(flavor).exists(),
                "resolving the document creates no profile directory"
            );
        }

        // The key container follows the resolved document: a development run
        // keeps `secrets.bin` beside its own `settings.json`, never in the
        // production or test profile.
        let dev_document = scratch.dev_bin().join(SETTINGS_FILE_NAME);
        let dev_vault = crate::secrets::secrets_path(&dev_document);
        assert_eq!(dev_vault, scratch.dev_bin().join("secrets.bin"));
        for flavor in [BuildFlavor::Production, BuildFlavor::Test] {
            assert_ne!(
                dev_vault,
                crate::secrets::secrets_path(&scratch.target_of(flavor).join(SETTINGS_FILE_NAME)),
                "the development vault is not the {flavor:?} vault"
            );
        }

        // A clean profile for the active flavor: writing the first-run document
        // produces a valid settings file the settings window would not reformat.
        let fresh = Scratch::new("first-run");
        let fresh_appdata = Some(fresh.appdata().as_os_str());
        let flavor = profile::ACTIVE;
        let defaults = profile::defaults(flavor);
        let fresh_executable = active_executable(&fresh);
        let path = resolve_settings_path(flavor, None, fresh_appdata, fresh_executable.as_deref())
            .expect("the active flavor's path");
        assert_eq!(
            path,
            fresh.target_of(flavor).join(SETTINGS_FILE_NAME),
            "the active flavor resolves the document its own source names"
        );
        assert!(create_default_if_missing(&path).expect("the first-run document"));
        assert!(!create_default_if_missing(&path).expect("an existing document"));
        let first_run = load_settings(&path).expect("the first-run document loads");
        assert_eq!(first_run.hotkey, defaults.hotkey);
        assert_eq!(first_run.mode, "live");
        assert!(!first_run.mute_during_recording);
        assert_eq!(first_run.input_device, None);

        let text = fs::read_to_string(&path).expect("the first-run document");
        assert_eq!(
            patch_settings_document(&text, &first_run).expect("a no-op patch"),
            text,
            "the first-run document is already the settings it loads as"
        );
        if flavor == BuildFlavor::Production {
            assert_eq!(
                fs::read(&path).expect("the first-run document"),
                SETTINGS_EXAMPLE.as_bytes(),
                "production writes the embedded template exactly"
            );
        } else {
            assert_ne!(
                fs::read(&path).expect("the first-run document"),
                SETTINGS_EXAMPLE.as_bytes(),
                "a debug flavor moves the template to its own first-run values"
            );
        }
    }

    /// The first-run document: a flavor's own chord and language are written
    /// into the annotated template, and the two debug flavors do not share one
    /// document. A template that already spells every wanted value is returned
    /// unchanged.
    #[test]
    fn the_first_run_document_moves_only_the_flavor_values() {
        let document = first_run_document(BuildFlavor::Production, Language::En)
            .expect("the production document");
        assert_eq!(document.as_ref(), SETTINGS_EXAMPLE);

        let template = parse_settings_document_with_language(
            SETTINGS_EXAMPLE,
            Path::new("settings.example.json"),
            MessageId::SettingsTemplateRole,
            Language::En,
        )
        .expect("the embedded template");
        let comments = |text: &str| {
            text.lines()
                .filter(|line| line.trim_start().starts_with("//"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };

        for flavor in [BuildFlavor::Development, BuildFlavor::Test] {
            let defaults = profile::defaults(flavor);
            let document =
                first_run_document(flavor, Language::Ru).expect("a debug first-run document");
            assert_ne!(document.as_ref(), SETTINGS_EXAMPLE);
            assert!(
                document.contains(&format!("\"{}\"", defaults.hotkey)),
                "the flavor's own chord is written"
            );

            let loaded = parse_settings_document_with_language(
                &document,
                Path::new("settings.json"),
                MessageId::SettingsFileRole,
                Language::En,
            )
            .expect("the debug first-run document");
            assert_eq!(loaded.hotkey, defaults.hotkey);
            assert_eq!(
                loaded.language,
                Language::Ru,
                "the chosen first-run language is written"
            );
            assert_eq!(loaded.mode, template.mode);
            assert_eq!(loaded.mute_during_recording, template.mute_during_recording);
            assert_eq!(loaded.input_device, template.input_device);
            assert_eq!(
                comments(&document),
                comments(SETTINGS_EXAMPLE),
                "the annotated comments survive untouched"
            );
        }

        assert_ne!(
            first_run_document(BuildFlavor::Development, Language::En)
                .expect("the development document")
                .as_ref(),
            first_run_document(BuildFlavor::Test, Language::En)
                .expect("the test document")
                .as_ref()
        );
    }

    /// A document written before the language was managed reads as the language
    /// the profile starts in, and the first successful save writes it in,
    /// preserving the mark, the comment, the CRLF endings and every unknown
    /// byte. The inserted value round-trips, and a later switch replaces it in
    /// place.
    #[test]
    fn an_old_document_gains_the_language_in_place() {
        let path = Path::new("settings.json");
        let document = "\u{feff}// старый файл\r\n{\r\n  // комментарий\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"notes\": \"kept\"\r\n}\r\n";

        let loaded =
            parse_settings_document_with_language(document, path, MessageId::SettingsFileRole, Language::Ru)
                .expect("a document from before the language");
        assert_eq!(loaded.language, Language::Ru);

        let patched = patch_settings_document(document, &loaded).expect("a patch");
        assert_eq!(
            patched,
            "\u{feff}// старый файл\r\n{\r\n  // комментарий\r\n  \"language\": \"ru\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"notes\": \"kept\"\r\n}\r\n"
        );

        let reread =
            parse_settings_document_with_language(&patched, path, MessageId::SettingsFileRole, Language::En)
                .expect("the patched document");
        assert_eq!(reread.language, Language::Ru);
        let mut switched = reread.clone();
        switched.language = Language::En;
        let english = patch_settings_document(&patched, &switched).expect("a patch");
        assert_eq!(
            english,
            "\u{feff}// старый файл\r\n{\r\n  // комментарий\r\n  \"language\": \"en\",\r\n  \"custom_vocabulary\": [],\r\n  \"dictation_languages\": [],\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n  \"input_device\": null,\r\n  \"notes\": \"kept\"\r\n}\r\n"
        );
        assert_eq!(
            parse_settings_document_with_language(&english, path, MessageId::SettingsFileRole, Language::Ru)
                .expect("the english document")
                .language,
            Language::En
        );
    }

    /// A present language has to be exactly `"en"` or `"ru"`: any other JSON
    /// value, an unsupported or differently-cased spelling, or a repeated
    /// property refuses the document and the patch, without quoting the value
    /// it refused.
    #[test]
    fn a_language_that_is_not_en_or_ru_is_refused() {
        let path = Path::new("settings.json");
        let base = |language: &str| {
            format!(
                "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": null,\n  \"language\": {language}\n}}\n"
            )
        };
        let settings = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            input_device: None,
            language: Language::En,
            custom_vocabulary: Arc::default(),
            dictation_languages: Arc::default(),
        };

        for value in ["\"de\"", "\"EN\"", "null", "5", "true", "{}", "\"\""] {
            let document = base(value);
            parse_settings_document_with_language(&document, path, MessageId::SettingsFileRole, Language::En)
                .expect_err("an unusable language");
            patch_settings_document(&document, &settings)
                .expect_err("an unusable language must refuse the patch");
        }

        let repeated = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"input_device\": null,\n  \"language\": \"en\",\n  \"language\": \"ru\"\n}\n";
        parse_settings_document_with_language(repeated, path, MessageId::SettingsFileRole, Language::En)
            .expect_err("a repeated language");
        patch_settings_document(repeated, &settings)
            .expect_err("a repeated language must refuse the patch");
    }

    /// The startup language is read from whatever the document names, even when
    /// the rest of the document is not usable yet: a sole valid language wins,
    /// and a missing, repeated, unusable or unreadable one falls back to the
    /// detected default. Nothing here repairs the file.
    #[test]
    fn startup_language_reads_the_sole_language_of_a_damaged_document() {
        let scratch = Scratch::new("startup-language");
        let path = scratch.target_settings();
        fs::create_dir_all(scratch.target()).expect("the profile directory");
        let wrapped = "// комментарий\r\n{ \"language\": \"ru\", \"hotkey\": \"F2\" }\r\n";

        fs::write(&path, wrapped).expect("a document with a language");
        assert_eq!(startup_language(Some(path.as_path())), Language::Ru);
        assert_eq!(
            fs::read(&path).expect("the document"),
            wrapped.as_bytes(),
            "the startup read repairs nothing"
        );

        let default = crate::i18n::detect_default_language();
        fs::write(&path, "{ \"hotkey\": \"F2\" }\r\n").expect("a document without a language");
        assert_eq!(startup_language(Some(path.as_path())), default);
        assert_eq!(startup_language(None), default);

        fs::write(&path, "{ \"language\": \"en\", \"language\": \"ru\" }\r\n")
            .expect("a repeated language");
        assert_eq!(startup_language(Some(path.as_path())), default);

        fs::write(&path, "{ not json }\r\n").expect("a damaged document");
        assert_eq!(startup_language(Some(path.as_path())), default);
    }

/// Regression matrix for the private archive of a development build: a scratch
/// tree under the system temporary directory, the real descriptor the checks
/// compare against, and the link a refusal is proved with. Every fixture is its
/// own tree, so the checks never share a name.
#[cfg(all(test, debug_assertions, feature = "dev-diagnostics"))]
mod private_archive_tests {
    use super::*;
    use std::io::{Read as _, Seek as _};
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::Storage::FileSystem::{
        CreateSymbolicLinkW, SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
        SYMBOLIC_LINK_FLAG_DIRECTORY,
    };

    /// The system code a symbolic link creation fails with when the process is
    /// neither elevated nor allowed to create links.
    const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;

    /// A directory of one fixture, removed when the fixture ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = env::temp_dir().join(format!(
                "speechek-archive-{name}-{}-{}",
                process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("a scratch directory");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The access list of an entry as the system reports it: one `(type, mask,
    /// SID)` per entry, read through the same handle the checks use, but with
    /// nothing of this module's own wording in the way.
    fn reported_dacl(path: &Path) -> Vec<(u8, u32, Vec<u8>)> {
        let entry = open_for_inspection(path).expect("an entry to inspect");
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR(ptr::null_mut());
        // SAFETY: the handle is open for the call and the descriptor it hands
        // back is released through `OwnedDescriptor`.
        let status = unsafe {
            GetSecurityInfo(
                HANDLE(entry.as_raw_handle()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                Some(&mut descriptor),
            )
        };
        assert!(status == ERROR_SUCCESS, "the access list of the entry");
        let descriptor = OwnedDescriptor(descriptor);

        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: the descriptor is the allocation the system just handed over.
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) }
            .expect("the control of the access list");
        assert_ne!(control & SE_DACL_PROTECTED.0, 0, "the access list is protected");

        // SAFETY: the DACL and the ACEs inside it belong to the descriptor that
        // is alive here, and `GetAce` points into that same allocation.
        let entries = unsafe { (*dacl).AceCount } as u32;
        assert!(entries > 0, "the access list names somebody");
        let mut reported = Vec::new();
        for index in 0..entries {
            let mut ace: *mut core::ffi::c_void = ptr::null_mut();
            // SAFETY: the index is inside the DACL.
            unsafe { GetAce(dacl, index, &mut ace) }.expect("an access entry");
            // SAFETY: an ACE starts with its header and, for an access-allowed
            // entry, carries the mask and then the SID.
            let (ace_type, mask, sid) = unsafe {
                let header = &*(ace as *const ACE_HEADER);
                let mask_offset = (ace as *const u8).add(core::mem::size_of::<ACE_HEADER>());
                let sid_offset = mask_offset.add(core::mem::size_of::<u32>());
                (
                    header.AceType,
                    ptr::read_unaligned(mask_offset as *const u32),
                    PSID(sid_offset as *mut _),
                )
            };
            // SAFETY: the SID is valid for the length the system reports.
            let length = unsafe { GetLengthSid(sid) } as usize;
            let bytes = unsafe { core::slice::from_raw_parts(sid.0 as *const u8, length) };
            reported.push((ace_type, mask, bytes.to_vec()));
        }
        reported
    }

    /// Whether the SID read out of an entry is exactly `sid`.
    fn is_sid(bytes: &[u8], sid: PSID) -> bool {
        // SAFETY: the SID is valid for the length the system reports.
        let length = unsafe { GetLengthSid(sid) } as usize;
        let expected = unsafe { core::slice::from_raw_parts(sid.0 as *const u8, length) };
        expected == bytes
    }

    /// Creates `link` as a directory symbolic link to `target`. `false` means
    /// Windows refused for the one reason this fixture tolerates: the process
    /// may not create links at all.
    fn directory_link(link: &Path, target: &Path) -> bool {
        let link = wide_path(link);
        let target = wide_path(target);
        // SAFETY: both paths are NUL-terminated buffers that outlive the call.
        let created = unsafe {
            CreateSymbolicLinkW(
                PCWSTR(link.as_ptr()),
                PCWSTR(target.as_ptr()),
                SYMBOLIC_LINK_FLAG_DIRECTORY | SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
            )
        };
        if created {
            return true;
        }
        // SAFETY: the failing call sets the last error.
        let code = unsafe { GetLastError() }.0;
        assert_eq!(
            code, ERROR_PRIVILEGE_NOT_HELD,
            "creating the link failed for another reason than the missing privilege"
        );
        false
    }

    /// A directory this module creates is reachable by the current user and
    /// `SYSTEM` and by nobody else, is checked rather than recreated when it is
    /// already there, and is refused when what is there is a foreign directory
    /// or a file.
    #[test]
    fn a_created_directory_is_private_and_anything_else_is_refused() {
        let scratch = Scratch::new("directory");
        let logs = scratch.join("logs");

        create_private_directory(&logs).expect("an archive directory");
        verify_private_path(&logs).expect("the directory just created");
        // The second call finds the directory and checks it: nothing is
        // recreated, replaced or removed.
        create_private_directory(&logs).expect("the directory that is already there");

        let entries = reported_dacl(&logs);
        assert_eq!(entries.len(), 2, "a private directory names exactly two accounts");
        assert!(
            entries
                .iter()
                .all(|(ace_type, mask, _)| *ace_type == ACCESS_ALLOWED_ACE_TYPE && *mask == ACCESS_FULL),
            "every entry grants full access and nothing else"
        );
        let mut user = SidBuffer::new();
        read_current_user_sid(&mut user).expect("the current user");
        let mut system = SidBuffer::new();
        read_local_system_sid(&mut system).expect("SYSTEM");
        assert!(
            entries
                .iter()
                .all(|(_, _, sid)| is_sid(sid, user.sid()) || is_sid(sid, system.sid())),
            "the two accounts are the current user and SYSTEM"
        );

        // A directory the system made carries the access of the directory that
        // holds it, which is not private: it is refused and left alone.
        let foreign = scratch.join("foreign");
        fs::create_dir(&foreign).expect("a directory with the inherited access list");
        assert!(verify_private_path(&foreign).is_err(), "a foreign directory is refused");
        assert!(create_private_directory(&foreign).is_err(), "and not repaired into place");
        assert!(fs::symlink_metadata(&foreign).is_ok(), "the foreign directory stays");

        // A file under the directory's name, and a parent that is not there.
        let occupied = scratch.join("occupied");
        let _file = create_private_file(&occupied).expect("a private file in the way");
        create_private_directory(&occupied).expect_err("a file is not a directory");
        assert!(
            create_private_directory(&scratch.join("missing").join("logs")).is_err(),
            "a directory is never created together with its parent"
        );
    }

    /// A file this module creates is brand new, private, and open for the
    /// header patch the journal needs: a name in use is refused with every byte
    /// of the file that holds it intact.
    #[test]
    fn a_created_file_is_private_brand_new_and_patchable() {
        let scratch = Scratch::new("file");
        let manifest = scratch.join("session.json.1.1.tmp");

        let mut file = create_private_file(&manifest).expect("a private staging file");
        io::Write::write_all(&mut file, b"{}").expect("the first bytes");
        file.sync_all().expect("the bytes on disk");
        drop(file);

        verify_private_path(&manifest).expect("the file just written");
        assert_eq!(fs::read(&manifest).expect("the bytes"), b"{}");
        let entries = reported_dacl(&manifest);
        assert_eq!(entries.len(), 2, "a private file names exactly two accounts");
        assert!(
            entries
                .iter()
                .all(|(ace_type, mask, _)| *ace_type == ACCESS_ALLOWED_ACE_TYPE && *mask == ACCESS_FULL),
            "every entry grants full access and nothing else"
        );

        assert!(create_private_file(&manifest).is_err(), "an existing name is never reused");
        assert_eq!(fs::read(&manifest).expect("the untouched bytes"), b"{}");
        assert!(verify_private_path(&scratch.join("absent.json.tmp")).is_err());

        // The handle reads back and patches a header in place, which is what
        // the FLAC STREAMINFO patch needs.
        let second = scratch.join("second.json.tmp");
        let mut file = create_private_file(&second).expect("a second staging file");
        io::Write::write_all(&mut file, b"aaaaaa").expect("six bytes");
        file.seek(io::SeekFrom::Start(0)).expect("back to the start");
        io::Write::write_all(&mut file, b"bb").expect("the patched header");
        file.sync_all().expect("the bytes on disk");
        let mut patched = Vec::new();
        let read_back = {
            file.seek(io::SeekFrom::Start(0)).expect("back to the start");
            file.read_to_end(&mut patched).expect("the bytes back")
        };
        drop(file);
        assert_eq!(read_back, 6);
        assert_eq!(patched, b"bbaaaa");
    }

    /// Publishing is a move that replaces nothing: the new name holds the
    /// bytes and passes the checks, and a destination that already exists is
    /// reported with both files exactly as they were.
    #[test]
    fn a_published_file_is_verified_and_never_replaces_anything() {
        let scratch = Scratch::new("publish");
        let part = scratch.join("audio.flac.part");
        let mut file = create_private_file(&part).expect("the staging file");
        io::Write::write_all(&mut file, b"fLaC").expect("the bytes");
        file.sync_all().expect("the bytes on disk");
        drop(file);

        let published = scratch.join("audio.flac");
        publish_private_file(&part, &published).expect("the published file");
        assert!(!part.exists(), "the staging name is gone");
        assert_eq!(fs::read(&published).expect("the published bytes"), b"fLaC");
        verify_private_path(&published).expect("the published file");

        let other = scratch.join("other.flac");
        let mut file = create_private_file(&other).expect("a second staging file");
        io::Write::write_all(&mut file, b"OggS").expect("the bytes");
        file.sync_all().expect("the bytes on disk");
        drop(file);

        assert!(
            publish_private_file(&other, &published).is_err(),
            "a name in use is never replaced"
        );
        assert_eq!(fs::read(&published).expect("the destination"), b"fLaC");
        assert_eq!(fs::read(&other).expect("the source"), b"OggS");
    }

    /// A link is refused wherever it can be made: it is not a plain file or
    /// directory, and the archive never writes through a name whose target
    /// somebody else chose. Where Windows will not let this process create a
    /// link, the refusal is not what is being checked, so the check is skipped
    /// rather than passed.
    #[test]
    fn a_link_is_refused_where_it_can_be_made() {
        let scratch = Scratch::new("link");
        let target = scratch.join("target");
        create_private_directory(&target).expect("the link target");

        let link = scratch.join("link");
        if directory_link(&link, &target) {
            let error = verify_private_path(&link).expect_err("a link is not a plain directory");
            assert_eq!(error.ui.key, MessageId::ConfigPathIsLink);
            assert!(
                create_private_directory(&link).is_err(),
                "a link under the directory name is refused"
            );
        }
    }
}
}
