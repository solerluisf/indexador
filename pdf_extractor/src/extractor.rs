use anyhow::{Context, Result};
use regex::Regex;
use std::path::Path;

use crate::indexer::TOKEN_PATTERN;
use crate::pdfium;
use crate::pdfium::FS_RECTF;

/// Lazily-initialized regex for cleaning word text to match Tantivy's
/// `[\p{L}\p{N}\p{S}]+` tokenizer pattern.
pub fn clean_word_text(s: &str) -> String {
    static INIT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = INIT.get_or_init(|| Regex::new(TOKEN_PATTERN).unwrap());
    let cleaned: String = re.find_iter(s).map(|m| m.as_str()).collect::<Vec<_>>().join(" ");
    cleaned
}

/// Check if the entire string matches TOKEN_PATTERN as a single token.
/// If so, no per-segment splitting is needed and we can use the
/// incrementally-computed bounding box directly.
fn is_single_token(s: &str) -> bool {
    static INIT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = INIT.get_or_init(|| Regex::new(&format!("^{}$", crate::indexer::TOKEN_PATTERN)).unwrap());
    re.is_match(s)
}

/// A word's bounding box on a single PDF page.
/// Coordinates are in PDF user space (origin bottom-left, units in points, 1/72 inch).
/// `text` is always a single token (no spaces) after extraction.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WordPosition {
    pub page: u32,
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
    pub text: String,
}

/// Per-character bounding box in PDF user space.
#[derive(Debug, Clone, Copy)]
struct CharPosition {
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
}

/// Split a word into individual token segments based on TOKEN_PATTERN,
/// computing each segment's exact bounding box from per-character positions.
/// Returns one WordPosition per segment (no spaces in any text field).
fn split_word_into_segments(
    raw_word: &str,
    char_positions: &[CharPosition],
    page: u32,
) -> Vec<WordPosition> {
    static INIT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = INIT.get_or_init(|| Regex::new(TOKEN_PATTERN).unwrap());

    let matches: Vec<_> = re.find_iter(raw_word).collect();
    if matches.is_empty() {
        return Vec::new();
    }

    matches
        .into_iter()
        .map(|m| {
            let char_start = raw_word[..m.start()].chars().count();
            let char_end = char_start + m.as_str().chars().count();
            let positions = &char_positions[char_start..char_end];
            WordPosition {
                page,
                x_min: positions.iter().map(|c| c.left).reduce(f32::min).unwrap_or(0.0),
                y_min: positions.iter().map(|c| c.bottom).reduce(f32::min).unwrap_or(0.0),
                x_max: positions.iter().map(|c| c.right).reduce(f32::max).unwrap_or(0.0),
                y_max: positions.iter().map(|c| c.top).reduce(f32::max).unwrap_or(0.0),
                text: m.as_str().to_string(),
            }
        })
        .collect()
}

#[derive(Debug)]
pub struct ExtractionResult {
    pub text: String,
    pub ocr_flag: bool,
    /// Per-word bounding boxes, aligned with tokenized text.
    pub word_positions: Vec<WordPosition>,
    /// Total number of pages processed in the PDF.
    pub page_count: u32,
    /// Wall-clock time for the entire extraction in milliseconds.
    pub extraction_ms: u64,
}

/// Extract text and word bounding boxes from a loaded PDFium document handle.
/// Returns (text, word_positions, total_chars_found).
///
/// # Safety
/// `doc` must be a valid non-null PDFium document handle from FPDF_LoadDocument
/// or FPDF_LoadMemDocument.  Pages and text pages are closed inside this function;
/// the document handle is NOT closed (caller manages that).
unsafe fn extract_text_and_positions(
    pdfium: &pdfium::Pdfium,
    doc: *mut std::ffi::c_void,
) -> (String, Vec<WordPosition>, usize, u32) {
    let page_count = (pdfium.FPDF_GetPageCount)(doc);
    let mut text = String::new();
    let mut word_positions: Vec<WordPosition> = Vec::new();
    let mut total_chars = 0usize;

    for page_idx in 0..page_count {
        let page = (pdfium.FPDF_LoadPage)(doc, page_idx);
        if page.is_null() {
            continue;
        }

        let text_page = (pdfium.FPDFText_LoadPage)(page);
        if text_page.is_null() {
            (pdfium.FPDF_ClosePage)(page);
            continue;
        }

        // Wrap in catch_unwind so FFI handles are always closed on panic.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // ── Page geometry for coordinate transforms ──
            let geom = crate::pdfium::PageGeometry::from_page(pdfium, page);

            let start_pos = word_positions.len();
            let char_count = (pdfium.FPDFText_CountChars)(text_page);
            let mut current_word = String::new();
            let mut current_char_positions: Vec<CharPosition> = Vec::new();
            let mut has_word = false;
            let mut last_char_was_space = true;
            // Previous character bbox for gap detection
            let mut prev_left = 0.0f64;
            let mut prev_right = 0.0f64;
            let mut prev_bottom = 0.0f64;
            let mut prev_top = 0.0f64;
            let mut has_prev = false;
            // Fast-path min/max for single-token words (avoids split_word_into_segments)
            let mut current_min_x: f32 = f32::MAX;
            let mut current_min_y: f32 = f32::MAX;
            let mut current_max_x: f32 = f32::MIN;
            let mut current_max_y: f32 = f32::MIN;
            // Running average character metrics for relative gap thresholds
            let mut total_height = 0.0f64;
            let mut total_width = 0.0f64;
            let mut char_count_in_page = 0usize;

            for i in 0..char_count {
                let ch = (pdfium.FPDFText_GetUnicode)(text_page, i);

                let mut left = 0.0f64;
                let mut right = 0.0f64;
                let mut bottom = 0.0f64;
                let mut top = 0.0f64;
                let bbox_ok = (pdfium.FPDFText_GetCharBox)(
                    text_page, i, &mut left, &mut right, &mut bottom, &mut top,
                ) != 0;

                // Unavailable character — treat as word separator
                if ch == 0 || !bbox_ok {
                    if has_word {
                        let page_num = (page_idx + 1) as u32;
                        if is_single_token(&current_word) {
                            word_positions.push(WordPosition {
                                page: page_num,
                                x_min: current_min_x,
                                y_min: current_min_y,
                                x_max: current_max_x,
                                y_max: current_max_y,
                                text: current_word.clone(),
                            });
                        } else {
                            word_positions
                                .extend(split_word_into_segments(&current_word, &current_char_positions, page_num));
                        }
                        current_word.clear();
                        current_char_positions.clear();
                        has_word = false;
                        has_prev = false;
                        current_min_x = f32::MAX;
                        current_min_y = f32::MAX;
                        current_max_x = f32::MIN;
                        current_max_y = f32::MIN;
                    }
                    if !last_char_was_space {
                        text.push(' ');
                    }
                    last_char_was_space = true;
                    continue;
                }

                let c = match char::from_u32(ch) {
                    Some(c) => c,
                    None => continue,
                };

                let is_newline = c == '\n' || c == '\r';

                if c.is_whitespace() || is_newline {
                    if has_word {
                        let page_num = (page_idx + 1) as u32;
                        if is_single_token(&current_word) {
                            word_positions.push(WordPosition {
                                page: page_num,
                                x_min: current_min_x,
                                y_min: current_min_y,
                                x_max: current_max_x,
                                y_max: current_max_y,
                                text: current_word.clone(),
                            });
                        } else {
                            word_positions
                                .extend(split_word_into_segments(&current_word, &current_char_positions, page_num));
                        }
                        current_word.clear();
                        current_char_positions.clear();
                        has_word = false;
                        has_prev = false;
                        current_min_x = f32::MAX;
                        current_min_y = f32::MAX;
                        current_max_x = f32::MIN;
                        current_max_y = f32::MIN;
                    }
                    if is_newline {
                        text.push('\n');
                    } else if !last_char_was_space {
                        text.push(' ');
                    }
                    last_char_was_space = true;
                } else {
                    // Gap detection: flush if vertical gap > 0.7×avg_h or horizontal gap magnitude > 1.5×avg_w
                    if has_word && has_prev && char_count_in_page > 0 {
                        let avg_h = total_height / char_count_in_page as f64;
                        let avg_w = total_width / char_count_in_page as f64;
                        let vert_gap = (bottom - prev_bottom).abs();
                        let horiz_gap = left - prev_right;
                        if vert_gap > avg_h * 0.7 || horiz_gap.abs() > avg_w * 1.5 {
                            let page_num = (page_idx + 1) as u32;
                            if is_single_token(&current_word) {
                                word_positions.push(WordPosition {
                                    page: page_num,
                                    x_min: current_min_x,
                                    y_min: current_min_y,
                                    x_max: current_max_x,
                                    y_max: current_max_y,
                                    text: current_word.clone(),
                                });
                            } else {
                                word_positions.extend(
                                    split_word_into_segments(&current_word, &current_char_positions, page_num),
                                );
                            }
                            current_word.clear();
                            current_char_positions.clear();
                            has_word = false;
                            current_min_x = f32::MAX;
                            current_min_y = f32::MAX;
                            current_max_x = f32::MIN;
                            current_max_y = f32::MIN;
                            text.push(' ');
                        }
                    }
                    current_word.push(c);
                    current_char_positions.push(CharPosition {
                        left: left as f32,
                        bottom: bottom as f32,
                        right: right as f32,
                        top: top as f32,
                    });
                    current_min_x = current_min_x.min(left as f32);
                    current_min_y = current_min_y.min(bottom as f32);
                    current_max_x = current_max_x.max(right as f32);
                    current_max_y = current_max_y.max(top as f32);
                    if !has_word {
                        has_word = true;
                    }
                    text.push(c);
                    last_char_was_space = false;
                    total_height += (top - bottom).abs();
                    total_width += (right - left).abs();
                    char_count_in_page += 1;
                    prev_left = left;
                    prev_right = right;
                    prev_bottom = bottom;
                    prev_top = top;
                    has_prev = true;
                }
            }

            if has_word {
                let page_num = (page_idx + 1) as u32;
                if is_single_token(&current_word) {
                    word_positions.push(WordPosition {
                        page: page_num,
                        x_min: current_min_x,
                        y_min: current_min_y,
                        x_max: current_max_x,
                        y_max: current_max_y,
                        text: current_word.clone(),
                    });
                } else {
                    word_positions
                        .extend(split_word_into_segments(&current_word, &current_char_positions, page_num));
                }
                text.push(' ');
            }

            // Apply Y-flip to all word positions on this page (PDF bottom-left → bitmap top-left)
            for pos in word_positions[start_pos..].iter_mut() {
                let (new_x, new_y) = geom.pdf_to_stored(pos.x_min as f64, pos.y_max as f64);
                let (_new_x2, new_y2) = geom.pdf_to_stored(pos.x_max as f64, pos.y_min as f64);
                pos.x_min = new_x as f32;
                pos.y_min = new_y as f32;
                pos.x_max = _new_x2 as f32;
                pos.y_max = new_y2 as f32;
            }

            total_chars += char_count as usize;
        }));

        // Always close FFI handles, even if the closure panicked.
        (pdfium.FPDFText_ClosePage)(text_page);
        (pdfium.FPDF_ClosePage)(page);

        // Resume panic after cleanup so caller can handle it.
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    (text, word_positions, total_chars, page_count as u32)
}

/// Extract text and word bounding boxes from a PDF using PDFium.
/// Falls back to ocr_flag=true if the PDF has no extractable text.
///
/// Loading strategy:
///   1. Try `FPDF_LoadDocument` (file path) — always calls `FPDF_CloseDocument`.
///   2. If that fails, fall back to `FPDF_LoadMemDocument` (memory buffer).
///      File-path loading may fail for some PDFs with unusual path encodings.
///      In both paths, `FPDF_CloseDocument` is always called and the buffer
///      (if any) is freed after CloseDocument.  No `ManuallyDrop` leak.
pub fn extract_pdf(path: &Path) -> Result<ExtractionResult> {
    let extract_start = std::time::Instant::now();
    let pdfium = pdfium::Pdfium::global().ok_or_else(|| {
        anyhow::anyhow!("pdfium.dll not available — install pdfium.dll in the application directory")
    })?;

    // Strip \\?\ prefix — PDFium's CreateFileW handles long paths natively.
    let clean = path.strip_prefix(r"\\?\").unwrap_or(path);
    let path_utf16 = pdfium::path_to_utf16(clean);

    // Try file-path loading first (supports clean CloseDocument).
    let doc = unsafe { (pdfium.FPDF_LoadDocument)(path_utf16.as_ptr(), std::ptr::null()) };

    if !doc.is_null() {
        let result = unsafe { extract_text_and_positions(pdfium, doc) };
        let extraction_ms = extract_start.elapsed().as_millis() as u64;
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        return Ok(build_result(result.0, result.1, result.2, result.3, extraction_ms));
    }

    // File-path loading failed — fall back to memory loading.
    // The buffer is kept alive until after CloseDocument to prevent
    // allocator-mismatch access violations.
    let pdf_data = std::fs::read(path).context("Failed to read PDF file")?;
    let doc = unsafe {
        (pdfium.FPDF_LoadMemDocument)(pdf_data.as_ptr(), pdf_data.len() as i32, std::ptr::null())
    };
    if doc.is_null() {
        let err = unsafe { (pdfium.FPDF_GetLastError)() };
        if err == pdfium::FPDF_ERR_FILE || err == pdfium::FPDF_ERR_FORMAT {
            anyhow::bail!("Failed to load PDF: {}", pdfium::error_str(err));
        }
        return Ok(ExtractionResult {
            text: String::new(),
            ocr_flag: true,
            word_positions: Vec::new(),
            page_count: 0,
            extraction_ms: extract_start.elapsed().as_millis() as u64,
        });
    }

    let result = unsafe { extract_text_and_positions(pdfium, doc) };
    let extraction_ms = extract_start.elapsed().as_millis() as u64;
    // CloseDocument while the buffer is still alive
    unsafe { (pdfium.FPDF_CloseDocument)(doc); }
    // pdf_data is dropped here, after CloseDocument
    Ok(build_result(result.0, result.1, result.2, result.3, extraction_ms))
}

/// Build an ExtractionResult from raw extraction output.
/// Shared by both `extract_pdf` and `extract_pdf_bytes`.
fn build_result(
    mut text: String,
    word_positions: Vec<WordPosition>,
    total_chars: usize,
    page_count: u32,
    extraction_ms: u64,
) -> ExtractionResult {
    if total_chars == 0 || text.trim().is_empty() {
        ExtractionResult {
            text: String::new(),
            ocr_flag: true,
            word_positions: Vec::new(),
            page_count,
            extraction_ms,
        }
    } else {
        let trimmed_len = text.trim_end().len();
        text.truncate(trimmed_len);
        ExtractionResult {
            text,
            ocr_flag: false,
            word_positions,
            page_count,
            extraction_ms,
        }
    }
}

/// Extract text and word positions from PDF data loaded from a byte buffer.
/// Uses `FPDF_LoadMemDocument` and always calls `FPDF_CloseDocument`.
/// The caller must keep `data` alive for the duration of this call.
///
/// This is useful for callers (e.g. CAPI) that already have the PDF bytes
/// in memory and want to avoid reading the file a second time.
pub fn extract_pdf_bytes(data: &[u8]) -> Result<ExtractionResult> {
    let extract_start = std::time::Instant::now();
    let pdfium = pdfium::Pdfium::global().ok_or_else(|| {
        anyhow::anyhow!("pdfium.dll not available — install pdfium.dll in the application directory")
    })?;

    let doc = unsafe {
        (pdfium.FPDF_LoadMemDocument)(data.as_ptr(), data.len() as i32, std::ptr::null())
    };
    if doc.is_null() {
        let err = unsafe { (pdfium.FPDF_GetLastError)() };
        if err == pdfium::FPDF_ERR_FILE || err == pdfium::FPDF_ERR_FORMAT {
            anyhow::bail!("Failed to load PDF: {}", pdfium::error_str(err));
        }
        return Ok(ExtractionResult {
            text: String::new(),
            ocr_flag: true,
            word_positions: Vec::new(),
            page_count: 0,
            extraction_ms: extract_start.elapsed().as_millis() as u64,
        });
    }

    let (text, word_positions, total_chars, page_count) =
        unsafe { extract_text_and_positions(pdfium, doc) };
    let extraction_ms = extract_start.elapsed().as_millis() as u64;
    unsafe { (pdfium.FPDF_CloseDocument)(doc); }
    Ok(build_result(text, word_positions, total_chars, page_count, extraction_ms))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    // These tests require pdfium.dll to be available.
    // If pdfium.dll is not found, they are skipped.
    fn skip_if_no_pdfium() -> bool {
        if !crate::pdfium::Pdfium::is_available() {
            eprintln!("SKIP: pdfium.dll not available");
            return true;
        }
        false
    }

    fn unique_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = temp_dir().join(format!("pdf_extractor_extract_{}", id));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Generate a minimal valid PDF with a single page containing the given text.
    // Uses lopdf (dev-dependency only) for test PDF generation.
    fn make_pdf(text: &str) -> PathBuf {
        let dir = unique_dir();
        let path = dir.join("test.pdf");
        make_pdf_at(&path, text);
        path
    }

    // Generate a minimal PDF at the given path with the given text content.
    fn make_pdf_at(path: &Path, text: &str) {
        use lopdf::*;

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
        doc.save(path).unwrap();
    }

    fn make_multipage_pdf(texts: &[&str]) -> PathBuf {
        use lopdf::*;
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
        use lopdf::*;
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
        let result = ExtractionResult { text: String::new(), ocr_flag: true, word_positions: Vec::new(), page_count: 0, extraction_ms: 0 };
        assert!(result.ocr_flag);
        assert!(result.text.is_empty());
    }

    #[test]
    fn test_extraction_result_has_text() {
        let result = ExtractionResult { text: "hello".into(), ocr_flag: false, word_positions: Vec::new(), page_count: 1, extraction_ms: 0 };
        assert!(!result.ocr_flag);
        assert_eq!(result.text, "hello");
    }

    // --- extract_pdf: basic flow (requires pdfium.dll) ---

    #[test]
    fn test_extract_pdf_with_text() {
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Hello World");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag, "PDF with text should not flag OCR");
        // PDFium may add extra spacing or formatting
        assert!(result.text.contains("Hello"), "Text should contain Hello, got: {:?}", result.text);
        assert!(result.text.contains("World"), "Text should contain World, got: {:?}", result.text);
    }

    #[test]
    fn test_extract_pdf_unicode_text() {
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Multi-word text with 123 numbers");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Multi"));
        assert!(result.text.contains("123"));
    }

    #[test]
    fn test_extract_pdf_multiple_pages() {
        if skip_if_no_pdfium() { return; }
        let path = make_multipage_pdf(&["Page One", "Page Two", "Page Three"]);
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Page One"), "text: {:?}", result.text);
        assert!(result.text.contains("Page Two"), "text: {:?}", result.text);
        assert!(result.text.contains("Page Three"), "text: {:?}", result.text);
    }

    #[test]
    fn test_extract_pdf_special_characters() {
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("x=1+2*3/(4-5) [test] {braces}");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("x=1+2*3"), "text: {:?}", result.text);
    }

    // --- extract_pdf: empty / OCR flow ---

    #[test]
    fn test_extract_pdf_empty_content() {
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("");
        let result = extract_pdf(&path).unwrap();
        assert!(result.ocr_flag, "Empty PDF should flag OCR");
        assert_eq!(result.text, "");
    }

    #[test]
    fn test_extract_pdf_whitespace_only_content() {
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("   ");
        let result = extract_pdf(&path).unwrap();
        assert!(result.ocr_flag, "Whitespace-only PDF should flag OCR");
    }

    #[test]
    fn test_extract_pdf_multi_page_all_empty() {
        if skip_if_no_pdfium() { return; }
        let path = make_empty_multipage_pdf(3);
        let result = extract_pdf(&path).unwrap();
        assert!(result.ocr_flag, "Image-only multi-page PDF should set ocr_flag");
        assert_eq!(result.text, "", "Text should be empty when all pages have no content");
    }

    // --- extract_pdf: error flows ---

    #[test]
    fn test_extract_pdf_nonexistent_file() {
        if skip_if_no_pdfium() { return; }
        let path = PathBuf::from(r"C:\NONEXISTENT_FILE_98765.pdf");
        let result = extract_pdf(&path);
        assert!(result.is_err(), "Non-existent file should error");
    }

    #[test]
    fn test_extract_pdf_invalid_file() {
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("not_a_pdf.txt");
        fs::write(&path, b"This is not a PDF").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err(), "Invalid file should error");
    }

    #[test]
    fn test_extract_pdf_empty_file() {
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("empty.pdf");
        fs::write(&path, b"").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err(), "Empty file should error");
    }

    #[test]
    fn test_extract_pdf_truncated_pdf() {
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("truncated.pdf");
        fs::write(&path, b"%PDF-1.4\n1 0 obj << /Type /Catalog").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err(), "Truncated PDF should error");
    }

    // --- WordPosition tests ---

    #[test]
    fn test_word_positions_basic() {
        if skip_if_no_pdfium() { return; }
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
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("");
        let result = extract_pdf(&path).unwrap();
        assert!(result.word_positions.is_empty(), "Empty PDF should have no positions");
        assert!(result.ocr_flag);
    }

    #[test]
    fn test_word_positions_multipage() {
        if skip_if_no_pdfium() { return; }
        let path = make_multipage_pdf(&["Page One", "Page Two"]);
        let result = extract_pdf(&path).unwrap();
        assert!(!result.word_positions.is_empty());
        let pages: std::collections::HashSet<u32> = result.word_positions.iter().map(|p| p.page).collect();
        assert!(pages.contains(&1), "Should have positions on page 1");
        assert!(pages.contains(&2), "Should have positions on page 2");
    }

    #[test]
    fn test_word_position_bounds_sensible() {
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Hello World");
        let result = extract_pdf(&path).unwrap();
        for pos in &result.word_positions {
            assert!(pos.x_min >= 0.0 && pos.x_max <= 612.0,
                "x bounds should be within page width: x_min={}, x_max={}", pos.x_min, pos.x_max);
            assert!(pos.y_min >= 0.0 && pos.y_max <= 792.0,
                "y bounds should be within page height: y_min={}, y_max={}", pos.y_min, pos.y_max);
        }
    }

    // --- clean_word_text tests ---

    #[test]
    fn test_clean_word_text_basic() {
        assert_eq!(clean_word_text("Hello"), "Hello");
    }

    #[test]
    fn test_clean_word_text_with_punctuation() {
        // clean_word_text only keeps \p{L}\p{N}\p{S}
        let cleaned = clean_word_text("hello!");
        assert_eq!(cleaned, "hello", "Punctuation should be stripped");
    }

    #[test]
    fn test_clean_word_text_mixed() {
        let cleaned = clean_word_text("don't");
        // clean_word_text extrae ["don", "t"] con la regex [\p{L}\p{N}\p{S}]+ y los une con espacio
        assert_eq!(cleaned, "don t", "Apostrophe no coincide con TOKEN_PATTERN, separa en dos tokens");
    }

    #[test]
    fn test_clean_word_text_symbols() {
        let cleaned = clean_word_text("∑=∑");
        assert_eq!(cleaned, "∑=∑", "Math symbols should be preserved");
    }

    #[test]
    fn test_clean_word_text_control_char() {
        let cleaned = clean_word_text("\u{0000}");
        assert_eq!(cleaned, "", "Null character should produce empty string");
    }

    // ── Regression: extract_pdf trim behavior ──
    //
    // extract_pdf uses in-place truncation (trim_end + truncate) instead of
    // allocating a new trimmed String.  These tests verify the observable
    // contract: trailing whitespace is removed, leading whitespace is preserved,
    // empty/whitespace-only input produces empty output with ocr_flag=true.

    /// Simulates the current in-place trim_end + truncate logic.
    fn current_trim(s: &str) -> String {
        let trimmed_len = s.trim_end().len();
        let mut buf = s.to_string();
        buf.truncate(trimmed_len);
        buf
    }

    #[test]
    fn test_trim_behavior_trailing_spaces() {
        assert_eq!(current_trim("hello world   "), "hello world");
    }

    #[test]
    fn test_trim_behavior_trailing_newlines() {
        assert_eq!(current_trim("hello world\n\n\n"), "hello world");
    }

    #[test]
    fn test_trim_behavior_trailing_mixed() {
        assert_eq!(current_trim("hello world \n \t "), "hello world");
    }

    #[test]
    fn test_trim_behavior_leading_spaces_preserved() {
        // In-place trim_end preserves leading whitespace (unlike trim())
        let result = current_trim("   hello world");
        assert_eq!(result, "   hello world", "leading spaces must be preserved");
    }

    #[test]
    fn test_trim_behavior_only_whitespace() {
        assert_eq!(current_trim("   \n  \t  "), "", "whitespace-only becomes empty");
    }

    #[test]
    fn test_trim_behavior_no_trailing_whitespace() {
        assert_eq!(current_trim("hello world"), "hello world", "no change when no trailing whitespace");
    }

    #[test]
    fn test_trim_behavior_empty_string() {
        assert_eq!(current_trim(""), "", "empty string stays empty");
    }

    // ── Regression: extract_pdf consistent output format ──
    //
    // These tests verify that extract_pdf still produces the same output
    // structure regardless of the in-place trim change (requires pdfium.dll).

    #[test]
    fn test_extract_pdf_text_not_empty_for_valid_content() {
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Regression test content");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag, "PDF with text must not flag OCR");
        assert!(!result.text.is_empty(), "extracted text must not be empty");
        // Verify no leading/trailing whitespace artifacts from in-place trim
        assert_eq!(result.text, result.text.trim_end(), "text must have no trailing whitespace");
    }

    #[test]
    fn test_extract_pdf_text_trim_end_only() {
        // Verify that leading whitespace (if any) is preserved,
        // and trailing whitespace is removed.
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("  leading spaces");
        let result = extract_pdf(&path).unwrap();
        // PDFium may or may not preserve leading spaces; check that at minimum
        // no trailing whitespace remains and the text is non-empty.
        assert!(!result.ocr_flag);
        assert!(!result.text.is_empty());
        // The text should never end with whitespace
        assert!(!result.text.ends_with(' '), "text must not end with space");
        assert!(!result.text.ends_with('\n'), "text must not end with newline");
    }

    // ── FPDF_CloseDocument crash regression tests ──
    //
    // The original bug: FPDF_LoadDocument signature used *const u8 (wrong for
    // Windows UTF-16), causing every file-path load to fail → memory fallback
    // → CloseDocument access violation.  These tests verify the fix:
    //   - File-path loads succeed and CloseDocument does not crash.
    //   - Memory fallback still works for edge cases.
    //   - All document lifecycle operations are safe.

    // ── Happy path ──

    #[test]
    fn test_close_document_sequential_extractions() {
        // Stress-test document lifecycle: each call must open, extract, and
        // close cleanly without crashing or leaking.
        if skip_if_no_pdfium() { return; }
        for i in 0..20 {
            let path = make_pdf(&format!("Sequential document {}", i));
            let result = extract_pdf(&path).unwrap();
            assert!(!result.ocr_flag, "Iter {}: should extract text, not OCR", i);
            assert!(result.text.contains(&format!("Sequential document {}", i)),
                "Iter {}: text should contain title", i);
        }
    }

    #[test]
    fn test_close_document_multipage_pdf() {
        // Verify CloseDocument works correctly after multi-page extraction.
        if skip_if_no_pdfium() { return; }
        let path = make_multipage_pdf(&["Page one", "Page two", "Page three"]);
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Page one"));
        assert!(result.text.contains("Page two"));
        assert!(result.text.contains("Page three"));
    }

    #[test]
    fn test_close_document_identical_pdf_twice() {
        // Two extractions of the exact same PDF — CloseDocument must handle
        // the second open-after-close correctly.
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Open close open close");
        let r1 = extract_pdf(&path).unwrap();
        let r2 = extract_pdf(&path).unwrap();
        assert!(!r1.ocr_flag && !r2.ocr_flag);
        assert!(r1.text.contains("Open close open close"));
        assert_eq!(r1.text, r2.text, "identical input must produce identical output");
    }

    #[test]
    fn test_close_document_large_text_content() {
        // Stress multi-byte text extraction; 100KB of text should not cause
        // CloseDocument to misbehave.
        if skip_if_no_pdfium() { return; }
        let body = "Hello World ".repeat(10_000);
        let path = make_pdf(&body);
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.len() > 100_000, "text should be over 100KB");
    }

    // ── File-path edge cases ──

    #[test]
    fn test_close_document_long_file_path() {
        // Long paths exercise the UTF-16 conversion in FPDF_LoadDocument.
        // Windows MAX_PATH is 260; test a path well over that.
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let name = format!("{}long.pdf", "a".repeat(200));
        let path = dir.join(&name);
        make_pdf_at(&path, "Long path extraction");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Long path extraction"),
            "text should contain content: {:?}", result.text);
    }

    #[test]
    fn test_close_document_path_with_spaces_and_symbols() {
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("(special) #$& + 'test'.pdf");
        make_pdf_at(&path, "Special chars in filename");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Special chars in filename"));
    }

    #[test]
    fn test_close_document_unicode_file_path() {
        // Non-ASCII characters in path stress the wide-string conversion.
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("föö bär üñîçødé.pdf");
        make_pdf_at(&path, "Unicode file path");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Unicode file path"));
    }

    #[test]
    fn test_close_document_verbatim_prefix_path() {
        // Windows \\?\ prefix (long-path bypass) — strip_verbatim_prefix must
        // handle this correctly so the UTF-16 path passed to PDFium is clean.
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let normal = dir.join("verbatim_test.pdf");
        make_pdf_at(&normal, "Verbatim prefix path");
        // Construct a \\?\ prefixed path
        let canonical = std::fs::canonicalize(&normal).unwrap();
        let verbatim = format!(r"\\?\{}", canonical.display());
        let verbatim_path = Path::new(&verbatim);
        let result = extract_pdf(verbatim_path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Verbatim prefix path"),
            "text should contain content even with \\\\?\\ prefix");
    }

    #[test]
    fn test_close_document_very_deep_directory() {
        // Deeply nested path (many subdirectories) to stress path resolution.
        if skip_if_no_pdfium() { return; }
        let mut dir = unique_dir();
        for _ in 0..15 {
            dir = dir.join("subdir");
        }
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deep.pdf");
        make_pdf_at(&path, "Deeply nested path");
        let result = extract_pdf(&path).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Deeply nested path"));
    }

    // ── Error scenarios (must return errors, not crash) ──

    #[test]
    fn test_close_document_nonexistent_file() {
        // Should return Err, not crash.
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("does_not_exist.pdf");
        let result = extract_pdf(&path);
        assert!(result.is_err(), "nonexistent file must return error, got: {:?}", result);
    }

    #[test]
    fn test_close_document_empty_file() {
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("empty.pdf");
        fs::write(&path, b"").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err(), "empty file must return error");
    }

    #[test]
    fn test_close_document_corrupted_pdf() {
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("corrupted.pdf");
        fs::write(&path, b"this is not a pdf file at all").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err(), "corrupted file must return error, got: {:?}", result);
    }

    #[test]
    fn test_close_document_truncated_pdf() {
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("truncated.pdf");
        // Write the PDF header but nothing else
        fs::write(&path, b"%PDF-1.4\n").unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err(), "truncated PDF must return error, got: {:?}", result);
    }

    #[test]
    fn test_close_document_zero_byte_file() {
        if skip_if_no_pdfium() { return; }
        let dir = unique_dir();
        let path = dir.join("zerobytes.pdf");
        // Create file without writing anything
        let _ = std::fs::File::create(&path).unwrap();
        let result = extract_pdf(&path);
        assert!(result.is_err(), "zero-byte file must return error, got: {:?}", result);
    }

    // ── Concurrent access ──

    #[test]
    fn test_close_document_concurrent_extractions() {
        // Multiple threads extracting concurrently must not trigger
        // CloseDocument crashes (each thread has its own document handle).
        if skip_if_no_pdfium() { return; }
        let paths: Vec<PathBuf> = (0..10).map(|i| {
            let p = make_pdf(&format!("Concurrent doc {}", i));
            p
        }).collect();
        let handles: Vec<_> = paths.into_iter().map(|path| {
            std::thread::spawn(move || {
                let result = extract_pdf(&path);
                match result {
                    Ok(r) => assert!(!r.ocr_flag || r.text.is_empty()),
                    Err(e) => panic!("concurrent extraction failed: {}", e),
                }
            })
        }).collect();
        for h in handles {
            h.join().expect("thread should not panic");
        }
    }

    #[test]
    fn test_close_document_same_path_concurrent() {
        // Multiple threads extracting the same file concurrently.
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Shared concurrent access");
        let arc_path = std::sync::Arc::new(path);
        let handles: Vec<_> = (0..8).map(|_| {
            let p = std::sync::Arc::clone(&arc_path);
            std::thread::spawn(move || {
                let result = extract_pdf(&p);
                match result {
                    Ok(r) => assert!(r.text.contains("Shared concurrent access")),
                    Err(e) => panic!("shared concurrent extraction failed: {}", e),
                }
            })
        }).collect();
        for h in handles {
            h.join().expect("thread should not panic");
        }
    }

    // ── extract_pdf_bytes (memory-only path) ──

    #[test]
    fn test_extract_pdf_bytes_basic() {
        // Happy path: read a PDF into bytes and extract via memory.
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Memory loaded text");
        let data = std::fs::read(&path).unwrap();
        let result = extract_pdf_bytes(&data).unwrap();
        assert!(!result.ocr_flag, "PDF with text must not flag OCR");
        assert!(result.text.contains("Memory loaded text"),
            "text should contain input: {}", result.text);
    }

    #[test]
    fn test_extract_pdf_bytes_multipage() {
        // Multipage PDF extracted via memory path.
        if skip_if_no_pdfium() { return; }
        let path = make_multipage_pdf(&["Page A", "Page B", "Page C"]);
        let data = std::fs::read(&path).unwrap();
        let result = extract_pdf_bytes(&data).unwrap();
        assert!(!result.ocr_flag);
        assert!(result.text.contains("Page A"));
        assert!(result.text.contains("Page B"));
        assert!(result.text.contains("Page C"));
    }

    #[test]
    fn test_extract_pdf_bytes_identical_twice() {
        // Same PDF extracted twice via memory — CloseDocument after each.
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Twice via memory");
        let data = std::fs::read(&path).unwrap();
        let r1 = extract_pdf_bytes(&data).unwrap();
        let r2 = extract_pdf_bytes(&data).unwrap();
        assert!(r1.text.contains("Twice via memory"));
        assert_eq!(r1.text, r2.text, "identical input must produce identical output");
    }

    #[test]
    fn test_extract_pdf_bytes_empty() {
        // Empty data should produce an error.
        if skip_if_no_pdfium() { return; }
        let result = extract_pdf_bytes(b"");
        assert!(result.is_err(), "empty data must produce error, got: {:?}", result);
    }

    #[test]
    fn test_extract_pdf_bytes_invalid() {
        // Invalid (non-PDF) data should return OCR flag or error.
        if skip_if_no_pdfium() { return; }
        let result = extract_pdf_bytes(b"not a pdf at all");
        match result {
            Ok(r) => {
                // Some pdfium versions return ocr_flag=true for garbage data
                assert!(r.ocr_flag || r.text.is_empty());
            }
            Err(_) => {} // error is also acceptable
        }
    }

    #[test]
    fn test_extract_pdf_bytes_truncated() {
        // Truncated PDF data — must not crash.
        if skip_if_no_pdfium() { return; }
        let path = make_pdf("Truncation test");
        let mut data = std::fs::read(&path).unwrap();
        data.truncate(data.len() / 2);
        let result = extract_pdf_bytes(&data);
        // Must not panic — error or OCR fallback are both acceptable.
        if let Ok(r) = result {
            assert!(r.ocr_flag || r.text.is_empty());
        }
    }
}
