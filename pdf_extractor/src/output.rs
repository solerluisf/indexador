use anyhow::Result;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Serialize)]
pub struct DocumentRecord {
    pub id: i64,
    pub path: String,
    pub checksum: String,
    pub ocr_flag: bool,
    pub text: String,
    #[serde(skip)]
    pub word_positions: Vec<crate::extractor::WordPosition>,
}

pub struct JsonlWriter {
    file: Mutex<Box<dyn Write + Send>>,
    count: Mutex<u64>,
}

impl JsonlWriter {
    pub fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|f| Box::new(f) as Box<dyn Write + Send>)?;

        Ok(Self {
            file: Mutex::new(file),
            count: Mutex::new(0),
        })
    }

    pub fn write_record(&self, record: &DocumentRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let mut file = self.file.lock().unwrap();
        writeln!(file, "{}", json)?;
        let mut count = self.count.lock().unwrap();
        *count += 1;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn count(&self) -> u64 {
        *self.count.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::fs;
    use std::io::Read;
    use std::path::PathBuf;

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
        };
        let json = serde_json::to_string(&record).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["text"], "こんにちは世界 ∑∫");
    }

    // --- JsonlWriter: basic ---

    #[test]
    fn test_jsonl_writer_writes_records() {
        let dir = temp_dir().join("pdf_extractor_test_jsonl");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");

        let writer = JsonlWriter::new(&path).unwrap();
        let record = DocumentRecord {
            id: 1,
            path: "/a.pdf".into(),
            checksum: "aaa".into(),
            ocr_flag: false,
            text: "hello".into(),
            word_positions: Vec::new(),
        };
        writer.write_record(&record).unwrap();

        let mut content = String::new();
        fs::File::open(&path).unwrap().read_to_string(&mut content).unwrap();
        assert!(content.contains("\"id\":1"));
        assert!(content.ends_with('\n'));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_jsonl_writer_appends() {
        let dir = temp_dir().join("pdf_extractor_test_append");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");

        let writer = JsonlWriter::new(&path).unwrap();
        writer.write_record(&DocumentRecord {
            id: 1, path: "/a.pdf".into(), checksum: "a".into(),
            ocr_flag: false, text: "one".into(),
            word_positions: Vec::new(),
        }).unwrap();
        writer.write_record(&DocumentRecord {
            id: 2, path: "/b.pdf".into(), checksum: "b".into(),
            ocr_flag: true, text: "two".into(),
            word_positions: Vec::new(),
        }).unwrap();

        let mut content = String::new();
        fs::File::open(&path).unwrap().read_to_string(&mut content).unwrap();
        assert_eq!(content.lines().count(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_jsonl_writer_count() {
        let dir = temp_dir().join("pdf_extractor_test_count");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");

        let writer = JsonlWriter::new(&path).unwrap();
        assert_eq!(writer.count(), 0);

        let rec = || DocumentRecord {
            id: 1, path: "/a.pdf".into(), checksum: "x".into(),
            ocr_flag: false, text: "t".into(),
            word_positions: Vec::new(),
        };
        writer.write_record(&rec()).unwrap();
        assert_eq!(writer.count(), 1);
        writer.write_record(&rec()).unwrap();
        assert_eq!(writer.count(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    // --- JsonlWriter: error flows ---

    #[test]
    fn test_jsonl_writer_invalid_path() {
        let bad = PathBuf::from(r"C:\NONEXISTENT_DIR\out.jsonl");
        let result = JsonlWriter::new(&bad);
        assert!(result.is_err());
    }

    #[test]
    fn test_jsonl_writer_write_large_text() {
        let dir = temp_dir().join("pdf_extractor_test_large");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");

        let writer = JsonlWriter::new(&path).unwrap();
        let record = DocumentRecord {
            id: 1,
            path: "/big.pdf".into(),
            checksum: "big".into(),
            ocr_flag: false,
            text: "x".repeat(100_000),
            word_positions: Vec::new(),
        };
        writer.write_record(&record).unwrap();

        let mut content = String::new();
        fs::File::open(&path).unwrap().read_to_string(&mut content).unwrap();
        assert!(content.contains("\"id\":1"));
        assert_eq!(content.lines().count(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_jsonl_writer_resumes_appending_to_existing() {
        let dir = temp_dir().join("pdf_extractor_test_resume");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");

        let w1 = JsonlWriter::new(&path).unwrap();
        w1.write_record(&DocumentRecord {
            id: 1, path: "/a.pdf".into(), checksum: "x".into(),
            ocr_flag: false, text: "first".into(),
            word_positions: Vec::new(),
        }).unwrap();
        drop(w1);

        let w2 = JsonlWriter::new(&path).unwrap();
        w2.write_record(&DocumentRecord {
            id: 2, path: "/b.pdf".into(), checksum: "y".into(),
            ocr_flag: true, text: "second".into(),
            word_positions: Vec::new(),
        }).unwrap();
        drop(w2);

        let mut content = String::new();
        fs::File::open(&path).unwrap().read_to_string(&mut content).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("first"));
        assert!(content.contains("second"));

        fs::remove_dir_all(&dir).unwrap();
    }
}
