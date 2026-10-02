//! Configuration storage for the Speechek shell: the editable settings
//! document, the launcher hotkey grammar and the atomic writer that both the
//! settings file and the DPAPI container (`secrets.rs`) are published through.
//!
//! `settings.json` belongs to the user. Whole-line `//` comments are allowed,
//! unknown properties and their formatting survive a value change byte for
//! byte, and a replacement is written as a same-directory temporary file that
//! is moved over the original, so a reader never sees a partial document. The
//! API keys live in `secrets.bin` beside this file; the runtime never reads
//! paths to plaintext key files from settings. The obsolete root `compare_all`
//! property is ignored on read and removed on the next save.
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

use crate::profile::{self, BuildFlavor};
use crate::secrets::{KeyRing, SecretKey};
use parking_lot::{Mutex, RwLock};
use std::borrow::Cow;
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
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The settings file every flavor resolves: inside the flavor's `%APPDATA%`
/// directory for production and test, and beside the executable for the
/// development flavor.
const SETTINGS_FILE_NAME: &str = "settings.json";

/// The local loopback port a document that does not name one is read with, and
/// the value the first-run document of this build spells out: the active
/// flavor's table value, so a debug build never claims the released shell's
/// port.
pub(crate) const DEFAULT_PORT: u16 = profile::defaults(profile::ACTIVE).port;

/// Annotated first-run template, embedded at build time so the executable needs
/// no file beside it.
const SETTINGS_EXAMPLE: &str = include_str!("../../config/settings.example.json");

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
/// `port` and `input_device` included.
///
/// `mute_during_recording`, `port` and `input_device` are the managed values a
/// document may omit: a file written before each existed has no such property,
/// and it reads as the documented default there - `false`, the active
/// flavor's default port and the system default input device - not as a guess
/// about a value the user chose.
/// The patch writes the properties into the document on the next change, so the
/// values a page chose survive the save rather than being read back as missing
/// and refused.
///
/// `port` is the local loopback port the next start uses; changing it while the
/// shell runs leaves the current server where it is, so the stored value is the
/// desired one and a restart is what applies it. `input_device` is a
/// `cpal::DeviceId` in its `Display`/`FromStr` form, or `None` for the system
/// default input device; the device is looked up when a recording opens, so a
/// stored id whose device is not connected keeps its stored value and is warned
/// about instead of being silently replaced.
///
/// `compare_all` is not part of this struct: a document may still carry the
/// root property from when it was managed, and it is ignored when the file is
/// read and removed by the next [`patch_settings_document`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub hotkey: String,
    pub mode: String,
    pub mute_during_recording: bool,
    /// The loopback port read at the next start, in `1..=65535`.
    pub port: u16,
    /// The selected input device id, or `None` for the system default device.
    pub input_device: Option<String>,
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
/// problem, never a value read from a configuration file.
#[derive(Clone, Debug)]
pub struct ConfigError {
    message: String,
    /// Set only when the destination file had already been replaced by the time
    /// this failure was reported: the previous bytes are gone then, so a caller
    /// that keeps a backup has to restore it.
    replaced: bool,
}

impl ConfigError {
    pub(crate) fn new(message: String) -> Self {
        Self {
            message,
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
            message: self.message,
            replaced: true,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ConfigError {}

/// The failure this module reports for a document that cannot be used as given.
fn config_invalid(message: String) -> ConfigError {
    ConfigError::new(message)
}

/* -------------------------------------------------------------------------- */
/* Settings document                                                          */
/* -------------------------------------------------------------------------- */

/// Reads and validates the settings file: `hotkey` and `mode` are both
/// required, so a half-written file stops the launcher instead of running with
/// a value nobody chose. `mute_during_recording`, `port` and `input_device` may
/// be absent - the documented defaults of a file written before they existed
/// (`false`, the active flavor's default port, the system default input
/// device) - but a value that is
/// present has to be a JSON boolean, an integer in `1..=65535` or `null` / a
/// non-empty device id string. An obsolete root `compare_all` property is
/// ignored here and removed by the next patch. Whole-line `//` comments are
/// allowed so the file can be annotated; inline comments and trailing commas
/// are not.
///
/// The hotkey is read as a non-empty string and is *not* checked against the
/// shortcut grammar here: [`validate_hotkey`] does that when the launcher
/// registers it, so a chord Windows refuses still starts the application and
/// can be corrected in the settings window. Other root properties are ignored
/// and preserved.
pub fn load_settings(config_path: &Path) -> Result<Settings, ConfigError> {
    read_settings_document(config_path, "settings file")
}

/// [`load_settings`] with the role of the file as an argument, so a diagnostic
/// names the file correctly; the settings window reads the document it is about
/// to patch through this. The text is read and released here: only the parsed
/// settings are returned.
pub(crate) fn read_settings_document(
    config_path: &Path,
    label: &str,
) -> Result<Settings, ConfigError> {
    let text = read_settings_text(config_path, label)?;
    parse_settings_document(&text, config_path, label)
}

/// Reads a settings document as the text an editor works on: the file's own
/// characters, with a leading byte order mark kept, because a document that is
/// patched and written back has to stay the file the user saved. Strict UTF-8
/// and the NUL check are [`decode_utf8_bytes`]'s.
pub(crate) fn read_settings_text(config_path: &Path, label: &str) -> Result<String, ConfigError> {
    if config_path.is_dir() {
        return Err(ConfigError::new(format!(
            "{}: {label} is a directory.",
            config_path.display()
        )));
    }
    let bytes = fs::read(config_path).map_err(|error| {
        ConfigError::new(format!(
            "{}: {label} {}.",
            config_path.display(),
            describe_read_failure(io_code(&error))
        ))
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
/// comments.
pub(crate) fn parse_settings_document(
    text: &str,
    config_path: &Path,
    label: &str,
) -> Result<Settings, ConfigError> {
    let text = strip_comment_lines(document_body(text));
    let context = format!("{}: ", config_path.display());

    // This parse proves the document is one JSON object; the properties are
    // then read from the text again, so a repeated root property is refused
    // instead of silently collapsing to its last occurrence.
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(_)) => {}
        Ok(_) => {
            return Err(config_invalid(format!(
                "{context}{label} must contain a JSON object."
            )))
        }
        Err(_) => {
            return Err(config_invalid(format!(
            "{context}{label} is not valid JSON; comments must be whole lines that start with //."
        )))
        }
    }

    let members = root_members(&text).ok_or_else(|| {
        config_invalid(format!(
            "{context}{label} could not be read as a JSON object."
        ))
    })?;
    settings_from_members(&members, &text, &context)
}

/// Replaces the launcher values in `raw` and removes the obsolete root
/// `compare_all` property, preserving comments, byte order mark, line endings,
/// property order and every unknown property byte for byte.
///
/// Only managed values are edited; `compare_all` is removed with its separator
/// comma. A required property that is missing or repeated is an error rather
/// than a guessed default, while `mute_during_recording`, `port` and
/// `input_device` are inserted together when missing, in that fixed order in
/// front of the first property using its indentation and line ending. The
/// result is read back and compared with `settings` before it is returned.
///
/// The result is the whole document: a leading byte order mark in `raw` is kept
/// in front of it, so the caller writes [`patch_settings_document`]'s text as
/// the bytes of the file without adding anything - the text
/// [`read_settings_text`] returns is the text to patch, and a document that has
/// no mark never gains one.
pub fn patch_settings_document(raw: &str, settings: &Settings) -> Result<String, ConfigError> {
    let hotkey = ts_trim(&settings.hotkey);
    if hotkey.is_empty() {
        return Err(config_invalid(
            "\"hotkey\" must be a non-empty string such as \"F2\" or \"Ctrl+Shift+Space\"."
                .to_owned(),
        ));
    }
    let mode = ts_trim(&settings.mode);
    if !matches!(mode, "live" | "smart" | "verbatim") {
        return Err(config_invalid(
            "\"mode\" must be \"live\", \"smart\" or \"verbatim\".".to_owned(),
        ));
    }
    // The values the patch is about to write are checked here, in the wording
    // the loader uses, so a caller cannot write a port or a device the next read
    // would refuse.
    let invalid = |name: &str| -> ConfigError {
        config_invalid(format!("\"{name}\" {}.", property_requirement(name)))
    };
    if settings.port == 0 {
        return Err(invalid("port"));
    }
    if settings
        .input_device
        .as_deref()
        .is_some_and(|device| usable_device_id(device).is_none())
    {
        return Err(invalid("input_device"));
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
        .ok_or_else(|| config_invalid("the settings document is not a JSON object.".to_owned()))?;

    // The values already in the document are read through the same rule the
    // loader uses, so a stored port or device that could never be used refuses
    // the write: it is repaired in the settings window, not silently rewritten
    // here with a value nobody chose.
    if let Some(member) = single_member(&members, "port")? {
        if member_port(member, &masked).is_none() {
            return Err(invalid("port"));
        }
    }
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
    match single_member(&members, "port")? {
        Some(member) => edits.push((member.value_span.clone(), settings.port.to_string())),
        None => missing.push(("port", settings.port.to_string())),
    }
    let device = match &settings.input_device {
        Some(device) => json_string(device),
        None => "null".to_owned(),
    };
    match single_member(&members, "input_device")? {
        Some(member) => edits.push((member.value_span.clone(), device)),
        None => missing.push(("input_device", device)),
    }
    member_insertion_edits(body, &masked, &members, &missing, &mut edits);

    // Retire the obsolete setting without changing unrelated document bytes.
    if let Some(member) = single_member(&members, "compare_all")? {
        member_removal_edits(&masked, member, &mut edits);
    }

    let patched = splice(body, &mut edits);
    verify_patched(&patched, settings)?;
    Ok(format!("{bom}{patched}"))
}

/// Reads a patched document back and refuses a patch that fails to round-trip
/// settings or still carries the obsolete `compare_all` property.
fn verify_patched(document: &str, settings: &Settings) -> Result<(), ConfigError> {
    let text = strip_comment_lines(document_body(document));
    if !matches!(serde_json::from_str::<Value>(&text), Ok(Value::Object(_))) {
        return Err(config_invalid(
            "the patched settings document is not valid JSON.".to_owned(),
        ));
    }
    let members = root_members(&text).ok_or_else(|| {
        config_invalid("the patched settings document is not a JSON object.".to_owned())
    })?;
    let written = settings_from_members(&members, &text, "")?;
    let expected = Settings {
        hotkey: ts_trim(&settings.hotkey).to_owned(),
        mode: ts_trim(&settings.mode).to_owned(),
        mute_during_recording: settings.mute_during_recording,
        port: settings.port,
        input_device: settings.input_device.clone(),
    };
    if written != expected {
        return Err(config_invalid(
            "the patched settings document does not read back the new values.".to_owned(),
        ));
    }
    if single_member(&members, "compare_all")?.is_some() {
        return Err(config_invalid(
            "the obsolete \"compare_all\" property could not be removed.".to_owned(),
        ));
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
fn member_removal_edits<'a>(
    document: &str,
    member: &'a RootMember,
    edits: &mut Vec<(Range<usize>, String)>,
) {
    let bytes = document.as_bytes();
    edits.push((member.name_span.start..member.value_span.end, String::new()));

    let after = skip_json_whitespace(bytes, member.value_span.end);
    if bytes.get(after) == Some(&b',') {
        edits.push((after..after + 1, String::new()));
    } else if let Some(comma) = comma_before(bytes, member.name_span.start) {
        edits.push((comma..comma + 1, String::new()));
    }
}

/// Adds the single edit that gives a document the properties it does not carry
/// yet. Each missing property is written as `"name": value` in the order given,
/// and all of them go into one insertion in front of the first root property,
/// with that property's own indentation and line ending: a document from before
/// `mute_during_recording`, `port` or `input_device` existed has no such values
/// to write, and one edit keeps the spans of several zero-width insertions from
/// piling up at the same offset. Every other byte - the comments, the order, the
/// mark - stays where it was, and an object whose first property shares a line
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
        (Some(_), Some(_)) => Err(config_invalid(format!(
            "\"{name}\" appears more than once at the root; keep exactly one."
        ))),
    }
}

/// A managed property that must be there exactly once.
fn managed_member<'a>(
    members: &'a [RootMember],
    name: &str,
) -> Result<&'a RootMember, ConfigError> {
    single_member(members, name)?
        .ok_or_else(|| config_invalid(format!("\"{name}\" is missing from the settings document.")))
}

/// What a managed property has to look like, in the wording the loader has
/// always used for a value that cannot be used.
fn property_requirement(name: &str) -> &'static str {
    match name {
        "hotkey" => "must be a non-empty string such as \"F2\" or \"Ctrl+Shift+Space\"",
        "mode" => "must be \"live\", \"smart\" or \"verbatim\"",
        "port" => "must be an integer between 1 and 65535",
        "input_device" => "must be null or a non-empty device id string",
        _ => "must be true or false",
    }
}

/// The managed properties of a document, each typed and checked.
/// `mute_during_recording`, `port` and `input_device` are the ones that may be
/// absent - a document written before them reads as `false`, the active
/// flavor's default port and the system default input device there - while
/// every other missing or malformed
/// property refuses the document. The obsolete root `compare_all`
/// is not read at all: the patch removes it, so it never becomes a value. The
/// `context` prefixes every message, so the loader can name the file while the
/// patch readback describes itself; a value is never quoted.
fn settings_from_members(
    members: &[RootMember],
    document: &str,
    context: &str,
) -> Result<Settings, ConfigError> {
    let member = |name: &str| -> Result<&RootMember, ConfigError> {
        match single_member(members, name) {
            Ok(Some(member)) => Ok(member),
            Ok(None) => Err(config_invalid(format!("{context}\"{name}\" is missing."))),
            Err(error) => Err(ConfigError::new(format!("{context}{error}"))),
        }
    };
    let invalid = |name: &str| -> ConfigError {
        config_invalid(format!(
            "{context}\"{name}\" {}.",
            property_requirement(name)
        ))
    };

    let hotkey = member("hotkey")?;
    let mode = member("mode")?;

    let hotkey = member_string(hotkey, document)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("hotkey"))?;
    let mode = member_string(mode, document)
        .filter(|value| matches!(value.as_str(), "live" | "smart" | "verbatim"))
        .ok_or_else(|| invalid("mode"))?;

    // The obsolete `compare_all` is not a value, but a document that carries it
    // more than once is one the patch could not migrate either, so it is refused
    // with the wording a repeated managed property gets rather than loading into
    // a file the writer will not touch.
    if let Err(error) = single_member(members, "compare_all") {
        return Err(ConfigError::new(format!("{context}{error}")));
    }

    let mute_during_recording = match single_member(members, "mute_during_recording") {
        Ok(Some(mute)) => member_value(mute, document)
            .as_bool()
            .ok_or_else(|| invalid("mute_during_recording"))?,
        // The property is optional and its absence is the documented default,
        // so an annotated file from before the switch still loads.
        Ok(None) => false,
        Err(error) => return Err(ConfigError::new(format!("{context}{error}"))),
    };

    let port = match single_member(members, "port") {
        Ok(Some(port)) => member_port(port, document).ok_or_else(|| invalid("port"))?,
        // A document written before the port was managed reads with the default
        // the first-run template documents.
        Ok(None) => DEFAULT_PORT,
        Err(error) => return Err(ConfigError::new(format!("{context}{error}"))),
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
        Err(error) => return Err(ConfigError::new(format!("{context}{error}"))),
    };

    Ok(Settings {
        hotkey,
        mode,
        mute_during_recording,
        port,
        input_device,
    })
}

/// The string value of a member, trimmed the way the loader has always trimmed
/// it; `None` for any other JSON type.
fn member_string(member: &RootMember, document: &str) -> Option<String> {
    member_value(member, document)
        .as_str()
        .map(|value| ts_trim(value).to_owned())
}

/// The value of a `port` member as the loopback port it names: a whole number
/// in `1..=65535`, and `None` for any other JSON value - a string, a fraction, a
/// boolean, a null or a number outside the range a port can have.
fn member_port(member: &RootMember, document: &str) -> Option<u16> {
    member_value(member, document)
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|port| *port > 0)
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
/// first-run document, and a preflight on top of this must not create one.
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
            dev_executable_error("the current executable path could not be read")
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
                        return Err(config_invalid(format!(
                            "SPEECHEK_CONFIG_PATH must be an absolute path (got \"{override_text}\")."
                        )));
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
        config_invalid(format!(
            "APPDATA is not set; Speechek keeps settings.json in %APPDATA%\\{directory}."
        ))
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
    let executable =
        executable.ok_or_else(|| dev_executable_error("the executable path is not available"))?;
    if !executable.is_absolute() {
        return Err(dev_executable_error(&format!(
            "\"{}\" is not an absolute path",
            executable.display()
        )));
    }
    let directory = executable
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .ok_or_else(|| {
            dev_executable_error(&format!("\"{}\" has no directory", executable.display()))
        })?;
    Ok(directory.join(SETTINGS_FILE_NAME))
}

/// The refusal of a development run whose document directory cannot be named:
/// the message names the unusable path, or the missing one, and never falls
/// back to another directory.
fn dev_executable_error(reason: &str) -> ConfigError {
    config_invalid(format!(
        "Cannot determine the Dev executable directory for settings.json: {reason}."
    ))
}

/// The loopback port the next start should claim, resolved before the shell is
/// built and without touching anything on disk.
///
/// The document is the one [`settings_path`] names - beside the development
/// executable, the test flavor's override or `%APPDATA%` document, the
/// production profile otherwise. A document that is not there yet reads as
/// [`DEFAULT_PORT`], the port the flavor's first-run document spells out; a
/// document that is there is read through the same reader the startup uses, so
/// a stored port that cannot be served, or a document that cannot be read at
/// all, stops this run instead of being replaced by a port nobody chose.
/// Nothing here creates or rewrites a file: writing the first-run document and
/// re-reading the port are the shell's startup, and the port returned here is
/// checked against that re-read before a socket is served or a window exists.
pub fn preflight_port() -> Result<u16, ConfigError> {
    match preflight_document(settings_path()?) {
        None => Ok(DEFAULT_PORT),
        Some(path) => read_settings_document(&path, "settings file").map(|settings| settings.port),
    }
}

/// [`preflight_port`] with the flavor, both environment values and the
/// executable path as arguments, for the regression matrix: the compiled-in
/// flavor and the process environment are never consulted, and the same
/// arguments resolve exactly the document [`settings_path`] would.
#[cfg(test)]
fn preflight_port_from(
    flavor: BuildFlavor,
    config_override: Option<&OsStr>,
    appdata: Option<&OsStr>,
    executable: Option<&Path>,
) -> Result<u16, ConfigError> {
    match preflight_document_from(flavor, config_override, appdata, executable)? {
        None => Ok(profile::defaults(flavor).port),
        Some(path) => read_settings_document(&path, "settings file").map(|settings| settings.port),
    }
}

/// The settings document that startup will read, resolved the way
/// [`settings_path`] resolves it, but without writing anything. Test-only: the
/// runtime path goes through [`settings_path`], so the environment and the
/// executable path are read in one place.
#[cfg(test)]
fn preflight_document_from(
    flavor: BuildFlavor,
    config_override: Option<&OsStr>,
    appdata: Option<&OsStr>,
    executable: Option<&Path>,
) -> Result<Option<PathBuf>, ConfigError> {
    Ok(preflight_document(resolve_settings_path(
        flavor,
        config_override,
        appdata,
        executable,
    )?))
}

/// The document under `path`, when an entry is there to read: `None` means the
/// startup will write the flavor's first-run document, whose port is
/// [`DEFAULT_PORT`].
fn preflight_document(path: PathBuf) -> Option<PathBuf> {
    fs::symlink_metadata(&path).is_ok().then_some(path)
}

/// Creates the settings file for a first run: the flavor's annotated template
/// (embedded at build time, then brought to the active flavor's first-run
/// hotkey and port) as `settings.json`, written with `create_new` so nothing
/// that exists is ever overwritten. Returns `true` when the default file was
/// written, which tells the caller to continue startup with the settings window
/// open on the key section; `false` means the existing file is left exactly as
/// it was found.
///
/// The key container is not created here: `secrets.bin` is written by the first
/// explicit key apply of the settings window, so an installation without keys
/// never has an empty vault to get wrong.
pub fn create_default_if_missing(path: &Path) -> Result<bool, ConfigError> {
    let directory = match path.parent() {
        Some(directory) if !directory.as_os_str().is_empty() => directory,
        _ => {
            return Err(config_invalid(format!(
                "{}: settings path has no directory to create.",
                path.display()
            )))
        }
    };

    if path.exists() {
        return Ok(false);
    }

    fs::create_dir_all(directory).map_err(|error| {
        ConfigError::new(format!(
            "{}: configuration directory {}.",
            directory.display(),
            describe_create_failure(io_code(&error))
        ))
    })?;

    write_new(path, first_run_document(profile::ACTIVE)?.as_bytes())?;
    Ok(true)
}

/// The annotated document [`create_default_if_missing`] writes: the production
/// template for [`BuildFlavor::Production`], and for a debug flavor the same
/// template brought to that flavor's first-run hotkey and port.
///
/// The template itself stays the production one: it is the document the
/// distributed shell ships and the file a user reads. A debug flavor only
/// rewrites the two values its own profile owns - through the same parser and
/// patch the settings window uses, so the template's comments, line endings and
/// every other byte survive - and never stores a development or test chord in a
/// file a production build could read.
fn first_run_document(flavor: BuildFlavor) -> Result<Cow<'static, str>, ConfigError> {
    if flavor == BuildFlavor::Production {
        return Ok(Cow::Borrowed(SETTINGS_EXAMPLE));
    }

    let defaults = profile::defaults(flavor);
    // The path is for the parser's wording only; nothing is read or written
    // here.
    let template_path = Path::new(SETTINGS_FILE_NAME);
    let mut settings =
        parse_settings_document(SETTINGS_EXAMPLE, template_path, "settings template")?;
    if settings.hotkey == defaults.hotkey && settings.port == defaults.port {
        return Ok(Cow::Borrowed(SETTINGS_EXAMPLE));
    }
    settings.hotkey = defaults.hotkey.to_owned();
    settings.port = defaults.port;
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
            ConfigError::new(format!(
                "{}: no directory to write the replacement into.",
                path.display()
            ))
        })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            ConfigError::new(format!("{}: not a file name to replace.", path.display()))
        })?
        .to_string_lossy()
        .into_owned();

    fs::create_dir_all(parent).map_err(|error| {
        ConfigError::new(format!(
            "{}: configuration directory {}.",
            parent.display(),
            describe_create_failure(io_code(&error))
        ))
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

    Err(ConfigError::new(format!(
        "{}: no temporary file could be created beside it.",
        path.display()
    )))
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
                code => Err(ConfigError::new(create_failure(path, code))),
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
        return Err(ConfigError::new(create_failure(path, io_code(&error))));
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
        ConfigError::new(format!(
            "{}: {}.",
            destination.display(),
            describe_replace_failure(windows_code(&error))
        ))
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
        ConfigError::new(format!(
            "{}: its access list could not be read because it {}.",
            path.display(),
            describe_read_failure(Some(windows_code(&error)))
        ))
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
                code => Err(ConfigError::new(create_failure(path, code))),
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
        return Err(ConfigError::new(format!(
            "{}: its access list could not be read ({}).",
            path.display(),
            describe_security_failure(status.0)
        )));
    }
    Ok(Some(OwnedDescriptor(descriptor)))
}

/// Checks that a file is reachable only by `user_sid` and `system_sid`: its
/// DACL is protected from the directory holding it, is not empty, and holds
/// nothing but full-access entries for those two accounts.
fn verify_private_descriptor(
    path: &Path,
    user_sid: PSID,
    system_sid: PSID,
) -> Result<(), ConfigError> {
    let file = open_for_read_control(path)?;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR(ptr::null_mut());
    // SAFETY: the handle stays open for the whole check, and the descriptor the
    // call allocates is released as soon as it is read out.
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
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
        return Err(ConfigError::new(format!(
            "{}: its access list could not be read ({}).",
            path.display(),
            describe_security_failure(status.0)
        )));
    }
    let descriptor = OwnedDescriptor(descriptor);

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: the descriptor is the allocation the system just handed over.
    unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) }.map_err(
        |error| {
            ConfigError::new(format!(
                "{}: its access list could not be read ({}).",
                path.display(),
                describe_security_failure(windows_code(&error))
            ))
        },
    )?;
    if control & SE_DACL_PROTECTED.0 == 0 {
        return Err(ConfigError::new(format!(
            "{}: its access list is not protected from the directory it lives in.",
            path.display()
        )));
    }
    if dacl.is_null() {
        return Err(ConfigError::new(format!(
            "{}: its access list is missing, which would let any account reach it.",
            path.display()
        )));
    }

    // SAFETY: the DACL and the ACEs inside it belong to the descriptor that is
    // alive here; `GetAce` returns a pointer into that same allocation.
    let entries = unsafe { (*dacl).AceCount } as u32;
    if entries == 0 {
        return Err(ConfigError::new(format!(
            "{}: its access list grants nothing, so the file could not be read back.",
            path.display()
        )));
    }
    for index in 0..entries {
        let mut ace: *mut core::ffi::c_void = ptr::null_mut();
        // SAFETY: the index is inside the DACL and the ACE it points at lives
        // in the same allocation.
        unsafe { GetAce(dacl, index, &mut ace) }.map_err(|error| {
            ConfigError::new(format!(
                "{}: its access list could not be read ({}).",
                path.display(),
                describe_security_failure(windows_code(&error))
            ))
        })?;
        if ace.is_null() {
            return Err(ConfigError::new(format!(
                "{}: its access list could not be read.",
                path.display()
            )));
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
            return Err(ConfigError::new(format!(
                "{}: its access list holds an entry that does not simply allow access.",
                path.display()
            )));
        }
        if mask & ACCESS_FULL != ACCESS_FULL {
            return Err(ConfigError::new(format!(
                "{}: its access list does not grant full access.",
                path.display()
            )));
        }
        // SAFETY: both SIDs are valid and are only read.
        let known = unsafe { EqualSid(sid, user_sid).is_ok() || EqualSid(sid, system_sid).is_ok() };
        if !known {
            return Err(ConfigError::new(format!(
                "{}: its access list grants access to an account it should not name.",
                path.display()
            )));
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
            .map_err(|error| identity_error("the process token is not available", &error))?;
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
        .map_err(|error| identity_error("the current user's identity is not available", &error))?;

        let (sid, length) = {
            let record = &*(staging.as_bytes() as *const TOKEN_USER);
            if !IsValidSid(record.User.Sid).as_bool() {
                return Err(identity_reason(
                    "the current user's identity is not a valid SID",
                ));
            }
            (record.User.Sid, GetLengthSid(record.User.Sid) as usize)
        };
        if length > buffer.words.len() * core::mem::size_of::<usize>() {
            return Err(identity_reason("the current user's SID is too long"));
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
        .map_err(|error| identity_error("SYSTEM's identity is not available", &error))
}

/// The failure of building the private descriptor, before any file exists.
fn private_descriptor_error(error: &windows::core::Error) -> ConfigError {
    ConfigError::new(format!(
        "a container private to the current user could not be described ({}).",
        describe_security_failure(windows_code(error))
    ))
}

/// The failure of reading the identity a private descriptor names.
fn identity_error(action: &str, error: &windows::core::Error) -> ConfigError {
    identity_reason(&format!(
        "{action} ({}).",
        describe_security_failure(windows_code(error))
    ))
}

/// An identity failure worded exactly as given: the callers know which part of
/// the identity could not be read, and none of them quotes a value.
fn identity_reason(reason: &str) -> ConfigError {
    ConfigError::new(reason.to_owned())
}

/// The wording for a file that could not be created or written: the path, and
/// the small set of system codes both the standard library and the Windows
/// bindings report.
fn create_failure(path: &Path, code: impl Into<Option<u32>>) -> String {
    format!(
        "{}: {}.",
        path.display(),
        describe_create_failure(code.into())
    )
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
    label: &str,
    bytes: &'a [u8],
) -> Result<&'a str, ConfigError> {
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    let text = str::from_utf8(body).map_err(|_| {
        ConfigError::new(format!(
            "{}: {label} is not valid UTF-8 text; re-save it as UTF-8.",
            path.display()
        ))
    })?;
    if text.contains('\0') {
        return Err(ConfigError::new(format!(
            "{}: {label} contains NUL bytes; re-save it as UTF-8, not UTF-16.",
            path.display()
        )));
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
            "must be a non-empty string such as \"F2\" or \"Ctrl+Shift+Space\"",
        ));
    }

    let parts: Vec<&str> = hotkey.split('+').map(ts_trim).collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(invalid_hotkey(
            settings_path,
            &format!("\"{hotkey}\" has an empty part; use modifiers followed by one key"),
        ));
    }

    let last_index = parts.len() - 1;
    for (index, part) in parts.iter().enumerate() {
        let lower = part.to_lowercase();
        let is_last = index == last_index;

        if lower == "fn" {
            return Err(invalid_hotkey(
                settings_path,
                "cannot use \"Fn\": Windows never reports that key to applications",
            ));
        }
        if lower == "f12" {
            return Err(invalid_hotkey(
                settings_path,
                "cannot use \"F12\": speechek reserves it for its own window",
            ));
        }
        if lower == "escape" && last_index == 0 {
            return Err(invalid_hotkey(
                settings_path,
                "cannot use \"Escape\" alone: while a dictation runs speechek reserves it to cancel that dictation; use another key, for example \"F2\", or add a modifier such as \"Ctrl+Escape\"",
            ));
        }
        if HOTKEY_MODIFIERS.contains(&lower.as_str()) {
            if is_last {
                return Err(invalid_hotkey(
                    settings_path,
                    &format!("\"{hotkey}\" is modifiers only; add a key such as \"F2\""),
                ));
            }
            continue;
        }
        if !is_last {
            return Err(invalid_hotkey(
                settings_path,
                &format!("\"{hotkey}\" must be modifiers followed by one key; \"{part}\" is not a modifier"),
            ));
        }
        if !HOTKEY_NAMED_KEYS.contains(&lower.as_str())
            && !is_single_key_character(&lower)
            && !is_function_key(&lower)
        {
            return Err(invalid_hotkey(
                settings_path,
                &format!("\"{part}\" is not a supported key (use A-Z, 0-9, F1-F24, or a named key such as \"Space\")"),
            ));
        }
    }

    Ok(hotkey.to_owned())
}

fn invalid_hotkey(settings_path: &Path, reason: &str) -> ConfigError {
    ConfigError::new(format!("{}: \"hotkey\" {reason}.", settings_path.display()))
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

/// Describes a failed read from its OS error code; the raw error text is never
/// quoted.
fn describe_read_failure(code: Option<u32>) -> &'static str {
    match code {
        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND) => "not found",
        Some(ERROR_ACCESS_DENIED) => "permission denied",
        Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION) => {
            "is locked by another program"
        }
        _ => "could not be read",
    }
}

/// The same codes as seen by a create or a write; Windows reports a locked or
/// protected destination through the same small set.
fn describe_create_failure(code: Option<u32>) -> &'static str {
    match code {
        Some(ERROR_ACCESS_DENIED) => "permission denied",
        Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION) => {
            "is locked by another program"
        }
        Some(ERROR_FILE_EXISTS) | Some(ERROR_ALREADY_EXISTS) => "already exists",
        Some(ERROR_DISK_FULL) => "has no space left",
        _ => "could not be created",
    }
}

/// The wording for a rename that could not replace the destination.
fn describe_replace_failure(code: u32) -> &'static str {
    match code {
        ERROR_ACCESS_DENIED => "the replacement could not be moved into place: permission denied",
        ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => {
            "the replacement could not be moved into place: another program has the file open"
        }
        ERROR_DISK_FULL => "the replacement could not be moved into place: no space left",
        _ => "the replacement could not be moved into place",
    }
}

/// The wording for a security call that refused, naming the code because this
/// is the one place where the reason matters for diagnosis and the code is not
/// sensitive.
fn describe_security_failure(code: u32) -> String {
    match code {
        ERROR_ACCESS_DENIED => "permission denied".to_owned(),
        _ => format!("the operating system refused it, error {code}"),
    }
}

/// Writes a brand-new file, never replacing one that exists; a racing creator
/// is reported instead of overwritten.
fn write_new(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file
            .write_all(contents)
            .map_err(|error| ConfigError::new(create_failure(path, io_code(&error)))),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(ConfigError::new(
            format!("{}: already exists and was left untouched.", path.display()),
        )),
        Err(error) => Err(ConfigError::new(create_failure(path, io_code(&error)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A byte order mark, CRLF and nested unknown values survive the patch.
    /// The obsolete `compare_all` property disappears, while unrelated root
    /// properties and user comments remain unchanged.
    #[test]
    fn patching_a_marked_commented_document_keeps_every_other_byte() {
        let document = "\u{feff}// Speechek settings\r\n{\r\n  // Этот файл сохранён в UTF-8 с BOM.\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"mute_during_recording\": false,\r\n  \"port\": 4173,\r\n  \"input_device\": \"wasapi:card-one\",\r\n  \"compare_all\": false,\r\n\r\n  // Заметки пользователя и вложенный объект.\r\n  \"notes\": {\r\n    \"text\": \"keep this value\",\r\n    \"nested\": [1, 2, { \"deep\": true }]\r\n  },\r\n  \"trailing\": null,\r\n  // Дополнительное поле пользователя.\r\n  \"extra\": \"kept\"\r\n}\r\n";
        let expected = "\u{feff}// Speechek settings\r\n{\r\n  // Этот файл сохранён в UTF-8 с BOM.\r\n  \"hotkey\": \"Ctrl+Shift+F9\",\r\n  \"mode\": \"verbatim\",\r\n  \"mute_during_recording\": true,\r\n  \"port\": 43118,\r\n  \"input_device\": \"wasapi:card-two\",\r\n  \r\n\r\n  // Заметки пользователя и вложенный объект.\r\n  \"notes\": {\r\n    \"text\": \"keep this value\",\r\n    \"nested\": [1, 2, { \"deep\": true }]\r\n  },\r\n  \"trailing\": null,\r\n  // Дополнительное поле пользователя.\r\n  \"extra\": \"kept\"\r\n}\r\n";

        let patched = patch_settings_document(
            document,
            &Settings {
                hotkey: "Ctrl+Shift+F9".to_owned(),
                mode: "verbatim".to_owned(),
                mute_during_recording: true,
                port: 43118,
                input_device: Some("wasapi:card-two".to_owned()),
            },
        )
        .expect("the documented patch");

        assert_eq!(patched, expected);
    }

    /// The optional values a document may not carry: a file from before them
    /// still loads as the documented defaults - `false`, the active flavor's
    /// default port and the system default input device - and the first patch
    /// that changes them writes all three properties into the document in one
    /// insertion, in front of the first property and in that property's
    /// indentation and CRLF, in the fixed order `mute_during_recording`,
    /// `port`, `input_device`, instead of readback refusing values the file
    /// does not hold. Later patches replace the same bytes in place, and every
    /// other byte of the document survives, including an unknown property
    /// between them.
    #[test]
    fn a_document_without_the_optional_values_gains_them_in_one_patch() {
        let path = Path::new("settings.json");
        let document = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"notes\": { \"keep\": true }\r\n}\r\n";

        let loaded = parse_settings_document(document, path, "settings file")
            .expect("a document from before the optional values");
        assert!(!loaded.mute_during_recording);
        assert_eq!(loaded.port, DEFAULT_PORT);
        assert_eq!(loaded.input_device, None);

        let switched_on = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: true,
            port: 43118,
            input_device: Some("wasapi:card-one".to_owned()),
        };
        let on_document = patch_settings_document(document, &switched_on).expect("a patch");
        assert_eq!(
            on_document,
            "{\r\n  \"mute_during_recording\": true,\r\n  \"port\": 43118,\r\n  \"input_device\": \"wasapi:card-one\",\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"notes\": { \"keep\": true }\r\n}\r\n"
        );
        assert_eq!(
            parse_settings_document(&on_document, path, "settings file")
                .expect("the patched document"),
            switched_on
        );

        let switched_off = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            port: DEFAULT_PORT,
            input_device: None,
        };
        let off_document =
            patch_settings_document(&on_document, &switched_off).expect("a patch");
        assert_eq!(
            off_document,
            format!(
                "{{\r\n  \"mute_during_recording\": false,\r\n  \"port\": {DEFAULT_PORT},\r\n  \"input_device\": null,\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"notes\": {{ \"keep\": true }}\r\n}}\r\n"
            )
        );
        assert_eq!(
            parse_settings_document(&off_document, path, "settings file")
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
        let with = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"port\": 4173,\r\n  \"input_device\": null,\r\n  \"compare_all\": true,\r\n  \"notes\": 1\r\n}\r\n";
        let without = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"port\": 4173,\r\n  \"input_device\": null,\r\n  \"notes\": 1\r\n}\r\n";
        let expected = Settings {
            hotkey: "F2".to_owned(),
            mode: "smart".to_owned(),
            mute_during_recording: false,
            port: 4173,
            input_device: None,
        };

        // Reading: the obsolete property is simply not a value, so an old and a
        // current document describe the same settings.
        assert_eq!(
            parse_settings_document(with, path, "settings file").expect("an old document"),
            expected
        );
        assert_eq!(
            parse_settings_document(without, path, "settings file").expect("a current document"),
            expected
        );

        // Writing: a property in the middle loses the comma that follows it; the
        // indentation and line ending of its own line are all that is left.
        let patched = patch_settings_document(with, &expected).expect("a patch");
        assert_eq!(
            patched,
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"port\": 4173,\r\n  \"input_device\": null,\r\n  \r\n  \"notes\": 1\r\n}\r\n"
        );
        assert_eq!(
            parse_settings_document(&patched, path, "settings file").expect("the patched document"),
            expected
        );

        // Writing: as the last property it loses the comma before it instead.
        let last = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"port\": 4173,\r\n  \"input_device\": null,\r\n  \"compare_all\": true\r\n}\r\n";
        assert_eq!(
            patch_settings_document(last, &expected).expect("a patch"),
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"smart\",\r\n  \"mute_during_recording\": false,\r\n  \"port\": 4173,\r\n  \"input_device\": null\r\n  \r\n}\r\n"
        );

        // A repeated obsolete property cannot be migrated by the same rule the
        // managed values use, so neither reading nor writing accepts it.
        let repeated = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"smart\",\n  \"mute_during_recording\": false,\n  \"compare_all\": true,\n  \"compare_all\": false\n}\n";
        let error = parse_settings_document(repeated, path, "settings file")
            .expect_err("a repeated obsolete property");
        assert!(error.to_string().contains("more than once"));
        let error = patch_settings_document(repeated, &expected)
            .expect_err("a repeated obsolete property must refuse the patch");
        assert!(error.to_string().contains("more than once"));
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
        let default_device = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"port\": 5000,\n  \"input_device\": null\n}\n";
        let loaded = parse_settings_document(default_device, path, "settings file")
            .expect("a null device");
        assert_eq!(loaded.port, 5000);
        assert_eq!(loaded.input_device, None);
        assert_eq!(
            patch_settings_document(default_device, &loaded).expect("a patch"),
            default_device
        );

        let chosen = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"port\": 5000,\n  \"input_device\": \"wasapi:card-one\"\n}\n";
        let loaded =
            parse_settings_document(chosen, path, "settings file").expect("a stored device id");
        assert_eq!(loaded.input_device.as_deref(), Some("wasapi:card-one"));
        assert_eq!(
            patch_settings_document(chosen, &loaded).expect("a patch"),
            chosen
        );
    }

    /// A `port` or `input_device` that is present has to be usable: a string, a
    /// fraction, zero, a number above the port range, a negative number, a null
    /// port, a non-string device, an empty string and a string no `cpal::DeviceId`
    /// can parse all refuse the document; a repeated property with either name is
    /// refused rather than one of the two values winning. No message quotes the
    /// value it refused, and neither the loader nor the patch substitutes one.
    #[test]
    fn a_port_or_a_device_that_is_not_usable_is_refused() {
        let path = Path::new("settings.json");
        let document_with = |property: &str, value: &str| {
            format!(
                "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"{property}\": {value}\n}}\n"
            )
        };
        let settings = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            port: DEFAULT_PORT,
            input_device: None,
        };

        for value in ["\"4173\"", "4173.5", "0", "65536", "-1", "null", "true"] {
            let document = document_with("port", value);
            let error = parse_settings_document(&document, path, "settings file")
                .expect_err("an unusable port");
            assert!(
                error.to_string().contains("\"port\""),
                "the message names the property and not the value: {error}"
            );
            let error = patch_settings_document(&document, &settings)
                .expect_err("an unusable port must refuse the patch");
            assert!(
                error.to_string().contains("\"port\""),
                "the message names the property and not the value: {error}"
            );
        }

        for value in ["5", "\"\"", "\"not-a-device-id\"", "\"wasapi:\"", "true", "{}"] {
            let document = document_with("input_device", value);
            let error = parse_settings_document(&document, path, "settings file")
                .expect_err("an unusable device");
            assert!(
                error.to_string().contains("\"input_device\""),
                "the message names the property and not the value: {error}"
            );
            let error = patch_settings_document(&document, &settings)
                .expect_err("an unusable device must refuse the patch");
            assert!(
                error.to_string().contains("\"input_device\""),
                "the message names the property and not the value: {error}"
            );
        }

        // A repeated property is refused by both paths instead of one of the two
        // values winning.
        for (property, first, second) in [
            ("port", "4173", "4174"),
            (
                "input_device",
                "\"wasapi:card-one\"",
                "\"wasapi:card-two\"",
            ),
        ] {
            let document = format!(
                "{{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"{property}\": {first},\n  \"{property}\": {second}\n}}\n"
            );
            let error = parse_settings_document(&document, path, "settings file")
                .expect_err("a repeated property");
            assert!(error.to_string().contains("more than once"));
            let error = patch_settings_document(&document, &settings)
                .expect_err("a repeated property must refuse the patch");
            assert!(error.to_string().contains("more than once"));
        }
    }

    /// A general change carries every managed value: patching with a new hotkey,
    /// mode and mute switch writes the chosen port and device too, so a port or a
    /// device stored by an earlier save is never left behind under a new
    /// configuration while unknown properties keep their bytes.
    #[test]
    fn a_general_change_does_not_leave_an_old_port_or_device_behind() {
        let path = Path::new("settings.json");
        let document = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"port\": 5000,\n  \"input_device\": \"wasapi:old\",\n  \"notes\": \"kept\"\n}\n";
        let changed = Settings {
            hotkey: "Ctrl+Shift+F9".to_owned(),
            mode: "verbatim".to_owned(),
            mute_during_recording: true,
            port: 43118,
            input_device: Some("wasapi:new".to_owned()),
        };

        let patched = patch_settings_document(document, &changed).expect("a patch");
        assert_eq!(
            patched,
            "{\n  \"hotkey\": \"Ctrl+Shift+F9\",\n  \"mode\": \"verbatim\",\n  \"mute_during_recording\": true,\n  \"port\": 43118,\n  \"input_device\": \"wasapi:new\",\n  \"notes\": \"kept\"\n}\n"
        );
        assert!(!patched.contains("5000"), "the old port is gone");
        assert!(!patched.contains("wasapi:old"), "the old device is gone");
        assert_eq!(
            parse_settings_document(&patched, path, "settings file").expect("the patched document"),
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
        let loaded = parse_settings_document(SETTINGS_EXAMPLE, path, "settings template")
            .expect("the embedded template");
        assert_eq!(loaded.hotkey, production.hotkey);
        assert_eq!(loaded.mode, "live");
        assert!(!loaded.mute_during_recording);
        assert_eq!(loaded.port, production.port);
        assert_eq!(loaded.input_device, None);

        assert_eq!(
            patch_settings_document(SETTINGS_EXAMPLE, &loaded).expect("a patch"),
            SETTINGS_EXAMPLE
        );
    }

    /// Readback refuses a document that still carries the retired property, so a
    /// patch can never report success on a file it failed to migrate even when
    /// the managed values themselves read back correctly.
    #[test]
    fn readback_refuses_a_residual_compare_all() {
        let settings = Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: false,
            port: DEFAULT_PORT,
            input_device: None,
        };
        let error = verify_patched(
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"compare_all\": true\r\n}\r\n",
            &settings,
        )
        .expect_err("a residual compare_all must refuse the readback");
        assert!(
            error.to_string().contains("\"compare_all\""),
            "the message names the property: {error}"
        );

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
            let error = parse_settings_document(&document, path, "settings file")
                .expect_err("a non-boolean switch");
            assert!(
                error.to_string().contains("\"mute_during_recording\""),
                "the message names the property and not the value: {error}"
            );
        }

        let duplicated = "{\n  \"hotkey\": \"F2\",\n  \"mode\": \"live\",\n  \"mute_during_recording\": false,\n  \"mute_during_recording\": true\n}\n";
        let error = parse_settings_document(duplicated, path, "settings file")
            .expect_err("a repeated switch");
        assert!(error.to_string().contains("more than once"));

        // The patch reads the document through the same rule, so a repeated
        // property refuses the write instead of one of the two values winning.
        let error = patch_settings_document(duplicated, &Settings {
            hotkey: "F2".to_owned(),
            mode: "live".to_owned(),
            mute_during_recording: true,
            port: DEFAULT_PORT,
            input_device: None,
        })
        .expect_err("a repeated switch must refuse the patch");
        assert!(error.to_string().contains("more than once"));
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
                port: DEFAULT_PORT,
                input_device: None,
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
                port: 43118,
                input_device: Some("wasapi:card-one".to_owned()),
            },
            keys: Arc::clone(&ring),
            revision: 2,
        }));

        let pinned_snapshot = runtime.resolve(Some(9)).expect("the pinned take");
        assert_eq!(pinned_snapshot.settings.mode, "live");
        assert!(!pinned_snapshot.settings.mute_during_recording);
        assert_eq!(pinned_snapshot.settings.port, DEFAULT_PORT);
        assert_eq!(pinned_snapshot.settings.input_device, None);
        let current = runtime.resolve(None).expect("current");
        assert_eq!(current.settings.mode, "smart");
        assert_eq!(
            current.settings.port, 43118,
            "the next session sees the published port"
        );
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
    /// The port the shell reserves comes from the document this start will read,
    /// and the preflight only reads: a stored port is returned as spelled, a
    /// document written before the port existed - or one that is not there yet -
    /// reads as the first-run default, and every byte of the file survives the
    /// question. Nothing is created for a profile that does not exist.
    #[test]
    fn the_preflight_reads_the_port_the_startup_will_use() {
        let scratch = Scratch::new("preflight-read");
        assert_eq!(
            preflight_port_from(
                profile::ACTIVE,
                None,
                Some(scratch.appdata().as_os_str()),
                active_executable(&scratch).as_deref(),
            )
            .expect("an absent profile"),
            DEFAULT_PORT,
            "no document yet reads as the default the flavor's first run documents"
        );
        assert!(
            !scratch.target().exists(),
            "the preflight creates no profile directory"
        );

        let without_port: &[u8] =
            b"// Speechek settings\r\n{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\"\r\n}\r\n";
        fs::create_dir_all(scratch.target()).expect("the profile directory");
        fs::write(scratch.target_settings(), without_port).expect("the document");
        assert_eq!(
            preflight_port_from(
                profile::ACTIVE,
                None,
                Some(scratch.appdata().as_os_str()),
                active_executable(&scratch).as_deref(),
            )
            .expect("a document without a port"),
            DEFAULT_PORT
        );
        assert_eq!(
            fs::read(scratch.target_settings()).expect("the document"),
            without_port,
            "the document keeps every byte"
        );

        let named = "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"port\": 43118\r\n}\r\n";
        fs::write(scratch.target_settings(), named).expect("the document");
        assert_eq!(
            preflight_port_from(
                profile::ACTIVE,
                None,
                Some(scratch.appdata().as_os_str()),
                active_executable(&scratch).as_deref(),
            )
            .expect("a stored port"),
            43118
        );
        assert_eq!(
            fs::read(scratch.target_settings()).expect("the document"),
            named.as_bytes()
        );
    }

    /// Port selection uses only Speechek's settings document, even when other
    /// directories exist alongside its profile.
    #[test]
    fn the_preflight_ignores_unrelated_sibling_directories() {
        let scratch = Scratch::new("preflight-siblings");
        let unrelated = scratch.appdata().join("another-application");
        fs::create_dir_all(&unrelated).expect("the unrelated directory");
        fs::write(unrelated.join(SETTINGS_FILE_NAME), b"not a settings document")
            .expect("the unrelated document");

        assert_eq!(
            preflight_port_from(
                profile::ACTIVE,
                None,
                Some(scratch.appdata().as_os_str()),
                active_executable(&scratch).as_deref(),
            )
            .expect("no Speechek profile yet"),
            DEFAULT_PORT
        );
        assert!(!scratch.target().exists());

        assert!(create_default_if_missing(&scratch.target_settings())
            .expect("the first-run Speechek profile"));
        assert_eq!(
            load_settings(&scratch.target_settings())
                .expect("the first-run settings")
                .port,
            DEFAULT_PORT
        );
        assert!(!create_default_if_missing(&scratch.target_settings())
            .expect("an existing Speechek profile"));
        fs::write(
            scratch.target_settings(),
            b"{\"hotkey\":\"F2\",\"mode\":\"live\",\"port\":43118}",
        )
        .expect("the settings document");
        assert_eq!(
            preflight_port_from(
                profile::ACTIVE,
                None,
                Some(scratch.appdata().as_os_str()),
                active_executable(&scratch).as_deref(),
            )
            .expect("the Speechek port"),
            43118
        );
        assert_eq!(
            fs::read(unrelated.join(SETTINGS_FILE_NAME)).expect("the unrelated document"),
            b"not a settings document"
        );
    }

    /// A port the loader would refuse, and a document that is not a document,
    /// refuse the preflight as well: the run stops before a socket exists
    /// instead of serving on a port nobody named, and the file it read keeps
    /// every byte so the user can repair it in the settings window.
    #[test]
    fn an_unusable_stored_port_or_document_refuses_the_preflight() {
        let scratch = Scratch::new("preflight-invalid");
        fs::create_dir_all(scratch.target()).expect("the profile directory");

        let mut documents: Vec<(String, &str)> = Vec::new();
        for value in ["\"4173\"", "4173.5", "0", "65536", "-1", "null", "true"] {
            documents.push((
                format!(
                    "{{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"port\": {value}\r\n}}\r\n"
                ),
                "\"port\"",
            ));
        }
        documents.push((
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"port\": 4173,\r\n  \"port\": 4174\r\n}\r\n".to_string(),
            "\"port\"",
        ));
        documents.push(("{ this is not json }\r\n".to_string(), "is not valid JSON"));

        for (document, expected) in &documents {
            fs::write(scratch.target_settings(), document).expect("the document");
            let error = preflight_port_from(
                profile::ACTIVE,
                None,
                Some(scratch.appdata().as_os_str()),
                active_executable(&scratch).as_deref(),
            )
            .expect_err("an unusable document refuses the preflight");
            assert!(
                error.to_string().contains(*expected),
                "the refusal names the problem ({expected}): {error}"
            );
            assert_eq!(
                fs::read(scratch.target_settings()).expect("the document"),
                document.as_bytes(),
                "the refused document is not rewritten"
            );
        }
    }

    /// The override belongs to the test flavor: a relative path is refused
    /// with the variable's name, an empty value is no override at all, and an
    /// absolute one names the document whether it exists yet or not - even
    /// without `%APPDATA%`. The development flavor ignores the same override
    /// and reads beside its executable, and production ignores it and still
    /// needs `%APPDATA%`, so neither can be moved off the profile it
    /// documents. The preflight and the resolver name exactly the same
    /// document.
    #[test]
    fn the_preflight_reads_the_debug_override_exactly_like_the_settings_path() {
        let scratch = Scratch::new("preflight-override");
        let appdata = Some(scratch.appdata().as_os_str());
        let executable = scratch.executable();
        let executable_arg = Some(executable.as_path());
        let dev_document = scratch.target_of(BuildFlavor::Development).join(SETTINGS_FILE_NAME);
        let elsewhere = scratch.appdata().join("elsewhere.json");

        // The test flavor takes the absolute override, with or without
        // `%APPDATA%`, and refuses a relative one with the variable's name.
        let error = preflight_port_from(
            BuildFlavor::Test,
            Some(Path::new("relative.json").as_os_str()),
            appdata,
            None,
        )
        .expect_err("a relative override");
        assert!(
            error.to_string().contains("SPEECHEK_CONFIG_PATH"),
            "the refusal names the variable: {error}"
        );

        assert_eq!(
            preflight_port_from(BuildFlavor::Test, Some(OsStr::new("  ")), appdata, None)
                .expect("an empty override is not one"),
            profile::defaults(BuildFlavor::Test).port
        );

        assert_eq!(
            preflight_document_from(
                BuildFlavor::Test,
                Some(elsewhere.as_os_str()),
                appdata,
                None,
            )
            .expect("an override to a document that is not there yet"),
            None
        );
        let named =
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"port\": 43118\r\n}\r\n";
        fs::write(&elsewhere, named).expect("the override document");
        assert_eq!(
            preflight_document_from(
                BuildFlavor::Test,
                Some(elsewhere.as_os_str()),
                appdata,
                None,
            )
            .expect("the override document"),
            Some(elsewhere.clone())
        );
        assert_eq!(
            preflight_port_from(BuildFlavor::Test, Some(elsewhere.as_os_str()), appdata, None)
                .expect("the override document"),
            43118
        );
        assert_eq!(
            fs::read(&elsewhere).expect("the override document"),
            named.as_bytes()
        );
        // The override alone suffices: `%APPDATA%` is not consulted at all.
        assert_eq!(
            preflight_port_from(BuildFlavor::Test, Some(elsewhere.as_os_str()), None, None)
                .expect("the override without %APPDATA%"),
            43118
        );

        // The development flavor ignores the same override: it reads beside
        // the executable it was given, creates nothing for a document that is
        // not there yet, and the overridden document is never selected.
        assert_eq!(
            preflight_port_from(BuildFlavor::Development, None, None, executable_arg)
                .expect("no development document yet"),
            profile::defaults(BuildFlavor::Development).port
        );
        assert!(
            !dev_document.exists() && !scratch.dev_bin().exists(),
            "the preflight creates no development document"
        );
        assert_eq!(
            preflight_document_from(
                BuildFlavor::Development,
                Some(elsewhere.as_os_str()),
                appdata,
                executable_arg,
            )
            .expect("the development document"),
            None
        );
        fs::create_dir_all(scratch.dev_bin()).expect("the development directory");
        fs::write(&dev_document, named).expect("the development document");
        assert_eq!(
            preflight_document_from(
                BuildFlavor::Development,
                Some(elsewhere.as_os_str()),
                appdata,
                executable_arg,
            )
            .expect("the development document"),
            Some(dev_document.clone())
        );
        assert_eq!(
            preflight_port_from(
                BuildFlavor::Development,
                Some(elsewhere.as_os_str()),
                None,
                executable_arg,
            )
            .expect("the development port"),
            43118
        );
        assert_eq!(
            fs::read(&dev_document).expect("the development document"),
            named.as_bytes(),
            "the development document is read and left as it is"
        );

        // Production ignores the override, relative or absolute, and the
        // executable path with it, and still needs `%APPDATA%`.
        let production_document = scratch.appdata().join("production.json");
        fs::write(
            &production_document,
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"port\": 43119\r\n}\r\n",
        )
        .expect("the ignored document");
        assert_eq!(
            preflight_port_from(
                BuildFlavor::Production,
                Some(production_document.as_os_str()),
                appdata,
                executable_arg,
            )
            .expect("production ignores the override"),
            profile::defaults(BuildFlavor::Production).port
        );
        let error = preflight_port_from(
            BuildFlavor::Production,
            Some(production_document.as_os_str()),
            None,
            None,
        )
        .expect_err("production still needs %APPDATA%");
        assert!(error.to_string().contains("APPDATA"), "{error}");
    }

    /// The flavor table is the single source of the identities, hotkeys and
    /// first-run ports, every row is distinct, only the development row names
    /// no `%APPDATA%` directory, and the compiled-in flavor matches the kind
    /// of build this test runs in.
    #[test]
    fn the_flavor_table_holds_each_builds_own_identity() {
        let production = profile::defaults(BuildFlavor::Production);
        assert_eq!(production.identifier, "app.speechek.desktop");
        assert_eq!(production.title, "Speechek");
        assert_eq!(production.directory, "Speechek");
        assert_eq!(production.hotkey, "F2");
        assert_eq!(production.port, 4173);

        let development = profile::defaults(BuildFlavor::Development);
        let test = profile::defaults(BuildFlavor::Test);
        assert_eq!(development.identifier, "app.speechek.dev");
        assert_eq!(development.title, "Speechek Dev");
        assert_eq!(development.hotkey, "Ctrl+Shift+F9");
        assert_eq!(development.port, 4174);
        assert_eq!(
            development.directory, "",
            "the development flavor resolves beside its executable, not through %APPDATA%"
        );
        assert_eq!(test.identifier, "app.speechek.test");
        assert_eq!(test.title, "Speechek Test");
        assert_eq!(test.directory, "Speechek-Test");
        assert_eq!(test.hotkey, "Ctrl+Shift+F10");
        assert_eq!(test.port, 4175);

        for (left, right) in [
            (&production, &development),
            (&production, &test),
            (&development, &test),
        ] {
            assert_ne!(left.identifier, right.identifier);
            assert_ne!(left.title, right.title);
            assert_ne!(left.hotkey, right.hotkey);
            assert_ne!(left.port, right.port);
        }
        assert!(
            !production.directory.is_empty() && !test.directory.is_empty(),
            "the flavors that resolve through %APPDATA% name their directory"
        );

        assert_eq!(DEFAULT_PORT, profile::defaults(profile::ACTIVE).port);

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
            let error = resolve_settings_path(BuildFlavor::Development, None, appdata, unusable)
                .expect_err("an unusable development executable");
            assert!(
                error.to_string().contains("Dev executable directory"),
                "the refusal names the directory it cannot determine: {error}"
            );
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
        let error = resolve_settings_path(
            BuildFlavor::Test,
            Some(Path::new("relative.json").as_os_str()),
            appdata,
            None,
        )
        .expect_err("a relative override");
        assert!(error.to_string().contains("SPEECHEK_CONFIG_PATH"), "{error}");

        // The test flavor's `%APPDATA%` fallback and its refusal name its own
        // directory.
        assert_eq!(
            resolve_settings_path(BuildFlavor::Test, None, appdata, None)
                .expect("the test document without an override"),
            scratch.target_of(BuildFlavor::Test).join(SETTINGS_FILE_NAME)
        );
        let error = resolve_settings_path(BuildFlavor::Test, None, None, None)
            .expect_err("no %APPDATA%");
        assert!(error.to_string().contains("APPDATA"), "{error}");
        assert!(
            error
                .to_string()
                .contains(profile::defaults(BuildFlavor::Test).directory),
            "the refusal names the flavor's directory: {error}"
        );

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
        let error = resolve_settings_path(
            BuildFlavor::Production,
            Some(elsewhere.as_os_str()),
            None,
            None,
        )
        .expect_err("production still needs %APPDATA%");
        assert!(error.to_string().contains("APPDATA"), "{error}");
        assert!(
            error
                .to_string()
                .contains(profile::defaults(BuildFlavor::Production).directory),
            "the refusal names the flavor's directory: {error}"
        );
    }

    /// The preflight and the first run agree on one document per flavor: the
    /// resolver names the flavor's own file - beside the executable for the
    /// development flavor, under `%APPDATA%` for production and test - the
    /// preflight reports it missing and creates nothing, a stored port in that
    /// flavor's document is what it reads, and the active flavor's first-run
    /// document loads with that flavor's own chord and port.
    #[test]
    fn the_preflight_and_the_first_run_document_agree_per_flavor() {
        let scratch = Scratch::new("preflight-agreement");
        let appdata = Some(scratch.appdata().as_os_str());
        let executable = scratch.executable();
        let executable_arg = Some(executable.as_path());
        let named =
            "{\r\n  \"hotkey\": \"F2\",\r\n  \"mode\": \"live\",\r\n  \"port\": 43118\r\n}\r\n";

        for flavor in [
            BuildFlavor::Production,
            BuildFlavor::Development,
            BuildFlavor::Test,
        ] {
            let path = resolve_settings_path(flavor, None, appdata, executable_arg)
                .expect("the flavor's document");
            assert_eq!(path, scratch.target_of(flavor).join(SETTINGS_FILE_NAME));
            assert_eq!(
                preflight_document_from(flavor, None, appdata, executable_arg)
                    .expect("a missing document"),
                None
            );
            assert!(
                !path.exists() && !scratch.target_of(flavor).exists(),
                "the preflight creates no profile directory"
            );

            fs::create_dir_all(scratch.target_of(flavor)).expect("the flavor's directory");
            fs::write(&path, named).expect("the document");
            assert_eq!(
                preflight_document_from(flavor, None, appdata, executable_arg)
                    .expect("the document"),
                Some(path.clone())
            );
            assert_eq!(
                preflight_port_from(flavor, None, appdata, executable_arg)
                    .expect("the stored port"),
                43118
            );
            assert_eq!(fs::read(&path).expect("the document"), named.as_bytes());
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

        // A clean profile for the active flavor: nothing there reads as the
        // flavor's first-run default, writing the first-run document makes the
        // same port the one the startup will reserve, and the document is a
        // valid settings file the settings window would not reformat.
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
        assert_eq!(
            preflight_port_from(flavor, None, fresh_appdata, fresh_executable.as_deref())
                .expect("no document yet"),
            DEFAULT_PORT
        );
        assert!(create_default_if_missing(&path).expect("the first-run document"));
        assert!(!create_default_if_missing(&path).expect("an existing document"));
        assert_eq!(
            preflight_port_from(flavor, None, fresh_appdata, fresh_executable.as_deref())
                .expect("the first-run port"),
            DEFAULT_PORT
        );
        let first_run = load_settings(&path).expect("the first-run document loads");
        assert_eq!(first_run.hotkey, defaults.hotkey);
        assert_eq!(first_run.port, defaults.port);
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

    /// The first-run document: production writes the embedded template exactly,
    /// a debug flavor writes the same annotated template with only its own
    /// chord and port, and the two debug flavors do not share one document.
    #[test]
    fn the_first_run_document_moves_only_the_flavor_values() {
        let document =
            first_run_document(BuildFlavor::Production).expect("the production document");
        assert!(
            matches!(&document, Cow::Borrowed(_)),
            "production borrows the embedded template instead of copying it"
        );
        assert_eq!(document.as_ref(), SETTINGS_EXAMPLE);

        let template = parse_settings_document(
            SETTINGS_EXAMPLE,
            Path::new("settings.example.json"),
            "settings template",
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
            let document = first_run_document(flavor).expect("a debug first-run document");
            assert_ne!(document.as_ref(), SETTINGS_EXAMPLE);
            assert!(
                document.contains(&format!("\"{}\"", defaults.hotkey)),
                "the flavor's own chord is written"
            );

            let loaded = parse_settings_document(
                &document,
                Path::new("settings.json"),
                "settings file",
            )
            .expect("the debug first-run document");
            assert_eq!(loaded.hotkey, defaults.hotkey);
            assert_eq!(loaded.port, defaults.port);
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
            first_run_document(BuildFlavor::Development)
                .expect("the development document")
                .as_ref(),
            first_run_document(BuildFlavor::Test)
                .expect("the test document")
                .as_ref()
        );
    }
}
