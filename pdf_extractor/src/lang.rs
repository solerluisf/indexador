/// Detect the language of a text string using `whatlang`.
/// Returns a 3-letter ISO 639-3 code (e.g. "eng", "jpn", "deu") or None
/// if the text is too short or detection is ambiguous.
pub fn detect_language(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.len() < 20 {
        return None;
    }
    let info = whatlang::detect(trimmed)?;
    if info.is_reliable() {
        Some(info.lang().code().to_string())
    } else {
        None
    }
}

/// Return the Tantivy tokenizer name appropriate for a given language code.
/// Falls back to "math" (the default) for unknown or Latin languages.
/// For languages that require specific tokenizers, return the registered name.
pub fn tokenizer_for_lang(lang: &str) -> &'static str {
    match lang {
        "jpn" | "ja" => "ja",       // Japanese (Lindera)
        "cmn" | "zh" => "zh",       // Chinese (Jieba)
        "kor" | "ko" => "ko",       // Korean
        "tha" | "th" => "th",       // Thai
        _ => "math",                // Default: the existing math tokenizer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_english() {
        let text = "The quick brown fox jumps over the lazy dog. This is a longer sentence that should be reliably detected.";
        let lang = detect_language(text);
        assert_eq!(lang.as_deref(), Some("eng"), "Should detect English, got {:?}", lang);
    }

    #[test]
    fn test_detect_short_text() {
        let lang = detect_language("Hello");
        assert_eq!(lang, None, "Short text should return None");
    }

    #[test]
    fn test_detect_empty_text() {
        let lang = detect_language("");
        assert_eq!(lang, None, "Empty text should return None");
    }

    #[test]
    fn test_detect_unicode_script() {
        let text = "これは日本語の文章です。十分な長さがあるので、信頼性高く検出できるはずです。";
        let lang = detect_language(text);
        assert_eq!(lang.as_deref(), Some("jpn"), "Should detect Japanese, got {:?}", lang);
    }

    #[test]
    fn test_tokenizer_for_japanese() {
        assert_eq!(tokenizer_for_lang("ja"), "ja");
        assert_eq!(tokenizer_for_lang("jpn"), "ja");
    }

    #[test]
    fn test_tokenizer_for_chinese() {
        assert_eq!(tokenizer_for_lang("zh"), "zh");
        assert_eq!(tokenizer_for_lang("cmn"), "zh");
    }

    #[test]
    fn test_tokenizer_for_unknown() {
        assert_eq!(tokenizer_for_lang("eng"), "math");
        assert_eq!(tokenizer_for_lang("deu"), "math");
        assert_eq!(tokenizer_for_lang(""), "math");
    }
}
