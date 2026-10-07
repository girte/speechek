//! The interface languages and the catalog messages the shell shows.
//!
//! `public/messages.json` is the one dictionary. `build.rs` validates it and
//! renders `MessageId` plus its `&'static str` templates into
//! `OUT_DIR/messages.rs`, so the running shell never parses the catalog and
//! never builds a map of it: [`render`] only substitutes the descriptor's
//! arguments. Substitution is single-pass — an argument's own text (a hotkey,
//! a device name, an OS error) is copied through untouched, even when it
//! happens to look like a placeholder.

use std::borrow::Cow;
use std::fmt;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_SZ,
    REG_VALUE_TYPE,
};

use crate::profile::{self, BuildFlavor};

include!(concat!(env!("OUT_DIR"), "/messages.rs"));

/// The languages the interface ships in.
///
/// The wire spelling is lowercase — `"en"` / `"ru"` — in settings files, in
/// the `speechek:settings-changed` payload and in `/api/settings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    En,
    Ru,
}

/// A catalog message plus the values its placeholders take.
///
/// The key is semantic: one reason keeps one key whatever the language, so
/// callers switch on the descriptor instead of matching translated prose.
/// `args` stays absent on the wire when the message has no placeholders; a
/// value is a number, a safe string or another `UiMessage` for an authored
/// nested cause. Secrets never go into `args`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiMessage {
    pub key: MessageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Map<String, serde_json::Value>>,
}

impl UiMessage {
    /// A message without placeholders.
    pub fn new(key: MessageId) -> Self {
        Self { key, args: None }
    }

    /// The same message with one more named placeholder value.
    pub fn with_arg(mut self, name: &'static str, value: serde_json::Value) -> Self {
        self.args
            .get_or_insert_with(serde_json::Map::new)
            .insert(name.to_owned(), value);
        self
    }
}

/// Show `message` in `language`.
///
/// Without arguments the template itself is borrowed; placeholders need the
/// rendered `String`. Each `{name}` is resolved once against the descriptor's
/// top-level `args` and copied in as text — never as HTML, never rescanned —
/// and a repeated placeholder sees the same value every time. A missing or
/// unusable argument leaves its placeholder visible (`{name}`) as a developer
/// diagnostic instead of silently dropping text. A nested `UiMessage` cause is
/// rendered in the same language, with the same contract.
pub fn render(language: Language, message: &UiMessage) -> Cow<'static, str> {
    render_parts(language, message.key, message.args.as_ref())
}

fn render_parts(
    language: Language,
    key: MessageId,
    args: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Cow<'static, str> {
    let template = key.template(language);
    let args = args.filter(|args| !args.is_empty());
    if args.is_none() || !template.contains('{') {
        diagnose_arguments(key, template, args);
        return Cow::Borrowed(template);
    }
    let mut rendered = String::with_capacity(template.len());
    append_parts(&mut rendered, language, key, args);
    Cow::Owned(rendered)
}

fn append_parts(
    rendered: &mut String,
    language: Language,
    key: MessageId,
    args: Option<&serde_json::Map<String, serde_json::Value>>,
) {
    let template = key.template(language);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        let Some(close) = tail.find('}') else {
            rendered.push_str(&rest[open..]);
            return;
        };
        let name = &tail[..close];
        rest = &tail[close + 1..];
        if !args.and_then(|args| args.get(name))
            .is_some_and(|value| append_value(rendered, language, value))
        {
            diagnostic(key, "has no usable argument named", name);
            rendered.push('{');
            rendered.push_str(name);
            rendered.push('}');
        }
    }
    rendered.push_str(rest);
    if let Some(args) = args {
        for name in args.keys() {
            if !template_names(template, name) {
                diagnostic(key, "ignores unused argument", name);
            }
        }
    }
}

fn diagnose_arguments(
    key: MessageId,
    template: &str,
    args: Option<&serde_json::Map<String, serde_json::Value>>,
) {
    if let Some(args) = args {
        for name in args.keys() {
            diagnostic(key, "ignores unused argument", name);
        }
        return;
    }
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let tail = &rest[open + 1..];
        let Some(close) = tail.find('}') else { return; };
        diagnostic(key, "has no usable argument named", &tail[..close]);
        rest = &tail[close + 1..];
    }
}

/// Whether `template` carries `name` as one of its `{name}` placeholders.
fn template_names(template: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut offset = 0;
    while let Some(index) = template[offset..].find(name) {
        let start = offset + index;
        let end = start + name.len();
        if template[..start].ends_with('{') && template[end..].starts_with('}') {
            return true;
        }
        offset = end;
    }
    false
}

/// Appends raw text, numbers and nested descriptors directly to the one final
/// buffer. Inserted strings are never rescanned as templates.
fn append_value(rendered: &mut String, language: Language, value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => rendered.push_str(text),
        serde_json::Value::Number(number) => {
            use std::fmt::Write as _;
            let _ = write!(rendered, "{number}");
        }
        serde_json::Value::Object(map) => {
            let Some(key) = map.get("key").and_then(serde_json::Value::as_str)
                .and_then(MessageId::from_key) else { return false; };
            let args = match map.get("args") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::Object(args)) => Some(args),
                Some(_) => return false,
            };
            append_parts(rendered, language, key, args);
        }
        _ => return false,
    }
    true
}

/// One diagnostic line for a malformed descriptor: the message key and the
/// argument name, never the argument's value. A diagnostic must not be able to
/// break rendering, so a failed write is ignored.
fn diagnostic(key: MessageId, problem: &str, name: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "i18n: {} {problem} {name}", key.as_str());
}

/// A failure that crosses a command or HTTP boundary.
///
/// `code` is the stable ASCII discriminator callers already switch on,
/// `message` is the English diagnostic for logs and older callers, and `ui` is
/// the descriptor the current language renders.
#[derive(Clone, Debug, Serialize)]
pub struct UiError {
    pub code: &'static str,
    pub message: String,
    pub ui: UiMessage,
}

impl UiError {
    /// Build a failure from its semantic descriptor; the English diagnostic is
    /// rendered from that descriptor, not kept as a second source of copy.
    pub fn new(code: &'static str, ui: UiMessage) -> Self {
        let message = render(Language::En, &ui).into_owned();
        Self { code, message, ui }
    }
}

impl fmt::Display for UiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UiError {}

/* -------------------------------------------------------------------------- */
/* Initial language                                                           */
/* -------------------------------------------------------------------------- */

/// The value name the NSIS language dialog writes its choice under, exactly
/// as `nsis/installer.nsi` defines it.
const INSTALLER_LANGUAGE_VALUE: &str = "Installer Language";

/// The key the production installer records its values under: the publisher
/// and product name of the distributed shell.
const PRODUCTION_REGISTRY_KEY: &str = r"Software\girte\Speechek";

/// The isolated test flavor's own key, so a test install never reads - and is
/// never mistaken for - the installed shell's record.
const TEST_REGISTRY_KEY: &str = r"Software\girte\Speechek Test";

/// The installer LANGIDs this interface has a language for.
const INSTALLER_ENGLISH: u16 = 1033;
const INSTALLER_RUSSIAN: u16 = 1049;

/// The primary-language bits of a LANGID that name Russian.
const RUSSIAN_PRIMARY_LANGUAGE: u16 = 0x19;

/// How many UTF-16 units the installer value is read into: a LANGID never
/// needs more, and a fixed buffer keeps a malformed value bounded.
const INSTALLER_LANGUAGE_UNITS: usize = 32;

/// The resolved default, computed once: reading the registry and the user's
/// UI language is a startup question, and the answer cannot change while the
/// shell runs.
static DETECTED_LANGUAGE: OnceLock<Language> = OnceLock::new();

/// The language a profile without a saved choice starts in: the installer's
/// recorded choice for this flavor, otherwise the user's Windows UI language,
/// with English for everything else.
pub fn detect_default_language() -> Language {
    *DETECTED_LANGUAGE.get_or_init(|| {
        resolve_initial_language(
            None,
            installer_langid(profile::ACTIVE),
            user_interface_language(),
        )
    })
}

/// The precedence rule behind [`detect_default_language`], with every input
/// explicit: an explicit saved choice wins over everything, then the
/// installer's recorded language, then the Windows UI language. Only the two
/// languages the interface ships in are supported: any other installer value
/// counts as absent, and any Windows language other than Russian reads as
/// English.
pub fn resolve_initial_language(
    saved: Option<Language>,
    installer_langid: Option<u16>,
    windows_langid: u16,
) -> Language {
    if let Some(saved) = saved {
        return saved;
    }
    match installer_langid {
        Some(INSTALLER_ENGLISH) => Language::En,
        Some(INSTALLER_RUSSIAN) => Language::Ru,
        _ => windows_primary_language(windows_langid),
    }
}

/// The single fact this build needs from a Windows LANGID: Russian, or
/// anything else.
fn windows_primary_language(windows_langid: u16) -> Language {
    if windows_langid & 0x03ff == RUSSIAN_PRIMARY_LANGUAGE {
        Language::Ru
    } else {
        Language::En
    }
}

/// The installer's recorded language for one flavor, or `None` when there is
/// none to read: the development flavor never looks at an installer's keys,
/// and a missing, unreadable or malformed value counts as absent.
fn installer_langid(flavor: BuildFlavor) -> Option<u16> {
    read_installer_langid(installer_registry_key(flavor)?)
}

/// The product key one flavor's installer writes `Installer Language` under.
/// The development flavor keeps none: it resolves beside its executable and
/// must never read a record that belongs to an installed shell.
fn installer_registry_key(flavor: BuildFlavor) -> Option<&'static str> {
    match flavor {
        BuildFlavor::Production => Some(PRODUCTION_REGISTRY_KEY),
        BuildFlavor::Test => Some(TEST_REGISTRY_KEY),
        BuildFlavor::Development => None,
    }
}

/// Reads `"Installer Language"` under `subkey` as the LANGID it spells. The
/// value is a `REG_SZ` UTF-16 decimal string, so anything else - a foreign
/// type, an odd byte count, an oversized value, a buffer without its single
/// terminating NUL, an embedded NUL or a non-digit - is no value at all.
/// Nothing is created or written: a key the installer never wrote is simply an
/// install without a recorded choice.
fn read_installer_langid(subkey: &str) -> Option<u16> {
    let key = open_registry_key(subkey)?;
    let value = wide(INSTALLER_LANGUAGE_VALUE);
    let mut value_type = REG_VALUE_TYPE(0);
    let mut buffer = [0u16; INSTALLER_LANGUAGE_UNITS];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS || value_type != REG_SZ || size % 2 != 0 {
        return None;
    }
    let units = (size / 2) as usize;
    if units > buffer.len() {
        return None;
    }
    parse_installer_langid(&buffer[..units])
}

/// The LANGID a `REG_SZ` value spells, from the UTF-16 code units
/// `RegQueryValueExW` returned: decimal digits followed by exactly one
/// terminating NUL. Anything else - an empty value, an embedded NUL, digits
/// missing their terminator or a number beyond `u16` - is `None`.
fn parse_installer_langid(units: &[u16]) -> Option<u16> {
    let (terminator, digits) = units.split_last()?;
    if *terminator != 0 || digits.is_empty() || digits.contains(&0) {
        return None;
    }
    let mut value: u32 = 0;
    for unit in digits {
        if !(0x30..=0x39).contains(unit) {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(unit - 0x30))?;
    }
    u16::try_from(value).ok()
}

/// An open registry key, closed when it drops.
struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Opens `subkey` under `HKEY_CURRENT_USER` for reading, or `None` when it is
/// not there. Nothing is created.
fn open_registry_key(subkey: &str) -> Option<RegistryKey> {
    let name = wide(subkey);
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(name.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut key,
        )
    };
    (status == ERROR_SUCCESS).then_some(RegistryKey(key))
}

/// The user's Windows UI language as a LANGID. This is the language of the
/// interface - not the regional format, the keyboard or the number locale -
/// and it is the last resort when neither the saved settings nor the installer
/// name a supported language.
fn user_interface_language() -> u16 {
    unsafe { GetUserDefaultUILanguage() }
}

/// `text` as a NUL-terminated UTF-16 string, the form the registry APIs take.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_are_substituted_into_both_languages() {
        for language in [Language::En, Language::Ru] {
            let message = UiMessage::new(MessageId::KeysCountOne).with_arg("count", json!(7));
            let rendered = render(language, &message);
            assert!(!rendered.contains("{count}"));
            assert!(rendered.contains('7'));
        }
    }

    #[test]
    fn a_missing_argument_keeps_its_placeholder_visible() {
        let message = UiMessage::new(MessageId::KeysCountOne).with_arg("other", json!(1));
        assert_eq!(
            render(Language::En, &message),
            MessageId::KeysCountOne.template(Language::En)
        );
    }

    #[test]
    fn substituted_values_are_text_not_templates() {
        let message = UiMessage::new(MessageId::KeysCountOne)
            .with_arg("count", json!("{second}"))
            .with_arg("second", json!("leaked"));
        let rendered = render(Language::En, &message);
        assert!(rendered.contains("{second}"));
        assert!(!rendered.contains("leaked"));
    }

    #[test]
    fn an_unused_argument_leaves_the_text_unchanged() {
        let message = UiMessage::new(MessageId::InternalError).with_arg("extra", json!("ignored"));
        assert_eq!(
            render(Language::En, &message),
            MessageId::InternalError.template(Language::En)
        );
    }

    #[test]
    fn a_nested_message_renders_in_the_same_language() {
        let message = UiMessage::new(MessageId::CombinedWarnings)
            .with_arg("first", json!({ "key": "InternalError" }))
            .with_arg("second", json!({ "key": "OverlayNotPasted" }));
        for language in [Language::En, Language::Ru] {
            let rendered = render(language, &message);
            assert!(rendered.contains(MessageId::InternalError.template(language)));
            assert!(rendered.contains(MessageId::OverlayNotPasted.template(language)));
        }
    }

    #[test]
    fn a_nested_message_fills_its_own_arguments() {
        let nested = json!({ "key": "KeysCountFew", "args": { "count": "2" } });
        let message = UiMessage::new(MessageId::CombinedWarnings)
            .with_arg("first", json!({ "key": "InternalError" }))
            .with_arg("second", nested);
        let expected = MessageId::KeysCountFew
            .template(Language::Ru)
            .replace("{count}", "2");
        assert!(render(Language::Ru, &message).contains(expected.as_str()));
    }

    #[test]
    fn an_unknown_nested_key_keeps_the_placeholder_visible() {
        let message = UiMessage::new(MessageId::CombinedWarnings)
            .with_arg("first", json!({ "key": "InternalError" }))
            .with_arg("second", json!({ "key": "NoSuchMessage" }));
        let rendered = render(Language::En, &message);
        assert!(rendered.contains("{second}"));
        assert!(rendered.contains(MessageId::InternalError.template(Language::En)));
    }

    #[test]
    fn language_spells_lowercase_and_rejects_strangers() {
        assert_eq!(
            serde_json::to_value(Language::En).expect("en serializes"),
            json!("en")
        );
        assert_eq!(
            serde_json::to_value(Language::Ru).expect("ru serializes"),
            json!("ru")
        );
        assert_eq!(
            serde_json::from_value::<Language>(json!("ru")).expect("ru parses"),
            Language::Ru
        );
        assert!(serde_json::from_value::<Language>(json!("de")).is_err());
    }

    #[test]
    fn a_saved_choice_beats_the_installer_and_windows() {
        assert_eq!(
            resolve_initial_language(Some(Language::Ru), Some(INSTALLER_ENGLISH), 0x0409),
            Language::Ru
        );
        assert_eq!(
            resolve_initial_language(Some(Language::En), Some(INSTALLER_RUSSIAN), 0x0419),
            Language::En
        );
    }

    #[test]
    fn the_installer_language_beats_windows() {
        assert_eq!(
            resolve_initial_language(None, Some(INSTALLER_ENGLISH), 0x0419),
            Language::En
        );
        assert_eq!(
            resolve_initial_language(None, Some(INSTALLER_RUSSIAN), 0x0409),
            Language::Ru
        );
    }

    #[test]
    fn an_absent_or_unsupported_installer_value_defers_to_windows() {
        for installer in [None, Some(0), Some(1031), Some(1032), Some(u16::MAX)] {
            assert_eq!(
                resolve_initial_language(None, installer, 0x0419),
                Language::Ru,
                "installer {installer:?} leaves the Windows language to decide"
            );
            assert_eq!(
                resolve_initial_language(None, installer, 0x0409),
                Language::En,
                "installer {installer:?} leaves the Windows language to decide"
            );
        }
    }

    #[test]
    fn a_windows_language_other_than_russian_reads_as_english() {
        for russian in [0x0019u16, 0x0419, 0x0819] {
            assert_eq!(resolve_initial_language(None, None, russian), Language::Ru);
        }
        for other in [0u16, 0x0409, 0x0422, 0x0407] {
            assert_eq!(resolve_initial_language(None, None, other), Language::En);
        }
    }

    #[test]
    fn the_installer_string_parses_as_a_decimal_langid() {
        for (text, langid) in [
            ("1033", INSTALLER_ENGLISH),
            ("1049", INSTALLER_RUSSIAN),
            ("0", 0),
            ("65535", u16::MAX),
        ] {
            let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            assert_eq!(parse_installer_langid(&units), Some(langid), "{text}");
        }
    }

    #[test]
    fn a_malformed_installer_string_is_no_value() {
        for units in [
            &[][..],
            &[0u16][..],
            // No terminating NUL.
            &[0x31, 0x30, 0x33, 0x33][..],
            // Two terminators, and a NUL inside the digits.
            &[0x31, 0x30, 0x33, 0x33, 0x00, 0x00][..],
            &[0x31, 0x00, 0x33, 0x00][..],
            // A character that is not a decimal digit.
            &[0x31, 0x78, 0x33, 0x33, 0x00][..],
            // Trailing data after the terminator.
            &[0x31, 0x30, 0x33, 0x33, 0x00, 0x31][..],
        ] {
            assert_eq!(parse_installer_langid(units), None, "{units:?}");
        }
        // A number that does not fit a LANGID is not a LANGID either.
        let units: Vec<u16> = "65536".encode_utf16().chain(Some(0)).collect();
        assert_eq!(parse_installer_langid(&units), None);
    }

    #[test]
    fn only_the_installed_flavors_read_an_installer_key() {
        assert_eq!(
            installer_registry_key(BuildFlavor::Production),
            Some(PRODUCTION_REGISTRY_KEY)
        );
        assert_eq!(
            installer_registry_key(BuildFlavor::Test),
            Some(TEST_REGISTRY_KEY)
        );
        assert_eq!(
            installer_registry_key(BuildFlavor::Development),
            None,
            "the development flavor reads no installer record"
        );
        assert_ne!(
            PRODUCTION_REGISTRY_KEY, TEST_REGISTRY_KEY,
            "a test install keeps its own key"
        );
    }
}
