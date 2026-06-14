use anyhow::Result;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Wall-clock time for extraction in milliseconds.
    pub file_extraction_ms: u64,
    /// Number of pages in the PDF.
    pub page_count: u32,
}

/// Buffered JSONL writer with periodic fsync.
///
/// Writes are buffered in a `BufWriter` for performance; the caller
/// should call `flush()` periodically (e.g. every N records and at
/// shutdown) to guarantee durability.  On `Drop` the buffer is also
/// flushed and fsynced so a graceful shutdown always persists all data.
///
/// Crash safety: at most the last buffered-but-not-yet-fsynced batch
/// of records may be lost.  Because each record is written as a
/// complete JSON line, a partially-written trailing line is never
/// produced — the `BufWriter` either flushes a full buffer or nothing.
pub struct JsonlWriter {
    inner: Mutex<BufWriter<File>>,
    count: AtomicU64,
}

impl JsonlWriter {
    pub fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        Ok(Self {
            inner: Mutex::new(BufWriter::new(file)),
            count: AtomicU64::new(0),
        })
    }

    /// Append one record as a JSON line.
    /// The line is buffered in memory; call `flush()` to persist.
    /// Auto-flushes every 500 records to bound crash data loss.
    pub fn write_record(&self, record: &DocumentRecord) -> Result<()> {
        let json = serde_json::to_string(record)?;
        let mut inner = self.inner.lock().unwrap();
        writeln!(inner, "{}", json)?;
        let count = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        if count % 500 == 0 {
            inner.flush()?;
            inner.get_ref().sync_all()?;
        }
        Ok(())
    }

    /// Write pre-serialized JSON bytes (must include trailing newline).
    /// Lock is held only for the BufWriter write, making this suitable
    /// for use from a dedicated writer thread.
    pub fn write_json_bytes(&self, json_bytes: &[u8]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.write_all(json_bytes)?;
        let count = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        if count % 500 == 0 {
            inner.flush()?;
            inner.get_ref().sync_all()?;
        }
        Ok(())
    }

    /// Flush the buffer and fsync the underlying file.
    /// Call periodically (every N records, or at pipeline end)
    /// to bound the window of lost data on crash.
    pub fn flush(&self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.flush()?;
        inner.get_ref().sync_all()?;
        Ok(())
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl Drop for JsonlWriter {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            let _ = inner.flush();
            let _ = inner.get_ref().sync_all();
        }
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

    // --- JsonlWriter: basic ---

    #[test]
    fn test_jsonl_writer_writes_records() {
        let dir = temp_dir().join("pdf_extractor_test_jsonl");
        let _ = fs::remove_dir_all(&dir);
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
            file_extraction_ms: 0,
            page_count: 0,
        };
        writer.write_record(&record).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let mut content = String::new();
        fs::File::open(&path).unwrap().read_to_string(&mut content).unwrap();
        assert!(content.contains("\"id\":1"));
        assert!(content.ends_with('\n'));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_jsonl_writer_appends() {
        let dir = temp_dir().join("pdf_extractor_test_append");
        // Clean slate from any previous failed run
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");

        let writer = JsonlWriter::new(&path).unwrap();
        writer.write_record(&DocumentRecord {
            id: 1, path: "/a.pdf".into(), checksum: "a".into(),
            ocr_flag: false, text: "one".into(),
            word_positions: Vec::new(),
            file_extraction_ms: 0,
            page_count: 0,
        }).unwrap();
        writer.write_record(&DocumentRecord {
            id: 2, path: "/b.pdf".into(), checksum: "b".into(),
            ocr_flag: true, text: "two".into(),
            word_positions: Vec::new(),
            file_extraction_ms: 0,
            page_count: 0,
        }).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let mut content = String::new();
        fs::File::open(&path).unwrap().read_to_string(&mut content).unwrap();
        let line_count = content.lines().count();
        assert_eq!(line_count, 2, "Expected 2 lines, got {}. Content:\n{}", line_count, content);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_jsonl_writer_count() {
        let dir = temp_dir().join("pdf_extractor_test_count");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl");

        let writer = JsonlWriter::new(&path).unwrap();
        assert_eq!(writer.count(), 0);

        let rec = || DocumentRecord {
            id: 1, path: "/a.pdf".into(), checksum: "x".into(),
            ocr_flag: false, text: "t".into(),
            word_positions: Vec::new(),
            file_extraction_ms: 0,
            page_count: 0,
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
        let _ = fs::remove_dir_all(&dir);
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
            file_extraction_ms: 0,
            page_count: 0,
        };
        writer.write_record(&record).unwrap();
        writer.flush().unwrap();
        drop(writer);

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
            file_extraction_ms: 0,
            page_count: 0,
        }).unwrap();
        drop(w1);

        let w2 = JsonlWriter::new(&path).unwrap();
        w2.write_record(&DocumentRecord {
            id: 2, path: "/b.pdf".into(), checksum: "y".into(),
            ocr_flag: true, text: "second".into(),
            word_positions: Vec::new(),
            file_extraction_ms: 0,
            page_count: 0,
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
