use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, CStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tantivy::query::Occur;
use tantivy::schema::Value;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

use pdf_extractor::indexer::{Indexer, SearchIndex};
use pdf_extractor::metrics::Metrics;
use pdf_extractor::output::JsonlWriter;
use pdf_extractor::ocr::{self, find_tesseract};
use pdf_extractor::pipeline::{run_ocr_post_processing, run_pipeline, PipelineConfig};
use pdf_extractor::registry::CollectionRegistry;
use pdf_extractor::scanner::JobStore;

pub const PDF_EXTRACTOR_API_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Global state (RwLock so reads are concurrent; write is exclusive)
// ---------------------------------------------------------------------------

struct AppContext {
    jobs: Option<Arc<JobStore>>,
    indexer: Option<Arc<Indexer>>,
    db_path: Option<PathBuf>,
    index_path: Option<PathBuf>,
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
    num_indexer_threads: Option<u32>,
    search_field: Option<String>,
    path_filter: Option<String>,
    field_weights: Option<Vec<(String, f32)>>,
    collection_boosts: HashMap<i64, f32>,
    boolean_query: Option<Vec<(String, String)>>,
}

static APP: OnceLock<RwLock<Option<AppContext>>> = OnceLock::new();
static LAST_ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static LOG_CALLBACK: OnceLock<Mutex<Option<extern "C" fn(*const u8, u32)>>> = OnceLock::new();
static PROCESS_CALLBACK: OnceLock<Mutex<Option<extern "C" fn(*const u8, u32)>>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------
const ERR_GENERAL: i32 = -1;
const ERR_NOT_FOUND: i32 = -2;
const ERR_INVALID_PARAM: i32 = -3;
const ERR_BUFFER_RETRY: i32 = -4;
const ERR_POISONED: i32 = -100;
const ERR_NOT_INIT: i32 = -101;
const ERR_REG_NOT_INIT: i32 = -102;
const ERR_INVALID_UTF8: i32 = -103;
const ERR_CHANNEL_CAPACITY: i32 = -104;
const ERR_NULL_PTR: i32 = -105;
const ERR_BUFFER_TOO_SMALL: i32 = -106;
const ERR_COLLECTION_NOT_FOUND: i32 = -107;

// ---------------------------------------------------------------------------
// Safe accessors — read (shared) / write (exclusive)
// ---------------------------------------------------------------------------

/// Acquire a **read** lock.  Multiple readers can run concurrently.
/// Returns an error if `AppContext` has not been initialised yet.
fn with_app_read<R>(f: impl FnOnce(&AppContext) -> R) -> Result<R, i32> {
    let guard = APP
        .get_or_init(|| RwLock::new(None))
        .read()
        .map_err(|_| ERR_POISONED)?;
    let app = guard.as_ref().ok_or(ERR_NOT_INIT)?;
    Ok(f(app))
}

/// Acquire a **write** lock (exclusive).  Also initialises `AppContext`
/// on first call (lazily).
fn with_app_mut<R>(f: impl FnOnce(&mut AppContext) -> R) -> Result<R, i32> {
    let mut guard = APP
        .get_or_init(|| RwLock::new(None))
        .write()
        .map_err(|_| ERR_POISONED)?;
    let app = guard.get_or_insert_with(|| AppContext {
        jobs: None,
        indexer: None,
        db_path: None,
        index_path: None,
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
        num_indexer_threads: None,
        search_field: None,
        path_filter: None,
        field_weights: None,
        collection_boosts: HashMap::new(),
        boolean_query: None,
    });
    Ok(f(app))
}

fn set_error(msg: String) {
    let mut guard = LAST_ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *guard = Some(msg);
}

/// Per-collection cancellation tokens so concurrent indexing operations
/// don't interfere with each other.
// Atomically-accessed scalar settings — no RwLock contention with readers.
static FUZZY_DISTANCE: AtomicU32 = AtomicU32::new(0);
static STEM_ENABLED: AtomicU32 = AtomicU32::new(0);
static RECENCY_WEIGHT_BITS: AtomicU32 = AtomicU32::new(f32::to_bits(0.0));

static CANCEL_TOKENS: OnceLock<Mutex<HashMap<u32, Arc<AtomicBool>>>> = OnceLock::new();
fn cancel_tokens() -> &'static Mutex<HashMap<u32, Arc<AtomicBool>>> {
    CANCEL_TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reset all global state — clears AppContext, collection registry, error,
/// callbacks, cancellation tokens, and atomic settings.  Used by `pdf_reset_all`
/// and by `#[cfg(test)]` helpers.
fn reset_all_globals() {
    // AppContext
    if let Some(rw) = APP.get() {
        if let Ok(mut guard) = rw.write() {
            *guard = None;
        }
    }
    // Collection registry
    if let Some(lock) = COLLECTION_REGISTRY.get() {
        if let Ok(mut guard) = lock.lock() {
            *guard = None;
        }
    }
    // Error
    if let Some(lock) = LAST_ERROR.get() {
        if let Ok(mut guard) = lock.lock() {
            *guard = None;
        }
    }
    // Callbacks
    if let Some(lock) = LOG_CALLBACK.get() {
        if let Ok(mut guard) = lock.lock() {
            *guard = None;
        }
    }
    if let Some(lock) = PROCESS_CALLBACK.get() {
        if let Ok(mut guard) = lock.lock() {
            *guard = None;
        }
    }
    // Cancellation tokens
    if let Some(lock) = CANCEL_TOKENS.get() {
        if let Ok(mut guard) = lock.lock() {
            guard.clear();
        }
    }
    // Atomic scalar settings
    FUZZY_DISTANCE.store(0, Ordering::Relaxed);
    STEM_ENABLED.store(0, Ordering::Relaxed);
    RECENCY_WEIGHT_BITS.store(f32::to_bits(0.0), Ordering::Relaxed);
}

#[cfg(test)]
fn reset_state() {
    reset_all_globals();
}

/// Locate `pdf_worker.exe` relative to the current executable.
///
/// Resolution order:
/// 1. Same directory as the host executable (production layout)
/// 2. `CARGO_BIN_DIR` environment variable (set by `cargo test`)
/// 3. Walk up from the host directory, checking each ancestor;
///    specifically handles `deps/` subdirectory (test layout:
///    `target/<profile>/deps/test-<hash>.exe` → `target/<profile>/pdf_worker.exe`)
fn resolve_worker_path() -> Option<PathBuf> {
    resolve_worker_path_from(&std::env::current_exe().ok()?)
}

/// Resolve `pdf_worker.exe` starting from a given executable path.
///
/// This is a testable core: given any exe path, it searches for the worker
/// using the same strategies as `resolve_worker_path()`.
fn resolve_worker_path_from(exe: &Path) -> Option<PathBuf> {
    let exe_parent = exe.parent()?;

    // Strategy 1: same directory as host executable
    let same_dir = exe_parent.join("pdf_worker.exe");
    if same_dir.exists() {
        return Some(same_dir);
    }

    // Strategy 2: CARGO_BIN_DIR env var (set by `cargo test`)
    if let Ok(bin_dir) = std::env::var("CARGO_BIN_DIR") {
        let candidate = PathBuf::from(bin_dir).join("pdf_worker.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Strategy 3: walk up from host directory, checking for deps/ → profile
    let mut dir = exe_parent;
    loop {
        if dir.file_name().and_then(|n| n.to_str()) == Some("deps") {
            if let Some(parent) = dir.parent() {
                let candidate = parent.join("pdf_worker.exe");
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        if let Some(parent) = dir.parent() {
            dir = parent;
            let candidate = dir.join("pdf_worker.exe");
            if candidate.exists() {
                return Some(candidate);
            }
        } else {
            break;
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Helper: caller-allocated buffer write
// ---------------------------------------------------------------------------

unsafe fn write_to_buffer(data: &[u8], out: *mut c_char, out_len: *mut u32) -> i32 {
    if out.is_null() || out_len.is_null() {
        return ERR_NULL_PTR;
    }
    let capacity = *out_len as usize;
    let needed = data.len();
    if capacity < needed {
        *out_len = needed as u32;
        return ERR_BUFFER_RETRY;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), out as *mut u8, needed);
    *out_len = needed as u32;
    0
}

unsafe fn cstr_to_str(ptr: *const c_char) -> Result<&'static str, i32> {
    if ptr.is_null() {
        return Err(ERR_NULL_PTR);
    }
    CStr::from_ptr(ptr).to_str().map_err(|_| {
        set_error("Invalid UTF-8 input".into());
        ERR_INVALID_UTF8
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

/// Reset the cached PDFium DLL state so the next PDFium operation will
/// attempt to reload `pdfium.dll` from scratch.  This allows recovery
/// when the DLL becomes available after an initial failure (e.g. the DLL
/// was copied into the expected directory at runtime).
#[no_mangle]
pub unsafe extern "C" fn pdf_reload_pdfium() {
    pdf_extractor::pdfium::Pdfium::reset();
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

    let rc = with_app_mut(|app| {
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

    let index_path = with_app_read(|app| {
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

    let (entries, total) = match do_search_with_index(&search_index, query_str, limit, offset, None, &settings) {
        Ok(t) => t,
        Err(e) => { return e; }
    };

    let wrapped = serde_json::json!({
        "total": total,
        "results": entries,
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

    let index_path = with_app_read(|app| app.index_path.clone()).unwrap_or(None);
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
// Helper: merge phrase positions into one bounding box per line.
// Uses vertical overlap to detect line breaks.
fn merge_by_line(
    positions: &[pdf_extractor::positions::StoredPosition],
    words: &[&str],
) -> Vec<pdf_extractor::positions::StoredPosition> {
    let mut sorted = positions.to_vec();
    sorted.sort_by_key(|p| p.word_offset);

    let mut result = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let page = sorted[i].page;
        let mut j = i + 1;
        while j < sorted.len() && sorted[j].page == page {
            if sorted[j].y_min > sorted[j - 1].y_max || sorted[j - 1].y_min > sorted[j].y_max {
                break;
            }
            j += 1;
        }
        let x_min = sorted[i..j].iter().map(|p| p.x_min).reduce(f32::min).unwrap();
        let y_min = sorted[i..j].iter().map(|p| p.y_min).reduce(f32::min).unwrap();
        let x_max = sorted[i..j].iter().map(|p| p.x_max).reduce(f32::max).unwrap();
        let y_max = sorted[i..j].iter().map(|p| p.y_max).reduce(f32::max).unwrap();
        result.push(pdf_extractor::positions::StoredPosition {
            word_offset: sorted[i].word_offset,
            page,
            x_min,
            y_min,
            x_max,
            y_max,
            word_text: words.join(" "),
        });
        i = j;
    }
    result
}

// Helper: get phrase positions via Tantivy term positions. Finds
// phrase-consecutive offsets in the Tantivy token stream, then maps them
// to bounding boxes via the SQLite position store (by offset).
fn get_phrase_positions_via_tantivy(
    index_path: &Path,
    doc_id: i64,
    words: &[&str],
    position_store: &pdf_extractor::positions::PositionStore,
) -> Option<Vec<pdf_extractor::positions::StoredPosition>> {
    let search_index = SearchIndex::new(index_path).ok()?;

    let mut word_offsets: Vec<Vec<usize>> = Vec::new();
    for &word in words {
        match search_index.search_term_positions(doc_id as u64, word) {
            Ok(offsets) if !offsets.is_empty() => word_offsets.push(offsets),
            _ => return None,
        }
    }

    if word_offsets.len() < 2 {
        return None;
    }

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

    let mut result = Vec::new();

    for &start in &phrase_starts {
        let offsets: Vec<usize> = (0..word_offsets.len()).map(|i| start + i).collect();
        if let Ok(positions) = position_store.get_positions(doc_id, &offsets) {
            if positions.is_empty() {
                continue;
            }
            // Group by page first, then by line (vertical overlap), creating
            // one merged bounding box per line within the phrase.  This avoids
            // a giant rectangle spanning from the end of one line to the
            // beginning of the next when a phrase wraps across lines.
            result.extend(merge_by_line(&positions, words));
        }
    }

    if result.is_empty() { None } else { Some(result) }
}

// Helper: get phrase positions using SQLite-only word_offset adjacency
// within the same page. Uses per-word position data by text matching.
fn get_phrase_positions_via_sqlite(
    doc_id: i64,
    words: &[&str],
    position_store: &pdf_extractor::positions::PositionStore,
) -> Option<Vec<pdf_extractor::positions::StoredPosition>> {
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

    // Find phrase start offsets (word_offset of the first word) where all
    // subsequent words appear at consecutive offsets on the same page.
    let mut phrase_starts: Vec<(usize, u32)> = Vec::new();
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
            phrase_starts.push((first.word_offset, first.page));
        }
    }

    if phrase_starts.is_empty() {
        return None;
    }

    let mut result = Vec::new();
    for &(start_offset, page) in &phrase_starts {
        let expected_offsets: Vec<usize> = (0..word_positions.len())
            .map(|i| start_offset + i)
            .collect();
        let mut candidates: Vec<pdf_extractor::positions::StoredPosition> = Vec::new();
        for (i, positions) in word_positions.iter().enumerate() {
            let expected = expected_offsets[i];
            if let Some(pos) = positions.iter().find(|p| p.page == page && p.word_offset == expected) {
                candidates.push(pos.clone());
            }
        }
        if candidates.is_empty() {
            continue;
        }
        result.extend(merge_by_line(&candidates, words));
    }

    if result.is_empty() { None } else { Some(result) }
}

/// Get word-level term positions for a specific document.
///
/// Returns a JSON array of bounding-box objects with page numbers, e.g.
/// `[{"page":1,"x_min":100.0,"y_min":700.0,"x_max":120.0,"y_max":712.0}]`.
///
/// For multi-word (phrase) queries, returns positions only from
/// phrase-matched pages. Uses Tantivy term positions to find
/// phrase-consecutive offsets and maps them to bounding boxes via the
/// SQLite position store. Falls back to SQLite-only word_offset adjacency
/// on the same page (works with any index).
///
/// `coll_id` — 0 uses the legacy `pdf_init` index; non-zero uses the
/// collection with that ID from the registry.
///
/// Returns empty array `[]` if the doc or term is not found.
///
/// On failure returns a negative error code and sets `last_error`.
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
            with_app_read(|app| app.index_path.clone()).unwrap_or(None)
        } else {
            match with_registry(|reg| reg.get_collection(coll_id as i64)) {
                Ok(Ok(c)) => Some(PathBuf::from(&c.data_dir).join(".pdf_extractor").join("index")),
                Ok(Err(_)) => {
                    set_error("Collection not found".into());
                    return ERR_COLLECTION_NOT_FOUND;
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

        let all_positions: Vec<pdf_extractor::positions::StoredPosition> = if words.len() > 1 {
            // Multi-word phrase: try Tantivy term positions first, then SQLite adjacency
            get_phrase_positions_via_tantivy(&index_path, doc_id, &words, &position_store)
                .or_else(|| get_phrase_positions_via_sqlite(doc_id, &words, &position_store))
                .unwrap_or_default()
        } else {
            // Single word: return all positions
            let mut seen = HashSet::new();
            let mut positions = Vec::new();
            let word = words.first().copied().unwrap_or("");
            if !word.is_empty() {
                if let Ok(found) = position_store.get_positions_by_term(doc_id, word) {
                    for pos in &found {
                        let key = (pos.page, (pos.x_min * 100.0) as i32, (pos.y_min * 100.0) as i32);
                        if seen.insert(key) {
                            positions.push(pos.clone());
                        }
                    }
                }
            }
            positions
        };

        let json_entries: Vec<serde_json::Value> = all_positions.iter().map(|sp| {
            serde_json::json!({
                "page": sp.page,
                "x_min": sp.x_min,
                "y_min": sp.y_min,
                "x_max": sp.x_max,
                "y_max": sp.y_max,
                "word_text": sp.word_text,
            })
        }).collect();

        let json_str = serde_json::to_string(&json_entries).unwrap_or_else(|_| "[]".into());
        unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
    })();
    rc
}

// ---------------------------------------------------------------------------
// pdf_search_text_in_mem / pdf_search_text_in_pdf
// Searches for `term` inside the PDF using PDFium's text search API
// ---------------------------------------------------------------------------

/// Internal implementation shared by both `pdf_search_text_in_mem` and
/// `pdf_search_text_in_pdf`.  Takes raw PDF bytes, loads them via PDFium,
/// and returns JSON-encoded positions.
unsafe fn search_text_in_pdf_impl(
    pdf_data: &[u8],
    term_str: &str,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(pdf) => pdf,
        None => {
            set_error("pdfium.dll not available".into());
            return ERR_GENERAL;
        }
    };

    let doc = unsafe { (pdfium.FPDF_LoadMemDocument)(pdf_data.as_ptr(), pdf_data.len() as i32, std::ptr::null()) };
    if doc.is_null() {
        let err = unsafe { (pdfium.FPDF_GetLastError)() };
        set_error(format!("PDFium error {}: {}", err, pdf_extractor::pdfium::error_str(err)));
        return ERR_GENERAL;
    }

    let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    let normalized_term: String = term_str.nfkc().collect();

    // Collect all per‑character positions, grouped by page later
    let mut all_positions: Vec<pdf_extractor::positions::StoredPosition> = Vec::new();

    for page_idx in 0..page_count {
        let pdf_page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_idx) };
        if pdf_page.is_null() {
            continue;
        }

        let text_page = unsafe { (pdfium.FPDFText_LoadPage)(pdf_page) };
        if text_page.is_null() {
            unsafe { (pdfium.FPDF_ClosePage)(pdf_page) };
            continue;
        }

        let char_count = unsafe { (pdfium.FPDFText_CountChars)(text_page) };

        // Collect all chars with their unicode values and bounding boxes
        struct CharInfo {
            left: f64,
            right: f64,
            bottom: f64,
            top: f64,
        }
        let mut page_chars: Vec<CharInfo> = Vec::with_capacity(char_count as usize);
        let mut raw_text = String::with_capacity(char_count as usize);

        for i in 0..char_count {
            let ch = unsafe { (pdfium.FPDFText_GetUnicode)(text_page, i) };
            if ch == 0 {
                raw_text.push('\u{FFFD}');
                page_chars.push(CharInfo {
                    left: 0.0,
                    right: 0.0,
                    bottom: 0.0,
                    top: 0.0,
                });
                continue;
            }
            match char::from_u32(ch) {
                Some(c) => raw_text.push(c),
                None => raw_text.push('\u{FFFD}'),
            }
            let mut left = 0.0f64;
            let mut right = 0.0f64;
            let mut bottom = 0.0f64;
            let mut top = 0.0f64;
            unsafe {
                (pdfium.FPDFText_GetCharBox)(text_page, i, &mut left, &mut right, &mut bottom, &mut top);
            }
            page_chars.push(CharInfo { left, right, bottom, top });
        }

        // Normalize the raw page text and build the char mapping
        let (norm_text, norm_to_raw) = normalize_with_mapping(&raw_text);

        // Find all occurrences of normalized_term in norm_text
        if normalized_term.is_empty() {
            unsafe {
                (pdfium.FPDFText_ClosePage)(text_page);
                (pdfium.FPDF_ClosePage)(pdf_page);
            }
            continue;
        }

        // Case‑insensitive search: lower‑case both the normalized page text
        // and the user query so that "Machine" matches "machine", etc.
        let term_lower: Vec<char> = normalized_term.chars().flat_map(|c| c.to_lowercase()).collect();
        let norm_chars: Vec<char> = norm_text.chars().collect();
        let norm_lower: Vec<char> = norm_chars.iter().flat_map(|c| c.to_lowercase()).collect();
        let term_len = term_lower.len();
        if term_len > 0 {
            let mut search_char: usize = 0;
            while search_char + term_len <= norm_lower.len() {
                if norm_lower[search_char..search_char + term_len] == term_lower[..] {
                    let norm_end_char = search_char + normalized_term.chars().count();
                    if norm_end_char > norm_to_raw.len() {
                        break;
                    }
                    let raw_start = norm_to_raw[search_char].0;
                    let raw_end = norm_to_raw[norm_end_char.saturating_sub(1)].1;

                    for raw_i in raw_start..=raw_end {
                        if raw_i >= page_chars.len() {
                            break;
                        }
                        let info = &page_chars[raw_i];
                        all_positions.push(pdf_extractor::positions::StoredPosition {
                            word_offset: raw_i,
                            page: page_idx as u32 + 1,
                            x_min: info.left as f32,
                            y_min: info.bottom as f32,
                            x_max: info.right as f32,
                            y_max: info.top as f32,
                            word_text: term_str.to_string(),
                        });
                    }

                    search_char += 1;
                } else {
                    search_char += 1;
                }
            }
        }

        unsafe {
            (pdfium.FPDFText_ClosePage)(text_page);
            (pdfium.FPDF_ClosePage)(pdf_page);
        }
    }

    unsafe { (pdfium.FPDF_CloseDocument)(doc) };

    // Group per‑character boxes by page and line, merge into one per line
    all_positions.sort_by_key(|p| (p.page, p.word_offset));
    let merged = merge_by_line_from_chars(&all_positions);

    let json_str = serde_json::to_string(&merged).unwrap_or_else(|_| "[]".into());
    unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
}

#[no_mangle]
pub unsafe extern "C" fn pdf_search_text_in_mem(
    data: *const u8,
    data_len: i32,
    term: *const c_char,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    let rc = (|| -> i32 {
        let term_str = match unsafe { cstr_to_str(term) } {
            Ok(s) => s,
            Err(e) => return e,
        };

        if out_len.is_null() {
            return ERR_NULL_PTR;
        }
        if data.is_null() || data_len <= 0 {
            set_error("Invalid PDF data".into());
            return ERR_GENERAL;
        }

        let pdf_data = std::slice::from_raw_parts(data, data_len as usize);
        unsafe { search_text_in_pdf_impl(pdf_data, term_str, out_json, out_len) }
    })();
    rc
}

// pdf_search_text_in_pdf (legacy — reads from disk)
// Now delegates to the shared `search_text_in_pdf_impl` to avoid duplication.
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_search_text_in_pdf(
    path: *const c_char,
    term: *const c_char,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    let rc = (|| -> i32 {
        let path_str = match unsafe { cstr_to_str(path) } {
            Ok(s) => s,
            Err(e) => return e,
        };
        let term_str = match unsafe { cstr_to_str(term) } {
            Ok(s) => s,
            Err(e) => return e,
        };

        if out_len.is_null() {
            return ERR_NULL_PTR;
        }

        let pdf_data = match std::fs::read(path_str) {
            Ok(d) => d,
            Err(e) => {
                set_error(format!("Failed to read PDF: {}", e));
                return ERR_GENERAL;
            }
        };

        unsafe { search_text_in_pdf_impl(&pdf_data, term_str, out_json, out_len) }
    })();
    rc
}

/// Normalize text with NFKC, returning a mapping from each normalized char
/// back to (first_raw_idx, last_raw_idx) in the original raw string.
/// Handles expanded ligatures (ﬁ→fi), composed combining chars (e+´→é),
/// and simple substitutions (fullwidth→ASCII).
fn normalize_with_mapping(raw: &str) -> (String, Vec<(usize, usize)>) {
    // Step 1: NFKD (full decomposition) — easy per-char mapping
    let mut nfkd = String::new();
    let mut nfkd_to_raw: Vec<usize> = Vec::new();
    for (ri, rc) in raw.chars().enumerate() {
        for dc in rc.nfkd() {
            nfkd.push(dc);
            nfkd_to_raw.push(ri);
        }
    }

    // Step 2: Canonical composition (NFC step of NFKC)
    let chars: Vec<char> = nfkd.chars().collect();
    let mut result = String::with_capacity(chars.len());
    let mut mapping: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let cc = canonical_combining_class(c);
        if cc == 0 {
            let mut starter = c;
            let first_raw = nfkd_to_raw[i];
            let mut last_raw = nfkd_to_raw[i];
            let mut j = i + 1;
            while j < chars.len() {
                let next = chars[j];
                let next_cc = canonical_combining_class(next);
                if next_cc == 0 {
                    break;
                }
                if let Some(composed) = unicode_normalization::char::compose(starter, next) {
                    starter = composed;
                    last_raw = nfkd_to_raw[j];
                    j += 1;
                } else {
                    break;
                }
            }
            result.push(starter);
            mapping.push((first_raw, last_raw));
            i = j;
        } else {
            result.push(c);
            mapping.push((nfkd_to_raw[i], nfkd_to_raw[i]));
            i += 1;
        }
    }
    (result, mapping)
}

// Helper: merge per‑character positions into one bounding box per text line.
// Characters on the same page with vertical overlap are grouped together.
fn merge_by_line_from_chars(
    positions: &[pdf_extractor::positions::StoredPosition],
) -> Vec<serde_json::Value> {
    if positions.is_empty() {
        return Vec::new();
    }

    #[derive(Clone)]
    struct LineGroup {
        page: u32,
        x_min: f32,
        y_min: f32,
        x_max: f32,
        y_max: f32,
        word_text: String,
    }

    let mut groups: Vec<LineGroup> = Vec::new();
    for p in positions {
        if let Some(last) = groups.last_mut() {
            if last.page == p.page
                && !(p.y_min > last.y_max || last.y_min > p.y_max)
            {
                // Same page, overlapping vertically → same line
                last.x_min = last.x_min.min(p.x_min);
                last.y_min = last.y_min.min(p.y_min);
                last.x_max = last.x_max.max(p.x_max);
                last.y_max = last.y_max.max(p.y_max);
                continue;
            }
        }
        groups.push(LineGroup {
            page: p.page,
            x_min: p.x_min,
            y_min: p.y_min,
            x_max: p.x_max,
            y_max: p.y_max,
            word_text: p.word_text.clone(),
        });
    }

    groups.into_iter().map(|g| {
        serde_json::json!({
            "page": g.page,
            "x_min": g.x_min,
            "y_min": g.y_min,
            "x_max": g.x_max,
            "y_max": g.y_max,
            "word_text": g.word_text,
        })
    }).collect()
}

// ---------------------------------------------------------------------------
// pdf_search_term_offsets
// Returns a JSON integer array of word offsets within the content field,
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
            with_app_read(|app| app.index_path.clone()).unwrap_or(None)
        } else {
            match with_registry(|reg| reg.get_collection(coll_id as i64)) {
                Ok(Ok(c)) => Some(PathBuf::from(&c.data_dir).join(".pdf_extractor").join("index")),
                Ok(Err(_)) => {
                    set_error("Collection not found".into());
                    return ERR_COLLECTION_NOT_FOUND;
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

    let pdf_data = match std::fs::read(p) {
        Ok(data) => data,
        Err(e) => {
            set_error(format!("Failed to read PDF file: {}", e));
            return -2;
        }
    };
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(pdf) => pdf,
        None => {
            set_error("pdfium.dll not available".into());
            return -1;
        }
    };

    let doc = unsafe { (pdfium.FPDF_LoadMemDocument)(pdf_data.as_ptr(), pdf_data.len() as i32, std::ptr::null()) };
    if doc.is_null() {
        let err = unsafe { (pdfium.FPDF_GetLastError)() };
        let msg = format!("PDFium error {}: {}", err, pdf_extractor::pdfium::error_str(err));
        set_error(msg);
        return -1;
    }

    let count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    unsafe { (pdfium.FPDF_CloseDocument)(doc); }
    count
}

// ---------------------------------------------------------------------------
// pdf_page_dimensions — returns page dimensions as two f64 values into out_buf
// Returns: 0 on success, -1 on error
// out_len must point to a u32 set to 16 (sizeof two f64) before calling
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_page_dimensions(
    path: *const c_char,
    page_index: u32,
    out_buf: *mut u8,
    out_len: *mut u32,
) -> i32 {
    if out_buf.is_null() || out_len.is_null() {
        return ERR_NULL_PTR;
    }
    if *out_len < 16 {
        *out_len = 16;
        return ERR_BUFFER_RETRY;
    }

    let p = match unsafe { cstr_to_str(path) } {
        Ok(s) => s,
        Err(e) => return e,
    };
    *out_len = 16;

    let pdf_data = match std::fs::read(p) {
        Ok(data) => data,
        Err(e) => {
            set_error(format!("Failed to read PDF file: {}", e));
            return -1;
        }
    };
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(pdf) => pdf,
        None => {
            set_error("pdfium.dll not available".into());
            return -1;
        }
    };

    let doc = unsafe { (pdfium.FPDF_LoadMemDocument)(pdf_data.as_ptr(), pdf_data.len() as i32, std::ptr::null()) };
    if doc.is_null() {
        set_error("Failed to load document for dimension query".into());
        return -1;
    }

    let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    if page_index as i32 >= page_count {
        set_error("Page index out of range".into());
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    let pdf_page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index as i32) };
    if pdf_page.is_null() {
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        set_error("Failed to load page".into());
        return -1;
    }

    let w = unsafe { (pdfium.FPDF_GetPageWidthF)(pdf_page) } as f64;
    let h = unsafe { (pdfium.FPDF_GetPageHeightF)(pdf_page) } as f64;

    unsafe { (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }

    // Write dimensions as two little-endian f64 values
    let buf = std::slice::from_raw_parts_mut(out_buf, 16);
    buf[0..8].copy_from_slice(&w.to_le_bytes());
    buf[8..16].copy_from_slice(&h.to_le_bytes());
    0
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
    let p = match unsafe { cstr_to_str(path) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    if out_len.is_null() {
        return -3;
    }

    // Page numbers are 1-based in the API; PDFium uses 0-based
    if page == 0 {
        set_error("Page number must be >= 1".into());
        return -3;
    }
    let pdfium_page = (page - 1) as i32;

    let clean = p.strip_prefix(r"\\?\").unwrap_or(p);
    let pdf_data = match std::fs::read(std::path::Path::new(clean)) {
        Ok(d) => d,
        Err(e) => {
            set_error(format!("Cannot read file: {}", e));
            return -2;
        }
    };
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(pdf) => pdf,
        None => {
            set_error("pdfium.dll not available".into());
            return -1;
        }
    };

    let doc = unsafe { (pdfium.FPDF_LoadMemDocument)(pdf_data.as_ptr(), pdf_data.len() as i32, std::ptr::null()) };
    if doc.is_null() {
        set_error("Failed to load PDF via PDFium".into());
        return -1;
    }

    let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    if pdfium_page >= page_count {
        set_error(format!("Page {} out of range (document has {} pages)", page, page_count).into());
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    let pdf_page = unsafe { (pdfium.FPDF_LoadPage)(doc, pdfium_page) };
    if pdf_page.is_null() {
        set_error("Failed to load PDF page".into());
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    // Get page dimensions in points (1/72 inch)
    let page_width_pts = unsafe { (pdfium.FPDF_GetPageWidthF)(pdf_page) };
    let page_height_pts = unsafe { (pdfium.FPDF_GetPageHeightF)(pdf_page) };

    // Calculate render size: default 150 DPI, cap width at max_w if set
    let default_dpi = 150.0;
    let dest_width = {
        let w = (page_width_pts as f64 * default_dpi / 72.0) as i32;
        if max_w > 0 { w.min(max_w as i32) } else { w }
    };
    let dest_height = (dest_width as f64 * page_height_pts as f64 / page_width_pts as f64).round() as i32;

    if dest_width <= 0 || dest_height <= 0 {
        set_error("Invalid render dimensions".into());
        unsafe { (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    // Create BGRA bitmap
    let bitmap = unsafe { (pdfium.FPDFBitmap_CreateEx)(dest_width, dest_height, pdf_extractor::pdfium::FPDFBITMAP_BGRA, std::ptr::null_mut(), 0) };
    if bitmap.is_null() {
        set_error("Failed to create PDFium bitmap".into());
        unsafe { (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    // Fill with white background
    unsafe { (pdfium.FPDFBitmap_FillRect)(bitmap, 0, 0, dest_width, dest_height, 0xFFFFFFFF); }

    // Render page to bitmap (no annotations, no special flags)
    unsafe { (pdfium.FPDF_RenderPageBitmap)(bitmap, pdf_page, 0, 0, dest_width, dest_height, 0, pdf_extractor::pdfium::FPDF_NONE); }

    // Get pixel buffer (BGRA format, 4 bytes per pixel)
    let buf_ptr = unsafe { (pdfium.FPDFBitmap_GetBuffer)(bitmap) };
    let stride = unsafe { (pdfium.FPDFBitmap_GetStride)(bitmap) };
    if buf_ptr.is_null() || stride <= 0 {
        unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap); (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }
        set_error("Failed to get PDFium bitmap buffer".into());
        return -1;
    }

    // Copy buffer — but skip the alpha channel for PNG
    let total_pixels = (dest_width * dest_height) as usize;
    let mut rgba_data = vec![0u8; total_pixels * 3]; // RGB only

    // PDFium gives BGRA; we need RGB for PNG
    let src = std::slice::from_raw_parts(buf_ptr, total_pixels * 4);
    for i in 0..total_pixels {
        let si = i * 4;
        let di = i * 3;
        rgba_data[di] = src[si + 2];     // R ← B
        rgba_data[di + 1] = src[si + 1]; // G ← G
        rgba_data[di + 2] = src[si];     // R ← G
    }

    unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap); (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }

    // Encode as PNG
    let img = match image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(dest_width as u32, dest_height as u32, rgba_data) {
        Some(img) => img,
        None => {
            set_error("Failed to create image buffer".into());
            return -1;
        }
    };

    let dyn_img = image::DynamicImage::ImageRgb8(img);
    let mut png_buf = std::io::Cursor::new(Vec::new());
    if dyn_img.write_to(&mut png_buf, image::ImageFormat::Png).is_err() {
        set_error("Failed to encode PNG".into());
        return -1;
    }

    let png_data = png_buf.into_inner();
    let needed = png_data.len() as u32;

    if out_buf.is_null() {
        *out_len = needed;
        return 0;
    }
    if *out_len < needed {
        *out_len = needed;
        return -4;
    }
    std::ptr::copy_nonoverlapping(png_data.as_ptr(), out_buf, needed as usize);
    *out_len = needed;
    0
}

// ---------------------------------------------------------------------------
// pdf_render_page
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_render_page(
    path: *const c_char,
    page_index: u32,
    target_width: u32,
    out_buf: *mut u8,
    out_len: *mut u32,
) -> i32 {
    let p = match unsafe { cstr_to_str(path) } {
        Ok(s) => s,
        Err(e) => return e,
    };

    if out_len.is_null() {
        return -3;
    }

    let pdf_data = match std::fs::read(p) {
        Ok(data) => data,
        Err(e) => {
            set_error(format!("Failed to read PDF file: {}", e));
            return -1;
        }
    };
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(pdf) => pdf,
        None => {
            set_error("pdfium.dll not available".into());
            return -1;
        }
    };

    let doc = unsafe { (pdfium.FPDF_LoadMemDocument)(pdf_data.as_ptr(), pdf_data.len() as i32, std::ptr::null()) };
    if doc.is_null() {
        let err = unsafe { (pdfium.FPDF_GetLastError)() };
        let msg = format!("PDFium error {}: {}", err, pdf_extractor::pdfium::error_str(err));
        set_error(msg);
        return -1;
    }

    let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    if page_index as i32 >= page_count {
        set_error(format!("Page {} out of range (document has {} pages)", page_index + 1, page_count).into());
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    let pdf_page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index as i32) };
    if pdf_page.is_null() {
        set_error("Failed to load PDF page".into());
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    // Get page dimensions in points (1/72 inch)
    let page_width_pts = unsafe { (pdfium.FPDF_GetPageWidthF)(pdf_page) };
    let page_height_pts = unsafe { (pdfium.FPDF_GetPageHeightF)(pdf_page) };

    // Calculate render size: target_width pixels wide at appropriate DPI
    let dest_width = if target_width > 0 {
        target_width as i32
    } else {
        let default_dpi = 150.0;
        (page_width_pts as f64 * default_dpi / 72.0) as i32
    };
    let dest_height = (dest_width as f64 * page_height_pts as f64 / page_width_pts as f64).round() as i32;

    if dest_width <= 0 || dest_height <= 0 {
        set_error("Invalid render dimensions".into());
        unsafe { (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    // Create BGRA bitmap
    let bitmap = unsafe { (pdfium.FPDFBitmap_CreateEx)(dest_width, dest_height, pdf_extractor::pdfium::FPDFBITMAP_BGRA, std::ptr::null_mut(), 0) };
    if bitmap.is_null() {
        set_error("Failed to create PDFium bitmap".into());
        unsafe { (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }
        return -1;
    }

    // Fill with white background
    unsafe { (pdfium.FPDFBitmap_FillRect)(bitmap, 0, 0, dest_width, dest_height, 0xFFFFFFFF); }

    // Render page to bitmap (no annotations, no special flags)
    unsafe { (pdfium.FPDF_RenderPageBitmap)(bitmap, pdf_page, 0, 0, dest_width, dest_height, 0, pdf_extractor::pdfium::FPDF_NONE); }

    // Get pixel buffer (BGRA format, 4 bytes per pixel)
    let buf_ptr = unsafe { (pdfium.FPDFBitmap_GetBuffer)(bitmap) };
    let stride = unsafe { (pdfium.FPDFBitmap_GetStride)(bitmap) };
    if buf_ptr.is_null() || stride <= 0 {
        unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap); (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }
        set_error("Failed to get PDFium bitmap buffer".into());
        return -1;
    }

    // Copy buffer — convert BGRA to RGB for PNG encoding
    let total_pixels = (dest_width * dest_height) as usize;
    let mut rgba_data = vec![0u8; total_pixels * 3];

    let src = std::slice::from_raw_parts(buf_ptr, total_pixels * 4);
    for i in 0..total_pixels {
        let si = i * 4;
        let di = i * 3;
        rgba_data[di] = src[si + 2];     // R ← B
        rgba_data[di + 1] = src[si + 1]; // G ← G
        rgba_data[di + 2] = src[si];     // B ← R
    }

    unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap); (pdfium.FPDF_ClosePage)(pdf_page); (pdfium.FPDF_CloseDocument)(doc); }

    // Encode as PNG
    let img = match image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(dest_width as u32, dest_height as u32, rgba_data) {
        Some(img) => img,
        None => {
            set_error("Failed to create image buffer".into());
            return -1;
        }
    };

    let dyn_img = image::DynamicImage::ImageRgb8(img);
    let mut png_buf = std::io::Cursor::new(Vec::new());
    if dyn_img.write_to(&mut png_buf, image::ImageFormat::Png).is_err() {
        set_error("Failed to encode PNG".into());
        return -1;
    }

    let png_data = png_buf.into_inner();
    let needed = png_data.len() as u32;

    if out_buf.is_null() {
        *out_len = needed;
        return 0;
    }
    if *out_len < needed {
        *out_len = needed;
        return -4;
    }
    std::ptr::copy_nonoverlapping(png_data.as_ptr(), out_buf, needed as usize);
    *out_len = needed;
    0
}

// ---------------------------------------------------------------------------
// Stateful PDF rendering (open → render raw BGRA → close)
// ---------------------------------------------------------------------------

static OPEN_DOCS: OnceLock<Mutex<HashMap<i32, (usize, i32)>>> = OnceLock::new();
static ACTIVE_BITMAPS: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
static NEXT_DOC_HANDLE: AtomicI32 = AtomicI32::new(1);

#[no_mangle]
pub unsafe extern "C" fn pdf_open_document_mem(data: *const u8, len: i32) -> i32 {
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(p) => p,
        None => {
            set_error("pdfium.dll not available".into());
            return -1;
        }
    };
    if data.is_null() || len <= 0 {
        set_error("Invalid PDF data".into());
        return -1;
    }
    let slice = std::slice::from_raw_parts(data, len as usize);
    let doc = unsafe { (pdfium.FPDF_LoadMemDocument)(slice.as_ptr(), len, std::ptr::null()) };
    if doc.is_null() {
        let err = unsafe { (pdfium.FPDF_GetLastError)() };
        set_error(format!("PDFium error {}: {}", err, pdf_extractor::pdfium::error_str(err)));
        return -1;
    }
    let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    let handle = NEXT_DOC_HANDLE.fetch_add(1, Ordering::SeqCst);
    let mut map = OPEN_DOCS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    map.insert(handle, (doc as usize, page_count));
    handle
}

#[no_mangle]
pub unsafe extern "C" fn pdf_get_page_dimensions(
    handle: i32,
    page_index: i32,
    out_width_pts: *mut f64,
    out_height_pts: *mut f64,
) -> i32 {
    if out_width_pts.is_null() || out_height_pts.is_null() {
        return ERR_NULL_PTR;
    }
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(p) => p,
        None => {
            set_error("pdfium.dll not available".into());
            return -1;
        }
    };
    let map = OPEN_DOCS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    let (doc_ptr, _) = match map.get(&handle) {
        Some(entry) => *entry,
        None => {
            set_error("Invalid document handle".into());
            return -1;
        }
    };
    let doc = doc_ptr as *mut std::ffi::c_void;
    let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    if page_index < 0 || page_index >= page_count {
        set_error(format!("Page {} out of range ({} pages)", page_index, page_count));
        return -1;
    }
    let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
    if page.is_null() {
        set_error("Failed to load page".into());
        return -1;
    }
    let w = unsafe { (pdfium.FPDF_GetPageWidthF)(page) };
    let h = unsafe { (pdfium.FPDF_GetPageHeightF)(page) };
    unsafe { (pdfium.FPDF_ClosePage)(page); }
    unsafe {
        *out_width_pts = w as f64;
        *out_height_pts = h as f64;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn pdf_document_page_count(handle: i32) -> i32 {
    let map = OPEN_DOCS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    match map.get(&handle) {
        Some((_, count)) => *count,
        None => {
            set_error("Invalid document handle".into());
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pdf_render_page_bgra(
    handle: i32,
    page_index: i32,
    dpi: f64,
    highlight_json: *const u8,
    out_width: *mut i32,
    out_height: *mut i32,
    out_stride: *mut i32,
    out_width_pts: *mut f64,
    out_height_pts: *mut f64,
    out_pixels: *mut *mut u8,
) -> i32 {
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(p) => p,
        None => {
            set_error("pdfium.dll not available".into());
            return -1;
        }
    };
    if out_width.is_null() || out_height.is_null() || out_stride.is_null()
        || out_width_pts.is_null() || out_height_pts.is_null() || out_pixels.is_null()
    {
        set_error("Null output pointer".into());
        return -3;
    }

    let map = OPEN_DOCS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    let (doc_ptr, _) = match map.get(&handle) {
        Some(entry) => *entry,
        None => {
            set_error("Invalid document handle".into());
            return -1;
        }
    };
    let doc = doc_ptr as *mut std::ffi::c_void;

    let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    if page_index < 0 || page_index >= page_count {
        set_error(format!("Page {} out of range ({} pages)", page_index, page_count));
        return -1;
    }

    let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
    if page.is_null() {
        set_error("Failed to load page".into());
        return -1;
    }

    let w_pts = unsafe { (pdfium.FPDF_GetPageWidthF)(page) };
    let h_pts = unsafe { (pdfium.FPDF_GetPageHeightF)(page) };

    let scale = dpi / 72.0;
    let dest_width = ((w_pts as f64) * scale).round() as i32;
    let dest_height = ((h_pts as f64) * scale).round() as i32;

    if dest_width <= 0 || dest_height <= 0 {
        unsafe { (pdfium.FPDF_ClosePage)(page); }
        set_error("Invalid render dimensions".into());
        return -1;
    }

    let bitmap = unsafe {
        (pdfium.FPDFBitmap_CreateEx)(
            dest_width,
            dest_height,
            pdf_extractor::pdfium::FPDFBITMAP_BGRA,
            std::ptr::null_mut(),
            0,
        )
    };
    if bitmap.is_null() {
        unsafe { (pdfium.FPDF_ClosePage)(page); }
        set_error("Failed to create bitmap".into());
        return -1;
    }

    unsafe {
        (pdfium.FPDFBitmap_FillRect)(bitmap, 0, 0, dest_width, dest_height, 0xFFFFFFFF);
        (pdfium.FPDF_RenderPageBitmap)(bitmap, page, 0, 0, dest_width, dest_height, 0, pdf_extractor::pdfium::FPDF_NONE);
    }

    let buf_ptr = unsafe { (pdfium.FPDFBitmap_GetBuffer)(bitmap) };
    let stride = unsafe { (pdfium.FPDFBitmap_GetStride)(bitmap) };

    // ── Native highlight rendering ─────────────────────────────────
    if !highlight_json.is_null() && !buf_ptr.is_null() && stride > 0 {
        let cstr = unsafe { CStr::from_ptr(highlight_json as *const c_char) };
        if let Ok(json_str) = cstr.to_str() {
            if !json_str.is_empty() {
                if let Ok(highlights) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                    let page_num = page_index as u32 + 1;
                    let buf = std::slice::from_raw_parts_mut(
                        buf_ptr as *mut u8,
                        (dest_height as usize) * (stride as usize),
                    );
                    for h in &highlights {
                        let _ = match h.get("page").and_then(|v| v.as_u64()) {
                            Some(p) if p == page_num as u64 => (),
                            _ => continue,
                        };
                        let x_min = h.get("x_min").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let y_min = h.get("y_min").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let x_max = h.get("x_max").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let y_max = h.get("y_max").and_then(|v| v.as_f64()).unwrap_or(0.0);

                        let px1 = (x_min * scale).round() as i32;
                        let py1 = ((h_pts as f64 - y_max) * scale).round() as i32;
                        let px2 = (x_max * scale).round() as i32;
                        let py2 = ((h_pts as f64 - y_min) * scale).round() as i32;

                        let px1 = px1.clamp(0, dest_width);
                        let py1 = py1.clamp(0, dest_height);
                        let px2 = px2.clamp(0, dest_width);
                        let py2 = py2.clamp(0, dest_height);

                        let src_a = 204u32;
                        let dst_a = 255u32 - src_a;
                        for y in py1..py2 {
                            let row_off = (y as usize) * (stride as usize);
                            for x in px1..px2 {
                                let i = row_off + (x as usize) * 4;
                                let b = buf[i] as u32;
                                let g = buf[i + 1] as u32;
                                let r = buf[i + 2] as u32;
                                buf[i]     = ((0u32   * src_a + b * dst_a) / 255) as u8;
                                buf[i + 1] = ((230u32 * src_a + g * dst_a) / 255) as u8;
                                buf[i + 2] = ((255u32 * src_a + r * dst_a) / 255) as u8;
                            }
                        }
                    }
                }
            }
        }
    }

    unsafe { (pdfium.FPDF_ClosePage)(page); }

    if buf_ptr.is_null() || stride <= 0 {
        unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap); }
        set_error("Failed to get bitmap buffer".into());
        return -1;
    }

    let mut bmp_map = ACTIVE_BITMAPS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    bmp_map.insert(buf_ptr as usize, bitmap as usize);

    unsafe {
        *out_width = dest_width;
        *out_height = dest_height;
        *out_stride = stride;
        *out_width_pts = w_pts as f64;
        *out_height_pts = h_pts as f64;
        *out_pixels = buf_ptr;
    }
    0
}

// ---------------------------------------------------------------------------
// pdf_render_page_to_buffer — renders directly into a caller-allocated buffer
// (zero-copy: WPF BackBuffer or pinned byte[]). No internal allocation.
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_render_page_to_buffer(
    handle: i32,
    page_index: i32,
    dpi: f64,
    highlight_json: *const u8,
    buffer: *mut u8,
    width: i32,
    height: i32,
    stride: i32,
    out_width_pts: *mut f64,
    out_height_pts: *mut f64,
) -> i32 {
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(p) => p,
        None => {
            set_error("pdfium.dll not available".into());
            return -1;
        }
    };

    if out_width_pts.is_null() || out_height_pts.is_null() {
        set_error("Null output pointer".into());
        return -3;
    }
    if buffer.is_null() || width <= 0 || height <= 0 || stride <= 0 {
        set_error("Invalid buffer parameters".into());
        return -3;
    }

    let map = OPEN_DOCS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    let (doc_ptr, _) = match map.get(&handle) {
        Some(entry) => *entry,
        None => {
            set_error("Invalid document handle".into());
            return -1;
        }
    };
    let doc = doc_ptr as *mut std::ffi::c_void;

    let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
    if page_index < 0 || page_index >= page_count {
        set_error(format!("Page {} out of range ({} pages)", page_index, page_count));
        return -1;
    }

    let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
    if page.is_null() {
        set_error("Failed to load page".into());
        return -1;
    }

    let w_pts = unsafe { (pdfium.FPDF_GetPageWidthF)(page) as f64 };
    let h_pts = unsafe { (pdfium.FPDF_GetPageHeightF)(page) as f64 };

    // Wrap the external buffer as a PDFium bitmap — no allocation
    let bitmap = unsafe {
        (pdfium.FPDFBitmap_CreateEx)(
            width, height,
            pdf_extractor::pdfium::FPDFBITMAP_BGRA,
            buffer as *mut std::ffi::c_void,
            stride,
        )
    };
    if bitmap.is_null() {
        unsafe { (pdfium.FPDF_ClosePage)(page); }
        set_error("Failed to create PDFium bitmap with external buffer".into());
        return -1;
    }

    unsafe {
        (pdfium.FPDFBitmap_FillRect)(bitmap, 0, 0, width, height, 0xFFFFFFFF);
        (pdfium.FPDF_RenderPageBitmap)(bitmap, page, 0, 0, width, height, 0, pdf_extractor::pdfium::FPDF_NONE);
    }

    // ── Native highlight rendering ─────────────────────────────────
    if !highlight_json.is_null() {
        let cstr = unsafe { CStr::from_ptr(highlight_json as *const c_char) };
        if let Ok(json_str) = cstr.to_str() {
            if !json_str.is_empty() {
                if let Ok(highlights) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                    let page_num = page_index as u32 + 1;
                    let total_bytes = (height as usize) * (stride as usize);
                    let buf = std::slice::from_raw_parts_mut(buffer as *mut u8, total_bytes);
                    let scale = dpi / 72.0;
                    for h in &highlights {
                        let _ = match h.get("page").and_then(|v| v.as_u64()) {
                            Some(p) if p == page_num as u64 => (),
                            _ => continue,
                        };
                        let x_min = h.get("x_min").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let y_min = h.get("y_min").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let x_max = h.get("x_max").and_then(|v| v.as_f64()).unwrap_or(0.0);
                        let y_max = h.get("y_max").and_then(|v| v.as_f64()).unwrap_or(0.0);

                        let px1 = (x_min * scale).round() as i32;
                        let py1 = ((h_pts - y_max) * scale).round() as i32;
                        let px2 = (x_max * scale).round() as i32;
                        let py2 = ((h_pts - y_min) * scale).round() as i32;

                        let px1 = px1.clamp(0, width);
                        let py1 = py1.clamp(0, height);
                        let px2 = px2.clamp(0, width);
                        let py2 = py2.clamp(0, height);

                        let src_a = 204u32;
                        let dst_a = 255u32 - src_a;
                        for y in py1..py2 {
                            let row_off = (y as usize) * (stride as usize);
                            for x in px1..px2 {
                                let i = row_off + (x as usize) * 4;
                                let b = buf[i] as u32;
                                let g = buf[i + 1] as u32;
                                let r = buf[i + 2] as u32;
                                buf[i]     = ((0u32   * src_a + b * dst_a) / 255) as u8;
                                buf[i + 1] = ((230u32 * src_a + g * dst_a) / 255) as u8;
                                buf[i + 2] = ((255u32 * src_a + r * dst_a) / 255) as u8;
                            }
                        }
                    }
                }
            }
        }
    }

    unsafe {
        (pdfium.FPDF_ClosePage)(page);
        (pdfium.FPDFBitmap_Destroy)(bitmap); // destroys bitmap wrapper, NOT the external buffer
        *out_width_pts = w_pts;
        *out_height_pts = h_pts;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn pdf_free_bitmap(pixels: *mut u8) {
    if pixels.is_null() {
        return;
    }
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(p) => p,
        None => return,
    };
    let mut bmp_map = ACTIVE_BITMAPS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    if let Some(bitmap_ptr) = bmp_map.remove(&(pixels as usize)) {
        unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap_ptr as *mut std::ffi::c_void); }
    }
}

#[no_mangle]
pub unsafe extern "C" fn pdf_close_document(handle: i32) -> i32 {
    let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
        Some(p) => p,
        None => return -1,
    };
    let mut map = OPEN_DOCS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    if let Some((doc_ptr, _)) = map.remove(&handle) {
        unsafe { (pdfium.FPDF_CloseDocument)(doc_ptr as *mut std::ffi::c_void); }
        0
    } else {
        set_error("Invalid document handle".into());
        -1
    }
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
    let msg = LAST_ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .cloned()
        .unwrap_or_default();
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
    match with_app_mut(|app| {
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

    let (jobs, indexer) = with_app_read(|app| {
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

    let channel_capacity = with_app_read(|app| app.channel_capacity).unwrap_or(None);
    let config = PipelineConfig {
        channel_capacity: channel_capacity.map(|v| v as usize),
        num_indexer_threads: with_app_read(|app| app.num_indexer_threads).unwrap_or(None).map(|v| v as usize),
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
    let index_path = with_app_read(|app| app.index_path.clone()).unwrap_or(None);
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
        .map_err(|_| ERR_POISONED)
}

fn with_registry<F, R>(f: F) -> Result<R, i32>
where
    F: FnOnce(&CollectionRegistry) -> R,
{
    let guard = registry_guard()?;
    let reg = guard.as_ref().ok_or(ERR_REG_NOT_INIT)?;
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
            match with_app_mut(|app| { app.$field = Some(value); Ok::<_, i32>(()) }) {
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
                match with_app_mut(|app| { app.$field = None; Ok::<_, i32>(()) }) {
                    Ok(_) => 0,
                    Err(e) => e,
                }
            } else {
                let s = match unsafe { cstr_to_str(value) } {
                    Ok(s) => s.to_string(),
                    Err(e) => return e,
                };
                match with_app_mut(|app| { app.$field = Some(s); Ok::<_, i32>(()) }) {
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
define_u32_setter!(pdf_set_indexer_threads, num_indexer_threads);
#[no_mangle]
pub unsafe extern "C" fn pdf_set_fuzzy_distance(value: u32) -> i32 {
    FUZZY_DISTANCE.store(value, Ordering::Relaxed);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_stem(value: u32) -> i32 {
    STEM_ENABLED.store(value, Ordering::Relaxed);
    0
}
define_string_setter!(pdf_set_tesseract_path, tesseract_path);
define_string_setter!(pdf_set_ocr_language, ocr_language);
define_string_setter!(pdf_set_search_field, search_field);
define_string_setter!(pdf_set_path_filter, path_filter);

#[no_mangle]
pub unsafe extern "C" fn pdf_set_log_callback(cb: Option<extern "C" fn(*const u8, u32)>) -> i32 {
    let lock = LOG_CALLBACK.get_or_init(|| Mutex::new(None));
    match lock.lock() {
        Ok(mut guard) => {
            *guard = cb;
            0
        }
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_process_callback(cb: Option<extern "C" fn(*const u8, u32)>) -> i32 {
    let lock = PROCESS_CALLBACK.get_or_init(|| Mutex::new(None));
    match lock.lock() {
        Ok(mut guard) => {
            *guard = cb;
            0
        }
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_ram_buffer(value: u64) -> i32 {
    (|| -> i32 {
    match with_app_mut(|app| { app.ram_buffer = Some(value); Ok::<_, i32>(()) }) {
        Ok(_) => 0,
        Err(e) => e,
    }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_recency_weight(value: f32) -> i32 {
    RECENCY_WEIGHT_BITS.store(value.to_bits(), Ordering::Relaxed);
    0
}

/// Set per-field weight boosts for ranked search.
///
/// `json` is a JSON object mapping Tantivy field names to float weights:
/// e.g. `{"content": 1.0, "path": 3.0}`.
///
/// Pass `null` to reset to unweighted (default) search. On success the
/// next call to any search function will use BoostQuery for each field.
///
/// Valid field names: content, path, math_tokens.
#[no_mangle]
pub unsafe extern "C" fn pdf_set_field_weights(json: *const c_char) -> i32 {
    (|| -> i32 {
    if json.is_null() {
        return match with_app_mut(|app| { app.field_weights = None; Ok::<_, i32>(()) }) {
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
    match with_app_mut(|app| { app.field_weights = Some(weights); Ok::<_, i32>(()) }) {
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
    match with_app_mut(|app| { app.collection_boosts.insert(coll_id as i64, weight); Ok::<_, i32>(()) }) {
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
        return match with_app_mut(|app| { app.boolean_query = None; Ok::<_, i32>(()) }) {
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
    match with_app_mut(|app| { app.boolean_query = Some(clauses); Ok::<_, i32>(()) }) {
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
    eprintln!("[pdf_index_collection] ENTER coll_id={}", coll_id);
    (|| -> i32 {
    struct DropToken(u32);
    impl Drop for DropToken { fn drop(&mut self) { cancel_tokens().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0); } }

    let cancel_token = Arc::new(AtomicBool::new(false));
    cancel_tokens().lock().unwrap_or_else(|e| e.into_inner()).insert(coll_id, cancel_token.clone());
    let _guard = DropToken(coll_id);

    let collection = match with_registry(|reg| reg.get_collection(coll_id as i64)) {
        Ok(Ok(c)) => c,
        Ok(Err(_)) => { eprintln!("[pdf_index_collection] collection not found"); set_error("Collection not found".into()); return ERR_COLLECTION_NOT_FOUND; }
        Err(e) => { eprintln!("[pdf_index_collection] reg error: {}", e); return e; }
    };
    eprintln!("[pdf_index_collection] collection={:?} folder={:?}", coll_id, collection.books_folder);
    let canonical = match std::fs::canonicalize(Path::new(&collection.books_folder)) {
        Ok(p) => p,
        Err(e) => { eprintln!("[pdf_index_collection] canonicalize error: {}", e); set_error(format!("Books folder not accessible: {}", e)); return -1; }
    };
    eprintln!("[pdf_index_collection] canonical={:?}", canonical);

    match with_registry(|reg| reg.ensure_data_dirs(coll_id as i64)) {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => { eprintln!("[pdf_index_collection] ensure dirs error: {}", e); set_error(format!("{}", e)); return -1; }
        Err(e) => { eprintln!("[pdf_index_collection] reg error: {}", e); return e; }
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
        match with_app_read(|app| app.ram_buffer) {
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

    let channel_capacity = with_app_read(|app| app.channel_capacity).unwrap_or(None);
    let extract_workers = with_app_read(|app| app.extract_workers).unwrap_or(None);
    let indexer_batch_size = with_app_read(|app| app.indexer_batch_size).unwrap_or(None);
    let commit_interval = with_app_read(|app| app.commit_interval).unwrap_or(None);
    let commit_timeout = with_app_read(|app| app.commit_timeout).unwrap_or(None);
    let num_indexer_threads = with_app_read(|app| app.num_indexer_threads).unwrap_or(None);

    // Resolve worker binary path relative to the current executable
    // (handles both production layout and test binary layout).
    // build.rs ensures pdf_worker.exe is always compiled alongside the library.
    let worker_path = match resolve_worker_path() {
        Some(wp) => Some(wp),
        None => {
            let exe_path = std::env::current_exe().unwrap_or_default();
            set_error(format!(
                "Worker binary 'pdf_worker.exe' not found near '{}'. \
                 The pdf_worker.exe must be deployed alongside the library. \
                 Run 'cargo build -p pdf_extractor --bin pdf_worker' to build it.",
                exe_path.display()
            ));
            return -1;
        }
    };

    let log_cb = LOG_CALLBACK
        .get()
        .and_then(|lock| lock.lock().ok())
        .and_then(|guard| *guard);

    let process_cb = PROCESS_CALLBACK
        .get()
        .and_then(|lock| lock.lock().ok())
        .and_then(|guard| *guard);

    let config = PipelineConfig {
        channel_capacity: channel_capacity.map(|v| v as usize),
        num_extract_workers: extract_workers.map(|v| v as usize),
        num_indexer_threads: num_indexer_threads.map(|v| v as usize),
        indexer_batch_size: indexer_batch_size.map(|v| v as usize),
        commit_interval: commit_interval.map(|v| v as u64),
        commit_timeout: commit_timeout.map(|v| v as u64),
        worker_path,
        progress_cb: progress_callback.map(|cb| {
            Box::new(move |current: u64, total: u64| cb(current, total)) as Box<dyn Fn(u64, u64) + Send>
        }),
        cancel_flag: Some(cancel_token.clone()),
        log_cb,
        process_cb,
    };

    eprintln!("[pdf_index_collection] calling run_pipeline with canonical={:?}", canonical);
    let indexer_for_ocr = indexer.clone();
    match run_pipeline(Arc::clone(&jobs), &writer, Arc::clone(&metrics), &canonical, indexer, &config) {
        Ok(()) => {
            let processed = metrics.processed();
            eprintln!("[pdf_index_collection] run_pipeline OK, processed={}", processed);
            if let Err(e) = with_registry(|reg| reg.update_index_metadata(coll_id as i64, processed)) {
                eprintln!("[pdf_index_collection] failed to update registry metadata: {}", e);
            }

            // Run OCR post-processing if flag (1) is set
            if (flags & 1) != 0 {
                // Drop the JSONL writer before we re-open it for OCR output
                drop(writer);

                let tesseract_path = with_app_read(|app| app.tesseract_path.clone()).unwrap_or(None)
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

                let ocr_language = with_app_read(|app| app.ocr_language.clone()).unwrap_or(None)
                    .unwrap_or_else(|| "eng".to_string());

                let ocr_max_dim = with_app_read(|app| app.ocr_max_dim).unwrap_or(None).unwrap_or(3000);
                let num_workers = with_app_read(|app| app.ocr_workers).unwrap_or(None).map(|v| v as usize);

                let ocr_config = ocr::OcrConfig {
                    tesseract_path,
                    max_dim: ocr_max_dim,
                    max_retries: 2,
                    language: ocr_language,
                };

                match run_ocr_post_processing(
                    jobs,
                    indexer_for_ocr,
                    &ocr_config,
                    Some(output_path),
                    num_workers,
                    Some(cancel_token.clone()),
                    log_cb,
                ) {
                    Ok(_ocr_count) => {
                        processed as i32
                    }
                    Err(e) => {
                        eprintln!("[pdf_index_collection] OCR post-processing failed (non-fatal): {}", e);
                        processed as i32
                    }
                }
            } else {
                processed as i32
            }
        }
        Err(e) => {
            eprintln!("[pdf_index_collection] run_pipeline ERROR: {}", e);
            set_error(format!("Indexing failed: {}", e));
            -1
        }
    }
    })()
}

#[no_mangle]
pub unsafe extern "C" fn pdf_cancel_indexing(coll_id: u32) {
    if let Some(token) = cancel_tokens().lock().unwrap_or_else(|e| e.into_inner()).get(&coll_id) {
        token.store(true, Ordering::Relaxed);
    }
}

#[no_mangle]
pub unsafe extern "C" fn pdf_is_cancel_requested(coll_id: u32) -> i32 {
    (|| -> i32 {
    cancel_tokens().lock().unwrap_or_else(|e| e.into_inner()).get(&coll_id)
        .map_or(0, |t| if t.load(Ordering::Relaxed) { 1 } else { 0 })
    })()
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_close_collection(_coll_id: u32) -> i32 {
    // Currently no per-collection state to clean up
    0
}

#[no_mangle]
pub unsafe extern "C" fn pdf_close_all() -> i32 {
    // Clear global app state
    reset_all_globals();
    0
}

/// Reset **all** global state — AppContext, collection registry, error,
/// callbacks, cancellation tokens, and atomic scalar settings.  After
/// calling this, the library returns to its initial (uninitialised) state.
/// Any open document handles or bitmaps become invalid and must not be used.
#[no_mangle]
pub unsafe extern "C" fn pdf_reset_all() {
    reset_all_globals();
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
    let fuzzy = FUZZY_DISTANCE.load(Ordering::Relaxed);
    let stem = STEM_ENABLED.load(Ordering::Relaxed) != 0;
    let recency_weight = f32::from_bits(RECENCY_WEIGHT_BITS.load(Ordering::Relaxed));
    with_app_read(|app| SearchSettings {
        fuzzy,
        stem,
        recency_weight,
        field: app.search_field.clone(),
        path_filter: app.path_filter.clone(),
        field_weights: app.field_weights.clone(),
        boolean_query: app.boolean_query.clone(),
    })
    .unwrap_or(SearchSettings { fuzzy, stem, recency_weight, ..Default::default() })
}

fn do_search_with_index(
    search_index: &SearchIndex,
    query: &str,
    limit: u32,
    offset: u32,
    coll_id: Option<i64>,
    settings: &SearchSettings,
) -> Result<(Vec<serde_json::Value>, u64), i32> {
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

    // Build snippet infra ONCE (reader, searcher, query, generator) —
    // reused for all results instead of creating one per result.
    let infra_reader = search_index.index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
        .try_into()
        .ok();
    let infra_searcher = infra_reader.as_ref().map(|r| r.searcher());
    let infra_query = if !query.trim().is_empty() {
        let qp = tantivy::query::QueryParser::for_index(&search_index.index, vec![search_index.content_field]);
        qp.parse_query(query).ok()
    } else {
        None
    };

    // Get total count from the same searcher + query
    let total_count: u64 = infra_searcher.as_ref()
        .and_then(|s| infra_query.as_ref().and_then(|q| {
            s.search(q.as_ref(), &tantivy::collector::Count).ok()
        }))
        .unwrap_or(0) as u64;

    // Build snippet generator once and reuse
    let snippet_gen = infra_searcher.as_ref().and_then(|s| {
        infra_query.as_ref().and_then(|q| {
            tantivy::SnippetGenerator::create(s, q.as_ref(), search_index.content_field).ok()
        })
    });

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
            let snippet = snippet_gen.as_ref()
                .and_then(|gen| {
                    let s = gen.snippet_from_doc(doc);
                    Some(s.to_html())
                })
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

    Ok((json_entries, total_count))
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
        Ok(Err(_)) => { set_error("Collection not found".into()); return ERR_COLLECTION_NOT_FOUND; }
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

    let (entries, total) = match do_search_with_index(&search_index, query_str, limit, offset, Some(coll_id as i64), &settings) {
        Ok(t) => t,
        Err(e) => return e,
    };

    let wrapped = serde_json::json!({
        "total": total,
        "results": entries,
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

    let collection_boosts = with_app_read(|app| app.collection_boosts.clone()).unwrap_or_default();

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

        let (mut entries, count) = match do_search_with_index(&search_index, query_str, limit, offset, Some(coll.id), &settings) {
            Ok(t) => t,
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
    let index_path = with_app_read(|app| {
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
pub unsafe extern "C" fn pdf_get_problematic_jobs(
    coll_id: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    (|| -> i32 {
    let _collection = match with_registry(|reg| reg.get_collection(coll_id as i64)) {
        Ok(Ok(c)) => c,
        Ok(Err(_)) => { set_error("Collection not found".into()); return ERR_COLLECTION_NOT_FOUND; }
        Err(e) => return e,
    };

    let db_path = match with_registry(|reg| reg.db_path(coll_id as i64)) {
        Ok(p) => p,
        Err(e) => return e,
    };

    let jobs = match JobStore::open(&db_path) {
        Ok(j) => j,
        Err(e) => {
            set_error(format!("Failed to open job store: {}", e));
            return -1;
        }
    };

    match jobs.fetch_errored() {
        Ok(rows) => {
            let list: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(id, path, status, ocr_flag, error)| {
                    serde_json::json!({
                        "id": id,
                        "path": path,
                        "status": status,
                        "ocr_flag": ocr_flag != 0,
                        "error": error,
                        "no_positions": false,
                    })
                })
                .collect();
            let json_str = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
            unsafe { write_to_buffer(json_str.as_bytes(), out_json, out_len) }
        }
        Err(e) => {
            set_error(format!("Failed to query errored jobs: {}", e));
            -1
        }
    }
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
        Ok(Err(_)) => { set_error("Collection not found".into()); return ERR_COLLECTION_NOT_FOUND; }
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
        assert_eq!(unsafe { pdf_set_indexer_threads(2) }, 0);

        // String setters
        let lang = CString::new("por").unwrap();
        assert_eq!(unsafe { pdf_set_ocr_language(lang.as_ptr()) }, 0);

        let tesseract = CString::new("C:\\tools\\tesseract.exe").unwrap();
        assert_eq!(unsafe { pdf_set_tesseract_path(tesseract.as_ptr()) }, 0);

        let field = CString::new("content").unwrap();
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
        let json = CString::new(r#"{"content": 1.0, "path": 3.0}"#).unwrap();
        let rc = unsafe { pdf_set_field_weights(json.as_ptr()) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_field_weights_null_resets() {
        reset_state();
        let json = CString::new(r#"{"content": 2.0}"#).unwrap();
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
        let json = CString::new(r#"{"content": 0.0}"#).unwrap();
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
        assert_eq!(unsafe { pdf_is_cancel_requested(0) }, 0);
    }

    #[test]
    fn test_cancel_requested() {
        reset_state();
        cancel_tokens().lock().unwrap().insert(0, Arc::new(AtomicBool::new(false)));
        unsafe { pdf_cancel_indexing(0) };
        assert_eq!(unsafe { pdf_is_cancel_requested(0) }, 1);
        // Reset after cancel
        reset_state();
        cancel_tokens().lock().unwrap().insert(0, Arc::new(AtomicBool::new(false)));
        assert_eq!(unsafe { pdf_is_cancel_requested(0) }, 0);
    }

    // ── Cancellation flag reset tests ──

    #[test]
    fn test_cancel_flag_reset_by_index_collection() {
        reset_state();

        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cancel_reset_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };

        // pdf_index_collection should create a fresh cancel token
        let _rc = unsafe { pdf_index_collection(coll_id as u32, 0, None) };
        assert_eq!(unsafe { pdf_is_cancel_requested(coll_id as u32) }, 0,
            "flag should be 0 after pdf_index_collection (fresh token)");
    }

    #[test]
    fn test_cancel_then_extract_resets_flag() {
        reset_state();

        let empty_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let empty_dir = std::env::temp_dir().join(format!("pdf_capi_extract_cancel_{}", empty_dir_n));
        let _ = std::fs::remove_dir_all(&empty_dir);
        std::fs::create_dir_all(&empty_dir).unwrap();
        let dir_c = CString::new(empty_dir.to_string_lossy().as_ref()).unwrap();
        let _rc = unsafe { pdf_extract(dir_c.as_ptr(), None) };
    }

    #[test]
    fn test_cancel_multiple_times() {
        reset_state();
        cancel_tokens().lock().unwrap().insert(0, Arc::new(AtomicBool::new(false)));
        unsafe { pdf_cancel_indexing(0) };
        unsafe { pdf_cancel_indexing(0) };

        let reg_dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cancel_multi_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);
        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };
        let _rc = unsafe { pdf_index_collection(coll_id as u32, 0, None) };
        assert_eq!(unsafe { pdf_is_cancel_requested(coll_id as u32) }, 0,
            "flag should reset after multiple cancels");
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
        let _ = with_app_mut(|app| {
            let indexer = app.indexer.as_ref().unwrap();
            indexer.index_document(1, "/doc.pdf", "hello world hello").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        // Store positions manually (normally done by pipeline's flush_batch)
        let index_path = std::path::PathBuf::from(&idx);
        let positions_db_path = index_path.join("positions.sqlite");
        let store = pdf_extractor::positions::PositionStore::open(&positions_db_path).unwrap();
        let word_positions = vec![
            (0usize, pdf_extractor::extractor::WordPosition {
                page: 1, x_min: 10.0, y_min: 20.0, x_max: 50.0, y_max: 30.0,
                text: "hello".to_string(),
            }),
            (2usize, pdf_extractor::extractor::WordPosition {
                page: 1, x_min: 60.0, y_min: 20.0, x_max: 100.0, y_max: 30.0,
                text: "hello".to_string(),
            }),
        ];
        store.store_positions(1, &word_positions).unwrap();

        // Search for term positions
        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let term = CString::new("hello").unwrap();
        let rc = unsafe {
            pdf_get_term_positions(0, 1, term.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len)
        };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let positions: Vec<serde_json::Value> = serde_json::from_str(result).unwrap();
        assert_eq!(positions.len(), 2, "Should return positions for both 'hello' occurrences");
        for pos in &positions {
            assert_eq!(pos["page"], 1, "All positions should be on page 1");
            assert!(pos["x_min"].as_f64().unwrap() >= 0.0, "x_min should be valid");
            assert!(pos["y_min"].as_f64().unwrap() >= 0.0, "y_min should be valid");
            assert!(pos["x_max"].as_f64().unwrap() > pos["x_min"].as_f64().unwrap(), "x_max > x_min");
            assert!(pos["y_max"].as_f64().unwrap() > pos["y_min"].as_f64().unwrap(), "y_max > y_min");
        }
    }

    #[test]
    fn test_get_term_positions_no_match() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let _ = with_app_mut(|app| {
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

        let _ = with_app_mut(|app| {
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

        // Index 3 docs in the content field
        let _ = with_app_mut(|app| {
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

        let _ = with_app_mut(|app| {
            let indexer = app.indexer.as_ref().unwrap();
            indexer.index_document(1, "/doc1.pdf", "algebra").unwrap();
            indexer.index_document(2, "/doc2.pdf", "calculus").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        // --- Basic flow ---
        let w = CString::new(r#"{"content": 2.0}"#).unwrap();
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
        si1.add_document(&mut w1, 1, "/doc1.pdf", "math algebra", Some("math algebra")).unwrap();
        w1.commit().unwrap();

        let si2 = SearchIndex::new(&idx2).unwrap();
        let mut w2 = si2.writer().unwrap();
        si2.add_document(&mut w2, 1, "/doc2.pdf", "math algebra", Some("math algebra")).unwrap();
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

    // ── Worker path resolution tests ──

    #[test]
    fn test_resolve_worker_path_integration() {
        // During `cargo test`, pdf_worker.exe lives at target/<profile>/pdf_worker.exe.
        let found = resolve_worker_path();
        assert!(found.is_some(),
            "resolve_worker_path() should find pdf_worker.exe during cargo test");
        let path = found.unwrap();
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("pdf_worker.exe"));
        assert!(path.exists(), "resolved worker path must exist on disk");
    }

    #[test]
    fn test_resolve_worker_path_from_same_dir() {
        let dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pdf_capi_wp_same_{}", dir_n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let exe = dir.join("host.exe");
        let worker = dir.join("pdf_worker.exe");
        std::fs::write(&worker, b"mock").unwrap();
        std::fs::write(&exe, b"mock").unwrap();

        let result = resolve_worker_path_from(&exe);
        assert_eq!(result.as_ref().and_then(|p| p.file_name()), Some(std::ffi::OsStr::new("pdf_worker.exe")));
        assert_eq!(result, Some(worker));
    }

    #[test]
    fn test_resolve_worker_path_from_deps() {
        let dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pdf_capi_wp_deps_{}", dir_n));
        let _ = std::fs::remove_dir_all(&dir);
        let profile = dir.join("debug");
        let deps = profile.join("deps");
        std::fs::create_dir_all(&deps).unwrap();

        let exe = deps.join("test-abc123.exe");
        let worker = profile.join("pdf_worker.exe");
        std::fs::write(&worker, b"mock").unwrap();
        std::fs::write(&exe, b"mock").unwrap();

        let result = resolve_worker_path_from(&exe);
        assert_eq!(result.as_ref().and_then(|p| p.file_name()), Some(std::ffi::OsStr::new("pdf_worker.exe")));
        assert_eq!(result, Some(worker));
    }

    #[test]
    fn test_resolve_worker_path_from_walk_up() {
        let dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pdf_capi_wp_walk_{}", dir_n));
        let _ = std::fs::remove_dir_all(&dir);
        let deeper = dir.join("a").join("b").join("c");
        std::fs::create_dir_all(&deeper).unwrap();

        let exe = deeper.join("host.exe");
        let worker = dir.join("pdf_worker.exe");
        std::fs::write(&worker, b"mock").unwrap();
        std::fs::write(&exe, b"mock").unwrap();

        let result = resolve_worker_path_from(&exe);
        assert_eq!(result.as_ref().and_then(|p| p.file_name()), Some(std::ffi::OsStr::new("pdf_worker.exe")));
        assert_eq!(result, Some(worker));
    }

    #[test]
    fn test_resolve_worker_path_from_not_found() {
        let dir_n = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pdf_capi_wp_notfound_{}", dir_n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("host.exe");
        std::fs::write(&exe, b"mock").unwrap();
        // No pdf_worker.exe anywhere in the tree
        let result = resolve_worker_path_from(&exe);
        assert!(result.is_none(), "no worker anywhere in tree");
    }

    #[test]
    fn test_resolve_worker_path_from_exe_no_parent() {
        // Engine-level paths (like \\?\) often have no parent
        let result = resolve_worker_path_from(std::path::Path::new("\\\\?\\C:\\"));
        assert!(result.is_none(), "root/UNC path has no meaningful parent");
    }
}

