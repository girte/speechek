//! Schema of the interface message catalog (`public/messages.json`).
//!
//! The catalog is the single source of every user-visible message. The build
//! script compiles this module to validate the catalog and render
//! `OUT_DIR/messages.rs`; the integration test compiles the same file through
//! `#[path]`, so the rules have exactly one home and can be exercised without
//! reading the real catalog. The running shell never sees this module: it only
//! sees the generated enum and its templates.
//!
//! Rules: the root is a non-empty object; every key is a PascalCase ASCII
//! identifier (an uppercase letter, then letters and digits); every entry
//! carries exactly the strings `en` and `ru`, neither empty nor whitespace-only;
//! and both templates name the identical set of `{placeholder}`s, where a name
//! starts with an ASCII letter and continues with letters, digits or
//! underscores. A translation may repeat or reorder a placeholder, never drop
//! or invent one.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

/// Validate a catalog document and return its messages in key order.
///
/// The map is exactly what the generator renders: `key -> (en, ru)`, with the
/// translation strings passed through verbatim. Errors name the offending key
/// and the broken rule; callers add their own context (the build script names
/// the file it failed to parse).
pub fn parse_catalog(text: &str) -> Result<BTreeMap<String, (String, String)>, String> {
    let root: Value = serde_json::from_str(text)
        .map_err(|error| format!("catalog is not valid JSON: {error}"))?;
    let Value::Object(entries) = root else {
        return Err("catalog must be one object of PascalCase messages".to_owned());
    };
    if entries.is_empty() {
        return Err("catalog must contain at least one message".to_owned());
    }

    // The parsed document is consumed: its keys and the two translation strings
    // move straight into the output map instead of being cloned out of it.
    let mut messages = BTreeMap::new();
    for (key, value) in entries {
        validate_identifier(&key)?;
        let mut entry = match value {
            Value::Object(entry) => entry,
            _ => return Err(format!("{key}: entry must be an object with en and ru")),
        };
        for field in entry.keys() {
            if field != "en" && field != "ru" {
                return Err(format!(
                    "{key}: unknown field {field:?}; an entry carries exactly en and ru"
                ));
            }
        }
        let en = translation(&key, "en", &mut entry)?;
        let ru = translation(&key, "ru", &mut entry)?;
        {
            let en_placeholders = placeholder_names(&key, "en", &en)?;
            let ru_placeholders = placeholder_names(&key, "ru", &ru)?;
            if en_placeholders != ru_placeholders {
                return Err(format!(
                    "{key}: en placeholders {en_placeholders:?} must match ru placeholders {ru_placeholders:?}"
                ));
            }
        }
        messages.insert(key, (en, ru));
    }
    Ok(messages)
}

/// One locale's translation, taken out of the entry: present, a string, and
/// not blank. Whitespace-only text counts as empty — such a message would
/// vanish in the UI. The owned string is handed back so the caller moves it
/// into the result instead of copying it.
fn translation(key: &str, locale: &str, entry: &mut Map<String, Value>) -> Result<String, String> {
    match entry.remove(locale) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text),
        Some(Value::String(_)) => {
            Err(format!("{key}: {locale} must not be empty or whitespace-only"))
        }
        _ => Err(format!("{key}: {locale} must be a string")),
    }
}

/// Every identifier is both the Rust variant name and the wire key, so it has
/// to be PascalCase ASCII: an uppercase letter followed by letters and digits.
fn validate_identifier(key: &str) -> Result<(), String> {
    let mut characters = key.chars();
    match characters.next() {
        Some(first) if first.is_ascii_uppercase() => {}
        _ => {
            return Err(format!(
                "message identifier {key:?} must start with an ASCII uppercase letter"
            ))
        }
    }
    if characters.all(|character| character.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(format!(
            "message identifier {key:?} must be ASCII letters and digits only"
        ))
    }
}

/// The `{name}` placeholders a template uses, as a borrow-only set: the two
/// languages are compared by name without copying the translations.
fn placeholder_names<'a>(
    key: &str,
    locale: &str,
    template: &'a str,
) -> Result<BTreeSet<&'a str>, String> {
    let mut names = BTreeSet::new();
    let mut position = 0;
    while let Some(open) = template[position..].find('{') {
        let open = position + open;
        // Text before an opening brace must not contain a closing one. The
        // previous scan only checked the text after the last placeholder, so a
        // stray `}` was missed whenever a valid placeholder followed it.
        if let Some(close) = template[position..open].find('}') {
            return Err(unmatched_closing_brace(
                key,
                locale,
                template,
                position + close,
            ));
        }
        let close = template[open + 1..].find('}').ok_or_else(|| {
            format!("{key}: unclosed {{ in {locale} template {template:?} at byte {open}")
        })?;
        let close = open + 1 + close;
        let name = &template[open + 1..close];
        if !valid_placeholder_name(name) {
            return Err(format!(
                "{key}: invalid placeholder {{{name}}} in {locale} template {template:?}"
            ));
        }
        names.insert(name);
        position = close + 1;
    }
    // Nothing may follow the last placeholder either (this is the whole
    // template when it has no placeholders at all).
    if let Some(close) = template[position..].find('}') {
        return Err(unmatched_closing_brace(
            key,
            locale,
            template,
            position + close,
        ));
    }
    Ok(names)
}

fn unmatched_closing_brace(key: &str, locale: &str, template: &str, byte: usize) -> String {
    format!("{key}: unmatched }} in {locale} template {template:?} at byte {byte}")
}

/// Placeholder names follow the WebView's own substitution scan: a letter
/// followed by letters, digits or underscores.
fn valid_placeholder_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first.is_ascii_alphabetic())
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}
