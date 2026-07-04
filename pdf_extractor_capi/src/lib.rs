use std::ffi::{c_char, CStr, CString};
use std::path::Path;
use std::sync::Arc;

use pdf_extractor::indexer::SearchIndex;

mod engine;
use engine::*;

pub const PDF_EXTRACTOR_API_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// pdf_api_version / pdf_reload_pdfium
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_api_version() -> u32 {
    PDF_EXTRACTOR_API_VERSION
}

#[no_mangle]
pub unsafe extern "C" fn pdf_reload_pdfium() {
    pdf_extractor::pdfium::Pdfium::reset();
}

// ---------------------------------------------------------------------------
// pdf_init
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_init(db_path: *const c_char, index_path: *const c_char) -> i32 {
    ffi_try(|| {
        let db = unsafe { cstr_to_str(db_path)? };
        let idx = unsafe { cstr_to_str(index_path)? };
        ensure_engine_mut(|eng| eng.init(Path::new(db), Path::new(idx)))?;
        Ok(())
    })
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
    ffi_try_with(
        || {
            let q = unsafe { cstr_to_str(query)? };
            with_engine(|eng| eng.search(q, limit, offset))
        },
        |(entries, total)| {
            let wrapped = serde_json::json!({"total": total, "results": entries});
            let s = serde_json::to_string(&wrapped).unwrap_or_else(|_| "{}".into());
            unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) }
        },
    )
}

// ---------------------------------------------------------------------------
// pdf_search_v2
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_search_v2(json_input: *const c_char) -> *mut c_char {
    let input = match unsafe { cstr_to_str(json_input) } {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match with_engine(|eng| eng.search_v2(input)) {
        Ok(json) => CString::new(json)
            .ok()
            .map_or(std::ptr::null_mut(), |cs| cs.into_raw()),
        Err(_) => std::ptr::null_mut(),
    }
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
    ffi_try_with(
        || {
            let q = unsafe { cstr_to_str(query)? };
            with_engine(|eng| eng.snippet(doc_id, q))
        },
        |s| unsafe { write_to_buffer(s.as_bytes(), out, out_len) },
    )
}

// ---------------------------------------------------------------------------
// pdf_get_term_positions
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct PositionQueryInput {
    matched_terms: Vec<String>,
    phrase_groups: Vec<Vec<String>>,
}

#[no_mangle]
pub unsafe extern "C" fn pdf_get_term_positions(
    coll_id: u32,
    doc_id: i64,
    input_json: *const c_char,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || {
            let json = unsafe { cstr_to_str(input_json)? };
            let input: PositionQueryInput = serde_json::from_str(json)
                .map_err(|_| { set_error("Invalid position query JSON".into()); ERR_INVALID_PARAM })?;
            with_engine(|eng| eng.get_term_positions(coll_id, doc_id, &input.matched_terms, &input.phrase_groups))
        },
        |s| unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) },
    )
}

// ---------------------------------------------------------------------------
// pdf_search_term_offsets
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_search_term_offsets(
    coll_id: u32,
    doc_id: i64,
    term: *const c_char,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || {
            let t = unsafe { cstr_to_str(term)? };
            with_engine(|eng| eng.search_term_offsets(coll_id, doc_id, t))
        },
        |s| unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) },
    )
}

// ---------------------------------------------------------------------------
// pdf_page_count
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_page_count(path: *const c_char) -> i32 {
    ffi_try_with(
        || {
            let p = unsafe { cstr_to_str(path)? };
            with_engine(|eng| eng.page_count(Path::new(p)))
        },
        |count| count,
    )
}

// ---------------------------------------------------------------------------
// pdf_page_dimensions
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_page_dimensions(
    path: *const c_char,
    page_index: i32,
    out_width: *mut f64,
    out_height: *mut f64,
) -> i32 {
    ffi_try(|| {
        let p = unsafe { cstr_to_str(path)? };
        let (w, h) = with_engine(|eng| eng.page_dimensions(Path::new(p), page_index))?;
        unsafe {
            *out_width = w;
            *out_height = h;
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// pdf_render_thumbnail
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_render_thumbnail(
    path: *const c_char,
    page_index: i32,
    max_dim: i32,
    out_buf: *mut u8,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || {
            let p = unsafe { cstr_to_str(path)? };
            with_engine(|eng| eng.render_thumbnail(Path::new(p), page_index, max_dim))
        },
        |png| {
            let needed = png.len() as u32;
            if unsafe { *out_len } < needed {
                unsafe { *out_len = needed; }
                return ERR_BUFFER_RETRY;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(png.as_ptr(), out_buf, png.len());
                *out_len = needed;
            }
            0
        },
    )
}

// ---------------------------------------------------------------------------
// pdf_render_page (PNG)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_render_page(
    path: *const c_char,
    page_index: u32,
    target_width: u32,
    out_buf: *mut u8,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || {
            let p = unsafe { cstr_to_str(path)? };
            with_engine(|eng| eng.render_page(Path::new(p), page_index as i32, target_width as i32))
        },
        |png| {
            let needed = png.len() as u32;
            if unsafe { *out_len } < needed {
                unsafe { *out_len = needed; }
                return ERR_BUFFER_RETRY;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(png.as_ptr(), out_buf, png.len());
                *out_len = needed;
            }
            0
        },
    )
}

// ---------------------------------------------------------------------------
// Stateful PDF rendering
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_open_document_mem(data: *const u8, len: i32) -> i32 {
    if data.is_null() || len <= 0 {
        set_error("Invalid PDF data".into());
        return ERR_INVALID_PARAM;
    }
    ffi_try_with(
        || {
            let slice = std::slice::from_raw_parts(data, len as usize);
            with_engine_mut(|eng| eng.open_document_mem(slice))
        },
        |handle| handle,
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_get_page_dimensions(
    handle: i32,
    page_index: i32,
    out_width_pts: *mut f64,
    out_height_pts: *mut f64,
) -> i32 {
    ffi_try(|| {
        let (w, h) = with_engine(|eng| eng.get_page_dimensions(handle, page_index))?;
        unsafe {
            *out_width_pts = w;
            *out_height_pts = h;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_get_all_page_dimensions(
    handle: i32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || with_engine(|eng| eng.get_all_page_dimensions(handle)),
        |s| unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) },
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_get_page_rotation(
    handle: i32,
    page_index: i32,
    out_rotation: *mut i32,
) -> i32 {
    if out_rotation.is_null() {
        return ERR_NULL_PTR;
    }
    ffi_try(|| {
        let rot = with_engine(|eng| eng.get_page_rotation(handle, page_index))?;
        unsafe { *out_rotation = rot; }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_document_page_count(handle: i32) -> i32 {
    ffi_try_with(
        || with_engine(|eng| eng.document_page_count(handle)),
        |count| count,
    )
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
    if out_width.is_null() || out_height.is_null() || out_stride.is_null()
        || out_width_pts.is_null() || out_height_pts.is_null() || out_pixels.is_null()
    {
        return ERR_NULL_PTR;
    }
    let hj: Option<&[u8]> = if !highlight_json.is_null() {
        match unsafe { cstr_to_str(highlight_json as *const std::ffi::c_char) } {
            Ok(s) => Some(s.as_bytes()),
            Err(_) => None,
        }
    } else {
        None
    };
    ffi_try(|| {
        let (w, h, stride, w_pts, h_pts, pixels) =
            with_engine_mut(|eng| eng.render_page_bgra(handle, page_index, dpi, hj))?;
        unsafe {
            *out_width = w;
            *out_height = h;
            *out_stride = stride;
            *out_width_pts = w_pts;
            *out_height_pts = h_pts;
            *out_pixels = pixels;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_render_page_bgra_no_invert(
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
    if out_width.is_null() || out_height.is_null() || out_stride.is_null()
        || out_width_pts.is_null() || out_height_pts.is_null() || out_pixels.is_null()
    {
        return ERR_NULL_PTR;
    }
    let hj: Option<&[u8]> = if !highlight_json.is_null() {
        match unsafe { cstr_to_str(highlight_json as *const std::ffi::c_char) } {
            Ok(s) => Some(s.as_bytes()),
            Err(_) => None,
        }
    } else {
        None
    };
    ffi_try(|| {
        let (w, h, stride, w_pts, h_pts, pixels) =
            with_engine_mut(|eng| eng.render_page_bgra_no_invert(handle, page_index, dpi, hj))?;
        unsafe {
            *out_width = w;
            *out_height = h;
            *out_stride = stride;
            *out_width_pts = w_pts;
            *out_height_pts = h_pts;
            *out_pixels = pixels;
        }
        Ok(())
    })
}

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
    if out_width_pts.is_null() || out_height_pts.is_null() {
        return ERR_NULL_PTR;
    }
    let hj: Option<&[u8]> = if !highlight_json.is_null() {
        match unsafe { cstr_to_str(highlight_json as *const std::ffi::c_char) } {
            Ok(s) => Some(s.as_bytes()),
            Err(_) => None,
        }
    } else {
        None
    };
    ffi_try(|| {
        with_engine(|eng| eng.render_page_to_buffer(
            handle, page_index, dpi, hj, buffer, width, height, stride,
            out_width_pts, out_height_pts,
        ))?;
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_free_bitmap(pixels: *mut u8) {
    if pixels.is_null() {
        return;
    }
    let _ = with_engine_mut(|eng| { eng.free_bitmap(pixels); Ok::<(), i32>(()) });
}

#[no_mangle]
pub unsafe extern "C" fn pdf_close_document(handle: i32) -> i32 {
    ffi_try(|| with_engine_mut(|eng| eng.close_document(handle)))
}

// ---------------------------------------------------------------------------
// pdf_last_error
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_last_error(out: *mut c_char, out_len: *mut u32) -> i32 {
    let msg = take_last_error().unwrap_or_default();
    unsafe { write_to_buffer(msg.as_bytes(), out, out_len) }
}

// ---------------------------------------------------------------------------
// pdf_find_tesseract
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_find_tesseract(out: *mut c_char, out_len: *mut u32) -> i32 {
    match pdf_extractor::ocr::find_tesseract() {
        Some(path) => {
            let s = path.to_string_lossy();
            unsafe { write_to_buffer(s.as_bytes(), out, out_len) }
        }
        None => {
            set_error("Tesseract not found".into());
            ERR_GENERAL
        }
    }
}

// ---------------------------------------------------------------------------
// pdf_extract (legacy)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_extract(
    input_dir: *const c_char,
    progress: Option<extern "C" fn(u64, u64)>,
) -> i32 {
    ffi_try_with(
        || -> Result<i32, i32> {
            let input = unsafe { cstr_to_str(input_dir)? };
            let db = with_engine(|eng| eng.db_path().map(|p| p.clone()))?;
            let idx = with_engine(|eng| eng.index_path().map(|p| p.clone()))?;

            let jobs = pdf_extractor::scanner::JobStore::open(&db)
                .map_err(|e| { set_error(format!("Failed to open job store: {}", e)); ERR_GENERAL })?;
            let metrics = Arc::new(pdf_extractor::metrics::Metrics::new());
            let indexer = pdf_extractor::indexer::Indexer::new(&idx)
                .map_err(|e| { set_error(format!("{}", e)); ERR_GENERAL })?;

            let config = pdf_extractor::pipeline::PipelineConfig {
                progress_cb: progress.map(|cb| {
                    Box::new(move |c: u64, t: u64| cb(c, t)) as Box<dyn Fn(u64, u64) + Send>
                }),
                worker_path: None,
                ..Default::default()
            };

            pdf_extractor::pipeline::run_pipeline(
                Arc::new(jobs),
                metrics.clone(),
                &std::path::PathBuf::from(input),
                Some(Arc::new(indexer)),
                config,
            )
            .map_err(|e| { set_error(format!("Extraction failed: {}", e)); ERR_GENERAL })?;

            Ok(metrics.processed() as i32)
        },
        |count| count,
    )
}

// ---------------------------------------------------------------------------
// pdf_stats
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_stats(_coll_id: u32, out_json: *mut c_char, out_len: *mut u32) -> i32 {
    ffi_try_with(
        || {
            let collections = with_engine(|eng| eng.list_collections())?;
            let mut total_docs = 0u64;
            let mut total_collections = 0u64;
            for coll in &collections {
                let index_path =
                    std::path::PathBuf::from(&coll.data_dir).join(".pdf_extractor").join("index");
                if let Ok(si) = SearchIndex::new(&index_path) {
                    if let Ok(stats) = si.compute_stats(&index_path) {
                        total_docs += stats.num_docs;
                    }
                }
                total_collections += 1;
            }
            let obj = serde_json::json!({
                "total_collections": total_collections,
                "total_docs": total_docs,
                "indexed_collections": total_collections,
            });
            serde_json::to_string(&obj).map_err(|_| ERR_GENERAL)
        },
        |s| unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) },
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_free_string(_s: *mut c_char) {}

// ---------------------------------------------------------------------------
// Config setters
// ---------------------------------------------------------------------------

macro_rules! def_u32_setter {
    ($name:ident, $field:ident) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(value: u32) -> i32 {
            ffi_try(|| {
                ensure_engine_mut(|eng| eng.config_mut().$field = Some(value));
                Ok(())
            })
        }
    };
}

def_u32_setter!(pdf_set_ocr_workers, ocr_workers);
def_u32_setter!(pdf_set_ocr_max_dim, ocr_max_dim);
def_u32_setter!(pdf_set_indexer_batch_size, indexer_batch_size);
def_u32_setter!(pdf_set_commit_interval, commit_interval);
def_u32_setter!(pdf_set_commit_timeout, commit_timeout);
def_u32_setter!(pdf_set_extract_workers, extract_workers);
def_u32_setter!(pdf_set_indexer_threads, num_indexer_threads);

fn string_setter(value: *const c_char, f: impl FnOnce(Option<String>)) -> i32 {
    ffi_try(|| {
        if value.is_null() {
            f(None);
        } else {
            let s = unsafe { cstr_to_str(value)?.to_string() };
            f(Some(s));
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_tesseract_path(value: *const c_char) -> i32 {
    string_setter(value, |v| {
        ensure_engine_mut(|eng| eng.config_mut().tesseract_path = v);
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_ocr_language(value: *const c_char) -> i32 {
    string_setter(value, |v| {
        ensure_engine_mut(|eng| eng.config_mut().ocr_language = v);
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_path_filter(value: *const c_char) -> i32 {
    string_setter(value, |v| {
        ensure_engine_mut(|eng| eng.config_mut().path_filter = v);
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_log_callback(cb: Option<extern "C" fn(*const u8, u32)>) -> i32 {
    ffi_try(|| {
        ensure_engine_mut(|eng| eng.config_mut().log_cb = cb);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_process_callback(cb: Option<extern "C" fn(*const u8, u32)>) -> i32 {
    ffi_try(|| {
        ensure_engine_mut(|eng| eng.config_mut().process_cb = cb);
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_log_path(path: *const c_char) -> i32 {
    if path.is_null() {
        pdf_extractor::pipeline::close_pipeline_log();
        return 0;
    }
    let path_str = match unsafe { CStr::from_ptr(path).to_str() } {
        Ok(s) => s,
        Err(_) => return ERR_GENERAL,
    };
    pdf_extractor::pipeline::set_pipeline_log_path(std::path::Path::new(path_str))
        .map(|_| 0)
        .unwrap_or(ERR_GENERAL)
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_ram_buffer(value: u64) -> i32 {
    ffi_try(|| {
        ensure_engine_mut(|eng| eng.config_mut().ram_buffer = Some(value));
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_channel_capacity(value: u32) -> i32 {
    if value == 0 {
        set_error("Channel capacity must be > 0".into());
        return ERR_INVALID_PARAM;
    }
    ffi_try(|| {
        ensure_engine_mut(|eng| eng.config_mut().channel_capacity = Some(value));
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_collection_boost(coll_id: u32, weight: f32) -> i32 {
    if weight <= 0.0 {
        set_error("Collection boost must be > 0.0".into());
        return ERR_INVALID_PARAM;
    }
    ffi_try(|| {
        ensure_engine_mut(|eng| {
            eng.config_mut().collection_boosts.insert(coll_id as i64, weight);
        });
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_boolean_query(json: *const c_char) -> i32 {
    ffi_try(|| {
        if json.is_null() {
            ensure_engine_mut(|eng| eng.config_mut().boolean_query = None);
            return Ok(());
        }
        let s = unsafe { cstr_to_str(json)? };
        let arr: Vec<serde_json::Value> = serde_json::from_str(s)
            .map_err(|e| { set_error(format!("Invalid boolean query JSON: {}", e)); ERR_INVALID_PARAM })?;
        if arr.is_empty() {
            set_error("Boolean query must have at least one clause".into());
            return Err(ERR_INVALID_PARAM);
        }
        let mut clauses = Vec::with_capacity(arr.len());
        for (i, clause) in arr.iter().enumerate() {
            let term = clause.get("term")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    set_error(format!("Clause {} missing non-empty 'term'", i));
                    ERR_INVALID_PARAM
                })?;
            let occur = match clause.get("occur").and_then(|v| v.as_str()) {
                Some("should") => "should",
                Some("must_not") => "must_not",
                Some("must") | None => "must",
                Some(other) => {
                    set_error(format!("Clause {} invalid 'occur': '{}'", i, other));
                    return Err(ERR_INVALID_PARAM);
                }
            };
            clauses.push((term.to_string(), occur.to_string()));
        }
        ensure_engine_mut(|eng| eng.config_mut().boolean_query = Some(clauses));
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_search_boolean_mode(enabled: i32) -> i32 {
    ffi_try(|| {
        ensure_engine_mut(|eng| {
            eng.config_mut().boolean_mode = enabled != 0;
            Ok(())
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_render_inverted(enabled: i32) -> i32 {
    ffi_try(|| {
        ensure_engine_mut(|eng| {
            eng.config_mut().render_inverted = enabled != 0;
            Ok(())
        })
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_set_highlight_color(r: u8, g: u8, b: u8, alpha: u8) -> i32 {
    ffi_try(|| {
        ensure_engine_mut(|eng| {
            let c = eng.config_mut();
            c.highlight_color = (r, g, b);
            c.highlight_alpha = alpha;
            Ok(())
        })
    })
}

// ---------------------------------------------------------------------------
// Registry API
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_create_registry(registry_dir: *const c_char) -> i32 {
    ffi_try(|| {
        let dir = unsafe { cstr_to_str(registry_dir)? };
        ensure_engine_mut(|eng| {
            eng.create_registry(Path::new(dir))
                .map_err(|e| e)
        })?;
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_add_collection(books_folder: *const c_char) -> i32 {
    ffi_try_with(
        || {
            let path = unsafe { cstr_to_str(books_folder)? };
            with_engine(|eng| eng.add_collection(Path::new(path)))
        },
        |id| id as i32,
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_remove_collection(coll_id: u32) -> i32 {
    ffi_try(|| with_engine(|eng| eng.remove_collection(coll_id)))
}

#[no_mangle]
pub unsafe extern "C" fn pdf_list_collections(out_json: *mut c_char, out_len: *mut u32) -> i32 {
    ffi_try_with(
        || {
            let cols = with_engine(|eng| eng.list_collections())?;
            let entries: Vec<serde_json::Value> = cols
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
            serde_json::to_string(&entries).map_err(|_| ERR_GENERAL)
        },
        |s| unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) },
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_index_collection(
    coll_id: u32,
    flags: u32,
    progress_callback: Option<extern "C" fn(u64, u64)>,
) -> i32 {
    ffi_try_with(
        || with_engine_mut(|eng| eng.index_collection(coll_id, flags, progress_callback)),
        |processed| processed,
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_cancel_indexing(coll_id: u32) {
    let _ = with_engine(|eng| { eng.cancel_indexing(coll_id); Ok(()) });
}

#[no_mangle]
pub unsafe extern "C" fn pdf_is_cancel_requested(coll_id: u32) -> i32 {
    with_engine(|eng| {
        Ok(if eng.is_cancel_requested(coll_id) { 1 } else { 0 })
    })
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_close_collection(_coll_id: u32) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn pdf_close_all() -> i32 {
    reset_engine();
    take_last_error();
    0
}

#[no_mangle]
pub unsafe extern "C" fn pdf_reset_all() {
    reset_engine();
    take_last_error();
}

// ---------------------------------------------------------------------------
// Search helpers
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pdf_search_collection(
    coll_id: u32,
    query: *const c_char,
    limit: u32,
    offset: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || {
            let q = unsafe { cstr_to_str(query)? };
            with_engine(|eng| eng.search_with_collection(coll_id, q, limit, offset))
        },
        |(entries, total)| {
            let wrapped = serde_json::json!({"total": total, "results": entries});
            let s = serde_json::to_string(&wrapped).unwrap_or_else(|_| "{}".into());
            unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) }
        },
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_search_all(
    query: *const c_char,
    limit: u32,
    offset: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || {
            let q = unsafe { cstr_to_str(query)? };
            with_engine(|eng| eng.search_all(q, limit, offset))
        },
        |(entries, total)| {
            let wrapped = serde_json::json!({"total": total, "results": entries});
            let s = serde_json::to_string(&wrapped).unwrap_or_else(|_| "{}".into());
            unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) }
        },
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_search_count(query: *const c_char, out_count: *mut u64) -> i32 {
    ffi_try(|| {
        let q = unsafe { cstr_to_str(query)? };
        let count = with_engine(|eng| eng.search_count(q))?;
        unsafe { *out_count = count; }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_search_count_all(query: *const c_char, out_count: *mut u64) -> i32 {
    ffi_try(|| {
        let q = unsafe { cstr_to_str(query)? };
        let count = with_engine(|eng| eng.search_count_all(q))?;
        unsafe { *out_count = count; }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn pdf_get_problematic_jobs(
    coll_id: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || with_engine(|eng| eng.get_errored_jobs(coll_id)),
        |list| {
            let s = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
            unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) }
        },
    )
}

#[no_mangle]
pub unsafe extern "C" fn pdf_collection_stats(
    coll_id: u32,
    out_json: *mut c_char,
    out_len: *mut u32,
) -> i32 {
    ffi_try_with(
        || with_engine(|eng| eng.collection_stats(coll_id)),
        |s| unsafe { write_to_buffer(s.as_bytes(), out_json, out_len) },
    )
}

// ── Internal helpers used by tests (kept for backward compat) ──

#[cfg(test)]
fn reset_state() {
    reset_engine();
    take_last_error();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::atomic::Ordering;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn setup_temp_index() -> (String, String) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        assert_eq!(rc, ERR_NULL_PTR);
    }

    #[test]
    fn test_search_null_query() {
        let rc = unsafe { pdf_search(std::ptr::null(), 10, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, ERR_NULL_PTR);
    }

    #[test]
    fn test_search_before_init() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_NOT_INIT);
    }

    #[test]
    fn test_snippet_before_init() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("hello").unwrap();
        let rc = unsafe { pdf_snippet(1, q.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_NOT_INIT);
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
        let rc = unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) };
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
        let rc = unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) };
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

        let empty_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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

        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);
        let empty_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        assert_eq!(rc, ERR_INVALID_PARAM);
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
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_stats_reg_{}", TEST_COUNTER.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let rc = unsafe { pdf_stats(0, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(result).unwrap();
        assert!(v.get("total_collections").is_some());
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
        let rc = unsafe { pdf_render_thumbnail(p.as_ptr(), 1, 100, std::ptr::null_mut(), &mut len) };
        assert!(rc != 0);
    }

    fn create_test_pdf(path: &PathBuf) {
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
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::create_dir_all(&pdf_dir);
        let pdf_path = pdf_dir.join("test.pdf");
        create_test_pdf(&pdf_path);
        let p = CString::new(pdf_path.to_string_lossy().as_ref()).unwrap();

        let mut buf = [0u8; 65536];
        let mut len = 65536u32;
        let rc = unsafe { pdf_render_thumbnail(p.as_ptr(), 99, 100, buf.as_mut_ptr(), &mut len) };
        assert!(rc != 0);
    }

    #[test]
    fn test_thumbnail_valid_pdf_non_existent_page() {
        reset_state();
        let pdf_dir = std::env::temp_dir().join(format!("pdf_capi_thumb2_{}",
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::create_dir_all(&pdf_dir);
        let pdf_path = pdf_dir.join("test.pdf");
        create_test_pdf(&pdf_path);
        let p = CString::new(pdf_path.to_string_lossy().as_ref()).unwrap();
        let mut buf = [0u8; 65536];
        let mut len = 65536u32;
        let rc = unsafe { pdf_render_thumbnail(p.as_ptr(), 0, 100, buf.as_mut_ptr(), &mut len) };
        assert!(rc != 0);
    }

    #[test]
    fn test_thumbnail_null_params() {
        reset_state();
        let rc = unsafe {
            pdf_render_thumbnail(std::ptr::null(), 0, 0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(rc, ERR_NULL_PTR);
    }

    #[test]
    fn test_buffer_too_small() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let mut buf = [0u8; 1];
        let mut len = 1u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_BUFFER_RETRY);
        assert!(len > 1);
    }

    #[test]
    fn test_create_registry_null_path() {
        reset_state();
        let rc = unsafe { pdf_create_registry(std::ptr::null()) };
        assert_eq!(rc, ERR_NULL_PTR);
    }

    #[test]
    fn test_create_registry_and_add_list() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        assert_eq!(rc, ERR_NOT_INIT);
    }

    #[test]
    fn test_search_collection_nonexistent() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_nx_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_collection(999, q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_COLLECTION_NOT_FOUND);
    }

    #[test]
    fn test_search_all_before_registry() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_all(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_NOT_INIT);
    }

    #[test]
    fn test_search_all_no_collections() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_em_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_all(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let json = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["total"], 0);
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_setters_stored_and_used() {
        reset_state();
        assert_eq!(unsafe { pdf_set_ram_buffer(100_000_000) }, 0);
        assert_eq!(unsafe { pdf_set_ocr_workers(3) }, 0);
        assert_eq!(unsafe { pdf_set_ocr_max_dim(2000) }, 0);
        assert_eq!(unsafe { pdf_set_indexer_batch_size(200) }, 0);
        assert_eq!(unsafe { pdf_set_commit_interval(1000) }, 0);
        assert_eq!(unsafe { pdf_set_commit_timeout(60) }, 0);
        assert_eq!(unsafe { pdf_set_extract_workers(4) }, 0);
        assert_eq!(unsafe { pdf_set_indexer_threads(2) }, 0);

        let lang = CString::new("por").unwrap();
        assert_eq!(unsafe { pdf_set_ocr_language(lang.as_ptr()) }, 0);
        let tesseract = CString::new("C:\\tools\\tesseract.exe").unwrap();
        assert_eq!(unsafe { pdf_set_tesseract_path(tesseract.as_ptr()) }, 0);
        let filter = CString::new("science").unwrap();
        assert_eq!(unsafe { pdf_set_path_filter(filter.as_ptr()) }, 0);
        assert_eq!(unsafe { pdf_set_path_filter(std::ptr::null()) }, 0);
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
        ).unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_boolean_query_null_resets() {
        reset_state();
        assert_eq!(unsafe { pdf_set_boolean_query(std::ptr::null()) }, 0);
    }

    #[test]
    fn test_set_boolean_query_empty_array() {
        reset_state();
        let json = CString::new("[]").unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, ERR_INVALID_PARAM);
    }

    #[test]
    fn test_set_boolean_query_missing_term() {
        reset_state();
        let json = CString::new(r#"[{"occur": "must"}]"#).unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, ERR_INVALID_PARAM);
    }

    #[test]
    fn test_set_boolean_query_invalid_occur() {
        reset_state();
        let json = CString::new(r#"[{"term": "test", "occur": "invalid"}]"#).unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, ERR_INVALID_PARAM);
    }

    #[test]
    fn test_set_boolean_query_invalid_json() {
        reset_state();
        let json = CString::new("not json").unwrap();
        let rc = unsafe { pdf_set_boolean_query(json.as_ptr()) };
        assert_eq!(rc, ERR_INVALID_PARAM);
    }

    #[test]
    fn test_set_collection_boost_valid() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cb_v_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };

        let rc = unsafe { pdf_set_collection_boost(coll_id as u32, 2.0) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_set_collection_boost_zero() {
        reset_state();
        let rc = unsafe { pdf_set_collection_boost(1, 0.0) };
        assert_eq!(rc, ERR_INVALID_PARAM);
    }

    #[test]
    fn test_set_collection_boost_negative() {
        reset_state();
        let rc = unsafe { pdf_set_collection_boost(1, -1.0) };
        assert_eq!(rc, ERR_INVALID_PARAM);
    }

    #[test]
    fn test_index_collection_not_indexed() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_ix_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };

        let rc = unsafe { pdf_index_collection(coll_id as u32, 0, None) };
        assert_eq!(rc, 0);

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_collection(coll_id as u32, q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) };
        if rc == 0 {
            let json = std::str::from_utf8(&buf[..len as usize]).unwrap();
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(v["total"], 0);
        }
    }

    #[test]
    fn test_list_collections_empty() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_reg_mc_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        for i in 0..3 {
            let books_dir = reg_dir.join(format!("books_{}", i));
            std::fs::create_dir_all(&books_dir).unwrap();
            let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
            unsafe { pdf_add_collection(books_c.as_ptr()) };
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
        ensure_engine_mut(|eng| { eng.register_cancel_token(0); });
        unsafe { pdf_cancel_indexing(0) };
        assert_eq!(unsafe { pdf_is_cancel_requested(0) }, 1);
        reset_state();
        ensure_engine_mut(|eng| { eng.register_cancel_token(0); });
        assert_eq!(unsafe { pdf_is_cancel_requested(0) }, 0);
    }

    #[test]
    fn test_cancel_flag_reset_by_index_collection() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cancel_reset_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };

        let _rc = unsafe { pdf_index_collection(coll_id as u32, 0, None) };
        assert_eq!(unsafe { pdf_is_cancel_requested(coll_id as u32) }, 0,
            "flag should be 0 after pdf_index_collection (fresh token)");
    }

    #[test]
    fn test_cancel_then_extract_resets_flag() {
        reset_state();
        let empty_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let empty_dir = std::env::temp_dir().join(format!("pdf_capi_extract_cancel_{}", empty_dir_n));
        let _ = std::fs::remove_dir_all(&empty_dir);
        std::fs::create_dir_all(&empty_dir).unwrap();
        let dir_c = CString::new(empty_dir.to_string_lossy().as_ref()).unwrap();
        let _rc = unsafe { pdf_extract(dir_c.as_ptr(), None) };
    }

    #[test]
    fn test_cancel_multiple_times() {
        reset_state();
        ensure_engine_mut(|eng| { eng.register_cancel_token(0); });
        unsafe { pdf_cancel_indexing(0) };
        unsafe { pdf_cancel_indexing(0) };

        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        assert_eq!(rc, ERR_NOT_INIT);
    }

    #[test]
    fn test_search_count_null_query() {
        reset_state();
        let mut count: u64 = 0;
        let rc = unsafe { pdf_search_count(std::ptr::null(), &mut count) };
        assert_eq!(rc, ERR_NULL_PTR);
    }

    #[test]
    fn test_search_count_null_out() {
        reset_state();
        let q = CString::new("test").unwrap();
        let rc = unsafe { pdf_search_count(q.as_ptr(), std::ptr::null_mut()) };
        assert_eq!(rc, ERR_NOT_INIT);
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
        assert_eq!(rc, ERR_NOT_INIT);
    }

    #[test]
    fn test_search_count_all_null_query() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_sca_null_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut count: u64 = 0;
        let rc = unsafe { pdf_search_count_all(std::ptr::null(), &mut count) };
        assert_eq!(rc, ERR_NULL_PTR);
    }

    #[test]
    fn test_search_count_all_empty_registry() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cs_nx_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let rc = unsafe { pdf_collection_stats(999, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_COLLECTION_NOT_FOUND);
    }

    #[test]
    fn test_collection_stats_before_registry() {
        reset_state();
        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let rc = unsafe { pdf_collection_stats(1, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_NOT_INIT);
    }

    #[test]
    fn test_get_term_positions_before_init() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let input = CString::new(r#"{"matched_terms":["hello"],"phrase_groups":[["hello"]]}"#).unwrap();
        let rc = unsafe { pdf_get_term_positions(0, 1, input.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_NOT_INIT);
    }

    #[test]
    fn test_get_term_positions_null_input() {
        reset_state();
        let mut buf = [0u8; 128];
        let mut len = 128u32;
        let rc = unsafe { pdf_get_term_positions(0, 1, std::ptr::null(), buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, ERR_NULL_PTR);
    }

    #[test]
    fn test_get_term_positions_basic() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let _ = with_engine_mut(|eng| {
            let indexer = eng.indexer().map_err(|e| e)?;
            indexer.index_document(1, "/doc.pdf", "hello world hello").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

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
        with_engine_mut(|eng| {
            let indexer = eng.indexer().map_err(|e| e)?;
            indexer.store_word_positions(1, &word_positions).unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let input = CString::new(r#"{"matched_terms":["hello"],"phrase_groups":[["hello"]]}"#).unwrap();
        let rc = unsafe { pdf_get_term_positions(0, 1, input.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(result).unwrap();
        let positions = v.as_array().unwrap();
        assert_eq!(positions.len(), 2);
    }

    #[test]
    fn test_get_term_positions_phrase_match_consecutive() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let word_positions = vec![
            (0usize, pdf_extractor::extractor::WordPosition {
                page: 1, x_min: 10.0, y_min: 20.0, x_max: 50.0, y_max: 30.0,
                text: "machine".to_string(),
            }),
            (1usize, pdf_extractor::extractor::WordPosition {
                page: 1, x_min: 60.0, y_min: 20.0, x_max: 100.0, y_max: 30.0,
                text: "learning".to_string(),
            }),
            (2usize, pdf_extractor::extractor::WordPosition {
                page: 1, x_min: 110.0, y_min: 20.0, x_max: 120.0, y_max: 30.0,
                text: "is".to_string(),
            }),
            (3usize, pdf_extractor::extractor::WordPosition {
                page: 2, x_min: 10.0, y_min: 40.0, x_max: 50.0, y_max: 50.0,
                text: "learning".to_string(),
            }),
            (4usize, pdf_extractor::extractor::WordPosition {
                page: 2, x_min: 60.0, y_min: 40.0, x_max: 100.0, y_max: 50.0,
                text: "machine".to_string(),
            }),
            (5usize, pdf_extractor::extractor::WordPosition {
                page: 2, x_min: 110.0, y_min: 40.0, x_max: 150.0, y_max: 50.0,
                text: "cool".to_string(),
            }),
            (6usize, pdf_extractor::extractor::WordPosition {
                page: 3, x_min: 10.0, y_min: 10.0, x_max: 50.0, y_max: 20.0,
                text: "machine".to_string(),
            }),
            (7usize, pdf_extractor::extractor::WordPosition {
                page: 3, x_min: 60.0, y_min: 10.0, x_max: 100.0, y_max: 20.0,
                text: "learning".to_string(),
            }),
        ];
        with_engine_mut(|eng| {
            let indexer = eng.indexer().map_err(|e| e)?;
            indexer.store_word_positions(1, &word_positions).unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let input = CString::new(
            r#"{"matched_terms":["machine","learning"],"phrase_groups":[["machine","learning"]]}"#
        ).unwrap();
        let rc = unsafe { pdf_get_term_positions(0, 1, input.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(result).unwrap();
        let positions = v.as_array().unwrap();

        // Should return ONLY phrase matches:
        // - Page 1: "machine" at offset 0 + "learning" at offset 1 (consecutive, same page) → MATCH
        // - Page 2: "learning" at offset 3 + "machine" at offset 4 (not consecutive, reversed) → NO MATCH
        // - Page 3: "machine" at offset 6 + "learning" at offset 7 (consecutive, same page) → MATCH
        // Total: 4 positions (2 phrase matches × 2 words each)
        assert_eq!(positions.len(), 4, "Expected 4 phrase positions, got {}: {:?}", positions.len(), positions);

        // Verify page distribution
        for pos in positions {
            let page = pos["page"].as_i64().unwrap();
            assert!(page == 1 || page == 3, "Position on unexpected page {}", page);
        }
    }

    #[test]
    fn test_get_term_positions_no_match() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let _ = with_engine_mut(|eng| {
            eng.indexer().map_err(|e| e)?.index_document(1, "/doc.pdf", "hello world").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let input = CString::new(r#"{"matched_terms":["nonexistent"],"phrase_groups":[["nonexistent"]]}"#).unwrap();
        let rc = unsafe { pdf_get_term_positions(0, 1, input.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(result).unwrap();
        let positions = v.as_array().unwrap();
        assert_eq!(positions.len(), 0);
    }

    #[test]
    fn test_get_term_positions_nonexistent_doc() {
        reset_state();
        let (db, idx) = setup_temp_index();
        let db_c = CString::new(db.clone()).unwrap();
        let idx_c = CString::new(idx.clone()).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);

        let _ = with_engine_mut(|eng| {
            eng.indexer().map_err(|e| e)?.index_document(1, "/doc.pdf", "hello world").unwrap();
            Ok::<_, i32>(())
        }).unwrap();

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let input = CString::new(r#"{"matched_terms":["hello"],"phrase_groups":[["hello"]]}"#).unwrap();
        let rc = unsafe { pdf_get_term_positions(0, 999, input.as_ptr(), buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert_eq!(rc, 0);
        let result = std::str::from_utf8(&buf[..len as usize]).unwrap();
        let v: serde_json::Value = serde_json::from_str(result).unwrap();
        let positions = v.as_array().unwrap();
        assert_eq!(positions.len(), 0);
    }

    #[test]
    fn test_collection_stats_unindexed() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cs_ui_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books_dir = reg_dir.join("books");
        std::fs::create_dir_all(&books_dir).unwrap();
        let books_c = CString::new(books_dir.to_string_lossy().as_ref()).unwrap();
        let coll_id = unsafe { pdf_add_collection(books_c.as_ptr()) };

        let mut buf = [0u8; 4096];
        let mut len = 4096u32;
        let rc = unsafe { pdf_collection_stats(coll_id as u32, buf.as_mut_ptr() as *mut c_char, &mut len) };
        assert!(rc == 0 || rc == -1);
    }

    #[test]
    fn test_collection_boost_end_to_end() {
        reset_state();
        let reg_dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let reg_dir = std::env::temp_dir().join(format!("pdf_capi_cb_e2e_{}", reg_dir_n));
        let _ = std::fs::remove_dir_all(&reg_dir);
        let reg_c = CString::new(reg_dir.to_string_lossy().as_ref()).unwrap();
        assert_eq!(unsafe { pdf_create_registry(reg_c.as_ptr()) }, 0);

        let books1 = reg_dir.join("books1");
        std::fs::create_dir_all(&books1).unwrap();
        let books1_c = CString::new(books1.to_string_lossy().as_ref()).unwrap();
        let coll1 = unsafe { pdf_add_collection(books1_c.as_ptr()) };

        let books2 = reg_dir.join("books2");
        std::fs::create_dir_all(&books2).unwrap();
        let books2_c = CString::new(books2.to_string_lossy().as_ref()).unwrap();
        let coll2 = unsafe { pdf_add_collection(books2_c.as_ptr()) };

        let (idx1, idx2) = with_engine(|eng| {
            let reg = eng.get_registry().map_err(|e| e)?;
            Ok::<(std::path::PathBuf, std::path::PathBuf), i32>((
                reg.index_path(coll1 as i64),
                reg.index_path(coll2 as i64),
            ))
        }).unwrap();

        std::fs::create_dir_all(&idx1).unwrap();
        std::fs::create_dir_all(&idx2).unwrap();

        let si1 = SearchIndex::new(&idx1).unwrap();
        let mut w1 = si1.writer().unwrap();
        si1.add_document(&mut w1, 1, "/doc1.pdf", "math algebra").unwrap();
        w1.commit().unwrap();

        let si2 = SearchIndex::new(&idx2).unwrap();
        let mut w2 = si2.writer().unwrap();
        si2.add_document(&mut w2, 1, "/doc2.pdf", "math algebra").unwrap();
        w2.commit().unwrap();

        assert_eq!(unsafe { pdf_set_collection_boost(coll2 as u32, 2.0) }, 0);

        let mut buf = [0u8; 8192];
        let mut len = 8192u32;
        let q = CString::new("math").unwrap();
        assert_eq!(unsafe { pdf_search_all(q.as_ptr(), 10, 0, buf.as_mut_ptr() as *mut c_char, &mut len) }, 0);
        let v: serde_json::Value = serde_json::from_str(
            std::str::from_utf8(&buf[..len as usize]).unwrap()
        ).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["path"], "/doc2.pdf", "Boosted doc2 first");
        assert_eq!(results[1]["path"], "/doc1.pdf");
        assert_eq!(results[0]["collection_id"], serde_json::json!(coll2));
        assert_eq!(results[1]["collection_id"], serde_json::json!(coll1));
        assert!(
            results[0]["score"].as_f64().unwrap() >= results[1]["score"].as_f64().unwrap(),
        );
    }

    #[test]
    fn test_resolve_worker_path_integration() {
        let found = PdfEngine::resolve_worker_path();
        assert!(found.is_some(),
            "PdfEngine::resolve_worker_path() should find pdf_worker.exe during cargo test");
        let path = found.unwrap();
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("pdf_worker.exe"));
        assert!(path.exists(), "resolved worker path must exist on disk");
    }

    #[test]
    fn test_resolve_worker_path_from_same_dir() {
        let dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        let dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        let dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
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
        let dir_n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pdf_capi_wp_notfound_{}", dir_n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("host.exe");
        std::fs::write(&exe, b"mock").unwrap();
        let result = resolve_worker_path_from(&exe);
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_worker_path_from_exe_no_parent() {
        let result = resolve_worker_path_from(std::path::Path::new("\\\\?\\C:\\"));
        assert!(result.is_none());
    }

    fn setup_search_v2_with_docs() -> (CString, CString) {
        reset_state();
        let (db, idx) = setup_temp_index();
        let idx_path = PathBuf::from(&idx);
        let search_index = SearchIndex::new(&idx_path).unwrap();
        let mut writer = search_index.writer().unwrap();
        search_index.add_document(&mut writer, 1, "/doc1.pdf", "hello world").unwrap();
        search_index.add_document(&mut writer, 2, "/doc2.pdf", "hello there").unwrap();
        search_index.add_document(&mut writer, 3, "/doc3.pdf", "rust programming language").unwrap();
        search_index.add_document(&mut writer, 4, "/doc4.pdf", "hello AND world boolean test").unwrap();
        search_index.add_document(&mut writer, 5, "/doc5.pdf", "hello OR world").unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(search_index);

        let db_c = CString::new(db).unwrap();
        let idx_c = CString::new(idx).unwrap();
        assert_eq!(unsafe { pdf_init(db_c.as_ptr(), idx_c.as_ptr()) }, 0);
        (db_c, idx_c)
    }

    fn call_search_v2(json_input: &str) -> Option<serde_json::Value> {
        let json_c = CString::new(json_input).unwrap();
        let ptr = unsafe { pdf_search_v2(json_c.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        let result = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
        unsafe { pdf_free_string(ptr) };
        serde_json::from_str(&result).ok()
    }

    #[test]
    fn test_search_v2_basic() {
        let (_db, _idx) = setup_search_v2_with_docs();
        let json = r#"{"query": "hello", "limit": 10}"#;
        let result = call_search_v2(json);
        let resp = result.expect("Should get a valid response");
        assert_eq!(resp["success"], true);
        assert!(resp["total_count"].as_u64().unwrap_or(0) > 0);
        assert_eq!(resp["strategy"], "auto_phrase");
        let results = resp["results"].as_array().unwrap();
        assert!(results.len() > 0);
    }

    #[test]
    fn test_search_v2_with_boolean_strategy() {
        let (_db, _idx) = setup_search_v2_with_docs();
        let json = r#"{"query": "hello AND world", "strategy": "boolean_phrase", "limit": 10}"#;
        let result = call_search_v2(json);
        let resp = result.expect("Should get a valid response");
        assert_eq!(resp["success"], true);
        assert_eq!(resp["strategy"], "boolean_phrase");
    }

    #[test]
    fn test_search_v2_with_path_filter() {
        let (_db, _idx) = setup_search_v2_with_docs();
        let json = r#"{"query": "hello", "path_filter": "/doc\\d+\\.pdf", "limit": 10}"#;
        let result = call_search_v2(json);
        let resp = result.expect("Should get a valid response");
        assert_eq!(resp["success"], true);
        for r in resp["results"].as_array().unwrap() {
            assert!(r["path"].as_str().unwrap().starts_with("/doc"));
        }
    }

    #[test]
    fn test_search_v2_pagination() {
        let (_db, _idx) = setup_search_v2_with_docs();
        let json = r#"{"query": "hello", "limit": 2, "offset": 0}"#;
        let result = call_search_v2(json);
        let resp = result.expect("Should get a valid response");
        assert_eq!(resp["page"], 1);
        assert_eq!(resp["page_size"], 2);
    }

    #[test]
    fn test_search_v2_empty_query() {
        let (_db, _idx) = setup_search_v2_with_docs();
        let json = r#"{"query": "", "limit": 10}"#;
        let result = call_search_v2(json);
        let resp = result.expect("Should get a valid response");
        assert_eq!(resp["success"], true);
        assert_eq!(resp["total_count"], 0);
    }

    #[test]
    fn test_search_v2_no_results() {
        let (_db, _idx) = setup_search_v2_with_docs();
        let json = r#"{"query": "zzzzzzzzzzz_nonexistent", "limit": 10}"#;
        let result = call_search_v2(json);
        let resp = result.expect("Should get a valid response");
        assert_eq!(resp["success"], true);
        assert_eq!(resp["total_count"], 0);
    }

    #[test]
    fn test_search_v2_invalid_json() {
        let (_db, _idx) = setup_search_v2_with_docs();
        let json_c = CString::new("not json").unwrap();
        let ptr = unsafe { pdf_search_v2(json_c.as_ptr()) };
        assert!(ptr.is_null());
    }

    #[test]
    fn test_search_v2_before_init() {
        reset_state();
        let json = r#"{"query": "hello", "limit": 10}"#;
        let result = call_search_v2(json);
        assert!(result.is_none());
    }

    #[test]
    fn test_search_v2_metadata_present() {
        let (_db, _idx) = setup_search_v2_with_docs();
        let json = r#"{"query": "hello", "limit": 10}"#;
        let result = call_search_v2(json);
        let resp = result.expect("Should get a valid response");
        let meta = resp["metadata"].as_object();
        assert!(meta.is_some());
    }
}
