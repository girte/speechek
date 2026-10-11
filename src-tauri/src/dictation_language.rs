//! The dictation languages: the BCP-47 codes Gemini 3.5 Transcribe documents for
//! both the batch and the Live model, each with the catalog key that names it.
//! Sources (identical lists, checked 2026-10-11):
//! https://ai.google.dev/gemini-api/docs/transcribe#supported-languages
//! https://ai.google.dev/gemini-api/docs/live-api/live-transcribe#supported-languages

use serde::Serialize;

use crate::i18n::MessageId;

/// One language the dictation-language pickers offer: the code sent to Gemini and
/// the catalog key of its display name.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct DictationLanguage {
    pub code: &'static str,
    pub label: MessageId,
}

/// The most hints the settings window offers and the document may store.
pub const MAX_DICTATION_LANGUAGES: usize = 2;

/// Every language Gemini documents for transcription, in the documentation's order.
pub const DICTATION_LANGUAGES: &[DictationLanguage] = &[
    DictationLanguage { code: "af-ZA", label: MessageId::DictationLanguageAfZa },
    DictationLanguage { code: "am-ET", label: MessageId::DictationLanguageAmEt },
    DictationLanguage { code: "ar-EG", label: MessageId::DictationLanguageArEg },
    DictationLanguage { code: "hy-AM", label: MessageId::DictationLanguageHyAm },
    DictationLanguage { code: "as-IN", label: MessageId::DictationLanguageAsIn },
    DictationLanguage { code: "az-AZ", label: MessageId::DictationLanguageAzAz },
    DictationLanguage { code: "be-BY", label: MessageId::DictationLanguageBeBy },
    DictationLanguage { code: "bn-BD", label: MessageId::DictationLanguageBnBd },
    DictationLanguage { code: "bn-IN", label: MessageId::DictationLanguageBnIn },
    DictationLanguage { code: "bs-BA", label: MessageId::DictationLanguageBsBa },
    DictationLanguage { code: "bg-BG", label: MessageId::DictationLanguageBgBg },
    DictationLanguage { code: "rup-BG", label: MessageId::DictationLanguageRupBg },
    DictationLanguage { code: "my-MM", label: MessageId::DictationLanguageMyMm },
    DictationLanguage { code: "yue-Hant-HK", label: MessageId::DictationLanguageYueHantHk },
    DictationLanguage { code: "ca-ES", label: MessageId::DictationLanguageCaEs },
    DictationLanguage { code: "ceb", label: MessageId::DictationLanguageCeb },
    DictationLanguage { code: "km-KH", label: MessageId::DictationLanguageKmKh },
    DictationLanguage { code: "hr-HR", label: MessageId::DictationLanguageHrHr },
    DictationLanguage { code: "cs-CZ", label: MessageId::DictationLanguageCsCz },
    DictationLanguage { code: "da-DK", label: MessageId::DictationLanguageDaDk },
    DictationLanguage { code: "nl-NL", label: MessageId::DictationLanguageNlNl },
    DictationLanguage { code: "en-GB", label: MessageId::DictationLanguageEnGb },
    DictationLanguage { code: "en-IN", label: MessageId::DictationLanguageEnIn },
    DictationLanguage { code: "en-US", label: MessageId::DictationLanguageEnUs },
    DictationLanguage { code: "et-EE", label: MessageId::DictationLanguageEtEe },
    DictationLanguage { code: "fa-IR", label: MessageId::DictationLanguageFaIr },
    DictationLanguage { code: "fil-PH", label: MessageId::DictationLanguageFilPh },
    DictationLanguage { code: "fi-FI", label: MessageId::DictationLanguageFiFi },
    DictationLanguage { code: "fr-FR", label: MessageId::DictationLanguageFrFr },
    DictationLanguage { code: "gl-ES", label: MessageId::DictationLanguageGlEs },
    DictationLanguage { code: "ka-GE", label: MessageId::DictationLanguageKaGe },
    DictationLanguage { code: "de-DE", label: MessageId::DictationLanguageDeDe },
    DictationLanguage { code: "el-GR", label: MessageId::DictationLanguageElGr },
    DictationLanguage { code: "gu-IN", label: MessageId::DictationLanguageGuIn },
    DictationLanguage { code: "ha-NG", label: MessageId::DictationLanguageHaNg },
    DictationLanguage { code: "he-IL", label: MessageId::DictationLanguageHeIl },
    DictationLanguage { code: "hi-IN", label: MessageId::DictationLanguageHiIn },
    DictationLanguage { code: "hu-HU", label: MessageId::DictationLanguageHuHu },
    DictationLanguage { code: "is-IS", label: MessageId::DictationLanguageIsIs },
    DictationLanguage { code: "id-ID", label: MessageId::DictationLanguageIdId },
    DictationLanguage { code: "it-IT", label: MessageId::DictationLanguageItIt },
    DictationLanguage { code: "ja-JP", label: MessageId::DictationLanguageJaJp },
    DictationLanguage { code: "jv-ID", label: MessageId::DictationLanguageJvId },
    DictationLanguage { code: "kea-CV", label: MessageId::DictationLanguageKeaCv },
    DictationLanguage { code: "kn-IN", label: MessageId::DictationLanguageKnIn },
    DictationLanguage { code: "kk-KZ", label: MessageId::DictationLanguageKkKz },
    DictationLanguage { code: "ko-KR", label: MessageId::DictationLanguageKoKr },
    DictationLanguage { code: "ky-KG", label: MessageId::DictationLanguageKyKg },
    DictationLanguage { code: "lv-LV", label: MessageId::DictationLanguageLvLv },
    DictationLanguage { code: "ln-CD", label: MessageId::DictationLanguageLnCd },
    DictationLanguage { code: "lt-LT", label: MessageId::DictationLanguageLtLt },
    DictationLanguage { code: "mk-MK", label: MessageId::DictationLanguageMkMk },
    DictationLanguage { code: "ms-MY", label: MessageId::DictationLanguageMsMy },
    DictationLanguage { code: "ml-IN", label: MessageId::DictationLanguageMlIn },
    DictationLanguage { code: "mt-MT", label: MessageId::DictationLanguageMtMt },
    DictationLanguage { code: "cmn-Hans-CN", label: MessageId::DictationLanguageCmnHansCn },
    DictationLanguage { code: "mr-IN", label: MessageId::DictationLanguageMrIn },
    DictationLanguage { code: "mn-MN", label: MessageId::DictationLanguageMnMn },
    DictationLanguage { code: "ne-NP", label: MessageId::DictationLanguageNeNp },
    DictationLanguage { code: "nb-NO", label: MessageId::DictationLanguageNbNo },
    DictationLanguage { code: "or-IN", label: MessageId::DictationLanguageOrIn },
    DictationLanguage { code: "pl-PL", label: MessageId::DictationLanguagePlPl },
    DictationLanguage { code: "pt-BR", label: MessageId::DictationLanguagePtBr },
    DictationLanguage { code: "pt-PT", label: MessageId::DictationLanguagePtPt },
    DictationLanguage { code: "pa-IN", label: MessageId::DictationLanguagePaIn },
    DictationLanguage { code: "pa-Guru-IN", label: MessageId::DictationLanguagePaGuruIn },
    DictationLanguage { code: "ro-RO", label: MessageId::DictationLanguageRoRo },
    DictationLanguage { code: "ru-RU", label: MessageId::DictationLanguageRuRu },
    DictationLanguage { code: "sr-RS", label: MessageId::DictationLanguageSrRs },
    DictationLanguage { code: "sd-Arab-IN", label: MessageId::DictationLanguageSdArabIn },
    DictationLanguage { code: "sk-SK", label: MessageId::DictationLanguageSkSk },
    DictationLanguage { code: "sl-SI", label: MessageId::DictationLanguageSlSi },
    DictationLanguage { code: "es-419", label: MessageId::DictationLanguageEs419 },
    DictationLanguage { code: "es-US", label: MessageId::DictationLanguageEsUs },
    DictationLanguage { code: "sw-KE", label: MessageId::DictationLanguageSwKe },
    DictationLanguage { code: "sv-SE", label: MessageId::DictationLanguageSvSe },
    DictationLanguage { code: "tg-TJ", label: MessageId::DictationLanguageTgTj },
    DictationLanguage { code: "te-IN", label: MessageId::DictationLanguageTeIn },
    DictationLanguage { code: "th-TH", label: MessageId::DictationLanguageThTh },
    DictationLanguage { code: "tr-TR", label: MessageId::DictationLanguageTrTr },
    DictationLanguage { code: "uk-UA", label: MessageId::DictationLanguageUkUa },
    DictationLanguage { code: "uz-UZ", label: MessageId::DictationLanguageUzUz },
    DictationLanguage { code: "vi-VN", label: MessageId::DictationLanguageViVn },
];

/// Whether `code` is one of [`DICTATION_LANGUAGES`], spelled exactly as the table spells it.
pub fn is_supported(code: &str) -> bool {
    DICTATION_LANGUAGES.iter().any(|language| language.code == code)
}

/// A list the document may store: at most [`MAX_DICTATION_LANGUAGES`], each code
/// supported and spelled exactly as the table spells it (case-sensitive), no
/// repeats. Empty means Auto-detect.
pub fn is_valid_list(codes: &[String]) -> bool {
    codes.len() <= MAX_DICTATION_LANGUAGES
        && codes
            .iter()
            .enumerate()
            .all(|(index, code)| is_supported(code) && !codes[..index].contains(code))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{is_valid_list, DICTATION_LANGUAGES};

    fn list(codes: &[&str]) -> Vec<String> {
        codes.iter().map(|code| (*code).to_owned()).collect()
    }

    #[test]
    fn the_table_matches_the_documented_languages() {
        assert_eq!(DICTATION_LANGUAGES.len(), 83);
        let codes: HashSet<&str> = DICTATION_LANGUAGES.iter().map(|l| l.code).collect();
        let labels: HashSet<&str> = DICTATION_LANGUAGES.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(codes.len(), 83, "codes repeat");
        assert_eq!(labels.len(), 83, "labels repeat");
        for code in ["ru-RU", "en-US", "es-419", "cmn-Hans-CN", "ceb"] {
            assert!(codes.contains(code), "{code} is missing");
        }
        assert!(!codes.contains("es-ES"), "es-ES is not documented as supported");
        for language in DICTATION_LANGUAGES {
            let pascal: String = language
                .code
                .split('-')
                .map(|segment| {
                    let mut chars = segment.chars();
                    let first = chars.next().expect("no empty segment");
                    first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
                })
                .collect();
            assert_eq!(
                language.label.as_str(),
                format!("DictationLanguage{pascal}"),
                "{} is labelled by another language's key",
                language.code
            );
        }
    }

    #[test]
    fn a_list_holds_at_most_two_different_supported_codes() {
        assert!(is_valid_list(&[]));
        assert!(is_valid_list(&list(&["ru-RU"])));
        assert!(is_valid_list(&list(&["ru-RU", "en-US"])));
        assert!(!is_valid_list(&list(&["ru-ru"])));
        assert!(!is_valid_list(&list(&["xx-XX"])));
        assert!(!is_valid_list(&list(&["ru-RU", "ru-RU"])));
        assert!(!is_valid_list(&list(&["ru-RU", "en-US", "de-DE"])));
    }
}
