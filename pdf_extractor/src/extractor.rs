use anyhow::Result;
use lopdf::Document;
use std::path::Path;
use tracing::warn;

pub struct ExtractionResult {
    pub text: String,
    pub ocr_flag: bool,
}

pub fn extract_pdf(path: &Path) -> Result<ExtractionResult> {
    let doc = Document::load(path)?;
    let mut text = String::new();

    let pages = doc.get_pages();
    let mut page_numbers: Vec<u32> = pages.keys().copied().collect();
    page_numbers.sort();

    for page_num in &page_numbers {
        if let Ok(content) = doc.extract_text(&[*page_num]) {
            text.push_str(&content);
            text.push('\n');
        }
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        warn!(path = %path.display(), "No text extracted, marking for OCR");
        return Ok(ExtractionResult {
            text: String::new(),
            ocr_flag: true,
        });
    }

    Ok(ExtractionResult {
        text: trimmed.to_string(),
        ocr_flag: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::*;
    use std::env::temp_dir;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = temp_dir().join(format!("pdf_extractor_extract_{}", id));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_pdf(text: &str) -> PathBuf {
        let dir = unique_dir();
        let path = dir.join("test.pdf");

        let mut doc = Document::new();
        doc.version = "1.4".to_string();

        let catalog_id = doc.new_object_id();
        let font_id = doc.new_object_id();
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let content_id = doc.new_object_id();

        let escaped: String = text
            .chars()
            .map(|c| match c {
                '(' | ')' | '\\' => format!("\\{}", c),
                _ => c.to_string(),
            })
            .collect();
        let stream_data = format!("BT /F1 12 Tf 100 700 Td ({}) Tj ET", escaped);

        doc.objects.insert(font_id, Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Font".as_bytes().to_vec())),
            ("Subtype", Object::Name("Type1".as_bytes().to_vec())),
            ("BaseFont", Object::Name("Helvetica".as_bytes().to_vec())),
        ])));

        doc.objects.insert(content_id, Object::Stream(Stream::new(
            Dictionary::from_iter([("Length", Object::Integer(stream_data.len() as i64))]),
            stream_data.as_bytes().to_vec(),
        )));

        doc.objects.insert(page_id, Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Page".as_bytes().to_vec())),
            ("Parent", Object::Reference(pages_id)),
            ("MediaBox", Object::Array(vec![
                Object::Integer(0), Object::Integer(0),
                Object::Integer(612), Object::Integer(792),
            ])),
            ("Contents", Object::Reference(content_id)),
            ("Resources", Object::Dictionary(Dictionary::from_iter([(
                "Font", Object::Dictionary(Dictionary::from_iter([(
                    "F1", Object::Reference(font_id),
                )])),
            )]))),
        ])));

        doc.objects.insert(pages_id, Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Pages".as_bytes().to_vec())),
            ("Kids", Object::Array(vec![Object::Reference(page_id)])),
            ("Count", Object::Integer(1)),
        ])));

        doc.objects.insert(catalog_id, Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Catalog".as_bytes().to_vec())),
            ("Pages", Object::Reference(pages_id)),
        ])));

        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc.save(&path).unwrap();
        path
    }

    fn make_multipage_pdf(texts: &[&str]) -> PathBuf {
        let dir = unique_dir();
        let path = dir.join("multi.pdf");

        let mut doc = Document::new();
        doc.version = "1.4".to_string();

        let catalog_id = doc.new_object_id();
        let font_id = doc.new_object_id();
        let pages_id = doc.new_object_id();
        let mut page_ids = Vec::new();
        let mut content_ids = Vec::new();

        doc.objects.insert(font_id, Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Font".as_bytes().to_vec())),
            ("Subtype", Object::Name("Type1".as_bytes().to_vec())),
            ("BaseFont", Object::Name("Helvetica".as_bytes().to_vec())),
        ])));

        for t in texts {
            let pid = doc.new_object_id();
            let cid = doc.new_object_id();
            let escaped: String = t.chars()
                .map(|c| match c { '(' | ')' | '\\' => format!("\\{}", c), _ => c.to_string() })
                .collect();
            let sd = format!("BT /F1 12 Tf 100 700 Td ({}) Tj ET", escaped);

            doc.objects.insert(cid, Object::Stream(Stream::new(
                Dictionary::from_iter([("Length", Object::Integer(sd.len() as i64))]),
                sd.as_bytes().to_vec(),
            )));

            doc.objects.insert(pid, Object::Dictionary(Dictionary::from_iter([
                ("Type", Object::Name("Page".as_bytes().to_vec())),
                ("Parent", Object::Reference(pages_id)),
                ("MediaBox", Object::Array(vec![
                    Object::Integer(0), Object::Integer(0),
                    Object::Integer(612), Object::Integer(792),
                ])),
                ("Contents", Object::Reference(cid)),
                ("Resources", Object::Dictionary(Dictionary::from_iter([(
                    "Font", Object::Dictionary(Dictionary::from_iter([(
                        "F1", Object::Reference(font_id),
                    )])),
                )]))),
            ])));

            page_ids.push(pid);
            content_ids.push(cid);
        }

        let kids: Vec<Object> = page_ids.iter().map(|&id| Object::Reference(id)).collect();
        doc.objects.insert(pages_id, Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Pages".as_bytes().to_vec())),
            ("Kids", Object::Array(kids)),
            ("Count", Object::Integer(texts.len() as i64)),
        ])));

        doc.objects.insert(catalog_id, Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Catalog".as_bytes().to_vec())),
            ("Pages", Object::Reference(pages_id)),
        ])));

        doc.trailer.set("Root", Object::Reference(catalog_id));
        doc.save(&path).unwrap();
        path
    }

    // --- struct tests ---

    #[test]
    fn test_extraction_result_ocr_flag() {
        let result = ExtractionResult { text: String::new(), ocr_flag: true };
        assert!(result.ocr_flag);
        assert!(result.text.is_empty());
    }

    #[test]
    fn test_extraction_result_has_text() {
        let result = ExtractionResult { text: "hello".into(), ocr_flag: false };
        assert!(!result.ocr_flag);
        assert_eq!(result.text, "hello");
    }

    // --- extract_pdf: basic flow ---

    #[test]
    fn test_extract_pdf_with_text() {
        let path = make_pdf("Hello World");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Hello World"));
    }

    #[test]
    fn test_extract_pdf_unicode_text() {
        let path = make_pdf("Multi-word text with 123 numbers");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Multi-word"));
        assert!(result.text.contains("123"));
    }

    #[test]
    fn test_extract_pdf_multiple_pages() {
        let path = make_multipage_pdf(&["Page One", "Page Two", "Page Three"]);
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Page One"));
        assert!(result.text.contains("Page Two"));
        assert!(result.text.contains("Page Three"));
    }

    #[test]
    fn test_extract_pdf_special_characters() {
        let path = make_pdf("x=1+2*3/(4-5) [test] {braces}");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("x=1+2*3"));
    }

    // --- extract_pdf: empty / OCR flow ---

    #[test]
    fn test_extract_pdf_empty_content() {
        let path = make_pdf("");
        let result = extract_pdf(&path).unwrap();
        assert!(result.ocr_flag);
        assert_eq!(result.text, "");
    }

    #[test]
    fn test_extract_pdf_whitespace_only_content() {
        let path = make_pdf("   ");
        let result = extract_pdf(&path).unwrap();
        assert!(result.ocr_flag);
        assert_eq!(result.text, "");
    }

    // --- extract_pdf: error flows ---

    #[test]
    fn test_extract_pdf_nonexistent_file() {
        let path = PathBuf::from(r"C:\NONEXISTENT_FILE_98765.pdf");
        let result = extract_pdf(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_pdf_invalid_file() {
        let dir = unique_dir();
        let path = dir.join("not_a_pdf.txt");
        fs::write(&path, b"This is not a PDF").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_pdf_empty_file() {
        let dir = unique_dir();
        let path = dir.join("empty.pdf");
        fs::write(&path, b"").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_pdf_truncated_pdf() {
        let dir = unique_dir();
        let path = dir.join("truncated.pdf");
        fs::write(&path, b"%PDF-1.4\n1 0 obj << /Type /Catalog").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err());
    }
}
