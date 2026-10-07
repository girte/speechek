//! Schema tests for the shared message catalog module.
//!
//! `#[path]` compiles the same `catalog.rs` the build script uses, so these
//! tests pin the rules that gate `public/messages.json` against inline fixtures
//! alone: they never read the real catalog, the environment or any profile.
//!
//! They assert accept/reject and the values that must survive validation, never
//! the wording of an error message — the rule is the contract, the diagnostics
//! are free to change.

#[path = "../catalog.rs"]
mod catalog;

use std::collections::BTreeMap;

use catalog::parse_catalog;

type Messages = BTreeMap<String, (String, String)>;

fn accepted(text: &str) -> Messages {
    parse_catalog(text).expect("fixture must be accepted")
}

fn rejected(text: &str) {
    assert!(
        parse_catalog(text).is_err(),
        "fixture must be rejected: {text}"
    );
}

#[test]
fn accepts_padded_translation_verbatim() {
    // Only all-whitespace text is empty; surrounding space is the author's and
    // is passed through unchanged.
    let messages = accepted(r#"{"Greeting": {"en": "  Hello  ", "ru": "  Привет  "}}"#);
    assert_eq!(
        messages["Greeting"],
        ("  Hello  ".to_owned(), "  Привет  ".to_owned())
    );
}

#[test]
fn accepts_repeated_and_reordered_placeholders() {
    // A translation may repeat or reorder a placeholder, never drop or invent one.
    accepted(
        r#"{"Repeat": {"en": "{name} meets {name} after {when}", "ru": "С {when} {name} снова {name}"}}"#,
    );
}

#[test]
fn rejects_empty_catalog() {
    rejected("{}");
}

#[test]
fn rejects_non_object_root() {
    rejected(r#"["Greeting"]"#);
}

#[test]
fn rejects_non_object_entry() {
    rejected(r#"{"Greeting": "Hello"}"#);
}

#[test]
fn rejects_missing_languages() {
    rejected(r#"{"Greeting": {"en": "Hello"}}"#);
    rejected(r#"{"Greeting": {"ru": "Привет"}}"#);
}

#[test]
fn rejects_empty_and_whitespace_only_translations() {
    let fixtures = [
        r#"{"Greeting": {"en": "", "ru": "Привет"}}"#,
        r#"{"Greeting": {"en": "   ", "ru": "Привет"}}"#,
        r#"{"Greeting": {"en": "\t\n ", "ru": "Привет"}}"#,
        r#"{"Greeting": {"en": "Hello", "ru": ""}}"#,
        r#"{"Greeting": {"en": "Hello", "ru": " \t "}}"#,
    ];
    for text in fixtures {
        rejected(text);
    }
}

#[test]
fn rejects_unknown_locale_fields() {
    rejected(r#"{"Greeting": {"en": "Hello", "ru": "Привет", "de": "Hallo"}}"#);
}

#[test]
fn rejects_non_string_translation() {
    rejected(r#"{"Greeting": {"en": 5, "ru": "Привет"}}"#);
}

#[test]
fn rejects_invalid_identifiers() {
    for key in ["greeting", "Greet-ing", "1Greeting", "Greet ing", ""] {
        rejected(&format!("{{\"{key}\": {{\"en\": \"Hello\", \"ru\": \"Привет\"}}}}"));
    }
}

#[test]
fn rejects_invalid_placeholder_names() {
    for template in ["{1st}", "{}", "{a-b}", "{a b}"] {
        rejected(&format!(
            "{{\"Greeting\": {{\"en\": \"Hi {template}\", \"ru\": \"Привет\"}}}}"
        ));
    }
}

#[test]
fn rejects_unclosed_placeholder() {
    rejected(r#"{"Greeting": {"en": "Hi {name", "ru": "Привет"}}"#);
}

#[test]
fn rejects_stray_closing_brace_before_later_placeholder() {
    // Regression: a suffix-only scan used to accept a stray `}` as long as a
    // valid placeholder followed it.
    rejected(r#"{"Greeting": {"en": "} {name}", "ru": "Привет {name}"}}"#);
}

#[test]
fn rejects_closing_brace_without_any_placeholder() {
    rejected(r#"{"Greeting": {"en": "All done}", "ru": "Готово}"}}"#);
}

#[test]
fn rejects_closing_brace_after_last_placeholder() {
    rejected(r#"{"Greeting": {"en": "Hi {name}}", "ru": "Привет {name}"}}"#);
}

#[test]
fn rejects_closing_brace_between_placeholders() {
    rejected(r#"{"Greeting": {"en": "{a} } {b}", "ru": "{a} {b}"}}"#);
}

#[test]
fn rejects_mismatched_placeholder_sets() {
    rejected(r#"{"Greeting": {"en": "Hi {name}", "ru": "Привет"}}"#);
    rejected(r#"{"Greeting": {"en": "Hi {first}", "ru": "Привет {second}"}}"#);
}

#[test]
fn rejects_malformed_json() {
    rejected("{not json");
}
