use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, CStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use pdf_extractor::indexer::{Indexer, SearchIndex};
use pdf_extractor::metrics::Metrics;
use pdf_extractor::ocr::find_tesseract;
use pdf_extractor::pipeline::{run_ocr_post_processing, run_pipeline, PipelineConfig};
use pdf_extractor::registry::CollectionRegistry;
use pdf_extractor::scanner::JobStore;
use pdf_extractor::search::builders::{AutoPhraseQueryBuilder, BooleanPhraseQueryBuilder};
use pdf_extractor::search::engines::TantivyEngine;
use pdf_extractor::search::pipeline::{SearchPipeline, default_enrichers};
use pdf_extractor::search::types::*;
use pdf_extractor::positions::StoredPosition;
use pdf_extractor::search::SearchResponse;

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

pub const ERR_GENERAL: i32 = -1;
pub const ERR_NOT_FOUND: i32 = -2;
pub const ERR_INVALID_PARAM: i32 = -3;
pub const ERR_BUFFER_RETRY: i32 = -4;
pub const ERR_POISONED: i32 = -100;
pub const ERR_NOT_INIT: i32 = -101;
pub const ERR_REG_NOT_INIT: i32 = -102;
pub const ERR_INVALID_UTF8: i32 = -103;
pub const ERR_CHANNEL_CAPACITY: i32 = -104;
pub const ERR_NULL_PTR: i32 = -105;
pub const ERR_BUFFER_TOO_SMALL: i32 = -106;
pub const ERR_COLLECTION_NOT_FOUND: i32 = -107;

static LAST_ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();

pub fn set_error(msg: String) {
    let mut guard = LAST_ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *guard = Some(msg);
}

pub fn take_last_error() -> Option<String> {
    let lock = LAST_ERROR.get_or_init(|| Mutex::new(None));
    let mut guard = match lock.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    guard.take()
}

/// Convert a Rust `Result<(), i32>` — where `Err(i32)` is already an error
/// code — into a CAPI i32 return value.  On success returns 0.
pub fn ffi_try(f: impl FnOnce() -> Result<(), i32>) -> i32 {
    match f() {
        Ok(_) => 0,
        Err(code) => code,
    }
}

/// Convert a Rust `Result<T, i32>` into a CAPI i32 by passing the value
/// through a writer closure on success.
pub fn ffi_try_with<T>(f: impl FnOnce() -> Result<T, i32>, write: impl FnOnce(T) -> i32) -> i32 {
    match f() {
        Ok(val) => write(val),
        Err(code) => code,
    }
}

// ---------------------------------------------------------------------------
// Raw pointer helpers
// ---------------------------------------------------------------------------

/// Write `data` into a caller-allocated buffer.  Returns:
/// - `0` on success
/// - `ERR_NULL_PTR` if `out` or `out_len` is null
/// - `ERR_BUFFER_RETRY` if the buffer is too small (sets `*out_len` to needed size)
pub unsafe fn write_to_buffer(data: &[u8], out: *mut c_char, out_len: *mut u32) -> i32 {
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

/// Convert a C string pointer to a `&str`.  Returns `Err` with the
/// appropriate error code on null or invalid UTF-8.
pub unsafe fn cstr_to_str(ptr: *const c_char) -> Result<&'static str, i32> {
    if ptr.is_null() {
        return Err(ERR_NULL_PTR);
    }
    CStr::from_ptr(ptr).to_str().map_err(|_| {
        set_error("Invalid UTF-8 input".into());
        ERR_INVALID_UTF8
    })
}

// ---------------------------------------------------------------------------
// Resource managers
// ---------------------------------------------------------------------------

struct DocumentEntry {
    doc_ptr: usize,
    page_count: i32,
}

struct DocumentManager {
    docs: HashMap<i32, DocumentEntry>,
    next_handle: i32,
}

impl DocumentManager {
    fn new() -> Self {
        Self { docs: HashMap::new(), next_handle: 1 }
    }

    fn alloc_handle(&mut self, doc_ptr: usize, page_count: i32) -> i32 {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.docs.insert(handle, DocumentEntry { doc_ptr, page_count });
        handle
    }

    fn get_doc_ptr(&self, handle: i32) -> Option<usize> {
        self.docs.get(&handle).map(|e| e.doc_ptr)
    }

    fn get_page_count(&self, handle: i32) -> Option<i32> {
        self.docs.get(&handle).map(|e| e.page_count)
    }

    fn remove(&mut self, handle: i32) -> Option<DocumentEntry> {
        self.docs.remove(&handle)
    }

    fn clear(&mut self) {
        self.docs.clear();
        self.next_handle = 1;
    }
}

struct BitmapManager {
    bitmaps: HashMap<usize, usize>,
}

impl BitmapManager {
    fn new() -> Self {
        Self { bitmaps: HashMap::new() }
    }

    fn insert(&mut self, pixel_ptr: usize, bitmap_ptr: usize) {
        self.bitmaps.insert(pixel_ptr, bitmap_ptr);
    }

    fn remove(&mut self, pixel_ptr: usize) -> Option<usize> {
        self.bitmaps.remove(&pixel_ptr)
    }

    fn clear(&mut self) {
        self.bitmaps.clear();
    }
}

struct IndexDropGuard(u32, Arc<AtomicBool>);
impl Drop for IndexDropGuard {
    fn drop(&mut self) {
        self.1.store(true, Ordering::Relaxed);
    }
}

/// Worker path resolver (split from PdfEngine for testability).
/// Tests that need absolute paths use this directly.
pub fn resolve_worker_path_from(exe: &Path) -> Option<PathBuf> {
    let exe_parent = exe.parent()?;

    let same_dir = exe_parent.join("pdf_worker.exe");
    if same_dir.exists() { return Some(same_dir); }

    if let Ok(bin_dir) = std::env::var("CARGO_BIN_DIR") {
        let candidate = PathBuf::from(bin_dir).join("pdf_worker.exe");
        if candidate.exists() { return Some(candidate); }
    }

    let mut dir = exe_parent;
    loop {
        if dir.file_name().and_then(|n| n.to_str()) == Some("deps") {
            if let Some(parent) = dir.parent() {
                let candidate = parent.join("pdf_worker.exe");
                if candidate.exists() { return Some(candidate); }
            }
        }
        if let Some(parent) = dir.parent() {
            dir = parent;
            let candidate = dir.join("pdf_worker.exe");
            if candidate.exists() { return Some(candidate); }
        } else { break; }
    }
    None
}

// ---------------------------------------------------------------------------
// Engine configuration
// ---------------------------------------------------------------------------

pub struct EngineConfig {
    pub channel_capacity: Option<u32>,
    pub tesseract_path: Option<String>,
    pub ocr_language: Option<String>,
    pub ocr_workers: Option<u32>,
    pub ocr_max_dim: Option<u32>,
    pub ram_buffer: Option<u64>,
    pub indexer_batch_size: Option<u32>,
    pub commit_interval: Option<u32>,
    pub commit_timeout: Option<u32>,
    pub extract_workers: Option<u32>,
    pub num_indexer_threads: Option<u32>,
    pub path_filter: Option<String>,
    pub collection_boosts: HashMap<i64, f32>,
    pub boolean_query: Option<Vec<(String, String)>>,
    pub boolean_mode: bool,
    pub render_inverted: bool,
    pub log_cb: Option<extern "C" fn(*const u8, u32)>,
    pub process_cb: Option<extern "C" fn(*const u8, u32)>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
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
            path_filter: None,
            collection_boosts: HashMap::new(),
            boolean_query: None,
            boolean_mode: false,
            render_inverted: false,
            log_cb: None,
            process_cb: None,
        }
    }
}

// ---------------------------------------------------------------------------
// PdfEngine — single unified facade for all CAPI state
// ---------------------------------------------------------------------------

pub struct PdfEngine {
    jobs: Option<Arc<JobStore>>,
    indexer: Option<Arc<Indexer>>,
    db_path: Option<PathBuf>,
    index_path: Option<PathBuf>,
    config: EngineConfig,

    registry: Option<CollectionRegistry>,
    cancel_tokens: HashMap<u32, Arc<AtomicBool>>,
    documents: DocumentManager,
    bitmaps: BitmapManager,
    positions_cache: Mutex<HashMap<(i64, String), Vec<StoredPosition>>>,
}

impl PdfEngine {
    // ── Lifecycle ──

    pub fn new() -> Self {
        Self {
            jobs: None,
            indexer: None,
            db_path: None,
            index_path: None,
            config: EngineConfig::default(),
            registry: None,
            cancel_tokens: HashMap::new(),
            documents: DocumentManager::new(),
            bitmaps: BitmapManager::new(),
            positions_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn init(&mut self, db_path: &Path, index_path: &Path) -> Result<(), i32> {
        if self.jobs.is_some() {
            return Ok(());
        }
        let store = JobStore::open(db_path)
            .map_err(|e| { set_error(format!("Failed to open job store: {}", e)); ERR_GENERAL })?;
        let idx = Indexer::new(index_path)
            .map_err(|e| { set_error(format!("Failed to open index: {}", e)); ERR_GENERAL })?;
        self.jobs = Some(Arc::new(store));
        self.indexer = Some(Arc::new(idx));
        self.db_path = Some(db_path.to_path_buf());
        self.index_path = Some(index_path.to_path_buf());
        Ok(())
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn is_initialized(&self) -> bool {
        self.jobs.is_some()
    }

    // ── Config accessors ──

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut EngineConfig {
        &mut self.config
    }

    // ── JobStore access (for callers that need it) ──

    pub fn jobs(&self) -> Result<&Arc<JobStore>, i32> {
        self.jobs.as_ref().ok_or_else(|| {
            set_error("Engine not initialized (pdf_init not called)".into());
            ERR_NOT_INIT
        })
    }

    pub fn indexer(&self) -> Result<&Arc<Indexer>, i32> {
        self.indexer.as_ref().ok_or_else(|| {
            set_error("Engine not initialized (pdf_init not called)".into());
            ERR_NOT_INIT
        })
    }

    pub fn db_path(&self) -> Result<&PathBuf, i32> {
        self.db_path.as_ref().ok_or_else(|| {
            set_error("Engine not initialized".into());
            ERR_NOT_INIT
        })
    }

    pub fn index_path(&self) -> Result<&PathBuf, i32> {
        self.index_path.as_ref().ok_or_else(|| {
            set_error("Engine not initialized".into());
            ERR_NOT_INIT
        })
    }

    // ── Search ──

    fn build_pipeline(&self, search_index: &SearchIndex, settings: &SearchSettings) -> SearchPipeline {
        let ctx = SearchContext {
            index: search_index.index.clone(),
            id_field: search_index.id_field,
            content_field: search_index.content_field,
            path_field: search_index.path_field,
            position_store: None,
        };
        let builder: Box<dyn pdf_extractor::search::traits::QueryBuilder> =
            if settings.boolean_mode || settings.boolean_query.is_some() {
                Box::new(BooleanPhraseQueryBuilder)
            } else {
                Box::new(AutoPhraseQueryBuilder)
            };
        SearchPipeline::new(ctx, builder, Box::new(TantivyEngine), default_enrichers())
    }

    fn search_input(&self, query: &str, limit: u32, offset: u32, settings: &SearchSettings) -> SearchInput {
        SearchInput {
            query_str: query.to_string(),
            field: None,
            limit: limit as usize,
            offset: offset as usize,
            path_filter: settings.path_filter.clone(),
            strategy: SearchStrategy::AutoPhrase,
        }
    }

    pub fn search(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<serde_json::Value>, u64), i32> {
        let index_path = self.index_path()?.clone();
        let search_index = SearchIndex::new(&index_path)
            .map_err(|e| { set_error(format!("Failed to open index: {}", e)); ERR_GENERAL })?;
        let settings = self.load_search_settings();
        self.do_search(&search_index, query, limit, offset, None, &settings)
    }

    pub fn search_with_collection(
        &self,
        coll_id: u32,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<serde_json::Value>, u64), i32> {
        let (search_index, coll_id_val) = self.open_collection_index(coll_id)?;
        if let Some(idx) = search_index {
            let settings = self.load_search_settings();
            self.do_search(&idx, query, limit, offset, Some(coll_id_val), &settings)
        } else {
            let empty = serde_json::json!({"total": 0, "results": []});
            let s = serde_json::to_string(&empty).unwrap();
            // Return the JSON empty result via buffer - caller handles separately
            let entries = Vec::new();
            let total = 0u64;
            Ok((entries, total))
        }
    }

    fn do_search(
        &self,
        search_index: &SearchIndex,
        query: &str,
        limit: u32,
        offset: u32,
        coll_id: Option<i64>,
        settings: &SearchSettings,
    ) -> Result<(Vec<serde_json::Value>, u64), i32> {
        let pipeline = self.build_pipeline(search_index, settings);
        let input = self.search_input(query, limit, offset, settings);

        let response: SearchResponse = pipeline.execute_to_response(&input)
            .map_err(|e| { set_error(format!("Search failed: {}", e)); ERR_GENERAL })?;

        let json_entries: Vec<serde_json::Value> = response.results.iter().map(|r| {
            let mut entry = serde_json::json!({
                "id": r.doc_id.unwrap_or(0),
                "score": r.score,
                "path": r.path,
                "snippet": r.snippet.as_deref().unwrap_or(""),
            });
            if let Some(cid) = coll_id {
                entry["collection_id"] = serde_json::json!(cid);
            }
            entry
        }).collect();

        Ok((json_entries, response.total_count))
    }

    fn load_search_settings(&self) -> SearchSettings {
        SearchSettings {
            path_filter: self.config.path_filter.clone(),
            boolean_query: self.config.boolean_query.clone(),
            boolean_mode: self.config.boolean_mode,
        }
    }

    /// Execute `search_v2` style search: returns a SearchResponse JSON string
    /// with enrichers applied (snippet, positions, etc.)
    pub fn search_v2(&self, json_input: &str) -> Result<String, i32> {
        let v: serde_json::Value = serde_json::from_str(json_input)
            .map_err(|_| { set_error("Invalid JSON input".into()); ERR_INVALID_PARAM })?;

        let query_str = v["query"].as_str()
            .ok_or_else(|| { set_error("Missing 'query' field".into()); ERR_INVALID_PARAM })?;
        let strategy = v["strategy"].as_str().unwrap_or("auto_phrase");
        let limit = v["limit"].as_u64().unwrap_or(50) as usize;
        let offset = v["offset"].as_u64().unwrap_or(0) as usize;
        let path_filter = v["path_filter"].as_str().map(|s| s.to_string());
        let coll_id = v["collection_id"].as_i64();

        let ctx: SearchContext = if let Some(cid) = coll_id {
            let collection = self.get_collection(cid)?;
            let index_path = PathBuf::from(&collection.data_dir).join(".pdf_extractor").join("index");
            let search_index = SearchIndex::new(&index_path)
                .map_err(|e| { set_error(format!("Failed to open index: {}", e)); ERR_GENERAL })?;
            SearchContext {
                index: search_index.index,
                id_field: search_index.id_field,
                content_field: search_index.content_field,
                path_field: search_index.path_field,
                position_store: None,
            }
        } else {
            let idx_path = self.index_path()?.clone();
            let search_index = SearchIndex::new(&idx_path)
                .map_err(|e| { set_error(format!("Failed to open index: {}", e)); ERR_GENERAL })?;
            SearchContext {
                index: search_index.index,
                id_field: search_index.id_field,
                content_field: search_index.content_field,
                path_field: search_index.path_field,
                position_store: None,
            }
        };

        let builder: Box<dyn pdf_extractor::search::traits::QueryBuilder> = match strategy {
            "boolean_phrase" => Box::new(BooleanPhraseQueryBuilder),
            _ => Box::new(AutoPhraseQueryBuilder),
        };

        let input = SearchInput {
            query_str: query_str.to_string(),
            field: None,
            limit,
            offset,
            path_filter,
            strategy: SearchStrategy::AutoPhrase,
        };

        let pipeline = SearchPipeline::new(ctx, builder, Box::new(TantivyEngine), default_enrichers());
        let response = pipeline.execute_to_response(&input)
            .map_err(|e| { set_error(format!("Search failed: {}", e)); ERR_GENERAL })?;

        serde_json::to_string(&response)
            .map_err(|_| { set_error("JSON serialization failed".into()); ERR_GENERAL })
    }

    pub fn snippet(&self, doc_id: i64, query: &str) -> Result<String, i32> {
        let index_path = self.index_path()?.clone();
        let search_index = SearchIndex::new(&index_path)
            .map_err(|e| { set_error(format!("Failed to open index: {}", e)); ERR_GENERAL })?;

        use tantivy::collector::TopDocs;
        use tantivy::query::TermQuery;
        use tantivy::schema::IndexRecordOption;
        use tantivy::Term;
        use tantivy::TantivyDocument;
        let reader = search_index.index.reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| { set_error(format!("{}", e)); ERR_GENERAL })?;
        let searcher = reader.searcher();
        let id_term = Term::from_field_u64(search_index.id_field, doc_id as u64);
        let id_query = TermQuery::new(id_term, IndexRecordOption::Basic);
        let top_docs = searcher.search(&id_query, &TopDocs::with_limit(1))
            .map_err(|e| { set_error(format!("{}", e)); ERR_GENERAL })?;
        let (_score, doc_address) = top_docs.first().ok_or_else(|| {
            set_error("Document not found".into());
            ERR_NOT_FOUND
        })?;
        let doc = searcher.doc::<TantivyDocument>(*doc_address)
            .map_err(|e| { set_error(format!("{}", e)); ERR_GENERAL })?;
        search_index.generate_snippet(&doc, query)
            .map_err(|e| { set_error(format!("Snippet failed: {}", e)); ERR_GENERAL })
    }

    pub fn search_count(&self, query: &str) -> Result<u64, i32> {
        let index_path = self.index_path()?.clone();
        let search_index = SearchIndex::new(&index_path)
            .map_err(|e| { set_error(format!("Failed to open index: {}", e)); ERR_GENERAL })?;
        search_index.search_count(query)
            .map_err(|e| { set_error(format!("Search count failed: {}", e)); ERR_GENERAL })
    }

    // ── Registry ──

    pub(crate) fn get_registry(&self) -> Result<&CollectionRegistry, i32> {
        self.registry.as_ref().ok_or_else(|| {
            set_error("Registry not initialized".into());
            ERR_REG_NOT_INIT
        })
    }

    fn get_registry_mut(&mut self) -> Result<&mut CollectionRegistry, i32> {
        self.registry.as_mut().ok_or_else(|| {
            set_error("Registry not initialized".into());
            ERR_REG_NOT_INIT
        })
    }

    fn get_collection(&self, coll_id: i64) -> Result<pdf_extractor::registry::CollectionInfo, i32> {
        self.get_registry()?.get_collection(coll_id)
            .map_err(|e| {
                if format!("{}", e).contains("not found") {
                    set_error("Collection not found".into());
                    ERR_COLLECTION_NOT_FOUND
                } else {
                    set_error(format!("{}", e));
                    ERR_GENERAL
                }
            })
    }

    /// Open a collection's search index.  Returns `Ok(None)` if the index
    /// does not exist yet (empty result, not an error).
    fn open_collection_index(&self, coll_id: u32) -> Result<(Option<SearchIndex>, i64), i32> {
        let collection = self.get_collection(coll_id as i64)?;
        let index_path = PathBuf::from(&collection.data_dir).join(".pdf_extractor").join("index");
        match SearchIndex::new(&index_path) {
            Ok(idx) => Ok((Some(idx), collection.id)),
            Err(_) => Ok((None, collection.id)),
        }
    }

    pub fn create_registry(&mut self, dir: &Path) -> Result<(), i32> {
        let reg = CollectionRegistry::open(dir)
            .map_err(|e| { set_error(format!("Failed to create registry: {}", e)); ERR_GENERAL })?;
        self.registry = Some(reg);
        Ok(())
    }

    pub fn add_collection(&self, books_folder: &Path) -> Result<i64, i32> {
        self.get_registry()?.add_collection(books_folder)
            .map_err(|e| { set_error(format!("{}", e)); ERR_GENERAL })
    }

    pub fn remove_collection(&self, coll_id: u32) -> Result<(), i32> {
        self.get_registry()?.remove_collection(coll_id as i64)
            .map_err(|e| { set_error(format!("{}", e)); ERR_GENERAL })
    }

    pub fn list_collections(&self) -> Result<Vec<pdf_extractor::registry::CollectionInfo>, i32> {
        self.get_registry()?.list_collections()
            .map_err(|e| { set_error(format!("{}", e)); ERR_GENERAL })
    }

    pub fn search_count_all(&self, query: &str) -> Result<u64, i32> {
        let collections = self.list_collections()?;
        let mut total: u64 = 0;
        for coll in &collections {
            let index_path = PathBuf::from(&coll.data_dir).join(".pdf_extractor").join("index");
            if !index_path.join("meta.json").exists() {
                continue;
            }
            if let Ok(si) = SearchIndex::new(&index_path) {
                if let Ok(cnt) = si.search_count(query) {
                    total += cnt;
                }
            }
        }
        Ok(total)
    }

    pub fn search_all(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<serde_json::Value>, u64), i32> {
        let collections = self.list_collections()?;
        let settings = self.load_search_settings();
        let boosts = self.config.collection_boosts.clone();

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
            let (mut entries, count) = match self.do_search(&search_index, query, limit, offset, Some(coll.id), &settings) {
                Ok(t) => t,
                Err(_) => return (Vec::new(), 0u64, true),
            };
            let boost = boosts.get(&coll.id).copied().unwrap_or(1.0);
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
            if error { had_error = true; }
            total_all += count;
            all_results.extend(entries);
        }
        if had_error && all_results.is_empty() {
            return Err(ERR_GENERAL);
        }

        all_results.sort_by(|a, b| {
            let sa = a["score"].as_f64().unwrap_or(0.0);
            let sb = b["score"].as_f64().unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });

        let offset_us = offset as usize;
        let limit_us = limit as usize;
        let sliced: Vec<_> = all_results.into_iter().skip(offset_us).take(limit_us).collect();

        Ok((sliced, total_all))
    }

    pub fn collection_stats(&self, coll_id: u32) -> Result<String, i32> {
        let collection = self.get_collection(coll_id as i64)?;
        let index_path = PathBuf::from(&collection.data_dir).join(".pdf_extractor").join("index");
        let search_index = SearchIndex::new(&index_path)
            .map_err(|e| { set_error(format!("Failed to open index: {}", e)); ERR_GENERAL })?;
        let stats = search_index.compute_stats(&index_path)
            .map_err(|e| { set_error(format!("Stats failed: {}", e)); ERR_GENERAL })?;
        let obj = serde_json::json!({
            "num_docs": stats.num_docs,
            "num_segments": stats.num_segments,
            "size_bytes": stats.size_bytes,
        });
        serde_json::to_string(&obj).map_err(|_| ERR_GENERAL)
    }

    pub fn get_errored_jobs(&self, coll_id: u32) -> Result<Vec<serde_json::Value>, i32> {
        let _collection = self.get_collection(coll_id as i64)?;

        let db_path = self.get_registry()?.db_path(coll_id as i64);

        let jobs = JobStore::open(&db_path)
            .map_err(|e| { set_error(format!("Failed to open job store: {}", e)); ERR_GENERAL })?;

        let rows = jobs.fetch_errored()
            .map_err(|e| { set_error(format!("Failed to query errored jobs: {}", e)); ERR_GENERAL })?;

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
        Ok(list)
    }

    // ── Cancellation ──

    pub fn register_cancel_token(&mut self, coll_id: u32) -> Arc<AtomicBool> {
        let token = Arc::new(AtomicBool::new(false));
        self.cancel_tokens.insert(coll_id, token.clone());
        token
    }

    pub fn remove_cancel_token(&mut self, coll_id: u32) {
        self.cancel_tokens.remove(&coll_id);
    }

    pub fn cancel_indexing(&self, coll_id: u32) {
        if let Some(token) = self.cancel_tokens.get(&coll_id) {
            token.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_cancel_requested(&self, coll_id: u32) -> bool {
        self.cancel_tokens.get(&coll_id)
            .map_or(false, |t| t.load(Ordering::Relaxed))
    }

    // ── Document operations ──

    fn pdfium(&self) -> Result<&'static pdf_extractor::pdfium::Pdfium, i32> {
        pdf_extractor::pdfium::Pdfium::global().ok_or_else(|| {
            set_error("pdfium.dll not available".into());
            ERR_GENERAL
        })
    }

    pub fn page_count(&self, path: &Path) -> Result<i32, i32> {
        let pdfium = self.pdfium()?;
        let path_utf16 = pdf_extractor::pdfium::path_to_utf16(path);
        let doc = unsafe { (pdfium.FPDF_LoadDocument)(path_utf16.as_ptr(), std::ptr::null()) };
        if doc.is_null() {
            let err = unsafe { (pdfium.FPDF_GetLastError)() };
            set_error(format!("Failed to load PDF: {}", pdf_extractor::pdfium::error_str(err)));
            return Err(ERR_GENERAL);
        }
        let count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        Ok(count)
    }

    pub fn page_dimensions(&self, path: &Path, page_index: i32) -> Result<(f64, f64), i32> {
        let pdfium = self.pdfium()?;
        let path_utf16 = pdf_extractor::pdfium::path_to_utf16(path);
        let doc = unsafe { (pdfium.FPDF_LoadDocument)(path_utf16.as_ptr(), std::ptr::null()) };
        if doc.is_null() { return Err(ERR_GENERAL); }
        let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
        if page_index < 0 || page_index >= page_count {
            set_error(format!("Page {} out of range ({} pages)", page_index, page_count));
            unsafe { (pdfium.FPDF_CloseDocument)(doc); }
            return Err(ERR_GENERAL);
        }
        let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
        if page.is_null() {
            unsafe { (pdfium.FPDF_CloseDocument)(doc); }
            return Err(ERR_GENERAL);
        }
        let w = unsafe { (pdfium.FPDF_GetPageWidthF)(page) } as f64;
        let h = unsafe { (pdfium.FPDF_GetPageHeightF)(page) } as f64;
        unsafe { (pdfium.FPDF_ClosePage)(page); }
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        Ok((w, h))
    }

    pub fn open_document_mem(&mut self, data: &[u8]) -> Result<i32, i32> {
        let pdfium = self.pdfium()?;
        let doc = unsafe { (pdfium.FPDF_LoadMemDocument)(data.as_ptr(), data.len() as i32, std::ptr::null()) };
        if doc.is_null() {
            let err = unsafe { (pdfium.FPDF_GetLastError)() };
            set_error(format!("PDFium error {}: {}", err, pdf_extractor::pdfium::error_str(err)));
            return Err(ERR_GENERAL);
        }
        let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
        let handle = self.documents.alloc_handle(doc as usize, page_count);
        Ok(handle)
    }

    pub fn close_document(&mut self, handle: i32) -> Result<(), i32> {
        let entry = self.documents.remove(handle).ok_or_else(|| {
            set_error("Invalid document handle".into());
            ERR_INVALID_PARAM
        })?;
        let doc = entry.doc_ptr as *mut std::ffi::c_void;
        let pdfium = self.pdfium()?;
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        Ok(())
    }

    pub fn document_page_count(&self, handle: i32) -> Result<i32, i32> {
        self.documents.get_page_count(handle).ok_or_else(|| {
            set_error("Invalid document handle".into());
            ERR_INVALID_PARAM
        })
    }

    pub fn get_page_dimensions(&self, handle: i32, page_index: i32) -> Result<(f64, f64), i32> {
        let pdfium = self.pdfium()?;
        let doc_ptr = self.documents.get_doc_ptr(handle).ok_or_else(|| {
            set_error("Invalid document handle".into());
            ERR_INVALID_PARAM
        })?;
        let doc = doc_ptr as *mut std::ffi::c_void;
        let pages = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
        if page_index < 0 || page_index >= pages {
            set_error(format!("Page {} out of range ({} pages)", page_index, pages));
            return Err(ERR_GENERAL);
        }
        let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
        if page.is_null() {
            set_error("Failed to load page".into());
            return Err(ERR_GENERAL);
        }
        let w = unsafe { (pdfium.FPDF_GetPageWidthF)(page) } as f64;
        let h = unsafe { (pdfium.FPDF_GetPageHeightF)(page) } as f64;
        let rotation = pdfium.FPDF_GetPageRotation.map_or(0, |f| unsafe { f(page) });
        unsafe { (pdfium.FPDF_ClosePage)(page); }
        if rotation == 1 || rotation == 3 {
            Ok((h, w))
        } else {
            Ok((w, h))
        }
    }

    pub fn get_all_page_dimensions(&self, handle: i32) -> Result<String, i32> {
        let pdfium = self.pdfium()?;
        let doc_ptr = self.documents.get_doc_ptr(handle).ok_or_else(|| {
            set_error("Invalid document handle".into());
            ERR_INVALID_PARAM
        })?;
        let doc = doc_ptr as *mut std::ffi::c_void;
        let total_pages = unsafe { (pdfium.FPDF_GetPageCount)(doc) };

        let mut dims: Vec<[f64; 2]> = Vec::with_capacity(total_pages as usize);
        for i in 0..total_pages {
            let page = unsafe { (pdfium.FPDF_LoadPage)(doc, i) };
            if page.is_null() {
                set_error(format!("Failed to load page {}", i));
                return Err(ERR_GENERAL);
            }
            let w = unsafe { (pdfium.FPDF_GetPageWidthF)(page) } as f64;
            let h = unsafe { (pdfium.FPDF_GetPageHeightF)(page) } as f64;
            let rotation = pdfium.FPDF_GetPageRotation.map_or(0, |f| unsafe { f(page) });
            unsafe { (pdfium.FPDF_ClosePage)(page); }
            if rotation == 1 || rotation == 3 {
                dims.push([h, w]);
            } else {
                dims.push([w, h]);
            }
        }

        serde_json::to_string(&dims).map_err(|e| {
            set_error(format!("Failed to serialize dimensions: {}", e));
            ERR_GENERAL
        })
    }

    pub fn get_page_rotation(&self, handle: i32, page_index: i32) -> Result<i32, i32> {
        let pdfium = self.pdfium()?;
        let doc_ptr = self.documents.get_doc_ptr(handle).ok_or_else(|| {
            set_error("Invalid document handle".into());
            ERR_INVALID_PARAM
        })?;
        let doc = doc_ptr as *mut std::ffi::c_void;
        let pages = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
        if page_index < 0 || page_index >= pages {
            return Err(ERR_GENERAL);
        }
        let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
        if page.is_null() { return Err(ERR_GENERAL); }
        let rotation = pdfium.FPDF_GetPageRotation.map_or(0, |f| unsafe { f(page) });
        unsafe { (pdfium.FPDF_ClosePage)(page); }
        Ok(rotation as i32)
    }

    pub(crate) fn resolve_worker_path() -> Option<PathBuf> {
        resolve_worker_path_from(&std::env::current_exe().ok()?)
    }

    // ── Index collection (pipeline runner) ──

    pub fn index_collection(
        &mut self,
        coll_id: u32,
        flags: u32,
        progress_callback: Option<extern "C" fn(u64, u64)>,
    ) -> Result<i32, i32> {
        let cancel_token = self.register_cancel_token(coll_id);
        let _guard = IndexDropGuard(coll_id, cancel_token.clone());

        let collection = self.get_collection(coll_id as i64)?;
        let canonical = std::fs::canonicalize(Path::new(&collection.books_folder))
            .map_err(|e| { set_error(format!("Books folder not accessible: {}", e)); ERR_GENERAL })?;

        self.get_registry_mut()?.ensure_data_dirs(coll_id as i64)
            .map_err(|e| { set_error(format!("{}", e)); ERR_GENERAL })?;

        let db_path = self.get_registry()?.db_path(coll_id as i64);
        let index_path = self.get_registry()?.index_path(coll_id as i64);

        let jobs = Arc::new(JobStore::open(&db_path)
            .map_err(|e| { set_error(format!("Failed to open job store: {}", e)); ERR_GENERAL })?);
        let metrics = Arc::new(Metrics::new());

        let no_index = (flags & 2) != 0;
        let indexer = if no_index {
            None
        } else {
            let idx = match self.config.ram_buffer {
                Some(buf) => Indexer::with_ram_buffer(&index_path, buf),
                None => Indexer::new(&index_path),
            };
            Some(Arc::new(idx.map_err(|e| {
                set_error(format!("Failed to open index: {}", e));
                ERR_GENERAL
            })?))
        };

        let worker_path = Self::resolve_worker_path().ok_or_else(|| {
            let exe_path = std::env::current_exe().unwrap_or_default();
            set_error(format!(
                "Worker binary 'pdf_worker.exe' not found near '{}'. \
                 The pdf_worker.exe must be deployed alongside the library. \
                 Run 'cargo build -p pdf_extractor --bin pdf_worker' to build it.",
                exe_path.display()
            ));
            ERR_GENERAL
        })?;

        let pipeline_cfg = PipelineConfig {
            channel_capacity: self.config.channel_capacity.map(|v| v as usize),
            num_extract_workers: self.config.extract_workers.map(|v| v as usize),
            num_indexer_threads: self.config.num_indexer_threads.map(|v| v as usize),
            indexer_batch_size: self.config.indexer_batch_size.map(|v| v as usize),
            commit_interval: self.config.commit_interval.map(|v| v as u64),
            commit_timeout: self.config.commit_timeout.map(|v| v as u64),
            worker_path: Some(worker_path),
            progress_cb: progress_callback.map(|cb| {
                Box::new(move |current: u64, total: u64| cb(current, total))
                    as Box<dyn Fn(u64, u64) + Send>
            }),
            cancel_flag: Some(cancel_token.clone()),
            log_cb: self.config.log_cb,
            process_cb: self.config.process_cb,
        };

        let indexer_for_ocr = indexer.clone();
        run_pipeline(
            Arc::clone(&jobs),
            Arc::clone(&metrics),
            &canonical,
            indexer,
            pipeline_cfg,
        ).map_err(|e| {
            set_error(format!("Indexing failed: {}", e));
            ERR_GENERAL
        })?;

        let processed = metrics.processed();
        if let Err(e) = self.get_registry_mut()?.update_index_metadata(coll_id as i64, processed) {
            eprintln!("[pdf_index_collection] failed to update registry metadata: {}", e);
        }

        if (flags & 1) != 0 {

            let tesseract_path = self.config.tesseract_path.clone()
                .map(PathBuf::from)
                .or_else(|| find_tesseract())
                .ok_or_else(|| {
                    set_error("Tesseract not found. Install Tesseract-OCR to the default location, \
                               add it to PATH, or set the path via pdf_set_tesseract_path.".into());
                    ERR_GENERAL
                })?;

            let ocr_language = self.config.ocr_language.clone()
                .unwrap_or_else(|| "eng".to_string());
            let ocr_max_dim = self.config.ocr_max_dim.unwrap_or(3000);
            let num_workers = self.config.ocr_workers.map(|v| v as usize);

            let ocr_config = pdf_extractor::ocr::OcrConfig {
                tesseract_path,
                max_dim: ocr_max_dim,
                max_retries: 2,
                language: ocr_language,
            };

            match run_ocr_post_processing(
                jobs,
                indexer_for_ocr,
                &ocr_config,
                num_workers,
                Some(cancel_token.clone()),
                self.config.log_cb,
            ) {
                Ok(_) => {
                    self.remove_cancel_token(coll_id);
                    Ok(processed as i32)
                },
                Err(e) => {
                    eprintln!("[pdf_index_collection] OCR post-processing failed (non-fatal): {}", e);
                    self.remove_cancel_token(coll_id);
                    Ok(processed as i32)
                }
            }
        } else {
            self.remove_cancel_token(coll_id);
            Ok(processed as i32)
        }
    }

    // ── Term positions ──

    pub fn get_term_positions(
        &self,
        coll_id: u32,
        doc_id: i64,
        term: &str,
    ) -> Result<String, i32> {
        let collection = if coll_id != 0 {
            Some(self.get_collection(coll_id as i64)?)
        } else {
            None
        };

        let index_path = match collection {
            Some(ref c) => PathBuf::from(&c.data_dir).join(".pdf_extractor").join("index"),
            None => self.index_path()?.clone(),
        };

        let positions_db_path = index_path.join("positions.sqlite");
        let position_store = match pdf_extractor::positions::PositionStore::open(&positions_db_path) {
            Ok(store) => store,
            Err(_) => return Ok("[]".to_string()),
        };

        let stripped: String = term.trim_matches('"').to_string();
        let is_phrase = !self.config.boolean_mode && is_search_phrase(&stripped);
        let words: Vec<&str> = stripped.split_whitespace().collect();

        // Load (or retrieve from cache) the full position list for this document.
        let all = {
            let cache_key = (doc_id, index_path.to_string_lossy().to_string());
            let mut cache = self.positions_cache.lock().unwrap();
            if let Some(cached) = cache.get(&cache_key) {
                cached.clone()
            } else {
                let loaded = position_store.load_all_for_doc(doc_id).unwrap_or_default();
                cache.insert(cache_key, loaded.clone());
                loaded
            }
        };

        let all_positions: Vec<pdf_extractor::positions::StoredPosition> = if is_phrase {
            // Phrase query: return only positions forming the exact consecutive phrase
            let by_offset: std::collections::HashMap<usize, &pdf_extractor::positions::StoredPosition> =
                all.iter().map(|p| (p.word_offset, p)).collect();

            let term_offsets: Vec<std::collections::HashSet<usize>> = words.iter().map(|word| {
                all.iter()
                    .filter(|p| p.word_text.to_lowercase() == word.to_lowercase())
                    .map(|p| p.word_offset)
                    .collect()
            }).collect();

            let mut matched_offsets = std::collections::HashSet::new();
            for &start in &term_offsets[0] {
                let mut ok = true;
                for i in 1..words.len() {
                    let expected = start + i;
                    if !term_offsets[i].contains(&expected) {
                        ok = false;
                        break;
                    }
                    if by_offset.get(&start).zip(by_offset.get(&expected))
                        .map_or(true, |(a, b)| a.page != b.page)
                    {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    for i in 0..words.len() {
                        matched_offsets.insert(start + i);
                    }
                }
            }

            all.into_iter()
                .filter(|p| matched_offsets.contains(&p.word_offset))
                .collect()
        } else if stripped.split_whitespace().count() > 1 {
            // Boolean query with operators: support quoted phrases + standalone terms
            let (quoted_phrases, standalone_terms) = parse_phrases_and_terms(term);

            if quoted_phrases.is_empty() {
                // Simple case: only standalone terms
                all.into_iter()
                    .filter(|p| standalone_terms.iter().any(|t| p.word_text.to_lowercase() == *t))
                    .collect()
            } else {
                // Mixed: quoted phrases + standalone terms
                let mut matched_offsets = HashSet::new();

                for phrase in &quoted_phrases {
                    let offsets = get_phrase_match_offsets(&all, phrase);
                    matched_offsets.extend(offsets);
                }

                for pos in &all {
                    if standalone_terms.iter().any(|t| pos.word_text.to_lowercase() == *t) {
                        matched_offsets.insert(pos.word_offset);
                    }
                }

                all.into_iter()
                    .filter(|p| matched_offsets.contains(&p.word_offset))
                    .collect()
            }
        } else {
            // Single-word query: filter in-memory from cached data
            let mut seen = std::collections::HashSet::new();
            let mut positions = Vec::new();
            let word = words.first().copied().unwrap_or("");
            if !word.is_empty() {
                let term_lower = word.to_lowercase();
                for pos in &all {
                    let lower = pos.word_text.to_lowercase();
                    if lower == term_lower || lower.split(' ').any(|seg| seg == term_lower) {
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
                "word_offset": sp.word_offset,
            })
        }).collect();

        serde_json::to_string(&json_entries).map_err(|_| ERR_GENERAL)
    }

    pub fn search_term_offsets(
        &self,
        coll_id: u32,
        doc_id: i64,
        term: &str,
    ) -> Result<String, i32> {
        let collection = if coll_id != 0 {
            Some(self.get_collection(coll_id as i64)?)
        } else {
            None
        };

        let index_path = match collection {
            Some(ref c) => PathBuf::from(&c.data_dir).join(".pdf_extractor").join("index"),
            None => self.index_path()?.clone(),
        };

        let search_index = SearchIndex::new(&index_path)
            .map_err(|e| { set_error(format!("Failed to open index: {}", e)); ERR_GENERAL })?;

        let offsets = search_index.search_term_positions(doc_id as u64, term)
            .map_err(|e| { set_error(format!("Search term positions failed: {}", e)); ERR_GENERAL })?;

        let result = serde_json::json!({
            "doc_id": doc_id,
            "offsets": offsets,
        });
        serde_json::to_string(&result).map_err(|_| ERR_GENERAL)
    }

    // ── Render helpers ──

    pub fn render_thumbnail(&self, path: &Path, page_index: i32, max_dim: i32) -> Result<Vec<u8>, i32> {
        let pdfium = self.pdfium()?;
        let path_utf16 = pdf_extractor::pdfium::path_to_utf16(path);
        let doc = unsafe { (pdfium.FPDF_LoadDocument)(path_utf16.as_ptr(), std::ptr::null()) };
        if doc.is_null() { return Err(ERR_GENERAL); }
        let result = self.render_page_to_png_inner(pdfium, doc, page_index, max_dim);
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        result
    }

    pub fn render_page(&self, path: &Path, page_index: i32, target_width: i32) -> Result<Vec<u8>, i32> {
        let pdfium = self.pdfium()?;
        let path_utf16 = pdf_extractor::pdfium::path_to_utf16(path);
        let doc = unsafe { (pdfium.FPDF_LoadDocument)(path_utf16.as_ptr(), std::ptr::null()) };
        if doc.is_null() { return Err(ERR_GENERAL); }
        let result = self.render_page_to_png_inner(pdfium, doc, page_index, target_width);
        unsafe { (pdfium.FPDF_CloseDocument)(doc); }
        result
    }

    fn apply_highlights_to_buffer(
        &self,
        buf: &mut [u8],
        dest_width: i32,
        dest_height: i32,
        stride: i32,
        page_index: i32,
        scale: f64,
        page: *mut std::ffi::c_void,
        highlight_json: &[u8],
    ) {
        if highlight_json.is_empty() {
            return;
        }
        let json_str = match std::str::from_utf8(highlight_json) {
            Ok(s) => s,
            Err(_) => return,
        };
        let highlights: Vec<serde_json::Value> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(_) => return,
        };
        let pdfium = match self.pdfium() {
            Ok(p) => p,
            Err(_) => return,
        };
        let geom = unsafe { pdf_extractor::pdfium::PageGeometry::from_page(&pdfium, page) };
        let page_num = page_index as u32 + 1;

        for h in &highlights {
            let pos_page = match h.get("page").and_then(|v| v.as_u64()) {
                Some(p) => p,
                None => continue,
            };
            if pos_page != page_num as u64 { continue; }
            let x_min = h.get("x_min").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y_min = h.get("y_min").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let x_max = h.get("x_max").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y_max = h.get("y_max").and_then(|v| v.as_f64()).unwrap_or(0.0);

            let (r_x_min, r_y_min, r_x_max, r_y_max) =
                geom.bbox_stored_to_render(x_min, y_min, x_max, y_max);
            let px1 = (r_x_min * scale).round() as i32;
            let py1 = (r_y_min * scale).round() as i32;
            let px2 = (r_x_max * scale).round() as i32;
            let py2 = (r_y_max * scale).round() as i32;

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
                    if i + 3 >= buf.len() { continue; }
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

    /// Invert BGRA pixel buffer colors (full channel inversion).
    /// White → Black, Black → White, preserving alpha.
    fn invert_page_colors(
        &self,
        buf: &mut [u8],
        dest_width: i32,
        dest_height: i32,
        stride: i32,
    ) {
        for y in 0..dest_height {
            let row_off = (y as usize) * (stride as usize);
            for x in 0..dest_width {
                let i = row_off + (x as usize) * 4;
                if i + 3 >= buf.len() { continue; }
                buf[i]     = 255u8.wrapping_sub(buf[i]);     // B
                buf[i + 1] = 255u8.wrapping_sub(buf[i + 1]); // G
                buf[i + 2] = 255u8.wrapping_sub(buf[i + 2]); // R
                // Alpha (buf[i + 3]) unchanged
            }
        }
    }

    pub fn render_page_bgra(
        &mut self,
        handle: i32,
        page_index: i32,
        dpi: f64,
        highlight_json: Option<&[u8]>,
    ) -> Result<(i32, i32, i32, f64, f64, *mut u8), i32> {
        let pdfium = self.pdfium()?;
        let doc_ptr = self.documents.get_doc_ptr(handle).ok_or_else(|| {
            set_error("Invalid document handle".into());
            ERR_INVALID_PARAM
        })?;
        let doc = doc_ptr as *mut std::ffi::c_void;
        let pages = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
        if page_index < 0 || page_index >= pages {
            return Err(ERR_GENERAL);
        }
        let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
        if page.is_null() { return Err(ERR_GENERAL); }

        let w_pts = unsafe { (pdfium.FPDF_GetPageWidthF)(page) };
        let h_pts = unsafe { (pdfium.FPDF_GetPageHeightF)(page) };
        let rotation = pdfium.FPDF_GetPageRotation.map_or(0, |f| unsafe { f(page) });
        let geom = unsafe { pdf_extractor::pdfium::PageGeometry::from_page(&pdfium, page) };
        let base_w = w_pts as f64;
        let base_h = geom.unrotated_height();

        let scale = dpi / 72.0;
        let dest_width = if rotation == 1 || rotation == 3 {
            (base_h * scale).round() as i32
        } else {
            (base_w * scale).round() as i32
        };
        let dest_height = if rotation == 1 || rotation == 3 {
            (base_w * scale).round() as i32
        } else {
            (base_h * scale).round() as i32
        };

        if dest_width <= 0 || dest_height <= 0 {
            unsafe { (pdfium.FPDF_ClosePage)(page); }
            return Err(ERR_GENERAL);
        }

        let bitmap = unsafe {
            (pdfium.FPDFBitmap_CreateEx)(
                dest_width, dest_height,
                pdf_extractor::pdfium::FPDFBITMAP_BGRA,
                std::ptr::null_mut(),
                0,
            )
        };
        if bitmap.is_null() {
            unsafe { (pdfium.FPDF_ClosePage)(page); }
            return Err(ERR_GENERAL);
        }

        unsafe {
            (pdfium.FPDFBitmap_FillRect)(bitmap, 0, 0, dest_width, dest_height, 0xFFFFFFFF);
            (pdfium.FPDF_RenderPageBitmap)(
                bitmap, page, 0, 0, dest_width, dest_height, 0,
                pdf_extractor::pdfium::FPDF_NONE,
            );
        }

        let buf_ptr = unsafe { (pdfium.FPDFBitmap_GetBuffer)(bitmap) };
        let stride = unsafe { (pdfium.FPDFBitmap_GetStride)(bitmap) };
        if buf_ptr.is_null() || stride <= 0 {
            unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap); (pdfium.FPDF_ClosePage)(page); }
            return Err(ERR_GENERAL);
        }

        let buf_size = (dest_height as usize) * (stride as usize);
        let buf = unsafe { std::slice::from_raw_parts_mut(buf_ptr, buf_size) };

        if self.config.render_inverted {
            self.invert_page_colors(buf, dest_width, dest_height, stride);
        }

        if let Some(hj) = highlight_json {
            self.apply_highlights_to_buffer(buf, dest_width, dest_height, stride, page_index, scale, page, hj);
        }

        unsafe { (pdfium.FPDF_ClosePage)(page); }

        self.bitmaps.insert(buf_ptr as usize, bitmap as usize);

        let (out_w, out_h) = if rotation == 1 || rotation == 3 {
            (h_pts as f64, w_pts as f64)
        } else {
            (w_pts as f64, h_pts as f64)
        };
        Ok((dest_width, dest_height, stride, out_w, out_h, buf_ptr))
    }

    pub fn render_page_to_buffer(
        &self,
        handle: i32,
        page_index: i32,
        dpi: f64,
        highlight_json: Option<&[u8]>,
        buffer: *mut u8,
        width: i32,
        height: i32,
        stride: i32,
        out_width_pts: *mut f64,
        out_height_pts: *mut f64,
    ) -> Result<i32, i32> {
        let pdfium = self.pdfium()?;
        let doc_ptr = self.documents.get_doc_ptr(handle).ok_or_else(|| {
            set_error("Invalid document handle".into());
            ERR_INVALID_PARAM
        })?;
        let doc = doc_ptr as *mut std::ffi::c_void;
        let pages = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
        if page_index < 0 || page_index >= pages {
            return Err(ERR_GENERAL);
        }
        let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
        if page.is_null() { return Err(ERR_GENERAL); }

        let w_pts = unsafe { (pdfium.FPDF_GetPageWidthF)(page) };
        let h_pts = unsafe { (pdfium.FPDF_GetPageHeightF)(page) };
        let rotation = pdfium.FPDF_GetPageRotation.map_or(0, |f| unsafe { f(page) });

        if width <= 0 || height <= 0 || stride <= 0 {
            unsafe { (pdfium.FPDF_ClosePage)(page); }
            return Err(ERR_GENERAL);
        }

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
            return Err(ERR_GENERAL);
        }

        unsafe {
            (pdfium.FPDFBitmap_FillRect)(bitmap, 0, 0, width, height, 0xFFFFFFFF);
            (pdfium.FPDF_RenderPageBitmap)(bitmap, page, 0, 0, width, height, 0, pdf_extractor::pdfium::FPDF_NONE);
        }

        let buf_size = (height as usize) * (stride as usize);
        let buf = unsafe { std::slice::from_raw_parts_mut(buffer, buf_size) };

        if self.config.render_inverted {
            self.invert_page_colors(buf, width, height, stride);
        }

        if let Some(hj) = highlight_json {
            let scale = dpi / 72.0;
            self.apply_highlights_to_buffer(buf, width, height, stride, page_index, scale, page, hj);
        }

        unsafe {
            (pdfium.FPDF_ClosePage)(page);
            (pdfium.FPDFBitmap_Destroy)(bitmap);
        }

        unsafe {
            let (out_w, out_h) = if rotation == 1 || rotation == 3 {
                (h_pts as f64, w_pts as f64)
            } else {
                (w_pts as f64, h_pts as f64)
            };
            *out_width_pts = out_w;
            *out_height_pts = out_h;
        }
        Ok(0)
    }

    fn render_page_to_png_inner(
        &self,
        pdfium: &pdf_extractor::pdfium::Pdfium,
        doc: *mut std::ffi::c_void,
        page_index: i32,
        target_dim: i32,
    ) -> Result<Vec<u8>, i32> {
        let page_count = unsafe { (pdfium.FPDF_GetPageCount)(doc) };
        if page_index < 0 || page_index >= page_count {
            set_error(format!("Page {} out of range ({} pages)", page_index, page_count));
            return Err(ERR_GENERAL);
        }
        let page = unsafe { (pdfium.FPDF_LoadPage)(doc, page_index) };
        if page.is_null() { return Err(ERR_GENERAL); }

        let page_width = unsafe { (pdfium.FPDF_GetPageWidthF)(page) } as f64;
        let page_height = unsafe { (pdfium.FPDF_GetPageHeightF)(page) } as f64;
        let scale = if page_width > page_height {
            target_dim as f64 / page_width
        } else {
            target_dim as f64 / page_height
        };
        let dest_width = (page_width * scale).round() as i32;
        let dest_height = (page_height * scale).round() as i32;
        if dest_width <= 0 || dest_height <= 0 {
            unsafe { (pdfium.FPDF_ClosePage)(page); }
            return Err(ERR_GENERAL);
        }

        let stride = dest_width * 4;
        let buf_size = (stride * dest_height) as usize;
        let mut bitmap_data = vec![0u8; buf_size];
        let bitmap = unsafe {
            (pdfium.FPDFBitmap_CreateEx)(
                dest_width, dest_height,
                pdf_extractor::pdfium::FPDFBITMAP_BGRA,
                bitmap_data.as_mut_ptr() as *mut std::ffi::c_void,
                stride,
            )
        };
        if bitmap.is_null() {
            unsafe { (pdfium.FPDF_ClosePage)(page); }
            return Err(ERR_GENERAL);
        }

        unsafe {
            (pdfium.FPDFBitmap_FillRect)(bitmap, 0, 0, dest_width, dest_height, 0xFFFFFFFF);
            (pdfium.FPDF_RenderPageBitmap)(bitmap, page, 0, 0, dest_width, dest_height, 0, pdf_extractor::pdfium::FPDF_ANNOT);
            (pdfium.FPDF_ClosePage)(page);
        }

        let png = self.bitmap_to_png(&bitmap_data, dest_width, dest_height);
        unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap); }
        png
    }

    fn bitmap_to_png(&self, rgba: &[u8], width: i32, height: i32) -> Result<Vec<u8>, i32> {
        use image::RgbaImage;
        let img = RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())
            .ok_or_else(|| { set_error("Failed to create image buffer".into()); ERR_GENERAL })?;
        let mut png_data = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png_data), image::ImageFormat::Png)
            .map_err(|_| { set_error("PNG encode failed".into()); ERR_GENERAL })?;
        Ok(png_data)
    }

    pub fn free_bitmap(&mut self, pixels: *mut u8) -> i32 {
        if pixels.is_null() {
            return 0;
        }
        let pdfium = match pdf_extractor::pdfium::Pdfium::global() {
            Some(p) => p,
            None => { set_error("pdfium.dll not available".into()); return ERR_GENERAL; }
        };
        if let Some(bitmap_ptr) = self.bitmaps.remove(pixels as usize) {
            unsafe { (pdfium.FPDFBitmap_Destroy)(bitmap_ptr as *mut std::ffi::c_void); }
        }
        0
    }
}

// ---------------------------------------------------------------------------
// Global engine accessor
// ---------------------------------------------------------------------------

static ENGINE: OnceLock<Mutex<Option<PdfEngine>>> = OnceLock::new();

fn engine_lock() -> &'static Mutex<Option<PdfEngine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// Acquire a lock on the global PdfEngine and call `f` with an immutable
/// reference.  Returns `ERR_POISONED` if the mutex is poisoned.
pub fn with_engine<F, T>(f: F) -> Result<T, i32>
where
    F: FnOnce(&PdfEngine) -> Result<T, i32>,
{
    let guard = engine_lock().lock().map_err(|_| ERR_POISONED)?;
    let engine = guard.as_ref().ok_or(ERR_NOT_INIT)?;
    f(engine)
}

/// Acquire a lock on the global PdfEngine and call `f` with a mutable
/// reference.  Returns `ERR_POISONED` if the mutex is poisoned.
pub fn with_engine_mut<F, T>(f: F) -> Result<T, i32>
where
    F: FnOnce(&mut PdfEngine) -> Result<T, i32>,
{
    let mut guard = engine_lock().lock().map_err(|_| ERR_POISONED)?;
    let engine = guard.as_mut().ok_or(ERR_NOT_INIT)?;
    f(engine)
}

/// Ensure the global engine exists (create if needed) and call `f` with
/// a mutable reference.  Unlike `with_engine_mut`, this does NOT require
/// the engine to be initialized.
pub fn ensure_engine_mut<R>(f: impl FnOnce(&mut PdfEngine) -> R) -> R {
    let mut guard = engine_lock().lock().unwrap_or_else(|e| e.into_inner());
    let engine = guard.get_or_insert_with(|| PdfEngine::new());
    f(engine)
}

/// Clear the engine (set to None).  Used by reset functions.
pub fn reset_engine() {
    if let Some(lock) = ENGINE.get() {
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

// ---------------------------------------------------------------------------
// Search settings (internal helper)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct SearchSettings {
    pub path_filter: Option<String>,
    pub boolean_query: Option<Vec<(String, String)>>,
    pub boolean_mode: bool,
}

/// Retorna true si `s` es una consulta multi-palabra sin operadores booleanos,
/// donde la intención es búsqueda de frase exacta (auto-phrase).
fn is_search_phrase(s: &str) -> bool {
    let t = s.trim();
    if !t.contains(' ') {
        return false;
    }
    if t.contains('"') || t.contains('(') || t.contains(')')
        || t.contains('+') || t.contains('-')
    {
        return false;
    }
    !t.split_whitespace().any(|w| {
        w.eq_ignore_ascii_case("AND")
            || w.eq_ignore_ascii_case("OR")
            || w.eq_ignore_ascii_case("NOT")
    })
}

/// Extrae frases entrecomilladas y términos sueltos (sin operadores) del query original.
/// Ej: `"machine learning" AND "signal processing"`
///     → phrases = ["machine learning", "signal processing"], standalone = []
/// Ej: `machine AND learning`
///     → phrases = [], standalone = ["machine", "learning"]
fn parse_phrases_and_terms(query: &str) -> (Vec<String>, Vec<String>) {
    let mut phrases: Vec<String> = Vec::new();
    let mut standalone: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;

    for ch in query.chars() {
        match ch {
            '"' => {
                if in_quote {
                    let phrase = current.trim().to_string();
                    if !phrase.is_empty() {
                        if phrase.contains(' ') {
                            phrases.push(phrase);
                        } else {
                            standalone.push(phrase);
                        }
                    }
                    current.clear();
                    in_quote = false;
                } else {
                    in_quote = true;
                }
            }
            ' ' | '\t' if !in_quote => {
                if !current.is_empty() {
                    let t = current.trim().to_string();
                    if !t.is_empty()
                        && !t.eq_ignore_ascii_case("AND")
                        && !t.eq_ignore_ascii_case("OR")
                        && !t.eq_ignore_ascii_case("NOT")
                    {
                        standalone.push(t.to_lowercase());
                    }
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }

    // Last token
    if !current.is_empty() {
        let t = current.trim().to_string();
        if !t.is_empty()
            && !t.eq_ignore_ascii_case("AND")
            && !t.eq_ignore_ascii_case("OR")
            && !t.eq_ignore_ascii_case("NOT")
        {
            standalone.push(t.to_lowercase());
        }
    }

    // Deduplicate standalone terms
    let mut seen = HashSet::new();
    standalone.retain(|t| seen.insert(t.clone()));

    (phrases, standalone)
}

/// Dado un slice de todas las posiciones del documento y una frase (query multi-palabra
/// sin operadores), retorna los `word_offset` que forman la frase exacta
/// (consecutivos y en la misma página).
fn get_phrase_match_offsets(
    all: &[pdf_extractor::positions::StoredPosition],
    phrase: &str,
) -> HashSet<usize> {
    let phrase_words: Vec<&str> = phrase.split_whitespace().collect();
    if phrase_words.len() < 2 {
        return HashSet::new();
    }

    let by_offset: HashMap<usize, &pdf_extractor::positions::StoredPosition> =
        all.iter().map(|p| (p.word_offset, p)).collect();

    let term_offsets: Vec<HashSet<usize>> = phrase_words.iter().map(|word| {
        all.iter()
            .filter(|p| p.word_text.to_lowercase() == word.to_lowercase())
            .map(|p| p.word_offset)
            .collect()
    }).collect();

    let mut matched = HashSet::new();
    for &start in &term_offsets[0] {
        let mut ok = true;
        for i in 1..phrase_words.len() {
            let expected = start + i;
            if !term_offsets[i].contains(&expected) {
                ok = false;
                break;
            }
            if by_offset.get(&start).zip(by_offset.get(&expected))
                .map_or(true, |(a, b)| a.page != b.page)
            {
                ok = false;
                break;
            }
        }
        if ok {
            for i in 0..phrase_words.len() {
                matched.insert(start + i);
            }
        }
    }
    matched
}
