use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DocumentRecord {
    pub id: i64,
    pub path: String,
    pub checksum: String,
    pub ocr_flag: bool,
    pub text: String,
    #[serde(skip)]
    pub word_positions: Vec<crate::extractor::WordPosition>,
    /// Wall-clock time for extraction in milliseconds.
    pub file_extraction_ms: u64,
    /// Number of pages in the PDF.
    pub page_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DocumentRecord: serialization ---

    #[test]
    fn test_document_record_serialization() {
        let record = DocumentRecord {
            id: 42,
            path: "/test.pdf".into(),
            checksum: "abcd1234".into(),
            ocr_flag: false,
            text: "hello world".into(),
            word_positions: Vec::new(),
            file_extraction_ms: 0,
            page_count: 0,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"id\":42"));
        assert!(json.contains("\"path\":\"/test.pdf\""));
        assert!(json.contains("\"ocr_flag\":false"));
        assert!(json.contains("\"text\":\"hello world\""));
    }

    #[test]
    fn test_document_record_special_chars_in_text() {
        let record = DocumentRecord {
            id: 3,
            path: "/f.pdf".into(),
            checksum: "c".into(),
            ocr_flag: false,
            text: "line1\nline2\ttabbed \"quoted\" \\backslash".into(),
            word_positions: Vec::new(),
            file_extraction_ms: 0,
            page_count: 0,
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["text"], "line1\nline2\ttabbed \"quoted\" \\backslash");
    }

    #[test]
    fn test_document_record_unicode_in_text() {
        let record = DocumentRecord {
            id: 4,
            path: "/u.pdf".into(),
            checksum: "d".into(),
            ocr_flag: false,
            text: "こんにちは世界 ∑∫".into(),
            word_positions: Vec::new(),
            file_extraction_ms: 0,
            page_count: 0,
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["text"], "こんにちは世界 ∑∫");
    }
}
