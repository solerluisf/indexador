use anyhow::Result;
use lopdf::content::Operation;
use lopdf::{Document, Object};
use regex::Regex;
use std::path::Path;
use tracing::warn;

/// Lazily-initialized regex for cleaning word text to match Tantivy's
/// `[\p{L}\p{N}\p{S}]+` tokenizer pattern.
pub fn clean_word_text(s: &str) -> String {
    static INIT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = INIT.get_or_init(|| Regex::new(r"[\p{L}\p{N}\p{S}]+").unwrap());
    let cleaned: String = re.find_iter(s).map(|m| m.as_str()).collect();
    cleaned
}

/// A word's bounding box on a single PDF page.
/// Coordinates are in PDF user space (origin bottom-left, units in points, 1/72 inch).
#[derive(Debug, Clone, PartialEq)]
pub struct WordPosition {
    pub page: u32,        // 1-indexed page number
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
    pub text: String,
}

pub struct ExtractionResult {
    pub text: String,
    pub ocr_flag: bool,
    /// Per-word bounding boxes, aligned with tokenized text.
    /// Each entry's index corresponds to the word offset in Tantivy's term positions.
    pub word_positions: Vec<WordPosition>,
}

fn update_text_matrix(tm: &mut [f32; 6], td: &[f32; 2]) {
    let (a, b, c, d, e, f) = (tm[0], tm[1], tm[2], tm[3], tm[4], tm[5]);
    tm[0] = a; tm[1] = b;
    tm[2] = c; tm[3] = d;
    tm[4] = e + td[0];
    tm[5] = f + td[1];
}

struct AccWord {
    chars: String,
    x_min: f32,
    x_max: f32,
    y_min: f32,
    y_max: f32,
}

fn flush_text(
    out: &mut String,
    current: &mut String,
) {
    if current.is_empty() {
        return;
    }
    out.push_str(current);
    out.push(' ');
    current.clear();
}

/// Flush the accumulated `current_text` as a word with its bounding box
/// derived from the current text matrix `tm` and `font_size`.
fn flush_word(
    acc: &mut Vec<AccWord>,
    out: &mut String,
    current: &mut String,
    tm: &[f32; 6],
    font_size: f32,
) {
    if current.is_empty() {
        return;
    }

    let x_start = tm[4];
    let y_pos = tm[5];
    let total_chars = current.chars().count() as f32;
    let total_width = total_chars * font_size * 0.5;
    let avg_char_width = if total_chars > 0.0 { total_width / total_chars } else { font_size * 0.5 };
    let ascender = font_size * 0.25;
    let descender = font_size * 0.1;

    let tokens: Vec<&str> = current.split_whitespace().collect();
    if tokens.is_empty() {
        out.push_str(current);
        out.push(' ');
        current.clear();
        return;
    }

    let mut x_cursor = x_start;
    for token in &tokens {
        if token.is_empty() {
            continue;
        }
        let token_len = token.chars().count() as f32;
        let token_width = token_len * avg_char_width;
        acc.push(AccWord {
            chars: token.to_string(),
            x_min: x_cursor,
            x_max: x_cursor + token_width,
            y_min: y_pos - descender,
            y_max: y_pos + ascender,
        });
        x_cursor += token_width + avg_char_width;
    }

    out.push_str(current);
    out.push(' ');
    current.clear();
}

/// Parse a PDF content stream at the page level, extracting plain text.
/// Handles the most common text-showing operators (Tj, TJ) with basic
/// text-state tracking (Tm, Td, Tf, Tc, Tw, Tz, T*, BT, ET).
fn parse_page_text(page_ops: &[Operation]) -> String {
    let mut out = String::new();
    let mut current = String::new();

    let mut in_text_object = false;

    for op in page_ops {
        match op.operator.as_str() {
            "BT" => {
                in_text_object = true;
            }
            "ET" => {
                in_text_object = false;
                flush_text(&mut out, &mut current);
            }
            "Tf" => {}
            "Tm" => {
                flush_text(&mut out, &mut current);
            }
            "Td" => {
                flush_text(&mut out, &mut current);
            }
            "T*" => {
                flush_text(&mut out, &mut current);
            }
            "Tj" => {
                if in_text_object {
                    if let Some(Object::String(bytes, _)) = op.operands.get(0) {
                        if let Ok(s) = String::from_utf8(bytes.clone()) {
                            current.push_str(&s);
                        } else {
                            flush_text(&mut out, &mut current);
                        }
                    } else {
                        flush_text(&mut out, &mut current);
                    }
                }
            }
            "TJ" => {
                if in_text_object {
                    flush_text(&mut out, &mut current);
                    if let Some(Object::Array(arr)) = op.operands.get(0) {
                        for item in arr {
                            match item {
                                Object::String(bytes, _) => {
                                    if let Ok(s) = String::from_utf8(bytes.clone()) {
                                        current.push_str(&s);
                                    }
                                }
                                _ => {}
                            }
                        }
                        flush_text(&mut out, &mut current);
                    }
                }
            }
            "'" => {
                flush_text(&mut out, &mut current);
                if in_text_object {
                    if let Some(Object::String(bytes, _)) = op.operands.get(0) {
                        if let Ok(s) = String::from_utf8(bytes.clone()) {
                            current.push_str(&s);
                        }
                    }
                }
            }
            "\"" => {
                flush_text(&mut out, &mut current);
                if in_text_object {
                    if let Some(Object::String(bytes, _)) = op.operands.get(2) {
                        if let Ok(s) = String::from_utf8(bytes.clone()) {
                            current.push_str(&s);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    flush_text(&mut out, &mut current);
    out.trim().to_string()
}

/// Parse a PDF content stream at the page level, extracting both plain text
/// and per-word bounding boxes. Handles the most common text-showing operators
/// (Tj, TJ) with basic text-state tracking (Tm, Td, Tf, Tc, Tw, Tz, T*, BT, ET).
///
/// Font metrics are approximated: each glyph is assumed to be `font_size × 0.5`
/// points wide.
fn parse_page_content(
    page_ops: &[Operation],
    page_num: u32,
) -> (String, Vec<WordPosition>) {
    let mut out = String::new();
    let mut acc: Vec<AccWord> = Vec::new();
    let mut current = String::new();

    // Text state tracking
    let mut tm: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut font_size: f32 = 12.0;
    let leading: f32 = 0.0;
    let mut in_text_object = false;

    for op in page_ops {
        match op.operator.as_str() {
            "BT" => {
                in_text_object = true;
                tm = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
            }
            "ET" => {
                in_text_object = false;
                flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
            }
            "Tf" => {
                if let Some(Object::Integer(size)) = op.operands.get(1) {
                    font_size = *size as f32 * 0.01;
                } else if let Some(Object::Real(size)) = op.operands.get(1) {
                    font_size = *size * 0.01;
                }
            }
            "Tm" => {
                flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                if op.operands.len() >= 6 {
                    let get_f32 = |i: usize| -> f32 {
                        match &op.operands[i] {
                            Object::Integer(v) => *v as f32,
                            Object::Real(v) => *v,
                            _ => 0.0,
                        }
                    };
                    tm = [
                        get_f32(0), get_f32(1), get_f32(2),
                        get_f32(3), get_f32(4), get_f32(5),
                    ];
                }
            }
            "Td" => {
                flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                let get_f32 = |i: usize| -> f32 {
                    match op.operands.get(i) {
                        Some(Object::Integer(v)) => *v as f32,
                        Some(Object::Real(v)) => *v,
                        _ => 0.0,
                    }
                };
                let tx_f = get_f32(0);
                let ty_f = get_f32(1);
                update_text_matrix(&mut tm, &[tx_f, ty_f]);
            }
            "T*" => {
                flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                update_text_matrix(&mut tm, &[0.0, -leading]);
            }
            "Tj" => {
                if in_text_object {
                    if let Some(Object::String(bytes, _)) = op.operands.get(0) {
                        if let Ok(s) = String::from_utf8(bytes.clone()) {
                            current.push_str(&s);
                        } else {
                            flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                        }
                    } else {
                        flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                    }
                }
            }
            "TJ" => {
                if in_text_object {
                    flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                    if let Some(Object::Array(arr)) = op.operands.get(0) {
                        for item in arr {
                            match item {
                                Object::String(bytes, _) => {
                                    if let Ok(s) = String::from_utf8(bytes.clone()) {
                                        current.push_str(&s);
                                    }
                                }
                                Object::Integer(offset) => {
                                    let kern = *offset as f32 / 1000.0 * font_size;
                                    tm[4] -= kern;
                                }
                                Object::Real(offset) => {
                                    let kern = *offset / 1000.0 * font_size;
                                    tm[4] -= kern;
                                }
                                _ => {}
                            }
                        }
                        flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                    }
                }
            }
            "'" => {
                flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                update_text_matrix(&mut tm, &[0.0, -leading]);
                if in_text_object {
                    if let Some(Object::String(bytes, _)) = op.operands.get(0) {
                        if let Ok(s) = String::from_utf8(bytes.clone()) {
                            current.push_str(&s);
                        }
                    }
                }
            }
            "\"" => {
                flush_word(&mut acc, &mut out, &mut current, &tm, font_size);
                update_text_matrix(&mut tm, &[0.0, -leading]);
                if in_text_object {
                    if let Some(Object::String(bytes, _)) = op.operands.get(2) {
                        if let Ok(s) = String::from_utf8(bytes.clone()) {
                            current.push_str(&s);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    flush_word(&mut acc, &mut out, &mut current, &tm, font_size);

    let raw_text = out.trim().to_string();

    // Build word_positions from accumulator, filtering out pure-punctuation tokens
    let word_positions: Vec<WordPosition> = acc.iter()
        .filter(|w| {
            let word = w.chars.trim();
            !word.is_empty() && !word.chars().all(|c| c.is_ascii_punctuation())
        })
        .map(|w| WordPosition {
            page: page_num,
            x_min: w.x_min,
            y_min: w.y_min,
            x_max: w.x_max,
            y_max: w.y_max,
            text: clean_word_text(&w.chars),
        })
        .collect();

    (raw_text, word_positions)
}

pub fn extract_pdf(path: &Path) -> Result<ExtractionResult> {
    // Strip \\?\ prefix — lopdf doesn't handle Windows long-path prefix
    let path_str = path.to_string_lossy();
    let clean_path = path_str.strip_prefix(r"\\?\").map(Path::new).unwrap_or(path);
    let doc = Document::load(clean_path)?;
    let mut text = String::new();
    let mut word_positions: Vec<WordPosition> = Vec::new();

    let pages = doc.get_pages();
    let mut page_numbers: Vec<u32> = pages.keys().copied().collect();
    page_numbers.sort();

    for page_num in page_numbers.iter() {
        let page_id = pages[page_num];
        match doc.get_and_decode_page_content(page_id) {
            Ok(content) => {
                let (page_text, page_words) = parse_page_content(&content.operations, *page_num);
                if !page_text.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&page_text);
                    word_positions.extend(page_words);
                }
            }
            Err(_) => {
                if let Ok(page_text) = doc.extract_text(&[*page_num]) {
                    let trimmed = page_text.trim();
                    if !trimmed.is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(trimmed);
                    }
                }
            }
        }
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        warn!(path = %path.display(), "No text extracted, marking for OCR");
        return Ok(ExtractionResult {
            text: String::new(),
            ocr_flag: true,
            word_positions: Vec::new(),
        });
    }

    Ok(ExtractionResult {
        text: trimmed.to_string(),
        ocr_flag: false,
        word_positions,
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

    fn make_empty_multipage_pdf(num_pages: usize) -> PathBuf {
        let dir = unique_dir();
        let path = dir.join("empty_multi.pdf");

        let mut doc = Document::new();
        doc.version = "1.4".to_string();

        let catalog_id = doc.new_object_id();
        let pages_id = doc.new_object_id();
        let mut page_ids = Vec::new();

        for _ in 0..num_pages {
            let pid = doc.new_object_id();
            doc.objects.insert(pid, Object::Dictionary(Dictionary::from_iter([
                ("Type", Object::Name("Page".as_bytes().to_vec())),
                ("Parent", Object::Reference(pages_id)),
                ("MediaBox", Object::Array(vec![
                    Object::Integer(0), Object::Integer(0),
                    Object::Integer(612), Object::Integer(792),
                ])),
            ])));
            page_ids.push(pid);
        }

        let kids: Vec<Object> = page_ids.iter().map(|&id| Object::Reference(id)).collect();
        doc.objects.insert(pages_id, Object::Dictionary(Dictionary::from_iter([
            ("Type", Object::Name("Pages".as_bytes().to_vec())),
            ("Kids", Object::Array(kids)),
            ("Count", Object::Integer(num_pages as i64)),
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
        let result = ExtractionResult { text: String::new(), ocr_flag: true, word_positions: Vec::new() };
        assert!(result.ocr_flag);
        assert!(result.text.is_empty());
    }

    #[test]
    fn test_extraction_result_has_text() {
        let result = ExtractionResult { text: "hello".into(), ocr_flag: false, word_positions: Vec::new() };
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

    #[test]
    fn test_extract_pdf_multi_page_all_empty() {
        let path = make_empty_multipage_pdf(3);
        let result = extract_pdf(&path).unwrap();
        assert!(result.ocr_flag, "Image-only multi-page PDF should set ocr_flag");
        assert_eq!(result.text, "", "Text should be empty when all pages have no content");
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

    // --- WordPosition tests ---

    #[test]
    fn test_word_positions_basic() {
        let path = make_pdf("Hello World");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.word_positions.is_empty(), "Should have word positions");
        for pos in &result.word_positions {
            assert_eq!(pos.page, 1, "All positions should be on page 1");
            assert!(pos.x_min >= 0.0, "x_min must be non-negative");
            assert!(pos.y_min >= 0.0, "y_min must be non-negative");
            assert!(pos.x_max > pos.x_min, "x_max must be greater than x_min");
            assert!(pos.y_max > pos.y_min, "y_max must be greater than y_min");
        }
    }

    #[test]
    fn test_word_positions_empty_pdf() {
        let path = make_pdf("");
        let result = extract_pdf(&path).unwrap();
        assert!(result.word_positions.is_empty(), "Empty PDF should have no positions");
        assert!(result.ocr_flag);
    }

    #[test]
    fn test_word_positions_multipage() {
        let path = make_multipage_pdf(&["Page One", "Page Two"]);
        let result = extract_pdf(&path).unwrap();
        assert!(!result.word_positions.is_empty());
        let pages: std::collections::HashSet<u32> = result.word_positions.iter().map(|p| p.page).collect();
        assert!(pages.contains(&1), "Should have positions on page 1");
        assert!(pages.contains(&2), "Should have positions on page 2");
    }

    #[test]
    fn test_word_position_bounds_sensible() {
        let path = make_pdf("Hello World");
        let result = extract_pdf(&path).unwrap();
        for pos in &result.word_positions {
            assert!(pos.x_min >= 0.0 && pos.x_max <= 700.0,
                "x bounds should be within page width: x_min={}, x_max={}", pos.x_min, pos.x_max);
            assert!(pos.y_min >= 0.0 && pos.y_max <= 800.0,
                "y bounds should be within page height: y_min={}, y_max={}", pos.y_min, pos.y_max);
            let width = pos.x_max - pos.x_min;
            let height = pos.y_max - pos.y_min;
            assert!(width > 0.0 && width < 200.0,
                "Word width should be reasonable: {}", width);
            assert!(height > 0.0 && height < 50.0,
                "Word height should be reasonable: {}", height);
        }
    }

    #[test]
    fn test_word_positions_with_td() {
        let dir = unique_dir();
        let path = dir.join("td_test.pdf");

        let mut doc = Document::new();
        doc.version = "1.4".to_string();

        let catalog_id = doc.new_object_id();
        let font_id = doc.new_object_id();
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let content_id = doc.new_object_id();

        let stream_data = "BT /F1 12 Tf 100 700 Td (Hello) Tj 50 0 Td (World) Tj ET";

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

        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(!result.word_positions.is_empty(), "Should have positions after Td");
        if result.word_positions.len() >= 2 {
            assert!(
                (result.word_positions[1].x_min - result.word_positions[0].x_min).abs() > 1.0,
                "Second word should be offset from first by Td"
            );
        }
    }
}
