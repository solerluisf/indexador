use serde::{Deserialize, Serialize};

/// CRC32 of the serialized frame payload (everything except the CRC field itself).
/// Computed by `frame_crc32()` on the write side and validated on `read_frame()`.
/// A CRC of 0 means "not checked" (backward-compat mode).
///
/// Wire format: [4 bytes frame_len][frame bytes][4 bytes CRC]
pub fn frame_crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// One output line from the worker process (stdout).
/// The pipeline reads these, looks up the DB id by path, and builds a DocumentRecord.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct WorkerOutput {
    pub path: String,
    pub checksum: String,
    pub ocr_flag: bool,
    pub text: String,
    pub word_positions: Vec<crate::extractor::WordPosition>,
    /// Wall-clock time for this file's extraction in milliseconds.
    pub file_extraction_ms: u64,
    /// Number of pages in the PDF.
    pub page_count: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum WorkerFrame {
    Success(WorkerOutput),
    Error { path: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_wo() -> WorkerOutput {
        WorkerOutput {
            path: "/doc.pdf".into(),
            checksum: "abc123".into(),
            ocr_flag: false,
            text: "extracted content".into(),
            word_positions: vec![],
            file_extraction_ms: 0,
            page_count: 1,
        }
    }

    #[test]
    fn test_worker_output_roundtrip() {
        let wo = make_wo();
        let json = serde_json::to_string(&wo).unwrap();
        let wo2: WorkerOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(wo, wo2);
    }

    #[test]
    fn test_worker_output_missing_path_field() {
        let result: Result<WorkerOutput, _> = serde_json::from_str(r#"{"checksum":"x","ocr_flag":false,"text":"","word_positions":[]}"#);
        assert!(result.is_err(), "missing path should fail");
    }

    #[test]
    fn test_worker_output_ocr_flag_true() {
        let json = r#"{"path":"/scan.pdf","checksum":"x","ocr_flag":true,"text":"","word_positions":[],"file_extraction_ms":0,"page_count":1}"#;
        let wo: WorkerOutput = serde_json::from_str(json).unwrap();
        assert!(wo.ocr_flag);
        assert!(wo.text.is_empty());
    }

    #[test]
    fn test_worker_output_unicode_text() {
        let wo = WorkerOutput {
            text: "こんにちは世界 ∑∫".into(),
            ..make_wo()
        };
        let json = serde_json::to_string(&wo).unwrap();
        let wo2: WorkerOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(wo2.text, "こんにちは世界 ∑∫");
    }

    #[test]
    fn test_worker_output_special_chars_in_path() {
        let wo = WorkerOutput {
            path: r"C:\Users\test\my (1) [special] #$&+.pdf".into(),
            ..make_wo()
        };
        let json = serde_json::to_string(&wo).unwrap();
        let wo2: WorkerOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(wo2.path, r"C:\Users\test\my (1) [special] #$&+.pdf");
    }
}
