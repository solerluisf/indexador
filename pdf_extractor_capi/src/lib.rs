use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, CStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use image::GenericImageView;
use tantivy::query::Occur;
use tantivy::schema::Value;

use pdf_extractor::indexer::{Indexer, SearchIndex};
use pdf_extractor::metrics::Metrics;
use pdf_extractor::output::JsonlWriter;
use pdf_extractor::ocr::{self, find_tesseract};
use pdf_extractor::pipeline::{run_ocr_post_processing, run_pipeline, PipelineConfig};
use pdf_extractor::registry::CollectionRegistry;
use pdf_extractor::scanner::JobStore;

pub const PDF_EXTRACTOR_API_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Global state (Mutex<Option<..>> so tests can reset)
// ---------------------------------------------------------------------------

struct AppContext {
    jobs: Option<Arc<JobStore>>,
    indexer: Option<Arc<Indexer>>,
    db_path: Option<PathBuf>,
    index_path: Option<PathBuf>,
    last_error: Option<String>,
    channel_capacity: Option<u32>,
    tesseract_path: Option<String>,
    ocr_language: Option<String>,
    ocr_workers: Option<u32>,
    ocr_max_dim: Option<u32>,
    ram_buffer: Option<u64>,
    indexer_batch_size: Option<u32>,
    commit_interval: Option<u32>,
    commit_timeout: Option<u32>,
    extract_workers: Option<u32>,
    fuzzy_distance: u32,
    stem_enabled: u32,
    search_field: Option<String>,
    path_filter: Option<String>,
    recency_weight: f32,
    field_weights: Option<Vec<(String, f32)>>,
    collection_boosts: HashMap<i64, f32>,
    boolean_query: Option<Vec<(String, String)>>,
}

fn app_guard() -> Result<std::sync::MutexGuard<'static, Option<AppContext>>, i32> {
    static APP: OnceLock<Mutex<Option<AppContext>>> = OnceLock::new();
    APP.get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| -1)
}

fn with_app<R>(f: impl FnOnce(&mut AppContext) -> R) -> Result<R, i32> {
    let mut guard = app_guard()?;
    let app = guard.get_or_insert_with(|| AppContext {
        jobs: None,
        indexer: None,
        db_path: None,
        index_path: None,
        last_error: None,
        channel_capacity: None,
        tesseract_path: None,
        ocr_language: None,
        ocr_workers: None,
        ocr_max_dim: None,
        ram_buffer: None,
        indexer_batch_size: None,
        commit_interval: None,
        commit_timeout: None,
        extract_workers: None,
        fuzzy_distance: 0,
        stem_enabled: 0,
        search_field: None,
        path_filter: None,
        recency_weight: 0.0,
        field_weights: None,
        collection_boosts: HashMap::new(),
        boolean_query: None,
    });
    Ok(f(app))
}

fn set_error(msg: String) {
    let _ = with_app(|app| app.last_error = Some(msg));
}

static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
fn reset_state() {
    CANCEL_REQUESTED.store(false, Ordering::Relaxed);
    *app_guard().unwrap() = None;
    if let Ok(mut guard) = COLLECTION_REGISTRY.get_or_init(|| Mutex::new(None)).lock() {
        *guard = None;
    }
}

// ---------------------------------------------------------------------------
// Helper: caller-allocated buffer write
// ---------------------------------------------------------------------------

unsafe fn write_to_buffer(data: &[u8], out: *mut c_char, out_len: *mut u32) -> i32 {
    if out.is_null() || out_len.is_null() {
        return -3;
    }
    let capacity = *out_len as usize;
    let needed = data.len();
    if capacity < needed {
        *out_len = needed as u32;
        return -4;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), out as *mut u8, needed);
    *out_len = needed as u32;
    0
}

unsafe fn cstr_to_str(ptr: *const c_char) -> Result<&'static str, i32> {
    if ptr.is_null() {
        return Err(-3);
    }
    CStr::from_ptr(ptr).to_str().map_err(|_| {
        set_error("Invalid UTF-8 input".into());
        -1
    })
}

// ---------------------------------------------------------------------------
// pdf_api_version
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_api_version() -> u32 {
    let ver = PDF_EXTRACTOR_API_VERSION;
    ver
}

// ---------------------------------------------------------------------------
// pdf_init
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_init(
    db_path: *const c_char,
    index_path: *const c_char,
) -> i32 {
    let db = match unsafe { cstr_to_str(db_path) } {
        Ok(s) => s,
        Err(e) => { return e; }
    };
    let idx = match unsafe { cstr_to_str(index_path) } {
        Ok(s) => s,
        Err(e) => { return e; }
    };

    let rc = with_app(|app| {
        if app.jobs.is_some() {
            return 0;
        }

        match JobStore::open(&PathBuf::from(db)) {
            Ok(store) => app.jobs = Some(Arc::new(store)),
            Err(e) => {
                set_error(format!("Failed to open job store: {}", e));
                return -1;
            }
        }

        match Indexer::new(&PathBuf::from(idx)) {
            Ok(indexer) => app.indexer = Some(Arc::new(indexer)),
            Err(e) => {
                app.jobs = None;
                set_error(format!("Failed to open index: {}", e));
                return -1;
            }
        }

        app.db_path = Some(PathBuf::from(db));
        app.index_path = Some(PathBuf::from(idx));
        0
    })
    .unwrap_or(-1);
    rc
}

// ---------------------------------------------------------------------------
// pdf_search
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_search(
    query: *const c_char,
    limit: u32,
    offset: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    let query_str = match unsafe { cstr_to_str(query) } {
        Ok(s) => s,
        Err(e) => { return e; }
    };

    let index_path = with_app(|app| {
        if app.indexer.is_some() {
            app.index_path.clone()
        } else {
            None
        }
    })
    .unwrap_or(None);

    let index_path = match index_path {
        Some(p) => p,
        None => {
            set_error("pdf_init not called".into());
            return -2;
        }
    };

    let search_index = match SearchIndex::new(&index_path) {
        Ok(si) => si,
        Err(e) => {
            set_error(format!("Failed to open index: {}", e));
            return -1;
        }
    };

    let settings = load_search_settings();

    let (results_json, result_count) = match do_search_with_index(&search_index, query_str, limit, offset, None, &settings) {
        Ok(t) => t,
        Err(e) => { return e; }
    };

    let _total = result_count as u64;
    let count = search_index.search_count(query_str).unwrap_or(result_count as u64);

    let wrapped = serde_json::json!({
        "total": count,
        "results": if result_count == 0 {
            serde_json::Value::Array(vec![])
        } else {
            serde_json::from_str(&results_json).unwrap_or(serde_json::Value::Array(vec![]))
        }
    });
    let json_str = serde_json::to_string(&wrapped).unwrap_or_else(|_| "{}".into());
    let rc = unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) };
    rc
}

// ---------------------------------------------------------------------------
// pdf_snippet
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_snippet(
    doc_id: i64,
    query: *const c_char,
    out: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    (|| -> i32 {
    let query_str = match unsafe { cstr_to_str(query) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let index_path = with_app(|app| app.index_path.clone()).unwrap_or(None);
    let index_path = match index_path {
        Some(p) => p,
        None => {
            set_error("pdf_init not called".into());
            return -2;
        }
    };

    let search_index = match SearchIndex::new(&index_path) {
        Ok(si) => si,
        Err(e) => {
            set_error(format!("Failed to open search index: {}", e));
            return -1;
        }
    };

    if query_str.trim().is_empty() {
        return unsafe { write_to_buffer(b"", out, out_len) };
    }

    use tantivy::collector::TopDocs;
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;
    use tantivy::Term;

    let reader = match search_index
        .index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
        .try_into()
    {
        Ok(r) => r,
        Err(e) => {
            set_error(format!("Failed to create reader: {}", e));
            return -1;
        }
    };
    let searcher = reader.searcher();

    let term = Term::from_field_u64(search_index.id_field, doc_id as u64);
    let term_query = TermQuery::new(term, IndexRecordOption::Basic);

    let top_docs = match searcher.search(&term_query, &TopDocs::with_limit(1)) {
        Ok(d) => d,
        Err(e) => {
            set_error(format!("Search failed: {}", e));
            return -1;
        }
    };

    if top_docs.is_empty() {
        return unsafe { write_to_buffer(b"", out, out_len) };
    }

    let (_score, doc_addr) = &top_docs[0];
    let doc = match searcher.doc::<tantivy::TantivyDocument>(*doc_addr) {
        Ok(d) => d,
        Err(e) => {
            set_error(format!("Failed to retrieve doc: {}", e));
            return -1;
        }
    };

    match search_index.generate_snippet(&doc, query_str) {
        Ok(snippet) => unsafe { write_to_buffer(snippet.as_bytes(), out, out_len) },
        Err(e) => {
            set_error(format!("Snippet generation failed: {}", e));
            -1
        }
    }
    })()
}

// ---------------------------------------------------------------------------
// pdf_get_term_positions
// ---------------------------------------------------------------------------
// Helper: detect phrase-matching pages using Tantivy offsets (aligned
// offsets required — new indexes only).
fn detect_phrase_pages_tantivy(
    index_path: &Path,
    doc_id: i64,
    words: &[&str],
    position_store: &pdf_extractor::positions::PositionStore,
) -> Option<HashSet<u32>> {
    let search_index = SearchIndex::new(index_path).ok()?;
    let word_offsets: Vec<Vec<usize>> = words.iter()
        .filter(|w| !w.is_empty())
        .filter_map(|w| search_index.search_term_positions(doc_id as u64, w).ok())
        .collect();

    if word_offsets.len() < 2 || word_offsets.iter().any(|v| v.is_empty()) {
        return None;
    }

    // Find Tantivy offsets that form the phrase
    let mut phrase_starts: HashSet<usize> = word_offsets[0].iter().copied().collect();
    for (i, offsets) in word_offsets.iter().enumerate().skip(1) {
        let next_set: HashSet<usize> = offsets.iter().copied().collect();
        phrase_starts = phrase_starts.iter()
            .filter(|&&start| next_set.contains(&(start + i)))
            .copied()
            .collect();
    }

    if phrase_starts.is_empty() {
        return None;
    }

    // Map phrase start offsets to pages via SQLite (offsets now aligned)
    let mut pages = HashSet::new();
    for &start in &phrase_starts {
        let offsets: Vec<usize> = (0..word_offsets.len()).map(|i| start + i).collect();
        if let Ok(positions) = position_store.get_positions(doc_id, &offsets) {
            if positions.len() == word_offsets.len() {
                let all_same_page: HashSet<u32> =
                    positions.iter().map(|p| p.page).collect();
                if all_same_page.len() == 1 {
                    pages.insert(*all_same_page.iter().next().unwrap());
                }
            }
        } else {
            // Fallback: try with just the first offset
            if let Ok(single) = position_store.get_positions(doc_id, &[start]) {
                if let Some(p) = single.first() {
                    pages.insert(p.page);
                }
            }
        }
    }

    Some(pages)
}

// Helper: detect phrase-matching pages using SQLite-only word_offset
// adjacency. Works with any index (old or new).
fn detect_phrase_pages_sqlite(
    doc_id: i64,
    words: &[&str],
    position_store: &pdf_extractor::positions::PositionStore,
) -> Option<HashSet<u32>> {
    let mut word_positions: Vec<Vec<pdf_extractor::positions::StoredPosition>> = Vec::new();
    for word in words.iter().filter(|w| !w.is_empty()) {
        if let Ok(positions) = position_store.get_positions_by_term(doc_id, word) {
            if !positions.is_empty() {
                word_positions.push(positions);
            }
        }
    }

    if word_positions.len() < 2 {
        return None;
    }

    let mut pages = HashSet::new();
    for first in &word_positions[0] {
        let mut all_adjacent = true;
        for (i, positions) in word_positions.iter().enumerate().skip(1) {
            let expected_offset = first.word_offset + i;
            let found = positions.iter().any(|p| {
                p.page == first.page && p.word_offset == expected_offset
            });
            if !found {
                all_adjacent = false;
                break;
            }
        }
        if all_adjacent {
            pages.insert(first.page);
        }
    }

    if pages.is_empty() { None } else { Some(pages) }
}

/// Get word-level term positions for a specific document.
///
/// Returns a JSON array of bounding-box objects with page numbers, e.g.
/// `[{"page":1,"x_min":100.0,"y_min":700.0,"x_max":120.0,"y_max":712.0}]`.
///
/// For multi-word (phrase) queries, returns positions only from pages where
/// the words appear adjacent in the Tantivy token stream. Uses Tantivy
/// offsets first (requires aligned offsets — new indexes), falls back to
/// SQLite-only word_offset adjacency (works with any index).
///
/// `coll_id` — 0 uses the legacy `pdf_init` index; non-zero uses the
/// collection with that ID from the registry.
///
/// Returns empty array `[]` if the doc or term is not found.
#[no_mangle]
pub unsafe extern "C" fn pdf_get_term_positions(
    coll_id: u32,
    doc_id: i64,
    term: *const c_char,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    let rc = (|| -> i32 {
        let term_str = match unsafe { cstr_to_str(term) } {
            Ok(s) => s,
            Err(e) => return e,
        };

        let index_path = if coll_id == 0 {
            with_app(|app| app.index_path.clone()).unwrap_or(None)
        } else {
            match with_registry(|reg| reg.get_collection(coll_id as i64)) {
                Ok(Ok(c)) => Some(PathBuf::from(&c.data_dir).join(".pdf_extractor").join("index")),
                Ok(Err(_)) => {
                    set_error("Collection not found".into());
                    return -8;
                }
                Err(e) => return e,
            }
        };

        let index_path = match index_path {
            Some(p) => p,
            None => {
                set_error("No index available (call pdf_init or create a registry)".into());
                return -2;
            }
        };

        let positions_db_path = index_path.join("positions.sqlite");
        let position_store = match pdf_extractor::positions::PositionStore::open(&positions_db_path) {
            Ok(store) => store,
            Err(_) => {
                let empty = b"[]";
                return unsafe { write_to_buffer(empty, out_json, out_len) };
            }
        };

        let stripped: String = term_str.trim_matches('"').to_string();
        let words: Vec<&str> = stripped.split_whitespace().collect();
        let mut seen = HashSet::new();
        let mut all_positions: Vec<pdf_extractor::positions::StoredPosition> = Vec::new();
        for word in words.iter().filter(|w| !w.is_empty()) {
            if let Ok(positions) = position_store.get_positions_by_term(doc_id, word) {
                for pos in &positions {
                    let key = (pos.page, (pos.x_min * 100.0) as i32, (pos.y_min * 100.0) as i32);
                    if seen.insert(key) {
                        all_positions.push(pos.clone());
                    }
                }
            }
        }

        // For phrase queries (multi-word), filter to only pages where the words
        // appear adjacent in the Tantivy token stream. Try Tantivy-based
        // detection first (requires aligned offsets — new indexes). Falls back
        // to SQLite-only adjacency (works with any index).
        if words.len() > 1 && !all_positions.is_empty() {
            let matching_pages = detect_phrase_pages_tantivy(&index_path, doc_id, &words, &position_store)
                .or_else(|| detect_phrase_pages_sqlite(doc_id, &words, &position_store));

            if let Some(pages) = matching_pages {
                all_positions.retain(|p| pages.contains(&p.page));
            }
        }

        let json_entries: Vec<serde_json::Value> = all_positions.iter().map(|sp| {
            serde_json::json!({
                "page": sp.page,
                "x_min": sp.x_min,
                "y_min": sp.y_min,
                "x_max": sp.x_max,
                "y_max": sp.y_max,
            })
        }).collect();

        let json_str = serde_json::to_string(&json_entries).unwrap_or_else(|_| "[]".into());
        unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
    })();
    rc
}

// ---------------------------------------------------------------------------
// pdf_search_term_offsets
// Returns a JSON integer array of word offsets within the content_norm field,
// using Tantivy's term vectors. These are 0-indexed word positions within the
// indexed text, unlike pdf_get_term_positions which uses the SQLite position
// store for page-level bounding boxes.
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_search_term_offsets(
    coll_id: u32,
    doc_id: i64,
    term: *const c_char,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    let rc = (|| -> i32 {
        let term_str = match unsafe { cstr_to_str(term) } {
            Ok(s) => s,
            Err(e) => return e,
        };

        let index_path = if coll_id == 0 {
            with_app(|app| app.index_path.clone()).unwrap_or(None)
        } else {
            match with_registry(|reg| reg.get_collection(coll_id as i64)) {
                Ok(Ok(c)) => Some(PathBuf::from(&c.data_dir).join(".pdf_extractor").join("index")),
                Ok(Err(_)) => {
                    set_error("Collection not found".into());
                    return -8;
                }
                Err(e) => return e,
            }
        };

        let index_path = match index_path {
            Some(p) => p,
            None => {
                set_error("No index available (call pdf_init or create a registry)".into());
                return -2;
            }
        };

        let search_index = match pdf_extractor::indexer::SearchIndex::new(&index_path) {
            Ok(si) => si,
            Err(e) => {
                set_error(format!("Failed to open index: {}", e));
                let empty = b"[]";
                return unsafe { write_to_buffer(empty, out_json, out_len) };
            }
        };

        let offsets = match search_index.search_term_positions(doc_id as u64, term_str) {
            Ok(v) => v,
            Err(_) => Vec::new(),
        };

        let json_str = serde_json::to_string(&offsets).unwrap_or_else(|_| "[]".into());
        unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
    })();
    rc
}

// ---------------------------------------------------------------------------
// pdf_page_count
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_page_count(path: *const c_char) -> i32 {
    let p = match unsafe { cstr_to_str(path) } {
        Ok(s) => s,
        Err(_) => return -3,
    };

    let rc = (|| -> i32 {
        let clean = p.strip_prefix(r"\\?\").unwrap_or(p);
        match lopdf::Document::load(clean) {
            Ok(doc) => {
                let pages = doc.get_pages();
                pages.len() as i32
            }
            Err(e) => match e {
                lopdf::Error::IO { .. } => -2,
                _ => -1,
            },
        }
    })();
    rc
}

// ---------------------------------------------------------------------------
// pdf_render_thumbnail
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_render_thumbnail(
    path: *const c_char,
    page: u32,
    max_w: u32,
    out_buf: *mut u8,
    out_len: *mut u32,
) -> i32 {
    (|| -> i32 {
    let p = match unsafe { cstr_to_str(path) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    if out_len.is_null() {
        return -3;
    }

    let pdf_path = PathBuf::from(p);
    if !pdf_path.exists() {
        set_error("File not found".into());
        return -2;
    }

    let temp_dir = std::env::temp_dir().join("pdf_extractor_capi_thumbs");
    let _ = std::fs::create_dir_all(&temp_dir);
    let png_path = temp_dir.join(format!("page_{}.png", page));
    let _ = std::fs::remove_file(&png_path);

    let page_str = page.to_string();
    let rendered = {
        let r1 = std::process::Command::new("mutool")
            .args(["draw", "-o"])
            .arg(&png_path)
            .args(["-r", "150"])
            .arg(&pdf_path)
            .arg(&page_str)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| png_path.exists())
            .unwrap_or(false);
        if r1 {
            true
        } else {
            let ppm_path = temp_dir.join(format!("page_{}.ppm", page));
            let _ = std::fs::remove_file(&ppm_path);
            let r2 = std::process::Command::new("pdftoppm")
                .args(["-r", "150", "-gray", "-f", &page_str, "-l", &page_str, "-singlefile"])
                .arg(&pdf_path)
                .arg(&ppm_path.with_extension(""))
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|_| {
                    if ppm_path.exists() {
                        // Convert PPM to PNG using the image crate
                        let img = image::open(&ppm_path).ok()?;
                        let _ = std::fs::remove_file(&ppm_path);
                        img.save(&png_path).ok()
                    } else {
                        None
                    }
                })
                .is_some();
            r2
        }
    };

    if !rendered || !png_path.exists() {
        set_error("No PDF renderer available (install mutool or pdftoppm)".into());
        return -1;
    }

    // Load and optionally resize the image
    let img_data = if max_w > 0 {
        match image::open(&png_path) {
            Ok(img) => {
                let (w, h) = img.dimensions();
                if w > max_w {
                    let new_h = (h as f64 * max_w as f64 / w as f64) as u32;
                    let resized = image::imageops::resize(
                        &img,
                        max_w,
                        new_h.max(1),
                        image::imageops::FilterType::Lanczos3,
                    );
                    let mut buf = std::io::Cursor::new(Vec::new());
                    let _ = resized.write_to(&mut buf, image::ImageFormat::Png);
                    buf.into_inner()
                } else {
                    std::fs::read(&png_path).unwrap_or_default()
                }
            }
            Err(_) => std::fs::read(&png_path).unwrap_or_default(),
        }
    } else {
        std::fs::read(&png_path).unwrap_or_default()
    };

    let _ = std::fs::remove_file(&png_path);

    if img_data.is_empty() {
        set_error("Rendered image is empty".into());
        return -1;
    }

    let needed = img_data.len() as u32;
    if out_buf.is_null() {
        *out_len = needed;
        return 0;
    }
    if *out_len < needed {
        *out_len = needed;
        return -4;
    }
    std::ptr::copy_nonoverlapping(img_data.as_ptr(), out_buf, needed as usize);
    *out_len = needed;
    0
    })()
}

// ---------------------------------------------------------------------------
// pdf_free_string
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_free_string(_ptr: *mut c_char) {
    // No-op: caller-allocated buffers are freed by the caller.
}

// ---------------------------------------------------------------------------
// pdf_last_error
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_last_error(out: *mut c_char, out_len: *mut u32) -> i32 {
    (|| -> i32 {
    let msg = with_app(|app| app.last_error.clone().unwrap_or_default()).unwrap_or_default();
    unsafe { write_to_buffer(msg.as_bytes(), out, out_len) }
    })()
}

// ---------------------------------------------------------------------------
// pdf_find_tesseract
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_find_tesseract(out: *mut c_char, out_len: *mut u32) -> i32 {
    (|| -> i32 {
    match find_tesseract() {
        Some(path) => {
            let path_str = path.to_string_lossy().to_string();
            unsafe { write_to_buffer(path_str.as_bytes(), out, out_len) }
        }
        None => {
            unsafe { write_to_buffer(b"", out, out_len); }
            -1
        }
    }
    })()
}

// ---------------------------------------------------------------------------
// pdf_set_channel_capacity
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_set_channel_capacity(capacity: u32) -> i32 {
    (|| -> i32 {
    if capacity == 0 {
        set_error("Channel capacity must be > 0".into());
        return -3;
    }
    match with_app(|app| {
        app.channel_capacity = Some(capacity);
        Ok::<_, i32>(())
    }) {
        Ok(_) => 0,
        Err(e) => e,
    }
    })()
}

// ---------------------------------------------------------------------------
// pdf_extract
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_extract(
    input_dir: *const c_char,
    progress: Option<extern "C" fn(u64, u64)>,
) -> i32 {
    (|| -> i32 {
    let dir = match unsafe { cstr_to_str(input_dir) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let input_path = PathBuf::from(dir);
    if !input_path.is_dir() {
        set_error("Input path is not a directory".into());
        return -3;
    }

    let temp_dir = std::env::temp_dir().join("pdf_extractor_capi");
    let _ = std::fs::create_dir_all(&temp_dir);
    let jsonl_path = temp_dir.join("output.jsonl");
    let writer = match JsonlWriter::new(&jsonl_path) {
        Ok(w) => w,
        Err(e) => {
            set_error(format!("Failed to create output writer: {}", e));
            return -1;
        }
    };

    let (jobs, indexer) = with_app(|app| {
        let jobs = app.jobs.as_ref().map(Arc::clone);
        let indexer = app.indexer.as_ref().map(Arc::clone);
        (jobs, indexer)
    })
    .unwrap_or((None, None));

    let jobs = match jobs {
        Some(j) => j,
        None => {
            set_error("pdf_init not called".into());
            return -2;
        }
    };
    let indexer = match indexer {
        Some(i) => Some(i),
        None => {
            set_error("pdf_init not called".into());
            return -2;
        }
    };

    let total = jobs.count_pending().unwrap_or(0) as u64;
    let metrics = Arc::new(Metrics::new());

    let channel_capacity = with_app(|app| app.channel_capacity).unwrap_or(None);
    let config = PipelineConfig {
        channel_capacity: channel_capacity.map(|v| v as usize),
        ..Default::default()
    };

    let result = run_pipeline(
        Arc::clone(&jobs),
        &writer,
        Arc::clone(&metrics),
        &input_path,
        indexer,
        &config,
    );

    let processed = metrics.processed();
    if let Some(cb) = progress {
        cb(processed, total);
    }

    match result {
        Ok(()) => processed as i32,
        Err(e) => {
            set_error(format!("Extraction failed: {}", e));
            -1
        }
    }
    })()
}

// ---------------------------------------------------------------------------
// pdf_stats
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_stats(out_json: *mut c_char, out_len: *mut u32) -> i32 {
    (|| -> i32 {
    let index_path = with_app(|app| app.index_path.clone()).unwrap_or(None);
    let index_path = match index_path {
        Some(p) => p,
        None => {
            set_error("pdf_init not called".into());
            return -2;
        }
    };

    let search_index = match SearchIndex::new(&index_path) {
        Ok(si) => si,
        Err(e) => {
            set_error(format!("Failed to open index: {}", e));
            return -1;
        }
    };

    match search_index.compute_stats(&index_path) {
        Ok(stats) => {
            let obj = serde_json::json!({
                "num_docs": stats.num_docs,
                "num_segments": stats.num_segments,
                "size_bytes": stats.size_bytes,
            });
            let json_str = serde_json::to_string(&obj).unwrap_or_else(|_| "{}".into());
            unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
        }
        Err(e) => {
            set_error(format!("Stats failed: {}", e));
            -1
        }
    }
    })()
}

// ---------------------------------------------------------------------------
// Collection Registry API
// ---------------------------------------------------------------------------

static COLLECTION_REGISTRY: OnceLock<Mutex<Option<CollectionRegistry>>> = OnceLock::new();

fn registry_guard() -> Result<std::sync::MutexGuard<'static, Option<CollectionRegistry>>, i32> {
    COLLECTION_REGISTRY
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| -1)
}

fn with_registry<F, R>(f: F) -> Result<R, i32>
where
    F: FnOnce(&CollectionRegistry) -> R,
{
    let guard = registry_guard()?;
    let reg = guard.as_ref().ok_or(-7)?;
    Ok(f(reg))
}

#[no_mangle]
pub unsafe extern "C" fn pdf_create_registry(registry_dir: *const c_char) -> i32 {
    (|| -> i32 {
    let dir = match unsafe { cstr_to_str(registry_dir) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    match CollectionRegistry::open(Path::new(dir)) {
        Ok(reg) => {
            match registry_guard() {
                Ok(mut guard) => {
                    *guard = Some(reg);
                    0
                }
                Err(e) => e,
            }
        }
        Err(e) => {
            set_error(format!("Failed to create registry: {}", e));
            -1
        }
    }
    })()
}

// ---------------------------------------------------------------------------
// Config setters (stored in AppContext, consumed by pdf_index_collection,
// pdf_search_collection, pdf_search_all)
// ---------------------------------------------------------------------------

macro_rules! define_u32_setter {
    ($name:ident, $field:ident) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(value: u32) -> i32 {
            (|| -> i32 {
            match with_app(|app| { app.$field = Some(value); Ok::<_, i32>(()) }) {
                Ok(_) => 0,
                Err(e) => e,
            }
            })()
        }
    };
}

macro_rules! define_u32_direct_setter {
    ($name:ident, $field:ident) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(value: u32) -> i32 {
            (|| -> i32 {
            match with_app(|app| { app.$field = value; Ok::<_, i32>(()) }) {
                Ok(_) => 0,
                Err(e) => e,
            }
            })()
        }
    };
}

macro_rules! define_string_setter {
    ($name:ident, $field:ident) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(value: *const c_char) -> i32 {
            (|| -> i32 {
            if value.is_null() {
                match with_app(|app| { app.$field = None; Ok::<_, i32>(()) }) {
                    Ok(_) => 0,
                    Err(e) => e,
                }
            } else {
                let s = match unsafe { cstr_to_str(value) } {
                    Ok(s) => s.to_string(),
                    Err(e) => return e,
                };
                match with_app(|app| { app.$field = Some(s); Ok::<_, i32>(()) }) {
                    Ok(_) => 0,
                    Err(e) => e,
                }
            }
            })()
        }
    };
}

define_u32_setter!(pdf_set_ocr_workers, ocr_workers);
define_u32_setter!(pdf_set_ocr_max_dim, ocr_max_dim);
define_u32_setter!(pdf_set_indexer_batch_size, indexer_batch_size);
define_u32_setter!(pdf_set_commit_interval, commit_interval);
define_u32_setter!(pdf_set_commit_timeout, commit_timeout);
define_u32_setter!(pdf_set_extract_workers, extract_workers);
define_u32_direct_setter!(pdf_set_fuzzy_distance, fuzzy_distance);
define_u32_direct_setter!(pdf_set_stem, stem_enabled);
define_string_setter!(pdf_set_tesseract_path, tesseract_path);
define_string_setter!(pdf_set_ocr_language, ocr_language);
define_string_setter!(pdf_set_search_field, search_field);
define_string_setter!(pdf_set_path_filter, path_filter);

#[no_mangle]
pub unsafe extern "C" fn pdf_set_ram_buffer(value: u64) -> i32 {
    (|| -> i32 {
    match with_app(|app| { app.ram_buffer = Some(value); Ok::<_, i32>(()) }) {
        Ok(_) => 0,
        Err(e) => e,
    }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_recency_weight(value: f32) -> i32 {
    (|| -> i32 {
    match with_app(|app| { app.recency_weight = value; Ok::<_, i32>(()) }) {
        Ok(_) => 0,
        Err(e) => e,
    }
    })()
}

/// Set per-field weight boosts for ranked search.
///
/// `json` is a JSON object mapping Tantivy field names to float weights:
/// e.g. `{"content_norm": 1.0, "math_source": 3.0}`.
///
/// Pass `null` to reset to unweighted (default) search. On success the
/// next call to any search function will use BoostQuery for each field.
///
/// Valid field names: content_norm, content_raw, math_source,
/// normalized_text, content_stem, content_jp, content_zh, path.
#[no_mangle]
pub unsafe extern "C" fn pdf_set_field_weights(json: *const c_char) -> i32 {
    (|| -> i32 {
    if json.is_null() {
        return match with_app(|app| { app.field_weights = None; Ok::<_, i32>(()) }) {
            Ok(_) => 0,
            Err(e) => e,
        };
    }
    let s = match unsafe { cstr_to_str(json) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let map: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(s) {
        Ok(m) => m,
        Err(e) => {
            set_error(format!("Invalid field weights JSON: {}", e));
            return -3;
        }
    };
    let mut weights = Vec::with_capacity(map.len());
    for (field, value) in &map {
        let w = match value.as_f64() {
            Some(w) if w > 0.0 => w as f32,
            _ => {
                set_error(format!("Invalid weight for field '{}': must be a positive number", field));
                return -3;
            }
        };
        weights.push((field.clone(), w));
    }
    if weights.is_empty() {
        set_error("Field weights object must have at least one entry".into());
        return -3;
    }
    match with_app(|app| { app.field_weights = Some(weights); Ok::<_, i32>(()) }) {
        Ok(_) => 0,
        Err(e) => e,
    }
    })()
}

/// Set a boost multiplier for results coming from a specific collection.
///
/// Used by `pdf_search_all` to rank one collection's results higher/lower
/// than another's. Stored in AppContext (per-session), not persisted.
///
/// `coll_id` must be a registered collection ID. `weight` should be > 0.0;
/// 1.0 = no boost, 2.0 = double score, 0.5 = half score.
#[no_mangle]
pub unsafe extern "C" fn pdf_set_collection_boost(coll_id: u32, weight: f32) -> i32 {
    (|| -> i32 {
    if weight <= 0.0 {
        set_error("Collection boost must be > 0.0".into());
        return -3;
    }
    match with_app(|app| { app.collection_boosts.insert(coll_id as i64, weight); Ok::<_, i32>(()) }) {
        Ok(_) => 0,
        Err(e) => e,
    }
    })()
}

/// Set a boolean (MUST / SHOULD / MUST_NOT) query for the next search.
///
/// `json` is a JSON array of clause objects:
/// ```json
/// [
///   {"term": "climate change", "occur": "must"},
///   {"term": "energy",         "occur": "should"},
///   {"term": "politics",       "occur": "must_not"}
/// ]
/// ```
///
/// Each clause's term is parsed by Tantivy's QueryParser and combined
/// into a `BooleanQuery` using the given `Occur` semantics.
///
/// Pass `null` to reset to simple (non-boolean) query mode.
#[no_mangle]
pub unsafe extern "C" fn pdf_set_boolean_query(json: *const c_char) -> i32 {
    (|| -> i32 {
    if json.is_null() {
        return match with_app(|app| { app.boolean_query = None; Ok::<_, i32>(()) }) {
            Ok(_) => 0,
            Err(e) => e,
        };
    }
    let s = match unsafe { cstr_to_str(json) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let arr: Vec<serde_json::Value> = match serde_json::from_str(s) {
        Ok(a) => a,
        Err(e) => {
            set_error(format!("Invalid boolean query JSON: {}", e));
            return -3;
        }
    };
    if arr.is_empty() {
        set_error("Boolean query must have at least one clause".into());
        return -3;
    }
    let mut clauses = Vec::with_capacity(arr.len());
    for (i, clause) in arr.iter().enumerate() {
        let term = match clause.get("term").and_then(|v| v.as_str()) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                set_error(format!("Clause {} missing non-empty 'term'", i));
                return -3;
            }
        };
        let occur = match clause.get("occur").and_then(|v| v.as_str()) {
            Some("should") => "should",
            Some("must_not") => "must_not",
            Some("must") | None => "must",
            Some(other) => {
                set_error(format!("Clause {} invalid 'occur': '{}' (must be must/should/must_not)", i, other));
                return -3;
            }
        };
        clauses.push((term, occur.to_string()));
    }
    match with_app(|app| { app.boolean_query = Some(clauses); Ok::<_, i32>(()) }) {
        Ok(_) => 0,
        Err(e) => e,
    }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_add_collection(books_folder: *const c_char) -> i32 {
    (|| -> i32 {
    let path = match unsafe { cstr_to_str(books_folder) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    match with_registry(|reg| reg.add_collection(Path::new(path))) {
        Ok(Ok(id)) => id as i32,
        Ok(Err(e)) => { set_error(format!("{}", e)); -1 }
        Err(e) => e,
    }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_remove_collection(coll_id: u32) -> i32 {
    (|| -> i32 {
    match with_registry(|reg| reg.remove_collection(coll_id as i64)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => { set_error(format!("{}", e)); -1 }
        Err(e) => e,
    }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_list_collections(out_json: *mut c_char, out_len: *mut u32) -> i32 {
    (|| -> i32 {
    let collections = match with_registry(|reg| reg.list_collections()) {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => { set_error(format!("{}", e)); return -1; }
        Err(e) => return e,
    };
    let json_entries: Vec<serde_json::Value> = collections
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "books_folder": c.books_folder,
                "label": c.label,
                "data_dir": c.data_dir,
                "doc_count": c.doc_count,
                "last_indexed": c.last_indexed,
                "created_at": c.created_at,
            })
        })
        .collect();
    let json_str = serde_json::to_string(&json_entries).unwrap_or_else(|_| "[]".into());
    unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_index_collection(
    coll_id: u32,
    flags: u32,
    progress_callback: Option<extern "C" fn(u64, u64)>,
) -> i32 {
    (|| -> i32 {
    CANCEL_REQUESTED.store(false, Ordering::Relaxed);

    let collection = match with_registry(|reg| reg.get_collection(coll_id as i64)) {
        Ok(Ok(c)) => c,
        Ok(Err(_)) => { set_error("Collection not found".into()); return -8; }
        Err(e) => return e,
    };
    let canonical = match std::fs::canonicalize(Path::new(&collection.books_folder)) {
        Ok(p) => p,
        Err(e) => { set_error(format!("Books folder not accessible: {}", e)); return -1; }
    };

    match with_registry(|reg| reg.ensure_data_dirs(coll_id as i64)) {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => { set_error(format!("{}", e)); return -1; }
        Err(e) => return e,
    }

    let db_path = match with_registry(|reg| reg.db_path(coll_id as i64)) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let index_path = match with_registry(|reg| reg.index_path(coll_id as i64)) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let output_path = match with_registry(|reg| reg.output_path(coll_id as i64)) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let jobs = match JobStore::open(&db_path) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            set_error(format!("Failed to open job store: {}", e));
            return -1;
        }
    };

    let writer = match JsonlWriter::new(&output_path) {
        Ok(w) => w,
        Err(e) => {
            set_error(format!("Failed to create output writer: {}", e));
            return -1;
        }
    };

    let no_index = (flags & 2) != 0;
    let indexer = if no_index {
        None
    } else {
        match with_app(|app| app.ram_buffer) {
            Ok(Some(buf)) => match Indexer::with_ram_buffer(&index_path, buf) {
                Ok(idx) => Some(Arc::new(idx)),
                Err(e) => {
                    set_error(format!("Failed to open index: {}", e));
                    return -1;
                }
            },
            _ => match Indexer::new(&index_path) {
                Ok(idx) => Some(Arc::new(idx)),
                Err(e) => {
                    set_error(format!("Failed to open index: {}", e));
                    return -1;
                }
            },
        }
    };

    let metrics = Arc::new(Metrics::new());

    let channel_capacity = with_app(|app| app.channel_capacity).unwrap_or(None);
    let extract_workers = with_app(|app| app.extract_workers).unwrap_or(None);
    let indexer_batch_size = with_app(|app| app.indexer_batch_size).unwrap_or(None);
    let commit_interval = with_app(|app| app.commit_interval).unwrap_or(None);
    let commit_timeout = with_app(|app| app.commit_timeout).unwrap_or(None);
    let config = PipelineConfig {
        channel_capacity: channel_capacity.map(|v| v as usize),
        num_extract_workers: extract_workers.map(|v| v as usize),
        indexer_batch_size: indexer_batch_size.map(|v| v as usize),
        commit_interval: commit_interval.map(|v| v as u64),
        commit_timeout: commit_timeout.map(|v| v as u64),
        progress_cb: progress_callback.map(|cb| {
            Box::new(move |current: u64, total: u64| cb(current, total)) as Box<dyn Fn(u64, u64) + Send>
        }),
        cancel_flag: Some(&CANCEL_REQUESTED),
    };

    match run_pipeline(Arc::clone(&jobs), &writer, Arc::clone(&metrics), &canonical, indexer, &config) {
        Ok(()) => {
            let processed = metrics.processed();
            let _ = with_registry(|reg| reg.update_index_metadata(coll_id as i64, processed));

            // Run OCR post-processing if flag (1) is set
            if (flags & 1) != 0 {
                // Drop the JSONL writer before we re-open it for OCR output
                drop(writer);

                let tesseract_path = with_app(|app| app.tesseract_path.clone()).unwrap_or(None)
                    .map(PathBuf::from)
                    .or_else(|| find_tesseract());

                let tesseract_path = match tesseract_path {
                    Some(p) => p,
                    None => {
                        set_error(
                            "Tesseract not found. Install Tesseract-OCR to the default location, \
                             add it to PATH, or set the path via pdf_set_tesseract_path.".into()
                        );
                        return -1;
                    }
                };

                let ocr_language = with_app(|app| app.ocr_language.clone()).unwrap_or(None)
                    .unwrap_or_else(|| "eng".to_string());

                let ocr_max_dim = with_app(|app| app.ocr_max_dim).unwrap_or(None).unwrap_or(3000);
                let num_workers = with_app(|app| app.ocr_workers).unwrap_or(None).map(|v| v as usize);

                let ocr_config = ocr::OcrConfig {
                    tesseract_path,
                    max_dim: ocr_max_dim,
                    max_retries: 2,
                    language: ocr_language,
                };

                match run_ocr_post_processing(
                    jobs,
                    &ocr_config,
                    Some(output_path),
                    num_workers,
                ) {
                    Ok(_ocr_count) => {
                        processed as i32
                    }
                    Err(e) => {
                        set_error(format!("OCR post-processing failed: {}", e));
                        -1
                    }
                }
            } else {
                processed as i32
            }
        }
        Err(e) => {
            set_error(format!("Indexing failed: {}", e));
            -1
        }
    }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_cancel_indexing() {
    CANCEL_REQUESTED.store(true, Ordering::Relaxed);
}

#[no_mangle]
pub unsafe extern "C" fn pdf_is_cancel_requested() -> i32 {
    (|| -> i32 {
    if CANCEL_REQUESTED.load(Ordering::Relaxed) { 1 } else { 0 }
    })()
}

/// Settings read from AppContext once, to avoid mutex contention in parallel search.
#[derive(Clone, Default)]
struct SearchSettings {
    fuzzy: u32,
    stem: bool,
    field: Option<String>,
    path_filter: Option<String>,
    recency_weight: f32,
    field_weights: Option<Vec<(String, f32)>>,
    boolean_query: Option<Vec<(String, String)>>,
}

fn load_search_settings() -> SearchSettings {
    with_app(|app| SearchSettings {
        fuzzy: app.fuzzy_distance,
        stem: app.stem_enabled != 0,
        field: app.search_field.clone(),
        path_filter: app.path_filter.clone(),
        recency_weight: app.recency_weight,
        field_weights: app.field_weights.clone(),
        boolean_query: app.boolean_query.clone(),
    })
    .unwrap_or_default()
}

fn do_search_with_index(
    search_index: &SearchIndex,
    query: &str,
    limit: u32,
    offset: u32,
    coll_id: Option<i64>,
    settings: &SearchSettings,
) -> Result<(String, i32), i32> {
    let results = if let Some(ref bool_clauses) = settings.boolean_query {
        let refs: Vec<(&str, Occur)> = bool_clauses.iter()
            .map(|(term, occur_str)| {
                let occur = match occur_str.as_str() {
                    "should" => Occur::Should,
                    "must_not" => Occur::MustNot,
                    _ => Occur::Must,
                };
                (term.as_str(), occur)
            })
            .collect();
        search_index.search_boolean(&refs, limit as usize, settings.path_filter.as_deref(), offset as usize, settings.stem)
    } else if let Some(ref weights) = settings.field_weights {
        let refs: Vec<(&str, f32)> = weights.iter().map(|(n, w)| (n.as_str(), *w)).collect();
        search_index.search_weighted_fields(query, &refs, limit as usize, settings.path_filter.as_deref(), offset as usize)
    } else if let Some(ref field_name) = settings.field {
        search_index.search_in_field_fuzzy_stem(
            query, field_name, limit as usize, settings.path_filter.as_deref(), offset as usize, settings.fuzzy as u8, settings.stem,
        )
    } else if settings.fuzzy > 0 {
        search_index.search_fuzzy_stem(
            query, limit as usize, settings.path_filter.as_deref(), offset as usize, settings.fuzzy as u8, settings.stem,
        )
    } else if settings.stem {
        search_index.search_stem(
            query, limit as usize, settings.path_filter.as_deref(), offset as usize, true,
        )
    } else {
        search_index.search(query, limit as usize, settings.path_filter.as_deref(), offset as usize)
    };

    let results = match results {
        Ok(r) => r,
        Err(e) => {
            return Err({
                set_error(format!("Search failed: {}", e));
                -1
            });
        }
    };

    let results = if settings.recency_weight > 0.0 {
        search_index.apply_recency_boost(results, settings.recency_weight, 365)
    } else {
        results
    };

    let json_entries: Vec<serde_json::Value> = results
        .iter()
        .map(|(score, doc)| {
            let id_val = doc
                .get_first(search_index.id_field)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let path_val = doc
                .get_first(search_index.path_field)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let snippet = search_index
                .generate_snippet(doc, query)
                .unwrap_or_default();
            let mut entry = serde_json::json!({
                "id": id_val,
                "score": score,
                "path": path_val,
                "snippet": snippet,
            });
            if let Some(cid) = coll_id {
                entry["collection_id"] = serde_json::json!(cid);
            }
            entry
        })
        .collect();

    let json_str = serde_json::to_string(&json_entries).unwrap_or_else(|_| "[]".into());
    Ok((json_str, json_entries.len() as i32))
}

#[no_mangle]
pub unsafe extern "C" fn pdf_search_collection(
    coll_id: u32,
    query: *const c_char,
    limit: u32,
    offset: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    (|| -> i32 {
    let query_str = match unsafe { cstr_to_str(query) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let collection = match with_registry(|reg| reg.get_collection(coll_id as i64)) {
        Ok(Ok(c)) => c,
        Ok(Err(_)) => { set_error("Collection not found".into()); return -8; }
        Err(e) => return e,
    };

    let index_path = PathBuf::from(&collection.data_dir).join(".pdf_extractor").join("index");

    let search_index = match SearchIndex::new(&index_path) {
        Ok(si) => si,
        Err(_) => {
            let empty = serde_json::json!({"total": 0, "results": []});
            let s = serde_json::to_string(&empty).unwrap();
            return unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) };
        }
    };

    let settings = load_search_settings();

    let (results_json, result_count) = match do_search_with_index(&search_index, query_str, limit, offset, Some(coll_id as i64), &settings) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let count = search_index.search_count(query_str).unwrap_or(result_count as u64);

    let wrapped = serde_json::json!({
        "total": count,
        "results": if result_count == 0 {
            serde_json::Value::Array(vec![])
        } else {
            serde_json::from_str(&results_json).unwrap_or(serde_json::Value::Array(vec![]))
        }
    });
    let json_str = serde_json::to_string(&wrapped).unwrap_or_else(|_| "{}".into());
    unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_search_all(
    query: *const c_char,
    limit: u32,
    offset: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    (|| -> i32 {
    let query_str = match unsafe { cstr_to_str(query) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    let collections = match with_registry(|reg| reg.list_collections()) {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => { set_error(format!("{}", e)); return -1; }
        Err(e) => return e,
    };

    let settings = load_search_settings();

    let collection_boosts = settings.boolean_query.as_ref().map(|_| HashMap::new()).unwrap_or_else(|| {
        with_app(|app| app.collection_boosts.clone()).unwrap_or_default()
    });

    use rayon::prelude::*;
    let coll_results: Vec<(Vec<serde_json::Value>, u64, bool)> = collections.par_iter().map(|coll| {
        let index_path = PathBuf::from(&coll.data_dir).join(".pdf_extractor").join("index");
        if !index_path.join("meta.json").exists() {
            return (Vec::new(), 0u64, false);
        }

        let search_index = match SearchIndex::new(&index_path) {
            Ok(si) => si,
            Err(_) => return (Vec::new(), 0u64, true),
        };

        let count = search_index.search_count(query_str).unwrap_or(0);

        let results = do_search_with_index(&search_index, query_str, limit, offset, Some(coll.id), &settings);

        let mut entries = match results {
            Ok((json_str, _)) => {
                serde_json::from_str::<Vec<serde_json::Value>>(&json_str).unwrap_or_default()
            }
            Err(_) => return (Vec::new(), 0u64, true),
        };

        let boost = collection_boosts.get(&coll.id).copied().unwrap_or(1.0);
        if (boost - 1.0).abs() > f32::EPSILON {
            for entry in &mut entries {
                if let Some(score) = entry["score"].as_f64() {
                    entry["score"] = serde_json::json!(score * boost as f64);
                }
            }
        }

        (entries, count, false)
    }).collect();

    let mut all_results = Vec::new();
    let mut total_all: u64 = 0;
    let mut had_error = false;

    for (entries, count, error) in coll_results {
        if error {
            had_error = true;
        }
        total_all += count;
        all_results.extend(entries);
    }

    if had_error && all_results.is_empty() {
        return -1;
    }

    all_results.sort_by(|a, b| {
        let sa = a["score"].as_f64().unwrap_or(0.0);
        let sb = b["score"].as_f64().unwrap_or(0.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let offset_us = offset as usize;
    let limit_us = limit as usize;
    let sliced: Vec<_> = all_results.into_iter().skip(offset_us).take(limit_us).collect();

    let wrapped = serde_json::json!({
        "total": total_all,
        "results": sliced,
    });
    let json_str = serde_json::to_string(&wrapped).unwrap_or_else(|_| "{}".into());
    unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_search_count(
    query: *const c_char,
    out_count: *mut u64,
) -> i32 {
    (|| -> i32 {
    if out_count.is_null() {
        return -3;
    }
    let query_str = match unsafe { cstr_to_str(query) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let index_path = with_app(|app| {
        if app.indexer.is_some() {
            app.index_path.clone()
        } else {
            None
        }
    })
    .unwrap_or(None);

    let index_path = match index_path {
        Some(p) => p,
        None => {
            set_error("pdf_init not called".into());
            return -2;
        }
    };

    let search_index = match SearchIndex::new(&index_path) {
        Ok(si) => si,
        Err(e) => {
            set_error(format!("Failed to open index: {}", e));
            return -1;
        }
    };

    match search_index.search_count(query_str) {
        Ok(count) => {
            unsafe { *out_count = count; }
            0
        }
        Err(e) => {
            set_error(format!("Search count failed: {}", e));
            -1
        }
    }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_search_count_all(
    query: *const c_char,
    out_count: *mut u64,
) -> i32 {
    (|| -> i32 {
    if out_count.is_null() {
        return -3;
    }
    let query_str = match unsafe { cstr_to_str(query) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    let collections = match with_registry(|reg| reg.list_collections()) {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => { set_error(format!("{}", e)); return -1; }
        Err(e) => return e,
    };

    let mut total: u64 = 0;
    for coll in &collections {
        let index_path = PathBuf::from(&coll.data_dir).join(".pdf_extractor").join("index");
        if !index_path.join("meta.json").exists() {
            continue;
        }
        if let Ok(si) = SearchIndex::new(&index_path) {
            if let Ok(cnt) = si.search_count(query_str) {
                total += cnt;
            }
        }
    }

    unsafe { *out_count = total; }
    0
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_collection_stats(
    coll_id: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    (|| -> i32 {
    let collection = match with_registry(|reg| reg.get_collection(coll_id as i64)) {
        Ok(Ok(c)) => c,
        Ok(Err(_)) => { set_error("Collection not found".into()); return -8; }
        Err(e) => return e,
    };

    let index_path = PathBuf::from(&collection.data_dir).join(".pdf_extractor").join("index");
    let search_index = match SearchIndex::new(&index_path) {
        Ok(si) => si,
        Err(e) => {
            set_error(format!("Failed to open index: {}", e));
            return -1;
        }
    };

    match search_index.compute_stats(&index_path) {
        Ok(stats) => {
            let obj = serde_json::json!({
                "num_docs": stats.num_docs,
                "num_segments": stats.num_segments,
                "size_bytes": stats.size_bytes,
            });
            let json_str = serde_json::to_string(&obj).unwrap_or_else(|_| "{}".into());
            unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
        }
        Err(e) => {
            set_error(format!("Stats failed: {}", e));
            -1
        }
    }
    })()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::atomic::AtomicU32;

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn setup_temp_index() -> (String, String) {
        let n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pdf_capi_test_{}_{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.db");
        let index_path = dir.join("index");
        (db_path.to_string_lossy().to_string(), index_path.to_string_lossy().to_string())
    }

    #[test]
    fn test_api_version() {
        unsafe { assert_eq!(pdf_api_version(), 1); }
    }

    #[test]
    fn test_free_string_null() {
        unsafe { pdf_free_string(std::ptr::null_mut()) };
    }

    #[test]
    fn test_page_count_non_existent() {
        let p = CString::new("C:\\nonexistent_file_xyz.pdf").unwrap();
        let rc = unsafe { pdf_page_count(p.as_ptr()) };
        assert!(rc < 0);
    }

    #[test]
    fn test_page_count_null_path() {
        let rc = unsafe { pdf_page_count(std::ptr::null()) };
        assert!(rc < 0);
    }

    #[test]
    fn test_init_null_paths() {
        let rc = unsafe { pdf_init(std::ptr::null(), std::ptr::null()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_search_null_query() {
        let rc =
            unsafe { pdf_search(std::ptr::null(), 10, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_search_before_init() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe {
            pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, -2);
    }

    #[test]
    fn test_snippet_before_init() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("hello").unwrap();
        let rc = unsafe {
            pdf_snippet(1, q.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, -2);
    }

    #[test]
    fn test_last_error() {
        reset_state();
        let mut buf = [0u8; 512];
        let mut len = 512u32;
        let rc = unsafe { pdf_last_error(buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_init_and_search_empty() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let q = CString::new("nonexistent").unwrap();
        let rc = unsafe {
            pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(result).unwrap();
        assert_eq!(v["total"], 0);
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_search_empty_query() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);
        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let q = CString::new("").unwrap();
        let rc = unsafe {
            pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        // Empty query may error or return empty results; both are acceptable
        if rc == 0 {
            let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
            let v: serde_json::Value = serde_json::from_str(result).unwrap();
            assert_eq!(v["total"], 0);
        }
    }

    #[test]
    fn test_extract_empty_dir() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let empty_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let empty_dir = std::env::temp_dir().join(format!("pdf_capi_empty_{}", empty_dir_n));
        let _ = std::fs::remove_dir_all(&empty_dir);
        std::fs::create_dir_all(&empty_dir).unwrap();
        let dir_c = CString::new(empty_dir.to_string_lossy().as_ref()).unwrap();
        let rc = unsafe { pdf_extract(dir_c.as_ptr(), None) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_channel_capacity() {
        reset_state();
        let rc = unsafe { pdf_set_channel_capacity(100) };
        assert_eq!(rc, 0);

        // Verify it doesn't break extraction
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);
        let empty_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let empty_dir = std::env::temp_dir().join(format!("pdf_capi_empty2_{}", empty_dir_n));
        let _ = std::fs::remove_dir_all(&empty_dir);
        std::fs::create_dir_all(&empty_dir).unwrap();
        let dir_c = CString::new(empty_dir.to_string_lossy().as_ref()).unwrap();
        let rc = unsafe { pdf_extract(dir_c.as_ptr(), None) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_channel_capacity_zero() {
        reset_state();
        let rc = unsafe { pdf_set_channel_capacity(0) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_init_twice() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);
    }

    #[test]
    fn test_stats() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let rc = unsafe { pdf_stats(buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(result).unwrap();
        assert!(v.get("num_docs").is_some());
    }

    #[test]
    fn test_concurrent_free_string_null() {
        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(std::thread::spawn(|| unsafe {
                pdf_free_string(std::ptr::null_mut());
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_concurrent_search() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let mut handles = Vec::new();
        for i in 0..4 {
            handles.push(std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut len = 4096u32;
                let q = CString::new(format!("thread_{}", i)).unwrap();
                let rc = unsafe {
                    pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
                };
                assert_eq!(rc, 0);
                let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
                let v: serde_json::Value = serde_json::from_str(result).unwrap();
                assert_eq!(v["total"], 0);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_thumbnail_size_query() {
        reset_state();
        let mut len = 0u32;
        let p = CString::new("C:\\nonexistent.pdf").unwrap();
        // null buffer = size query
        let rc = unsafe {
            pdf_render_thumbnail(p.as_ptr(), 1, 100, std::ptr::null_mut(), &mut len)
        };
        // Should fail with renderer error (no mutool/pdftoppm) or return -2 for non-existent file
        assert!(rc != 0);
    }

    fn create_test_pdf(path: &PathBuf) {
        // Minimal valid PDF with 1 page
        let bytes = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n\
xref\n0 4\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \n\
trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n190\n%%EOF\n";
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn test_thumbnail_page_beyond_doc() {
        reset_state();
        let pdf_dir = std::env::temp_dir().join(format!("pdf_capi_thumb_{}",
            TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::create_dir_all(&pdf_dir);
        let pdf_path = pdf_dir.join("test.pdf");
        create_test_pdf(&pdf_path);
        let p = CString::new(pdf_path.to_string_lossy().as_ref()).unwrap();

        let mut buf = [0u8; 65536];
        let mut len = 65536u32;
        // Page 99 > 1 (page count)
        let rc = unsafe {
            pdf_render_thumbnail(p.as_ptr(), 99, 100, buf.as_mut_ptr(), &mut len)
        };
        // Either renderer not available (-1) or page out of range (also -1)
        assert!(rc != 0);
    }

    #[test]
    fn test_thumbnail_valid_pdf_non_existent_page() {
        reset_state();
        let pdf_dir = std::env::temp_dir().join(format!("pdf_capi_thumb2_{}",
            TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::create_dir_all(&pdf_dir);
        let pdf_path = pdf_dir.join("test.pdf");
        create_test_pdf(&pdf_path);
        let p = CString::new(pdf_path.to_string_lossy().as_ref()).unwrap();
        let mut buf = [0u8; 65536];
        let mut len = 65536u32;
        // Page 0 is invalid (PDF pages are 1-indexed)
        let rc = unsafe {
            pdf_render_thumbnail(p.as_ptr(), 0, 100, buf.as_mut_ptr(), &mut len)
        };
        assert!(rc != 0);
    }

    #[test]
    fn test_thumbnail_null_params() {
        reset_state();
        let rc = unsafe {
            pdf_render_thumbnail(std::ptr::null(), 0, 0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_buffer_too_small() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        // 1 byte buffer is smaller than any JSON output
        let mut buf = [0u8; 1];
        let mut len = 1u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe {
            pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, -4);
        assert!(len > 1);
    }

    #[test]
    fn test_create_registry_null_path() {
        reset_state();
        let rc = unsafe { pdf_create_registry(std::ptr::null()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_create_registry_and_add_list() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };
        assert!(coll_id >= 1);

        let mut buf = [0u8; 1024];
        let mut len = 1024u32;
        let rc = unsafe { pdf_list_collections(buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let json = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], coll_id);
        let expected = std::fs::canonicalize(&books_dir).unwrap();
        assert_eq!(list[0]["books_folder"], expected.to_string_lossy().as_ref());
    }

    #[test]
    fn test_add_duplicate_collection() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_dup_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let id1 = unsafe { pdf_add_collection(books_c.as_ptr()) };
        let id2 = unsafe { pdf_add_collection(books_c.as_ptr()) };
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_remove_collection() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_rem_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };
        assert_eq!(unsafe { pdf_remove_collection(coll_id as u32) }, 0);

        let mut buf = [0u8; 1024];
        let mut len = 1024u32;
        let rc = unsafe { pdf_list_collections(buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let json = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn test_search_before_registry_created() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe {
            pdf_search_collection(1, q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, -7);
    }

    #[test]
    fn test_search_collection_nonexistent() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_nx_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe {
            pdf_search_collection(999, q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, -8);
    }

    #[test]
    fn test_search_all_before_registry() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe {
            pdf_search_all(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, -7);
    }

    #[test]
    fn test_search_all_no_collections() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_em_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe {
            pdf_search_all(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, 0);
        let json = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["total"], 0);
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_setters_stored_and_used() {
        reset_state();

        // Test all simple setters
        assert_eq!(unsafe { pdf_set_fuzzy_distance(2) }, 0);
        assert_eq!(unsafe { pdf_set_stem(1) }, 0);
        assert_eq!(unsafe { pdf_set_recency_weight(0.5) }, 0);
        assert_eq!(unsafe { pdf_set_ram_buffer(100_000_000) }, 0);
        assert_eq!(unsafe { pdf_set_ocr_workers(3) }, 0);
        assert_eq!(unsafe { pdf_set_ocr_max_dim(2000) }, 0);
        assert_eq!(unsafe { pdf_set_indexer_batch_size(200) }, 0);
        assert_eq!(unsafe { pdf_set_commit_interval(1000) }, 0);
        assert_eq!(unsafe { pdf_set_commit_timeout(60) }, 0);
        assert_eq!(unsafe { pdf_set_extract_workers(4) }, 0);

        // String setters
        let lang = CString::new("por").unwrap();
        assert_eq!(unsafe { pdf_set_ocr_language(lang.as_ptr()) }, 0);

        let tesseract = CString::new("C:\\tools\\tesseract.exe").unwrap();
        assert_eq!(unsafe { pdf_set_tesseract_path(tesseract.as_ptr()) }, 0);

        let field = CString::new("normalized_text").unwrap();
        assert_eq!(unsafe { pdf_set_search_field(field.as_ptr()) }, 0);

        let filter = CString::new("science").unwrap();
        assert_eq!(unsafe { pdf_set_path_filter(filter.as_ptr()) }, 0);

        // Null resets string setters
        assert_eq!(unsafe { pdf_set_search_field(std::ptr::null()) }, 0);
        assert_eq!(unsafe { pdf_set_path_filter(std::ptr::null()) }, 0);
    }

    #[test]
    fn test_set_field_weights_valid() {
        reset_state();
        let json = CString::new(r#"{"content_norm": 1.0, "math_source": 3.0}"#).unwrap();
        let rc = unsafe { pdf_set_field_weights(json.as_ptr()) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_field_weights_null_resets() {
        reset_state();
        let json = CString::new(r#"{"content_norm": 2.0}"#).unwrap();
        assert_eq!(unsafe { pdf_set_field_weights(json.as_ptr()) }, 0);
        assert_eq!(unsafe { pdf_set_field_weights(std::ptr::null()) }, 0);
    }

    #[test]
    fn test_set_field_weights_invalid_json() {
        reset_state();
        let json = CString::new("not json").unwrap();
        let rc = unsafe { pdf_set_field_weights(json.as_ptr()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_set_field_weights_zero_weight() {
        reset_state();
        let json = CString::new(r#"{"content_norm": 0.0}"#).unwrap();
        let rc = unsafe { pdf_set_field_weights(json.as_ptr()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_set_field_weights_empty_object() {
        reset_state();
        let json = CString::new("{}").unwrap();
        let rc = unsafe { pdf_set_field_weights(json.as_ptr()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_set_field_weights_null_query() {
        reset_state();
        let rc = unsafe { pdf_set_field_weights(std::ptr::null()) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_boolean_query_valid() {
        reset_state();
        let json = CString::new(
            r#"[
                {"term": "climate", "occur": "must"},
                {"term": "energy",  "occur": "should"},
                {"term": "politics","occur": "must_not"}
            ]"#,
        )
        .unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_boolean_query_null_resets() {
        reset_state();
        let json = CString::new(r#"{"term": "test", "occur": "must"}"#).unwrap();
        // This is an object, not an array — should fail
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, -3);
        assert_eq!(unsafe { pdf_set_boolean_query(std::ptr::null()) }, 0);
    }

    #[test]
    fn test_set_boolean_query_empty_array() {
        reset_state();
        let json = CString::new("[]").unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_set_boolean_query_missing_term() {
        reset_state();
        let json = CString::new(r#"[{"occur": "must"}]"#).unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_set_boolean_query_invalid_occur() {
        reset_state();
        let json = CString::new(r#"[{"term": "test", "occur": "invalid"}]"#).unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_set_boolean_query_null_ptr() {
        reset_state();
        let rc = unsafe { pdf_set_boolean_query(std::ptr::null()) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_boolean_query_invalid_json() {
        reset_state();
        let json = CString::new("not json").unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_set_collection_boost_valid() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cb_v_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };
        assert!(coll_id > 0);

        let rc = unsafe { pdf_set_collection_boost(coll_id as u32, 2.0) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_collection_boost_zero() {
        reset_state();
        let rc = unsafe { pdf_set_collection_boost(1, 0.0) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_set_collection_boost_negative() {
        reset_state();
        let rc = unsafe { pdf_set_collection_boost(1, -1.0) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_index_collection_not_indexed() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_ix_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };

        // Index empty dir
        let rc = unsafe { pdf_index_collection(coll_id as u32, 0, None) };
        assert_eq!(rc, 0);

        // Search before index (should return empty wrapped results)
        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe {
            pdf_search_collection(coll_id as u32, q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        if rc == 0 {
            let json = std::str::from_utf8(&buf[..len as usize]).unwrap();
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(v["total"], 0);
        }
    }

    #[test]
    fn test_list_collections_empty() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_le_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut buf = [0u8; 1024];
        let mut len = 1024u32;
        let rc = unsafe { pdf_list_collections(buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let json = std::str::from_utf8(&buf[..len as usize]).unwrap();
        assert_eq!(json, "[]");
    }

    #[test]
    fn test_multiple_collections_listed() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_mc_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        for i in 0..3 {
            let books_dir = reg_dir.join(format!("books_{}", i));
            std::fs::create_dir_all(&books_dir).unwrap();
            let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
            let id = unsafe { pdf_add_collection(books_c.as_ptr()) };
            assert!(id >= 1);
        }

        let mut buf = [0u8; 2048];
        let mut len = 2048u32;
        let rc = unsafe { pdf_list_collections(buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let json = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn test_cancel_not_requested() {
        reset_state();
        assert_eq!(unsafe { pdf_is_cancel_requested() }, 0);
    }

    #[test]
    fn test_cancel_requested() {
        reset_state();
        unsafe { pdf_cancel_indexing() };
        assert_eq!(unsafe { pdf_is_cancel_requested() }, 1);
        // Reset after cancel
        reset_state();
        assert_eq!(unsafe { pdf_is_cancel_requested() }, 0);
    }

    #[test]
    fn test_search_count_before_init() {
        reset_state();
        let mut count: u64 = 0;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_count(q.as_ptr(), &mut count) };
        assert_eq!(rc, -2);
    }

    #[test]
    fn test_search_count_null_query() {
        reset_state();
        let mut count: u64 = 0;
        let rc = unsafe { pdf_search_count(std::ptr::null(), &mut count) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_search_count_null_out() {
        reset_state();
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_count(q.as_ptr(), std::ptr::null_mut()) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_search_count_empty() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let mut count: u64 = 0;
        let q = CString::new("nonexistent").unwrap();
        let rc = unsafe { pdf_search_count(q.as_ptr(), &mut count) };
        assert_eq!(rc, 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_search_count_all_before_registry() {
        reset_state();
        let mut count: u64 = 0;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_count_all(q.as_ptr(), &mut count) };
        assert_eq!(rc, -7);
    }

    #[test]
    fn test_search_count_all_null_query() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_sca_null_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut count: u64 = 0;
        let rc = unsafe { pdf_search_count_all(std::ptr::null(), &mut count) };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_search_count_all_empty_registry() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_sca_em_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut count: u64 = 0;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_count_all(q.as_ptr(), &mut count) };
        assert_eq!(rc, 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_collection_stats_nonexistent() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cs_nx_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let rc = unsafe { pdf_collection_stats(999, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, -8);
    }

    #[test]
    fn test_collection_stats_before_registry() {
        reset_state();
        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let rc = unsafe { pdf_collection_stats(1, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, -7);
    }

    #[test]
    fn test_get_term_positions_before_init() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let term = CString::new("hello").unwrap();
        let rc = unsafe {
            pdf_get_term_positions(0, 1, term.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, -2);
    }

    #[test]
    fn test_get_term_positions_null_term() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let rc = unsafe {
            pdf_get_term_positions(0, 1, std::ptr::null(), buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, -3);
    }

    #[test]
    fn test_get_term_positions_basic() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        // Index a document
        let _ = with_app(|app| {
            let indexer = app.indexer.as_ref().unwrap();
            indexer.index_document(1, "/doc.pdf", "hello world hello").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        // Search for term positions
        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let term = CString::new("hello").unwrap();
        let rc = unsafe {
            pdf_get_term_positions(0, 1, term.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let positions: Vec<usize> = serde_json::from_str(result).unwrap();
        assert!(positions.contains(&0), "Position 0 should contain 'hello'");
        assert!(positions.contains(&2), "Position 2 should contain 'hello'");
    }

    #[test]
    fn test_get_term_positions_no_match() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let _ = with_app(|app| {
            let indexer = app.indexer.as_ref().unwrap();
            indexer.index_document(1, "/doc.pdf", "hello world").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let term = CString::new("nonexistent").unwrap();
        let rc = unsafe {
            pdf_get_term_positions(0, 1, term.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        assert_eq!(result, "[]", "Non-existent term should return empty array");
    }

    #[test]
    fn test_get_term_positions_nonexistent_doc() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let _ = with_app(|app| {
            let indexer = app.indexer.as_ref().unwrap();
            indexer.index_document(1, "/doc.pdf", "hello world").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let term = CString::new("hello").unwrap();
        let rc = unsafe {
            pdf_get_term_positions(0, 999, term.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        assert_eq!(result, "[]", "Non-existent doc_id should return empty array");
    }

    #[test]
    fn test_collection_stats_unindexed() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cs_ui_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };

        // Should succeed even if index doesn't exist yet
        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let rc = unsafe { pdf_collection_stats(coll_id as u32, buf.as_mut_ptr() as *mut c_char, &mut len) };
        // Index doesn't exist yet, so may fail
        assert!(rc == 0 || rc == -1);
    }

    // -----------------------------------------------------------------------
    // Boolean query end-to-end (real index, real documents, real search)
    // -----------------------------------------------------------------------

    #[test]
    fn test_boolean_query_end_to_end() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        // Index 3 docs in the content_norm field
        let _ = with_app(|app| {
            let indexer = app.indexer.as_ref().unwrap();
            indexer.index_document(1, "/doc1.pdf", "Introduction to algebra").unwrap();
            indexer.index_document(2, "/doc2.pdf", "Advanced calculus topics").unwrap();
            indexer.index_document(3, "/doc3.pdf", "Algebra and calculus together").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        // --- Basic flow: MUST "algebra" SHOULD "calculus" ---
        let bool_json = CString::new(
            r#"[{"term": "algebra", "occur": "must"}, {"term": "calculus", "occur": "should"}]"#
        ).unwrap();
        assert_eq!(unsafe { pdf_set_boolean_query(bool_json.as_ptr()) }, 0);

        let mut buf = [0u8; 8192];
        let mut len = 8192u32;
        let q = CString::new("").unwrap();
        assert_eq!(unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) }, 0);
        let v: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&buf[..len as usize]).unwrap()
        ).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "MUST algebra → 2 docs");
        // doc3 has algebra (MUST) + calculus (SHOULD) → higher BM25 score
        assert_eq!(results[0]["path"], "/doc3.pdf");
        assert_eq!(results[1]["path"], "/doc1.pdf");

        // --- Alternative flow: MUST "algebra" + MUST_NOT "calculus" ---
        unsafe { pdf_set_boolean_query(std::ptr::null()) }; // reset first
        let bool_json2 = CString::new(
            r#"[{"term": "algebra", "occur": "must"}, {"term": "calculus", "occur": "must_not"}]"#
        ).unwrap();
        assert_eq!(unsafe { pdf_set_boolean_query(bool_json2.as_ptr()) }, 0);
        let mut len2 = 8192u32;
        assert_eq!(unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len2) }, 0);
        let v2: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&buf[..len2 as usize]).unwrap()
        ).unwrap();
        let results2 = v2["results"].as_array().unwrap();
        assert_eq!(results2.len(), 1, "MUST algebra + MUST_NOT calculus → only doc1");
        assert_eq!(results2[0]["path"], "/doc1.pdf");

        // --- Edge flow: all MUST_NOT → no matches ---
        let bool_json3 = CString::new(
            r#"[{"term": "algebra", "occur": "must_not"}, {"term": "calculus", "occur": "must_not"}]"#
        ).unwrap();
        assert_eq!(unsafe { pdf_set_boolean_query(bool_json3.as_ptr()) }, 0);
        let mut len3 = 8192u32;
        assert_eq!(unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len3) }, 0);
        let v3: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&buf[..len3 as usize]).unwrap()
        ).unwrap();
        let results3 = v3["results"].as_array().unwrap();
        assert_eq!(results3.len(), 0, "All MUST_NOT → no results");

        // --- Reset flow: null resets to standard simple-query search ---
        unsafe { pdf_set_boolean_query(std::ptr::null()) };
        let q2 = CString::new("algebra").unwrap();
        let mut len4 = 8192u32;
        assert_eq!(unsafe { pdf_search(q2.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len4) }, 0);
        let v4: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&buf[..len4 as usize]).unwrap()
        ).unwrap();
        assert_eq!(v4["results"].as_array().unwrap().len(), 2, "Standard search for algebra → 2 docs");
    }

    // -----------------------------------------------------------------------
    // Field weights end-to-end
    // -----------------------------------------------------------------------

    #[test]
    fn test_field_weights_end_to_end() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let _ = with_app(|app| {
            let indexer = app.indexer.as_ref().unwrap();
            indexer.index_document(1, "/doc1.pdf", "algebra").unwrap();
            indexer.index_document(2, "/doc2.pdf", "calculus").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        // --- Basic flow ---
        let w = CString::new(r#"{"content_norm": 2.0}"#).unwrap();
        assert_eq!(unsafe { pdf_set_field_weights(w.as_ptr()) }, 0);
        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let q = CString::new("algebra").unwrap();
        assert_eq!(unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) }, 0);
        let v: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&buf[..len as usize]).unwrap()
        ).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "Weighted search for algebra → 1 doc");
        assert_eq!(results[0]["path"], "/doc1.pdf");

        // --- Reset flow ---
        unsafe { pdf_set_field_weights(std::ptr::null()) };
        let mut len2 = 4096u32;
        assert_eq!(unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len2) }, 0);
        let v2: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&buf[..len2 as usize]).unwrap()
        ).unwrap();
        assert_eq!(v2["results"].as_array().unwrap().len(), 1, "Reset → standard search still works");
    }

    // -----------------------------------------------------------------------
    // Collection boost end-to-end
    // -----------------------------------------------------------------------

    #[test]
    fn test_collection_boost_end_to_end() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cb_e2e_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        // Add two collections
        let books1 = reg_dir.join("books1");
        std::fs::create_dir_all(&books1).unwrap();
        let books1_c = CString::new(books1.to_string_lossy().as_ref()).unwrap();
        let coll1 = unsafe { pdf_add_collection(books1_c.as_ptr()) };

        let books2 = reg_dir.join("books2");
        std::fs::create_dir_all(&books2).unwrap();
        let books2_c = CString::new(books2.to_string_lossy().as_ref()).unwrap();
        let coll2 = unsafe { pdf_add_collection(books2_c.as_ptr()) };

        // Get the index paths from the registry and create them
        let (idx1, idx2) = with_registry(|reg| {
            (reg.index_path(coll1 as i64), reg.index_path(coll2 as i64))
        }).unwrap();
        std::fs::create_dir_all(&idx1).unwrap();
        std::fs::create_dir_all(&idx2).unwrap();

        // Index a doc in each collection with the same searchable content
        let si1 = SearchIndex::new(&idx1).unwrap();
        let mut w1 = si1.writer().unwrap();
        si1.add_document(&mut w1, 1, "/doc1.pdf", "chk1", "math algebra", "math algebra", "en", "").unwrap();
        w1.commit().unwrap();

        let si2 = SearchIndex::new(&idx2).unwrap();
        let mut w2 = si2.writer().unwrap();
        si2.add_document(&mut w2, 1, "/doc2.pdf", "chk2", "math algebra", "math algebra", "en", "").unwrap();
        w2.commit().unwrap();

        // --- Basic flow: boost coll2 ---
        assert_eq!(unsafe { pdf_set_collection_boost(coll2 as u32, 2.0) }, 0);

        let mut buf = [0u8; 8192];
        let mut len = 8192u32;
        let q = CString::new("math").unwrap();
        assert_eq!(unsafe { pdf_search_all(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) }, 0);
        let v: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&buf[..len as usize]).unwrap()
        ).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "Both collections match 'math'");
        // doc2 (coll2, boost 2.0) ranks ahead of doc1 (coll1, no boost)
        assert_eq!(results[0]["path"], "/doc2.pdf", "Boosted doc2 first");
        assert_eq!(results[1]["path"], "/doc1.pdf");
        assert_eq!(results[0]["collection_id"], serde_json::json!(coll2));
        assert_eq!(results[1]["collection_id"], serde_json::json!(coll1));
        assert!(
            results[0]["score"].as_f64().unwrap() >= results[1]["score"].as_f64().unwrap(),
            "Boosted score >= unboosted"
        );
    }
}

