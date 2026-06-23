use anyhow::{Context, Result};
use crossbeam_channel::bounded;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Create a Command that suppresses console window creation on Windows.
/// On non-Windows platforms it behaves identically to `Command::new`.
fn cmd_no_window<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | CREATE_DEFAULT_ERROR_MODE
        cmd.creation_flags(0x08000000 | 0x00000400 | 0x04000000);
    }
    cmd
}

use crate::indexer::align_offsets_to_tantivy;
use crate::indexer::Indexer;
use tantivy::merge_policy::LogMergePolicy;
use crate::metrics::Metrics;
use crate::ocr;
use crate::output::DocumentRecord;
use crate::scanner::{scan_directory, JobStore};
use crate::worker_ipc::{frame_crc32, WorkerFrame};

/// Dedicated rayon thread pool for token alignment in the indexer.
///
/// Alignment (`align_offsets_to_tantivy`) is CPU-intensive and runs on the
/// full batch via `par_iter()`.  Using a small dedicated pool prevents these
/// parallel tasks from saturating all cores and starving other pipeline
/// components (producer, IPC reader, writer).  The pool size is capped at 4
/// threads — alignment is memory-bound (large text buffers + word positions)
/// so beyond 4 threads the gains are marginal while the memory pressure grows
/// linearly.
static ALIGN_POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();

fn align_pool() -> &'static rayon::ThreadPool {
    ALIGN_POOL.get_or_init(|| {
        let threads = std::cmp::min(std::cmp::max(1, num_cpus::get() / 2), 4);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("align-{}", i))
            .build()
            .expect("Failed to create alignment thread pool")
    })
}

// Channel capacity for DocumentRecord queues.  Each record carries the full
// extracted text (up to several MB for large PDFs), so a small default bounds
// peak memory even when the indexer is the bottleneck.
//   peak memory ≈ (CHANNEL_CAP + INDEXER_BATCH) × avg_text_size
//   = (256 + 500) × text_size
const DEFAULT_CHANNEL_CAPACITY: usize = 256;
const BATCH_SIZE: i64 = 30;
const DEFAULT_INDEXER_BATCH_SIZE: usize = 500;
const DEFAULT_COMMIT_INTERVAL: u64 = 5000;
const DEFAULT_COMMIT_TIMEOUT_SECS: u64 = 30;
const RESERVOIR_FACTOR: usize = 4;

pub struct PipelineConfig {
    pub num_extract_workers: Option<usize>,
    pub num_indexer_threads: Option<usize>,
    pub indexer_batch_size: Option<usize>,
    pub commit_interval: Option<u64>,
    pub commit_timeout: Option<u64>,
    pub channel_capacity: Option<usize>,
    pub progress_cb: Option<Box<dyn Fn(u64, u64) + Send>>,
    pub cancel_flag: Option<Arc<AtomicBool>>,
    /// Path to the pdf_worker binary. When set, extraction runs in a
    /// separate OS process per batch (PDFium is not thread-safe).
    /// When None, extraction runs in-thread (the original behaviour).
    pub worker_path: Option<PathBuf>,
    /// Optional callback for log messages from the extraction pipeline.
    /// Called with a UTF-8 byte pointer and length.  The callback must
    /// copy the data before returning — the pointer is only valid for
    /// the duration of the call.
    pub log_cb: Option<extern "C" fn(*const u8, u32)>,
    /// Optional callback for per‑process metrics (PID, state, memory).
    /// Same calling convention as log_cb.
    pub process_cb: Option<extern "C" fn(*const u8, u32)>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            num_extract_workers: None,
            num_indexer_threads: None,
            indexer_batch_size: None,
            commit_interval: None,
            commit_timeout: None,
            channel_capacity: None,
            progress_cb: None,
            cancel_flag: None,
            worker_path: None,
            log_cb: None,
            process_cb: None,
        }
    }
}

impl PipelineConfig {
    pub fn extract_workers(&self) -> usize {
        self.num_extract_workers
            .map(|v| std::cmp::max(1, v))
            .unwrap_or_else(|| {
                let cores = num_cpus::get();
                // Each worker is a full subprocess loading pdfium.dll (~40 MB+).
                // Cap the auto-detected default so we don't OOM on high-core machines.
                std::cmp::min(std::cmp::max(1, cores.saturating_sub(2)), 8)
            })
    }

    pub fn indexer_batch(&self) -> usize {
        self.indexer_batch_size
            .map(|v| if v == 0 { 1 } else { v })
            .unwrap_or(DEFAULT_INDEXER_BATCH_SIZE)
    }

    pub fn commit_int(&self) -> u64 {
        self.commit_interval
            .map(|v| if v == 0 { 1 } else { v })
            .unwrap_or(DEFAULT_COMMIT_INTERVAL)
    }

    pub fn commit_to(&self) -> u64 {
        self.commit_timeout
            .map(|v| if v == 0 { 1 } else { v })
            .unwrap_or(DEFAULT_COMMIT_TIMEOUT_SECS)
    }

    pub fn channel_cap(&self) -> usize {
        self.channel_capacity
            .map(|v| if v == 0 { 1 } else { v })
            .unwrap_or(DEFAULT_CHANNEL_CAPACITY)
    }

    pub fn indexer_threads(&self) -> usize {
        self.num_indexer_threads
            .map(|v| std::cmp::max(1, v))
            .unwrap_or_else(|| std::cmp::min(num_cpus::get(), 4))
    }

    pub fn reservoir_size(&self) -> usize {
        let nw = self.extract_workers();
        std::cmp::min(200, nw * RESERVOIR_FACTOR)
    }
}

struct ExtractorTask {
    id: i64,
    path: String,
    checksum: String,
}

thread_local! {
    static TEXT_BUF: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
}

fn with_scratch_buf<F, R>(f: F) -> R
where
    F: FnOnce(&mut Vec<String>) -> R,
{
    TEXT_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        let result = f(&mut buf);
        buf.clear();
        if buf.capacity() > 1_000_000 {
            buf.shrink_to(64_000);
        }
        result
    })
}

pub fn run_pipeline(
    jobs: Arc<JobStore>,
    metrics: Arc<Metrics>,
    input: &PathBuf,
    indexer: Option<Arc<Indexer>>,
    config: PipelineConfig,
) -> Result<()> {
    PipelineOrchestrator::new(config, jobs, metrics, input.clone(), indexer).run()
}

fn indexer_thread(
    indexer: &Indexer,
    jobs: Arc<JobStore>,
    rx: crossbeam_channel::Receiver<IndexerMsg>,
    metrics: &Metrics,
    batch_size: usize,
    commit_interval: u64,
    commit_timeout: u64,
    num_threads: usize,
    log_cb: Option<extern "C" fn(*const u8, u32)>,
) {
    let index_writer = match indexer.search_index().writer_with_num_threads(num_threads) {
        Ok(w) => w,
        Err(e) => {
            log_msg(log_cb, &format!("[indexer_thread] failed to create index writer: {}", e));
            metrics.set_indexer_failed();
            return;
        }
    };

    let mut buf: Vec<DocumentRecord> = Vec::with_capacity(batch_size);
    let mut done_ids: Vec<(i64, bool, String)> = Vec::new();
    let mut doc_count: u64 = 0;
    let mut last_commit = Instant::now();
    let mut writer = index_writer;

    // Keep a single segment so delete_term is fast — no per-segment scans.
    let mut merge_policy = LogMergePolicy::default();
    merge_policy.set_min_num_segments(1);
    writer.set_merge_policy(Box::new(merge_policy));
    let mut total_flush_time = Duration::ZERO;
    let mut flush_count: u64 = 0;

    loop {
        let timeout = Duration::from_secs(commit_timeout);
        let result = rx.recv_timeout(timeout);
        match result {
            Ok(IndexerMsg::Index(record)) => {
                buf.push(record);
                if buf.len() >= batch_size {
                    let batch: Vec<DocumentRecord> = buf.drain(..).collect();
                    let flush_start = Instant::now();
                    flush_batch(&writer, indexer, &mut done_ids, &batch, log_cb);
                    let flush_elapsed = flush_start.elapsed();
                    total_flush_time += flush_elapsed;
                    flush_count += 1;
                    doc_count += batch.len() as u64;
                    log_msg(log_cb, &format!(
                        "[indexer] flushed batch {} ({} docs) in {:.3}s",
                        flush_count, batch.len(), flush_elapsed.as_secs_f64(),
                    ));
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                if !buf.is_empty() {
                    let batch: Vec<DocumentRecord> = buf.drain(..).collect();
                    flush_batch(&writer, indexer, &mut done_ids, &batch, log_cb);
                }
                break;
            }
        }

        let should_commit = !done_ids.is_empty()
            && (doc_count >= commit_interval
                || last_commit.elapsed() > Duration::from_secs(commit_timeout));
        if should_commit {
            // Commit Tantivy FIRST, then mark done in SQLite.
            // `add_document` now deletes any pre-existing document for the
            // same id, so re-processing after a crash is idempotent.
            // This way a crash between w.commit() and batch_mark_done() only
            // causes harmless re-indexing — never permanent search loss.
            let commit_ok = writer.commit().is_ok();

            if commit_ok {
                let _ = jobs.batch_mark_done(&done_ids);
                done_ids.clear();

                let total = indexer.metrics().docs_indexed();
                metrics.set_indexer_docs_indexed(total);
                metrics.set_indexer_last_commit_age(0);

                last_commit = Instant::now();
                doc_count = 0;
            } else {
                log_msg(log_cb, &format!(
                    "[indexer_thread] commit failed — Tantivy could not persist {} docs. Check disk space and index integrity.",
                    done_ids.len()
                ));
            }
        }

        // Update age between commits so the 5s snapshot sees the real value
        metrics.set_indexer_last_commit_age(last_commit.elapsed().as_secs());
    }

    // Final commit: Tantivy first, then mark done in SQLite.
    let commit_start = Instant::now();
    if writer.commit().is_ok() {
        if !done_ids.is_empty() {
            jobs.batch_mark_done(&done_ids).ok();
            done_ids.clear();
        }
        let total = indexer.metrics().docs_indexed();
        metrics.set_indexer_docs_indexed(total);
        metrics.set_indexer_last_commit_age(0);
        log_msg(log_cb, &format!(
            "[timing] indexer: final commit took {:.3}s for {} docs total",
            commit_start.elapsed().as_secs_f64(),
            total,
        ));
    } else {
        log_msg(log_cb, &format!(
            "[timing] indexer: final commit FAILED after {:.3}s",
            commit_start.elapsed().as_secs_f64(),
        ));
    }
}

fn flush_batch(
    writer: &tantivy::IndexWriter,
    indexer: &Indexer,
    done_ids: &mut Vec<(i64, bool, String)>,
    batch: &[DocumentRecord],
    log_cb: Option<extern "C" fn(*const u8, u32)>,
) {
    use rayon::prelude::*;

    let batch_size = batch.len();
    let phase1_start = Instant::now();
    let mut add_time = Duration::ZERO;
    let mut align_time = Duration::ZERO;

    // Phase 1a: add documents to Tantivy sequentially.
    // Tantivy's IndexWriter::add_document is internally serialized — all calls
    // contend for the same internal pipeline lock.  Parallelising this with
    // rayon creates lock contention that *slows down* indexing (more threads
    // just means more waiting + cache bouncing).  A plain for-loop avoids this.
    let mut pending_done: Vec<(i64, bool, String)> = Vec::new();
    let mut pending_align: Vec<(i64, &str, &[crate::extractor::WordPosition])> = Vec::new();

    let add_start = Instant::now();
    for record in batch {
        let result = indexer.search_index().add_document(
            writer,
            record.id,
            &record.path,
            &record.text,
        );
        match result {
            Ok(()) => {
                indexer.metrics().docs_indexed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                pending_done.push((record.id, record.ocr_flag, record.checksum.clone()));

                if !record.word_positions.is_empty() {
                    pending_align.push((record.id, &record.text, &record.word_positions));
                }
            }
            Err(e) => {
                log_msg(log_cb, &format!(
                    "[flush_batch] add_document failed for id={} path='{}': {}",
                    record.id, record.path, e
                ));
            }
        }
    }
    add_time += add_start.elapsed();

    // Phase 1b: align offsets in parallel using a dedicated small thread-pool.
    // Using the global rayon pool here could starve other pipeline components
    // (producer, IPC reader, writer) on large batches.  The ALIGN_POOL is
    // capped to at most 4 threads, balancing throughput vs memory pressure.
    let align_start = Instant::now();
    let positions_to_store: Vec<(i64, Vec<(usize, &crate::extractor::WordPosition)>)> = align_pool()
        .install(|| {
            pending_align
                .par_iter()
                .filter_map(|&(id, text, wps)| {
                    let aligned = align_offsets_to_tantivy(text, wps);
                    if aligned.is_empty() { None } else { Some((id, aligned)) }
                })
                .collect()
        });
    done_ids.extend(pending_done);
    align_time += align_start.elapsed();

    let phase1 = phase1_start.elapsed();

    let phase2_start = Instant::now();

    // Phase 2: store word positions (SQLite I/O, batched in one transaction).
    if !positions_to_store.is_empty() {
        if let Ok(pos_store) = indexer.position_store.lock() {
            let _ = pos_store.store_positions_batch(&positions_to_store);
        }
    }

    let phase2_elapsed = phase2_start.elapsed();

    log_msg(log_cb, &format!(
        "[timing] flush_batch({}): phase1={:.3}s (add={:.3}s align={:.3}s) store={:.3}s wall={:.3}s docs_with_positions={}",
        batch_size,
        phase1.as_secs_f64(),
        add_time.as_secs_f64(),
        align_time.as_secs_f64(),
        phase2_elapsed.as_secs_f64(),
        (phase1 + phase2_elapsed).as_secs_f64(),
        positions_to_store.len(),
    ));
}

/// Resolve the effective number of OCR worker threads.
/// If an override is given, clamps to at least 1.
/// Otherwise auto-detects as `min(max(1, cores/2 - 1), 2)`.
fn resolve_ocr_workers(override_val: Option<usize>) -> usize {
    override_val
        .map(|v| std::cmp::max(1, v))
        .unwrap_or_else(|| {
            let cores = num_cpus::get();
            // OCR workers are memory-heavy (tesseract + temp images per worker).
            // Cap the auto-detected default conservatively.
            std::cmp::min(std::cmp::max(1, cores / 2 - 1), 2)
        })
}

pub fn run_ocr_post_processing(
    jobs: Arc<JobStore>,
    indexer: Option<Arc<Indexer>>,
    ocr_config: &ocr::OcrConfig,
    num_workers_override: Option<usize>,
    cancel_flag: Option<Arc<AtomicBool>>,
    log_cb: Option<extern "C" fn(*const u8, u32)>,
) -> Result<u64> {
    let num_workers = resolve_ocr_workers(num_workers_override);
    OcrPipelineActor::new(jobs, indexer, ocr_config, num_workers, cancel_flag, log_cb).run()
}

/// Attempt OCR on a PDF by extracting pages as images, preprocessing, and running Tesseract.
/// Falls back gracefully if Tesseract is not available.
///
/// When `worker` is `Some`, uses a persistent worker process (avoids per-call `tesseract.exe` spawn).
/// When `None`, falls back to a fresh `Command::new(tesseract_path)` for each call.
fn run_single_ocr(path: &Path, config: &ocr::OcrConfig, mut worker: Option<&mut ocr::WorkerProcess>) -> Result<String> {
    // For image-based PDFs, need a PDF renderer.
    let temp_dir = tempfile::tempdir().context("Failed to create temp dir for OCR")?;
    let page_count = get_pdf_page_count(path).unwrap_or(1);

    let mut full_text = String::new();
    for page_num in 1..=page_count {
        let image_path = temp_dir.path().join(format!("page-{}.png", page_num));
        let rendered = render_pdf_page(path, page_num, &image_path);
        match rendered {
            Ok(true) => {
                let preprocessed = match ocr::preprocess_image(&image_path, config.max_dim) {
                    Ok(img) => img,
                    Err(_e) => {
                        continue;
                    }
                };
                let text = match &mut worker {
                    Some(w) => w.process(&preprocessed),
                    None => ocr::run_tesseract(&preprocessed, &config.tesseract_path, &config.language),
                };
                let text = match text {
                    Ok(t) => t,
                    Err(_e) => {
                        continue;
                    }
                };
                if !text.is_empty() {
                    if !full_text.is_empty() {
                        full_text.push('\n');
                    }
                    full_text.push_str(&text);
                }
            }
            Ok(false) => {
                continue;
            }
            Err(_e) => {
                continue;
            }
        }
    }

    if full_text.is_empty() {
        anyhow::bail!("No text extracted from any page of {}", path.display());
    }
    Ok(full_text)
}

/// Probe whether a tool binary is available on PATH, caching the result.
fn tool_is_available(name: &str) -> bool {
    // Use the name as the key; we keep one probe per name.
    // The map is append-only so it's safe behind OnceLock.
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
        OnceLock::new();
    let mut map = CACHE
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *map.entry(name.to_string()).or_insert_with(|| {
        cmd_no_window(name)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|mut child| {
                let _ = child.kill();
                let _ = child.wait();
                true
            })
            .unwrap_or(false)
    })
}

/// Try to determine the number of pages in a PDF using available tools.
/// Checks tool availability once (cached) to avoid spawning missing binaries.
/// Tries `mutool info` and `pdfinfo`, falling back to 1.
fn get_pdf_page_count(pdf_path: &Path) -> Result<u32> {
    if tool_is_available("mutool") {
        if let Ok(output) = cmd_no_window("mutool")
            .args(["info"])
            .arg(pdf_path)
            .output()
        {
            if let Some(n) = extract_page_count_from_stdout(&String::from_utf8_lossy(&output.stdout)) {
                return Ok(n);
            }
        }
    }

    if tool_is_available("pdfinfo") {
        if let Ok(output) = cmd_no_window("pdfinfo")
            .arg(pdf_path)
            .output()
        {
            if let Some(n) = extract_page_count_from_stdout(&String::from_utf8_lossy(&output.stdout)) {
                return Ok(n);
            }
        }
    }

    Ok(1)
}

/// Extract the page count from the stdout of `mutool info` or `pdfinfo`.
/// Returns `None` if the output does not contain a valid `Pages: N` line.
fn extract_page_count_from_stdout(stdout: &str) -> Option<u32> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?m)^Pages:\s+(\d+)\s*$").unwrap());
    re.captures(stdout)
        .and_then(|caps| caps[1].parse::<u32>().ok())
        .filter(|&n| n > 0)
}

/// Try to render a single page of a PDF to an image using available system tools.
/// `page_num` is 1-indexed.
/// Returns Ok(true) if rendering succeeded, Ok(false) if no renderer available.
fn render_pdf_page(pdf_path: &Path, page_num: u32, output_image: &Path) -> Result<bool> {
    let page_str = page_num.to_string();

    // Try `mutool draw` (MuPDF)
    if tool_is_available("mutool") {
        if let Ok(output) = cmd_no_window("mutool")
            .args(["draw", "-o"])
            .arg(output_image)
            .args(["-r", "300"])
            .arg(pdf_path)
            .arg(&page_str)
            .output()
        {
            if output.status.success() && output_image.exists() {
                return Ok(true);
            }
        }
    }

    // Try `pdftoppm` (poppler)
    if tool_is_available("pdftoppm") {
        let ppm_path = output_image.with_extension("ppm");
        if let Ok(output) = cmd_no_window("pdftoppm")
            .args(["-r", "300", "-gray", "-f", &page_str, "-l", &page_str, "-singlefile"])
            .arg(pdf_path)
            .arg(&ppm_path.with_extension(""))
            .output()
        {
            if output.status.success() && ppm_path.exists() {
                std::fs::rename(&ppm_path, output_image)?;
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Filter a batch of OCR jobs, skipping those already in `sent_ids`.
/// Returns the jobs that should be sent and records their IDs as sent.
/// This is the core dedup logic used by the producer thread.
fn deduplicate_ocr_batch(
    batch: Vec<(i64, String, String)>,
    sent_ids: &mut HashSet<i64>,
) -> Vec<(i64, String, String)> {
    batch
        .into_iter()
        .filter(|(id, _, _)| sent_ids.insert(*id))
        .collect()
}

// ---------------------------------------------------------------------------
// Process-based extraction worker (full OS process isolation)
// ---------------------------------------------------------------------------

/// Collect exactly one batch from the producer channel.
fn collect_batch(
    task_rx: &crossbeam_channel::Receiver<ExtractorMsg>,
) -> Vec<ExtractorTask> {
    match task_rx.recv() {
        Ok(ExtractorMsg::Extract(tasks)) => tasks,
        Err(_) => Vec::new(),
    }
}

/// Read one length-prefixed bincode WorkerFrame from a buffered reader.
/// Wire format: [4 bytes data_len][data_len bytes bincode][4 bytes CRC32]
/// CRC is validated after deserialization; mismatches are reported as InvalidData.
fn read_frame<R: io::Read>(reader: &mut io::BufReader<R>) -> io::Result<WorkerFrame> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut data = vec![0u8; len];
    reader.read_exact(&mut data)?;
    let mut crc_buf = [0u8; 4];
    reader.read_exact(&mut crc_buf)?;
    let expected_crc = u32::from_le_bytes(crc_buf);
    let actual_crc = frame_crc32(&data);
    if actual_crc != expected_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CRC mismatch: expected {:08x}, actual {:08x} ({} bytes)", expected_crc, actual_crc, len),
        ));
    }
    bincode::deserialize(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// After a worker crash, drain all complete frames from the stdout pipe buffer.
/// The worker flushes each frame immediately after writing, so frames for
/// already-processed documents are always complete in the pipe.
/// Recovered frames are forwarded to `result_tx` / `jobs.mark_error`.
/// Returns `(success_count, error_count)` of recovered frames.
fn drain_crash_frames<R: io::Read>(
    reader: &mut io::BufReader<R>,
    path_map: &HashMap<String, (i64, String)>,
    result_tx: &crossbeam_channel::Sender<DocumentRecord>,
    jobs: &Arc<JobStore>,
    metrics: &Arc<Metrics>,
) -> (usize, usize) {
    let mut ok = 0usize;
    let mut err = 0usize;
    loop {
        let frame = match read_frame(reader) {
            Ok(f) => f,
            Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(_) => break,
        };
        match frame {
            WorkerFrame::Success(wo) => {
                if let Some(&(id, _)) = path_map.get(&wo.path) {
                    let record = DocumentRecord {
                        id,
                        path: wo.path,
                        checksum: wo.checksum,
                        ocr_flag: wo.ocr_flag,
                        text: wo.text,
                        word_positions: wo.word_positions,
                        file_extraction_ms: wo.file_extraction_ms,
                        page_count: wo.page_count,
                    };
                    let _ = result_tx.send(record);
                    ok += 1;
                }
            }
            WorkerFrame::Error { path, message } => {
                if let Some(&(id, _)) = path_map.get(&path) {
                    jobs.mark_error(id, &message).ok();
                    metrics.increment_errored();
                    err += 1;
                }
            }
        }
    }
    (ok, err)
}

/// Long-lived extraction worker per thread.
///
/// Spawns ONE pdf_worker.exe and feeds it paths in mini-batches.
/// Each mini-batch writes ALL paths to stdin in one burst (minimising
/// context switches), then reads ALL response frames sequentially.
///
/// If the worker crashes mid-batch, buffered frames from completed
/// documents are drained before exit so they aren't silently lost.
/// Un-accounted tasks stay in 'extracting' status; the pipeline's
/// final `reprocess_extracting` step resets them to 'pending'.
fn run_extraction_process(
    worker_path: &Path,
    task_rx: &crossbeam_channel::Receiver<ExtractorMsg>,
    result_tx: &crossbeam_channel::Sender<DocumentRecord>,
    jobs: &Arc<JobStore>,
    metrics: &Arc<Metrics>,
    cancel_flag: Option<Arc<AtomicBool>>,
    log_cb: Option<extern "C" fn(*const u8, u32)>,
    process_cb: Option<extern "C" fn(*const u8, u32)>,
) {
    let is_cancelled = || cancel_flag.as_ref().map_or(false, |f| f.load(Ordering::Relaxed));
    // --- spawn ONE worker for this extractor thread's lifetime ---
    if !worker_path.exists() {
        drain_all_for_error(task_rx, |t| {
            jobs.mark_error(t.id, &format!("worker not found: {}", worker_path.display())).ok();
            metrics.increment_errored();
        });
        return;
    }
    let mut child = match cmd_no_window(worker_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            drain_all_for_error(task_rx, |t| {
                jobs.mark_error(t.id, &format!("worker launch failed [{}]: {}", worker_path.display(), e)).ok();
                metrics.increment_errored();
            });
            return;
        }
    };

    let pid = child.id();
    let thread_name = thread::current().name().unwrap_or("?").to_string();
    report_process(process_cb, &thread_name, pid, "started", None, "");

    let mut child_stdin = child.stdin.take().expect("stdin piped");
    let child_stdout = child.stdout.take().expect("stdout piped");
    let mut stdout_reader = io::BufReader::new(child_stdout);
    let mut batch_count: u64 = 0;
    let worker_start = Instant::now();
    let mut files_ok: u64 = 0;
    let mut files_err: u64 = 0;
    let mut total_batch_time = Duration::ZERO;

    'outer: loop {
        batch_count += 1;
        let batch_start = Instant::now();

        let batch = collect_batch(task_rx);
        if batch.is_empty() {
            break;
        }

        // Build path → (id, checksum) lookup table ONCE per batch
        let mut path_map: HashMap<String, (i64, String)> = HashMap::with_capacity(batch.len());
        for task in &batch {
            path_map.insert(task.path.clone(), (task.id, task.checksum.clone()));
        }

        if is_cancelled() {
            log_msg(log_cb, &format!("[extraction] {} — cancel requested, killing worker", thread_name));
            let _ = child.kill();
            break;
        }

        // --- Phase 1: write ALL paths to stdin in one burst ---
        for task in &batch {
            if is_cancelled() {
                log_msg(log_cb, &format!("[extraction] {} — cancel during stdin write, killing worker", thread_name));
                let _ = child.kill();
                break 'outer;
            }
            if writeln!(child_stdin, "{}", task.path).is_err() {
                // Worker died during Phase 1. Frames for paths that were
                // already written to stdin and processed are in the stdout
                // pipe buffer — drain them so they aren't silently lost.
                let (recovered_ok, recovered_err) =
                    drain_crash_frames(&mut stdout_reader, &path_map, result_tx, jobs, metrics);
                files_err += recovered_err as u64;
                let msg = format!(
                    "[extraction] worker stdin write failed on '{}' — drained {} success + {} error frames from buffer",
                    task.path, recovered_ok, recovered_err
                );
                log_msg(log_cb, &msg);
                break 'outer;
            }
        }
        let _ = child_stdin.flush();

        // --- Phase 2: read ALL response frames ---
        for (task_idx, task) in batch.iter().enumerate() {
            if is_cancelled() {
                let msg = format!(
                    "[extraction] {} — cancel during frame read on batch[{}] '{}', killing worker",
                    thread_name, task_idx, task.path
                );
                log_msg(log_cb, &msg);
                let _ = child.kill();
                // Drain any frames already buffered before exit
                let (_recovered_ok, recovered_err) =
                    drain_crash_frames(&mut stdout_reader, &path_map, result_tx, jobs, metrics);
                files_err += recovered_err as u64;
                for remaining in &batch[task_idx..] {
                    jobs.mark_error(remaining.id, "cancelled").ok();
                    metrics.increment_errored();
                }
                break 'outer;
            }
            let frame = match read_frame(&mut stdout_reader) {
                Ok(f) => f,
                Err(e) => {
                    // Worker died mid-Phase 2. Drain any frames the worker
                    // wrote before crashing (worker processes asynchronously
                    // and may have completed several docs ahead of our reads).
                    let (recovered_ok, recovered_err) =
                        drain_crash_frames(&mut stdout_reader, &path_map, result_tx, jobs, metrics);
                    files_err += recovered_err as u64;
                    let uptime = format_uptime(worker_start.elapsed());
                    let msg = format!(
                        "[extraction] read_frame failed on batch[{}] '{}': {} — drained {} success + {} error frames (uptime {}, {} ok, {} err)",
                        task_idx, task.path, e, recovered_ok, recovered_err, uptime, files_ok, files_err
                    );
                    log_msg(log_cb, &msg);
                    // Mark remaining tasks as errored so they aren't stuck.
                    for remaining in &batch[task_idx..] {
                        jobs.mark_error(remaining.id, "worker crashed").ok();
                        metrics.increment_errored();
                    }
                    break 'outer;
                }
            };
            match frame {
                WorkerFrame::Success(wo) => {
                    let record = DocumentRecord {
                        id: task.id,
                        path: wo.path,
                        checksum: wo.checksum,
                        ocr_flag: wo.ocr_flag,
                        text: wo.text,
                        word_positions: wo.word_positions,
                        file_extraction_ms: wo.file_extraction_ms,
                        page_count: wo.page_count,
                    };
                    if result_tx.send(record).is_err() {
                        // Cannot send downstream — pipeline shutting down.
                        // Drain any frames already buffered.
                let (_recovered_ok, recovered_err) =
                            drain_crash_frames(&mut stdout_reader, &path_map, result_tx, jobs, metrics);
                        files_err += recovered_err as u64;
                        let msg = format!(
                            "[extraction] pipeline disconnected while reading '{}' — drained {} success + {} error frames",
                            task.path, _recovered_ok, recovered_err
                        );
                        log_msg(log_cb, &msg);
                        // Mark rest of batch as errored so they aren't stuck.
                        for remaining in &batch[task_idx..] {
                            jobs.mark_error(remaining.id, "pipeline disconnected").ok();
                            metrics.increment_errored();
                        }
                        break 'outer;
                    }
                    files_ok += 1;
                }
                WorkerFrame::Error { path: _, message } => {
                        jobs.mark_error(task.id, &message).ok();
                        metrics.increment_errored();
                        files_err += 1;
                    }
                }
            let batch_elapsed = batch_start.elapsed();
            total_batch_time += batch_elapsed;
            let mem = proc_mon::working_set_mib(pid);
            report_process(process_cb, &thread_name, pid, "running", mem, "");
            log_msg(log_cb, &format!(
                "[extraction] {} batch {}: {} docs in {:.3}s (total: {} ok, {} err, uptime {})",
                thread_name, batch_count, files_ok + files_err,
                batch_elapsed.as_secs_f64(),
                files_ok, files_err, format_uptime(worker_start.elapsed()),
            ));
        }
    }

    // --- cleanup ---
    drop(child_stdin);

    // Collect stderr the worker wrote before exiting (e.g. panic messages,
    // PDFium errors, abort traces). The pipe buffer is small, so we read it
    // AFTER dropping stdin to avoid deadlocking.
    let stderr_dump = child.stderr.take().and_then(|stderr| {
        let mut buf = String::new();
        io::BufReader::new(stderr).read_to_string(&mut buf).ok().map(|_| buf)
    });

    let (exit_code, wait_err) = match child.wait() {
        Ok(status) if status.success() => {
            (None, None)
        }
        Ok(status) => {
            (status.code(), None)
        }
        Err(e) => {
            (None, Some(e))
        }
    };

    let uptime = format_uptime(worker_start.elapsed());
    let base_info = format!("{} (uptime {}, {} ok, {} err)", thread_name, uptime, files_ok, files_err);

    let final_state = if let Some(code) = exit_code {
        let exit_str = format_exit_code(code);
        let stderr_snippet = stderr_dump
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("\n  stderr: {}", s.trim()))
            .unwrap_or_default();
        let msg = format!(
            "[extraction] worker {} — {}{}",
            base_info, exit_str, stderr_snippet
        );
        log_msg(log_cb, &msg);
        exit_str
    } else if let Some(e) = wait_err {
        let stderr_snippet = stderr_dump
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("\n  stderr: {}", s.trim()))
            .unwrap_or_default();
        let msg = format!(
            "[extraction] failed to wait on {}: {}{}",
            base_info, e, stderr_snippet
        );
        log_msg(log_cb, &msg);
        "crashed".to_string()
    } else {
        let mem = proc_mon::working_set_mib(pid);
        log_msg(log_cb, &format!(
            "[extraction] worker {} — exited(0) batches={} avg_batch={:.3}s ({} ok, {} err, mem={:.0}MB)",
            base_info, batch_count,
            total_batch_time.as_secs_f64() / batch_count.max(1) as f64,
            files_ok, files_err,
            mem.unwrap_or(0.0),
        ));
        "exited(0)".to_string()
    };

    let mem = proc_mon::working_set_mib(pid);
    let stderr_snippet = stderr_dump
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let trimmed = s.trim();
            if trimmed.len() > 120 { &trimmed[..120] } else { trimmed }
        })
        .unwrap_or("");
    let stderr_part = if stderr_snippet.is_empty() {
        String::new()
    } else {
        format!(" | stderr: {}", stderr_snippet)
    };
    let extra = format!("{} ok, {} err, uptime {}{}",
        files_ok, files_err, uptime, stderr_part);
    report_process(process_cb, &thread_name, pid, &final_state, mem, &extra);
}

/// Format a Duration as a human-readable uptime string.
fn format_uptime(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Global log file — when set, all `log_msg` output is also written here.
static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// Set the global pipeline log file path. Creates/appends to the file.
/// All subsequent `log_msg` calls will write to this file.
pub fn set_pipeline_log_path(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open pipeline log file: {}", path.display()))?;
    let lock = LOG_FILE.get_or_init(|| Mutex::new(None));
    *lock.lock().unwrap() = Some(file);
    Ok(())
}

/// Close the global pipeline log file if open.
pub fn close_pipeline_log() {
    if let Some(lock) = LOG_FILE.get() {
        *lock.lock().unwrap() = None;
    }
}

/// Send a log message to the optional C callback, falling back to stderr.
/// When the global log file is set (via `set_pipeline_log_path`), the message
/// is also written to that file.
fn log_msg(log_cb: Option<extern "C" fn(*const u8, u32)>, msg: &str) {
    match log_cb {
        Some(cb) => {
            let bytes = msg.as_bytes();
            cb(bytes.as_ptr(), bytes.len() as u32);
        }
        None => eprintln!("{}", msg),
    }
    if let Some(lock) = LOG_FILE.get() {
        if let Ok(mut guard) = lock.lock() {
            if let Some(ref mut file) = *guard {
                let _ = writeln!(file, "{}", msg);
                let _ = file.flush();
            }
        }
    }
}

/// Map a Windows NTSTATUS exit code to a human-readable description.
/// On non-Windows, shows the raw code in decimal.
fn describe_exit_code(code: i32) -> String {
    #[cfg(windows)]
    {
        match code as u32 {
            0xC0000005 => "0xC0000005 ACCESS_VIOLATION (segfault)".into(),
            0xC0000017 => "0xC0000017 NO_MEMORY — probable OOM".into(),
            0xC000009A => "0xC000009A INSUFFICIENT_RESOURCES — probable OOM".into(),
            0xC00000D5 => "0xC00000D5 COMMITMENT_LIMIT — probable OOM".into(),
            0xC000013A => "0xC000013A CONTROL_C_EXIT (user interrupted)".into(),
            0xC0000142 => "0xC0000142 DLL_INIT_FAILED".into(),
            0xC0000409 => "0xC0000409 STACK_BUFFER_OVERRUN".into(),
            _ => format!("0x{:08X}", code as u32),
        }
    }
    #[cfg(not(windows))]
    {
        format!("{}", code)
    }
}

/// Prepend `describe_exit_code` info if the code is a known NTSTATUS.
fn format_exit_code(code: i32) -> String {
    #[cfg(windows)]
    {
        // NTSTATUS codes have the high bit set (>= 0x80000000 as u32)
        if (code as u32) >= 0x80000000 {
            format!("exited({})", describe_exit_code(code))
        } else {
            format!("exited({})", code)
        }
    }
    #[cfg(not(windows))]
    {
        format!("exited({})", code)
    }
}

// ---------------------------------------------------------------------------
// Process monitoring — query a child process’s working set (Windows only).
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod proc_mon {
    #![allow(non_snake_case, non_camel_case_types)]

    use std::mem;

    type HANDLE = *mut std::ffi::c_void;
    type DWORD = u32;
    type BOOL = i32;

    #[repr(C)]
    struct PROCESS_MEMORY_COUNTERS {
        cb: DWORD,
        PageFaultCount: DWORD,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
    }

    const PROCESS_QUERY_INFORMATION: DWORD = 0x0400;
    const PROCESS_VM_READ: DWORD = 0x0010;

    extern "system" {
        fn OpenProcess(dwDesiredAccess: DWORD, bInheritHandle: BOOL, dwProcessId: DWORD) -> HANDLE;
        fn CloseHandle(hObject: HANDLE) -> BOOL;
        fn GetProcessMemoryInfo(
            hProcess: HANDLE,
            ppsmemCounters: *mut PROCESS_MEMORY_COUNTERS,
            cb: DWORD,
        ) -> BOOL;
    }

    /// Returns the working-set size of the process in MiB, or `None`.
    pub fn working_set_mib(pid: u32) -> Option<f64> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut pmc: PROCESS_MEMORY_COUNTERS = mem::zeroed();
            pmc.cb = mem::size_of::<PROCESS_MEMORY_COUNTERS>() as DWORD;
            let ok = GetProcessMemoryInfo(handle, &mut pmc, pmc.cb);
            CloseHandle(handle);
            if ok == 0 { None } else { Some(pmc.WorkingSetSize as f64 / (1024.0 * 1024.0)) }
        }
    }
}

#[cfg(not(windows))]
mod proc_mon {
    pub fn working_set_mib(_pid: u32) -> Option<f64> { None }
}

/// Helper: format a process‑metrics string and send it through the callback.
fn report_process(
    process_cb: Option<extern "C" fn(*const u8, u32)>,
    thread_name: &str,
    pid: u32,
    state: &str,          // "started", "running", "exited(N)", "crashed"
    memory_mb: Option<f64>,
    extra: &str,          // short stderr snippet or empty
) {
    let mem = match memory_mb {
        Some(m) => format!("{:.1}", m),
        None => "?".to_string(),
    };
    let msg = format!("PROC|{}|{}|{}|{}|{}", thread_name, pid, state, mem, extra);
    match process_cb {
        Some(cb) => {
            let bytes = msg.as_bytes();
            cb(bytes.as_ptr(), bytes.len() as u32);
        }
        None => eprintln!("{}", msg),
    }
}

/// Drain all remaining batches from the channel and apply an error action.
fn drain_all_for_error(
    task_rx: &crossbeam_channel::Receiver<ExtractorMsg>,
    on_task: impl Fn(&ExtractorTask),
) {
    loop {
        let batch = collect_batch(task_rx);
        if batch.is_empty() {
            break;
        }
        for t in &batch {
            on_task(t);
        }
    }
}

// =========================================================================
// Actor-based extraction pipeline
// =========================================================================
//
// Independent actor threads communicating via typed crossbeam channels.
// PipelineOrchestrator manages setup, lifecycle, and the consumer loop.
// OcrPipelineActor wraps the OCR post-processing pipeline.

// ── Formal Actor Infrastructure ──────────────────────────────────────────

/// A typed sender reference to an actor's mailbox.
#[derive(Clone)]
pub(crate) struct ActorRef<Msg> {
    tx: crossbeam_channel::Sender<Msg>,
}

impl<Msg> ActorRef<Msg> {
    pub fn new(tx: crossbeam_channel::Sender<Msg>) -> Self {
        Self { tx }
    }

    pub fn send(&self, msg: Msg) -> Result<(), crossbeam_channel::SendError<Msg>> {
        self.tx.send(msg)
    }
}

/// Common trait for all pipeline actors.
/// Implementors process one message at a time via `handle()`.
#[allow(dead_code)]
trait Actor: Send + 'static {
    type Msg: Send + 'static;
    fn handle(&mut self, msg: Self::Msg) -> Result<()>;
}

// ── Formal Message Types ─────────────────────────────────────────────────

/// Messages for the extraction worker pool.
enum ExtractorMsg {
    Extract(Vec<ExtractorTask>),
}

/// Messages for the Tantivy indexer actor.
enum IndexerMsg {
    Index(DocumentRecord),
}

/// Format a slice of batch sizes into a compact human-readable string.
/// Examples: `[5/2/3]`, `[8]`, `[0/0/0]`
fn format_batch_dist(sizes: &[usize]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(sizes.len() * 4);
    out.push('[');
    for (i, s) in sizes.iter().enumerate() {
        if i > 0 { out.push('/'); }
        let _ = write!(out, "{}", s);
    }
    out.push(']');
    out
}

/// Claims pending jobs from the DB and distributes them across workers
/// using heap-greedy balancing. Runs until no more jobs are available
/// or cancel is requested.
struct ScannerActor {
    jobs: Arc<JobStore>,
    metrics: Arc<Metrics>,
    task_tx: crossbeam_channel::Sender<ExtractorMsg>,
    num_workers: usize,
    cancel_flag: Option<Arc<AtomicBool>>,
    reservoir_size: i64,
    log_cb: Option<extern "C" fn(*const u8, u32)>,
}

impl ScannerActor {
    fn run(self) {
        let mut reserved: Vec<(i64, String, String, i64)> = Vec::new();
        let mut round: u64 = 0;
        let mut total_claimed: u64 = 0;

        loop {
            if self.cancel_flag.as_ref().map_or(false, |f| f.load(Ordering::Relaxed)) {
                log_msg(self.log_cb, "[scanner] cancel requested — stopping");
                break;
            }

            round += 1;
            let round_start = Instant::now();

            let mut claim_size = self.num_workers as i64 * BATCH_SIZE;
            let mut all: Vec<(i64, String, String, i64)>;

            let mut from_reservoir: usize = 0;
            if !reserved.is_empty() {
                let take = std::cmp::min(claim_size, reserved.len() as i64) as usize;
                all = reserved.drain(..take).collect();
                from_reservoir = all.len();
                claim_size -= all.len() as i64;
            } else {
                all = Vec::new();
            }

            if claim_size > 0 {
                match self.jobs.claim_pending(claim_size + self.reservoir_size) {
                    Ok(batch) => {
                        total_claimed += batch.len() as u64;
                        self.metrics.add_scanner_claimed(batch.len() as u64);
                        all.extend(batch);
                    }
                    Err(_e) => { if all.is_empty() { break; } }
                }
            }

            if all.is_empty() {
                log_msg(self.log_cb, &format!("[scanner] round {} — no more pending jobs, stopping", round));
                break;
            }

            all.sort_by(|a, b| b.3.cmp(&a.3));

            let target = self.num_workers * BATCH_SIZE as usize;
            let distribute_end = std::cmp::min(target, all.len());

            let mut worker_batches: Vec<Vec<ExtractorTask>> =
                (0..self.num_workers).map(|_| Vec::new()).collect();

            #[derive(Clone, Eq, PartialEq)]
            struct HeapEntry {
                total_mb: i64,
                count: usize,
                worker_idx: usize,
            }
            impl Ord for HeapEntry {
                fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                    other.total_mb.cmp(&self.total_mb)
                        .then(other.count.cmp(&self.count))
                }
            }
            impl PartialOrd for HeapEntry {
                fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                    Some(self.cmp(other))
                }
            }

            let mut heap: std::collections::BinaryHeap<HeapEntry> =
                std::collections::BinaryHeap::new();
            for i in 0..self.num_workers {
                heap.push(HeapEntry { total_mb: 0, count: 0, worker_idx: i });
            }

            for &(id, ref path, ref checksum, size) in &all[..distribute_end] {
                let mut entry = loop {
                    let e = heap.pop().expect("heap should not be empty");
                    if e.count < BATCH_SIZE as usize { break e; }
                };
                worker_batches[entry.worker_idx].push(ExtractorTask {
                    id,
                    path: path.clone(),
                    checksum: checksum.clone(),
                });
                entry.total_mb += size;
                entry.count += 1;
                heap.push(entry);
            }

            let leftover = all.len() - distribute_end;
            reserved.extend(all.into_iter().skip(distribute_end));

            let sent_count: usize = worker_batches.iter().map(|b| b.len()).sum();
            let batch_sizes: Vec<usize> = worker_batches.iter().map(|b| b.len()).collect();

            log_msg(self.log_cb, &format!(
                "[scanner] round {} — claimed {}, from_reservoir={}, distributed {} in {:?} batches, {} to reservoir, {:.3}s",
                round, sent_count + leftover + from_reservoir, from_reservoir,
                sent_count, format_batch_dist(&batch_sizes),
                leftover + reserved.len(),
                round_start.elapsed().as_secs_f64(),
            ));

            for tasks in worker_batches {
                if !tasks.is_empty() {
                    self.metrics.set_task_queue_depth(self.task_tx.len() as u64);
                    if self.task_tx.send(ExtractorMsg::Extract(tasks)).is_err() {
                        log_msg(self.log_cb, "[scanner] task channel disconnected — stopping");
                        return;
                    }
                }
            }
        }

        log_msg(self.log_cb, &format!(
            "[scanner] done — {} total claimed, {} rounds",
            total_claimed, round
        ));
    }
}

/// Manages N extraction worker threads, each running one IPC subprocess.
/// Workers share the task_rx (each gets a clone) and feed results into
/// result_tx. Exits when all workers have finished (triggered when the
/// ScannerActor drops task_tx, causing task_rx to return Disconnected).
struct WorkerPoolActor {
    task_rx: crossbeam_channel::Receiver<ExtractorMsg>,
    result_tx: crossbeam_channel::Sender<DocumentRecord>,
    jobs: Arc<JobStore>,
    metrics: Arc<Metrics>,
    num_workers: usize,
    worker_path: PathBuf,
    cancel_flag: Option<Arc<AtomicBool>>,
    log_cb: Option<extern "C" fn(*const u8, u32)>,
    process_cb: Option<extern "C" fn(*const u8, u32)>,
}

impl WorkerPoolActor {
    fn run(self) {
        let mut handles = Vec::new();
        for i in 0..self.num_workers {
            let task_rx = self.task_rx.clone();
            let result_tx = self.result_tx.clone();
            let jobs = Arc::clone(&self.jobs);
            let metrics = Arc::clone(&self.metrics);
            let wp = self.worker_path.clone();
            let cancel = self.cancel_flag.clone();
            let log_cb = self.log_cb;
            let process_cb = self.process_cb;
            let handle = thread::Builder::new()
                .name(format!("extract-{}", i))
                .spawn(move || {
                    run_extraction_process(
                        &wp, &task_rx, &result_tx, &jobs, &metrics,
                        cancel, log_cb, process_cb,
                    );
                })
                .expect("Failed to spawn worker thread");
            handles.push(handle);
        }

        for h in handles {
            h.join().expect("Worker panicked");
        }
    }
}

impl Actor for WorkerPoolActor {
    type Msg = ExtractorMsg;
    fn handle(&mut self, _msg: ExtractorMsg) -> Result<()> {
        // WorkerPoolActor is a manager — individual workers consume
        // ExtractorMsg directly from the shared channel.  The handle()
        // method is provided for API completeness.
        Ok(())
    }
}

/// Batches DocumentRecords and writes them to a Tantivy index.
/// Delegates to the existing indexer_thread helper.
struct IndexerActor {
    rx: crossbeam_channel::Receiver<IndexerMsg>,
    indexer: Arc<Indexer>,
    jobs: Arc<JobStore>,
    metrics: Arc<Metrics>,
    batch_size: usize,
    commit_interval: u64,
    commit_timeout: u64,
    num_threads: usize,
    log_cb: Option<extern "C" fn(*const u8, u32)>,
}

impl IndexerActor {
    fn run(self) {
        indexer_thread(
            &self.indexer, self.jobs, self.rx, &self.metrics,
            self.batch_size, self.commit_interval, self.commit_timeout,
            self.num_threads, self.log_cb,
        );
    }
}

impl Actor for IndexerActor {
    type Msg = IndexerMsg;
    fn handle(&mut self, msg: IndexerMsg) -> Result<()> {
        let IndexerMsg::Index(_record) = msg;
        // The full batching + commit logic lives in indexer_thread;
        // handle() provides a formal per-message entry for testing.
        // In practice, run() delegates to indexer_thread which owns
        // the receive loop.
        Ok(())
    }
}

/// Orchestrates the entire extraction pipeline:
/// 1. Scans directory, creates channels
/// 2. Spawns ScannerActor, WorkerPoolActor, IndexerActor
/// 3. Runs the consumer loop (blocking, in orchestrator's thread)
/// 4. Handles cancel, joins all actors, cleans up
pub struct PipelineOrchestrator {
    config: PipelineConfig,
    progress_cb: Option<Box<dyn Fn(u64, u64) + Send>>,
    jobs: Arc<JobStore>,
    metrics: Arc<Metrics>,
    input: PathBuf,
    indexer: Option<Arc<Indexer>>,
}

impl PipelineOrchestrator {
    pub fn new(
        mut config: PipelineConfig,
        jobs: Arc<JobStore>,
        metrics: Arc<Metrics>,
        input: PathBuf,
        indexer: Option<Arc<Indexer>>,
    ) -> Self {
        let progress_cb = config.progress_cb.take();
        Self {
            config: PipelineConfig {
                progress_cb: None,
                ..config
            },
            progress_cb,
            jobs,
            metrics,
            input,
            indexer,
        }
    }

    fn process_record(
        &self,
        record: DocumentRecord,
        result_rx: &crossbeam_channel::Receiver<DocumentRecord>,
        index_ref: Option<&ActorRef<IndexerMsg>>,
        total_pending: u64,
        progress_throttle: u64,
        log_cb: Option<extern "C" fn(*const u8, u32)>,
    ) -> bool {
        self.metrics.set_result_queue_depth(result_rx.len() as u64);
        let processed = self.metrics.increment_processed();

        if record.file_extraction_ms > 0 {
            log_msg(log_cb, &format!(
                "[timing] file={} pages={} extraction={:.3}s",
                record.path,
                record.page_count,
                record.file_extraction_ms as f64 / 1000.0,
            ));
        }

        if let Some(index_ref) = index_ref {
            let id = record.id;
            let path = record.path.clone();
            if index_ref.send(IndexerMsg::Index(record)).is_err() {
                log_msg(log_cb, &format!(
                    "[pipeline] indexer channel disconnected for id={} path='{}' — indexer unavailable, will retry on next run",
                    id, path
                ));
            }
        }

        if let Some(ref cb) = self.progress_cb {
            let done = processed + self.metrics.errored();
            if done % progress_throttle == 0 || done >= total_pending {
                cb(done, total_pending);
            }
        }
        self.metrics.log_summary_with(log_cb);
        true
    }

    /// Run the full extraction pipeline. Blocks until all documents
    /// are processed, cancelled, or an error occurs.
    pub fn run(self) -> Result<()> {
        let pipeline_start = Instant::now();

        scan_directory(&self.jobs, &self.input)?;
        let scan_elapsed = pipeline_start.elapsed();
        let pending = self.jobs.count_pending()?;
        if pending == 0 {
            log_msg(self.config.log_cb, "[pipeline] no pending jobs");
            return Ok(());
        }

        log_msg(self.config.log_cb, &format!(
            "[pipeline] scan_directory took {:.3}s, {} pending",
            scan_elapsed.as_secs_f64(), pending
        ));

        self.config.worker_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(
                "worker_path is required: pdf_worker.exe must be deployed alongside the library. \
                 Run 'cargo build -p pdf_extractor --bin pdf_worker' to build it."
            ))?;

        let cap = self.config.channel_cap();
        let (task_tx, task_rx) = bounded::<ExtractorMsg>(cap);
        let (result_tx, result_rx) = bounded::<DocumentRecord>(cap);
        let (index_tx, index_rx) = bounded::<IndexerMsg>(cap);

        let index_ref = self.indexer.as_ref().map(|_| ActorRef::new(index_tx.clone()));

        let num_workers = self.config.extract_workers();
        let log_cb = self.config.log_cb;
        let process_cb = self.config.process_cb;
        let worker_path = self.config.worker_path.clone().expect("worker_path validated above");

        // ── Spawn ScannerActor ──
        let scanner = ScannerActor {
            jobs: Arc::clone(&self.jobs),
            metrics: Arc::clone(&self.metrics),
            task_tx,
            num_workers,
            cancel_flag: self.config.cancel_flag.clone(),
            reservoir_size: self.config.reservoir_size() as i64,
            log_cb,
        };
        let scanner_handle = thread::Builder::new()
            .name("scanner".into())
            .spawn(move || scanner.run())
            .expect("Failed to spawn scanner thread");

        // ── Spawn WorkerPoolActor ──
        let pool = WorkerPoolActor {
            task_rx,
            result_tx: result_tx.clone(),
            jobs: Arc::clone(&self.jobs),
            metrics: Arc::clone(&self.metrics),
            num_workers,
            worker_path,
            cancel_flag: self.config.cancel_flag.clone(),
            log_cb,
            process_cb,
        };
        let pool_handle = thread::Builder::new()
            .name("worker-pool".into())
            .spawn(move || pool.run())
            .expect("Failed to spawn worker pool thread");

        drop(result_tx);

        // ── Spawn IndexerActor ──
        let indexer_handle = self.indexer.as_ref().map(|idx| {
            let idx = Arc::clone(idx);
            let metrics = Arc::clone(&self.metrics);
            let jobs = Arc::clone(&self.jobs);
            let ibs = self.config.indexer_batch();
            let ci = self.config.commit_int();
            let ct = self.config.commit_to();
            let it = self.config.indexer_threads();
            thread::Builder::new()
                .name("indexer".into())
                .spawn(move || {
                    IndexerActor {
                        rx: index_rx,
                        indexer: idx,
                        jobs,
                        metrics,
                        batch_size: ibs,
                        commit_interval: ci,
                        commit_timeout: ct,
                        num_threads: it,
                        log_cb,
                    }.run();
                })
                .expect("Failed to spawn indexer thread")
        });

        // ── Consumer loop ──
        let extraction_start = Instant::now();
        let total_pending = pending as u64;
        let is_cancelled = || self.config.cancel_flag.as_ref().map_or(false, |f| f.load(Ordering::Relaxed));
        let progress_throttle = 50u64;

        // Phase 1: normal blocking receive, check cancel between records
        for record in &result_rx {
            if is_cancelled() {
                break;
            }
            if !self.process_record(record, &result_rx, index_ref.as_ref(),
                total_pending, progress_throttle, log_cb)
            {
                break;
            }
        }

        // Phase 2: drain any records still in-flight from workers
        if is_cancelled() {
            loop {
                match result_rx.try_recv() {
                    Ok(record) => {
                        self.process_record(record, &result_rx, index_ref.as_ref(),
                            total_pending, progress_throttle, log_cb);
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }
            }

            pool_handle.join().expect("Worker pool panicked");

            loop {
                match result_rx.try_recv() {
                    Ok(record) => {
                        self.process_record(record, &result_rx, index_ref.as_ref(),
                            total_pending, progress_throttle, log_cb);
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }
            }
        } else {
            pool_handle.join().expect("Worker pool panicked");
        }

        // ── Shutdown remaining actors ──
        drop(index_tx);
        drop(index_ref);
        scanner_handle.join().expect("Scanner panicked");
        if let Some(h) = indexer_handle {
            h.join().expect("Indexer panicked");
        }

        // ── Cleanup ──
        let extract_elapsed = extraction_start.elapsed();
        let total_elapsed = pipeline_start.elapsed();
        log_msg(log_cb, &format!(
            "[timing] scan={:.3}s extract={:.3}s total={:.3}s",
            scan_elapsed.as_secs_f64(),
            extract_elapsed.as_secs_f64(),
            total_elapsed.as_secs_f64(),
        ));

        self.jobs.reprocess_extracting()?;

        let processed = self.metrics.processed();
        let errored = self.metrics.errored();
        let elapsed = format_uptime(total_elapsed);
        log_msg(log_cb, &format!(
            "[pipeline] done — {} total ({:?} processed, {:?} errored) in {} ({:.1} docs/s)",
            processed + errored, processed, errored, elapsed,
            if total_elapsed.as_secs_f64() > 0.0 { processed as f64 / total_elapsed.as_secs_f64() } else { 0.0 },
        ));

        if self.metrics.indexer_failed() {
            anyhow::bail!("Indexer failed to initialize — Tantivy index writer could not be created. Check disk space, permissions, and index integrity.");
        }

        Ok(())
    }
}

/// Self-contained actor for OCR post-processing.
///
/// Creates internal producer/worker/consumer threads:
/// - Producer fetches OCR-needed docs from DB in batches
/// - N workers run Tesseract on rendered PDF page images
/// - Consumer re-indexes OCR results into Tantivy
pub struct OcrPipelineActor {
    jobs: Arc<JobStore>,
    indexer: Option<Arc<Indexer>>,
    ocr_config: ocr::OcrConfig,
    num_workers: usize,
    cancel_flag: Option<Arc<AtomicBool>>,
    log_cb: Option<extern "C" fn(*const u8, u32)>,
}

impl OcrPipelineActor {
    pub fn new(
        jobs: Arc<JobStore>,
        indexer: Option<Arc<Indexer>>,
        ocr_config: &ocr::OcrConfig,
        num_workers: usize,
        cancel_flag: Option<Arc<AtomicBool>>,
        log_cb: Option<extern "C" fn(*const u8, u32)>,
    ) -> Self {
        Self {
            jobs,
            indexer,
            ocr_config: ocr::OcrConfig {
                max_retries: ocr_config.max_retries,
                tesseract_path: ocr_config.tesseract_path.clone(),
                max_dim: ocr_config.max_dim,
                language: ocr_config.language.clone(),
            },
            num_workers,
            cancel_flag,
            log_cb,
        }
    }

    /// Run the OCR post-processing pipeline. Returns the number of
    /// documents successfully OCR-processed.
    pub fn run(self) -> Result<u64> {
        let pipeline_start = Instant::now();
        let max_retries = self.ocr_config.max_retries;
        let pending = self.jobs.count_ocr_pending(max_retries)?;
        if pending == 0 {
            log_msg(self.log_cb, "[ocr] no pending OCR jobs");
            return Ok(0);
        }

        log_msg(self.log_cb, &format!(
            "[ocr] starting — {} pending docs, {} workers",
            pending, self.num_workers
        ));

        let (ocr_tx, ocr_rx) = bounded::<(i64, String, String)>(100);
        let (result_tx, result_rx) = bounded::<(i64, String, String, String)>(100);

        let consumer_cancel = self.cancel_flag.clone();
        let consumer_jobs = Arc::clone(&self.jobs);
        let consumer_indexer = self.indexer.clone();
        let consumer_log_cb = self.log_cb;
        let consumer_handle = thread::spawn(move || {
            let mut ocr_processed: u64 = 0;
            let mut ocr_errored: u64 = 0;

            let mut index_writer = consumer_indexer.as_ref().and_then(|idx| {
                let result = idx.search_index().writer_with_num_threads(1);
                if result.is_err() {
                    log_msg(consumer_log_cb, "[ocr-consumer] failed to create Tantivy IndexWriter for OCR re-indexing");
                }
                result.ok()
            });

            for (id, path, _checksum, ocr_text) in &result_rx {
                if consumer_cancel.as_ref().map_or(false, |f| f.load(Ordering::Relaxed)) {
                    break;
                }
                if ocr_text.is_empty() {
                    consumer_jobs.mark_ocr_attempt(id, false, Some("OCR returned empty text"), max_retries).ok();
                    ocr_errored += 1;
                    continue;
                }
                ocr_processed += 1;
                consumer_jobs.mark_ocr_attempt(id, true, None, max_retries).ok();

                if let (Some(idx), Some(w)) = (consumer_indexer.as_ref(), index_writer.as_mut()) {
                    let term = tantivy::Term::from_field_u64(idx.search_index().id_field, id as u64);
                    w.delete_term(term);
                    if let Err(e) = idx.search_index().add_document(
                        w, id, &path, &ocr_text,
                    ) {
                        log_msg(consumer_log_cb, &format!(
                            "[ocr-consumer] add_document_with_ts failed for id={} path='{}': {}",
                            id, path, e
                        ));
                    }
                }
            }

            if let Some(ref mut w) = index_writer {
                if let Err(e) = w.commit() {
                    log_msg(consumer_log_cb, &format!(
                        "[ocr-consumer] final commit failed: {}", e
                    ));
                }
            }

            (ocr_processed, ocr_errored)
        });

        let producer_jobs = Arc::clone(&self.jobs);
        let producer_handle = thread::spawn(move || {
            let mut sent_ids = HashSet::new();
            loop {
                match producer_jobs.fetch_ocr_needed(100, max_retries) {
                    Ok(batch) if batch.is_empty() => break,
                    Ok(batch) => {
                        for (id, path, checksum) in deduplicate_ocr_batch(batch, &mut sent_ids) {
                            if ocr_tx.send((id, path, checksum)).is_err() {
                                return;
                            }
                        }
                    }
                    Err(_e) => break,
                }
            }
            drop(ocr_tx);
        });

        let mut worker_handles = Vec::new();
        let config = ocr::OcrConfig {
            max_retries: self.ocr_config.max_retries,
            tesseract_path: self.ocr_config.tesseract_path.clone(),
            max_dim: self.ocr_config.max_dim,
            language: self.ocr_config.language.clone(),
        };

        let mut pool = ocr::TesseractPool::new(self.num_workers, &config.tesseract_path, &config.language)
            .context("Failed to create Tesseract subprocess pool")?;

        for i in 0..self.num_workers {
            let rx = ocr_rx.clone();
            let tx = result_tx.clone();
            let cfg = ocr::OcrConfig {
                max_retries: config.max_retries,
                tesseract_path: config.tesseract_path.clone(),
                max_dim: config.max_dim,
                language: config.language.clone(),
            };
            let mut worker = pool.take_worker()
                .expect("Not enough workers in pool");
            let worker_cancel = self.cancel_flag.clone();

            let handle = thread::Builder::new()
                .name(format!("ocr-{}", i))
                .spawn(move || {
                    for (id, path_str, checksum) in &rx {
                        if worker_cancel.as_ref().map_or(false, |f| f.load(Ordering::Relaxed)) {
                            break;
                        }
                        let path_obj = PathBuf::from(&path_str);
                        let ocr_result = run_single_ocr(&path_obj, &cfg, Some(&mut worker));
                        match ocr_result {
                            Ok(text) if !text.is_empty() => {
                                tx.send((id, path_str, checksum, text)).ok();
                            }
                            _ => {
                                tx.send((id, path_str, checksum, String::new())).ok();
                            }
                        }
                    }
                })
                .expect("Failed to spawn OCR worker");
            worker_handles.push(handle);
        }

        drop(ocr_rx);
        drop(result_tx);

        producer_handle.join().expect("OCR producer panicked");

        for h in worker_handles {
            h.join().expect("OCR worker panicked");
        }

        let (ocr_processed, ocr_errored) = consumer_handle.join().expect("OCR consumer panicked");

        let ocr_elapsed = pipeline_start.elapsed();
        log_msg(self.log_cb, &format!(
            "[ocr] done — {} processed, {} errored, {} pending, {:.3}s total ({:.1} docs/s)",
            ocr_processed, ocr_errored, pending,
            ocr_elapsed.as_secs_f64(),
            if ocr_elapsed.as_secs_f64() > 0.0 { ocr_processed as f64 / ocr_elapsed.as_secs_f64() } else { 0.0 },
        ));

        Ok(ocr_processed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_ipc::WorkerOutput;

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    /// Locate `pdf_worker.exe` from the test binary's directory.
    /// The test binary lives at `target/<profile>/deps/pdf_extractor-<hash>.exe`;
    /// the worker lives at `target/<profile>/pdf_worker.exe`.
    fn find_worker_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let profile_dir = exe.parent()?.parent()?; // target/<profile>/
        let worker = profile_dir.join("pdf_worker.exe");
        if worker.exists() { Some(worker) } else { None }
    }

    /// Check whether `pdfium.dll` is available next to the worker binary.
    /// Extraction tests are skipped when the DLL is absent (CI/dev machines
    /// without a pdfium deployment).
    fn is_pdfium_available() -> bool {
        find_worker_binary()
            .and_then(|w| w.parent().map(|d| d.join("pdfium.dll")))
            .map_or(false, |dll| dll.exists())
    }

    /// Create a minimal valid PDF at `path` containing `text` on a single page.
    fn make_test_pdf_at(path: &Path, text: &str) {
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
        // Validate by loading back
        Document::load(path).unwrap();
    }

    /// Create a minimal PDF and return its path (inside `dir`).
    fn make_test_pdf(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join(format!("doc_{}.pdf", text.len()));
        make_test_pdf_at(&path, text);
        path
    }

    #[test]
    fn test_dedup_ocr_batch_all_new() {
        let batch = vec![
            (1, "a.pdf".into(), "aaa".into()),
            (2, "b.pdf".into(), "bbb".into()),
            (3, "c.pdf".into(), "ccc".into()),
        ];
        let mut sent = HashSet::new();
        let result = deduplicate_ocr_batch(batch, &mut sent);
        assert_eq!(result.len(), 3);
        assert!(sent.contains(&1));
        assert!(sent.contains(&2));
        assert!(sent.contains(&3));
    }

    #[test]
    fn test_dedup_ocr_batch_partial_duplicates() {
        let batch = vec![
            (1, "a.pdf".into(), "aaa".into()),
            (2, "b.pdf".into(), "bbb".into()),
            (1, "a.pdf".into(), "aaa".into()),
            (3, "c.pdf".into(), "ccc".into()),
            (2, "b.pdf".into(), "bbb".into()),
        ];
        let mut sent = HashSet::new();
        let result = deduplicate_ocr_batch(batch, &mut sent);
        assert_eq!(result.len(), 3, "should only return unique IDs");
        assert_eq!(result[0].0, 1);
        assert_eq!(result[1].0, 2);
        assert_eq!(result[2].0, 3);
    }

    #[test]
    fn test_dedup_ocr_batch_all_already_sent() {
        let mut sent = HashSet::from([1, 2, 3]);
        let batch = vec![
            (1, "a.pdf".into(), "aaa".into()),
            (2, "b.pdf".into(), "bbb".into()),
        ];
        let result = deduplicate_ocr_batch(batch, &mut sent);
        assert!(result.is_empty(), "no new jobs to send");
    }

    #[test]
    fn test_dedup_ocr_batch_empty_batch() {
        let mut sent = HashSet::new();
        let result = deduplicate_ocr_batch(vec![], &mut sent);
        assert!(result.is_empty());
    }

    #[test]
    fn test_dedup_ocr_batch_preserves_path_and_checksum() {
        let batch = vec![(
            1i64,
            "/path/to/doc.pdf".into(),
            "deadbeef12345678".into(),
        )];
        let mut sent = HashSet::new();
        let result = deduplicate_ocr_batch(batch, &mut sent);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, "/path/to/doc.pdf");
        assert_eq!(result[0].2, "deadbeef12345678");
    }

    #[test]
    fn test_dedup_ocr_batch_mixed_insertion_after_send() {
        let batch = vec![(1, "x.pdf".into(), "x".into())];
        let mut sent = HashSet::new();
        let r1 = deduplicate_ocr_batch(batch, &mut sent);
        assert_eq!(r1.len(), 1);
        let r2 = deduplicate_ocr_batch(vec![(1, "x.pdf".into(), "x".into())], &mut sent);
        assert!(r2.is_empty(), "ID 1 was already sent");
    }

    #[test]
    fn test_with_scratch_buf_basic_write_and_read() {
        let result = with_scratch_buf(|buf| {
            buf.push("hello".to_string());
            buf.push("world".to_string());
            buf.len()
        });
        assert_eq!(result, 2);
    }

    #[test]
    fn test_with_scratch_buf_clears_between_calls() {
        with_scratch_buf(|buf| {
            buf.push("data".to_string());
        });
        let len = with_scratch_buf(|buf| {
            buf.len()
        });
        assert_eq!(len, 0, "Buffer should be cleared after each call");
    }

    #[test]
    fn test_with_scratch_buf_shrink_large_capacity() {
        with_scratch_buf(|buf| {
            let mut large = String::with_capacity(2_000_000);
            large.push_str("x");
            buf.push(large);
        });
        with_scratch_buf(|buf| {
            assert!(buf.capacity() <= 64_000, "Buffer should be shrunk after large allocation");
        });
    }

    #[test]
    fn test_with_scratch_buf_keeps_small_capacity() {
        with_scratch_buf(|buf| {
            buf.push("small".to_string());
        });
        with_scratch_buf(|buf| {
            assert!(buf.capacity() < 1_000_000 || buf.capacity() <= 64_000);
        });
    }

    #[test]
    fn test_extract_page_count_basic_mutool_format() {
        let output = "PDF 1.7\nPages: 5\nSome other metadata\n";
        assert_eq!(extract_page_count_from_stdout(output), Some(5));
    }

    #[test]
    fn test_extract_page_count_basic_pdfinfo_format() {
        let output = "Creator: foo\nProducer: bar\nPages: 12\nFile size: 1234\n";
        assert_eq!(extract_page_count_from_stdout(output), Some(12));
    }

    #[test]
    fn test_extract_page_count_single_page() {
        assert_eq!(extract_page_count_from_stdout("Pages: 1\n"), Some(1));
    }

    #[test]
    fn test_extract_page_count_zero_is_rejected() {
        assert_eq!(extract_page_count_from_stdout("Pages: 0\n"), None);
    }

    #[test]
    fn test_extract_page_count_no_pages_line() {
        let output = "Title: Untitled\nAuthor: nobody\n";
        assert_eq!(extract_page_count_from_stdout(output), None);
    }

    #[test]
    fn test_extract_page_count_empty_output() {
        assert_eq!(extract_page_count_from_stdout(""), None);
    }

    #[test]
    fn test_extract_page_count_malformed_number() {
        let output = "Pages: abc\n";
        assert_eq!(extract_page_count_from_stdout(output), None);
    }

    #[test]
    fn test_extract_page_count_case_sensitive() {
        // regex is case-sensitive; lowercase "pages:" should not match
        assert_eq!(extract_page_count_from_stdout("pages: 3\n"), None);
    }

    // --- resolve_ocr_workers ---

    #[test]
    fn test_resolve_workers_uses_override() {
        assert_eq!(resolve_ocr_workers(Some(5)), 5);
    }

    #[test]
    fn test_resolve_workers_clamps_zero_to_one() {
        assert_eq!(resolve_ocr_workers(Some(0)), 1);
    }

    #[test]
    fn test_resolve_workers_accepts_large_value() {
        assert_eq!(resolve_ocr_workers(Some(100)), 100);
    }

    #[test]
    fn test_resolve_workers_fallback_at_least_one() {
        // None → auto-detect; even on a 1-core machine the result should be ≥1
        let result = resolve_ocr_workers(None);
        assert!(result >= 1, "Auto-detect should yield at least 1 worker, got {}", result);
    }

    // --- PipelineConfig ---

    #[test]
    fn test_pipeline_config_default_extract_workers() {
        let cfg = PipelineConfig::default();
        let workers = cfg.extract_workers();
        assert!(workers >= 1, "Default workers should be ≥1, got {}", workers);
    }

    #[test]
    fn test_pipeline_config_custom_extract_workers() {
        let cfg = PipelineConfig {
            num_extract_workers: Some(4),
            ..Default::default()
        };
        assert_eq!(cfg.extract_workers(), 4);
    }

    #[test]
    fn test_pipeline_config_extract_workers_clamps_zero() {
        let cfg = PipelineConfig {
            num_extract_workers: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.extract_workers(), 1, "Zero workers should clamp to 1");
    }

    #[test]
    fn test_pipeline_config_default_indexer_batch() {
        let cfg = PipelineConfig::default();
        assert_eq!(cfg.indexer_batch(), DEFAULT_INDEXER_BATCH_SIZE);
    }

    #[test]
    fn test_pipeline_config_custom_indexer_batch() {
        let cfg = PipelineConfig {
            indexer_batch_size: Some(100),
            ..Default::default()
        };
        assert_eq!(cfg.indexer_batch(), 100);
    }

    #[test]
    fn test_pipeline_config_indexer_batch_clamps_zero() {
        let cfg = PipelineConfig {
            indexer_batch_size: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.indexer_batch(), 1, "Zero batch should clamp to 1");
    }

    #[test]
    fn test_pipeline_config_default_commit_interval() {
        let cfg = PipelineConfig::default();
        assert_eq!(cfg.commit_int(), DEFAULT_COMMIT_INTERVAL);
    }

    #[test]
    fn test_pipeline_config_custom_commit_interval() {
        let cfg = PipelineConfig {
            commit_interval: Some(1000),
            ..Default::default()
        };
        assert_eq!(cfg.commit_int(), 1000);
    }

    #[test]
    fn test_pipeline_config_commit_interval_clamps_zero() {
        let cfg = PipelineConfig {
            commit_interval: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.commit_int(), 1, "Zero interval should clamp to 1");
    }

    #[test]
    fn test_pipeline_config_default_commit_timeout() {
        let cfg = PipelineConfig::default();
        assert_eq!(cfg.commit_to(), DEFAULT_COMMIT_TIMEOUT_SECS);
    }

    #[test]
    fn test_pipeline_config_custom_commit_timeout() {
        let cfg = PipelineConfig {
            commit_timeout: Some(60),
            ..Default::default()
        };
        assert_eq!(cfg.commit_to(), 60);
    }

    #[test]
    fn test_pipeline_config_commit_timeout_clamps_zero() {
        let cfg = PipelineConfig {
            commit_timeout: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.commit_to(), 1, "Zero timeout should clamp to 1");
    }

    #[test]
    fn test_pipeline_config_default_indexer_threads() {
        let cfg = PipelineConfig::default();
        assert!(cfg.indexer_threads() >= 1, "Default indexer threads should be ≥1, got {}", cfg.indexer_threads());
    }

    #[test]
    fn test_pipeline_config_custom_indexer_threads() {
        let cfg = PipelineConfig {
            num_indexer_threads: Some(4),
            ..Default::default()
        };
        assert_eq!(cfg.indexer_threads(), 4);
    }

    #[test]
    fn test_pipeline_config_indexer_threads_clamps_zero() {
        let cfg = PipelineConfig {
            num_indexer_threads: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.indexer_threads(), 1, "Zero indexer threads should clamp to 1");
    }

    // ── collect_batch tests ──

    #[allow(dead_code)]
    fn make_task(id: i64, path: &str) -> ExtractorTask {
        ExtractorTask { id, path: path.into(), checksum: format!("c{}", id) }
    }

    fn make_batch(ids: &[i64], prefix: &str) -> Vec<ExtractorTask> {
        ids.iter().map(|&id| make_task(id, &format!("{}{}.pdf", prefix, id))).collect()
    }

    #[test]
    fn test_collect_batch_empty_disconnected() {
        let (tx, rx) = bounded::<ExtractorMsg>(10);
        drop(tx);
        let batch = collect_batch(&rx);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_collect_batch_single_task() {
        let (tx, rx) = bounded::<ExtractorMsg>(10);
        tx.send(ExtractorMsg::Extract(vec![make_task(1, "a.pdf")])).unwrap();
        drop(tx);
        let batch = collect_batch(&rx);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1);
        assert_eq!(batch[0].path, "a.pdf");
    }

    #[test]
    fn test_collect_batch_multiple_tasks_in_one_batch() {
        let (tx, rx) = bounded::<ExtractorMsg>(10);
        tx.send(ExtractorMsg::Extract(make_batch(&[1, 2, 3, 4, 5], ""))).unwrap();
        drop(tx);
        let batch = collect_batch(&rx);
        assert_eq!(batch.len(), 5);
    }

    #[test]
    fn test_collect_batch_blocks_until_first_arrives() {
        let (tx, rx) = bounded::<ExtractorMsg>(10);
        let tx_clone = tx.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            tx_clone.send(ExtractorMsg::Extract(vec![make_task(99, "late.pdf")])).unwrap();
            drop(tx_clone);
        });
        let batch = collect_batch(&rx);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 99);
        handle.join().unwrap();
    }

    #[test]
    fn test_collect_batch_disconnected_returns_partial() {
        let (tx, rx) = bounded::<ExtractorMsg>(10);
        tx.send(ExtractorMsg::Extract(make_batch(&[1, 2, 3], ""))).unwrap();
        // drop tx without sending more — should return [1, 2, 3]
        drop(tx);
        let batch = collect_batch(&rx);
        assert_eq!(batch.len(), 3);
    }

    // ── Producer-side batch sending ──

    #[test]
    fn test_producer_claims_nothing_when_empty() {
        let store = super::super::scanner::JobStore::open_in_memory().unwrap();
        let batch = store.claim_pending(100).unwrap();
        assert!(batch.is_empty(), "no pending jobs → empty batch");
    }

    #[test]
    fn test_producer_claims_and_sends_batch() {
        let store = super::super::scanner::JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "cs1").unwrap();
        store.upsert_file("/b.pdf", "cs2").unwrap();
        store.upsert_file("/c.pdf", "cs3").unwrap();

        let (task_tx, task_rx) = bounded::<ExtractorMsg>(10);
        let batch_size: i64 = 2;

        // Simulate the producer: claim pending, batch, send
        let batch = store.claim_pending(batch_size).unwrap();
        assert_eq!(batch.len(), 2, "should claim up to batch_size=2");
        let tasks: Vec<ExtractorTask> = batch
            .into_iter()
            .map(|(id, path, checksum, _size)| ExtractorTask { id, path, checksum })
            .collect();
        task_tx.send(ExtractorMsg::Extract(tasks)).unwrap();

        // Verify the batch arrived on the channel
        let received = collect_batch(&task_rx);
        assert_eq!(received.len(), 2);
        assert!(received.iter().any(|t| t.path == "/a.pdf"));
        assert!(received.iter().any(|t| t.path == "/b.pdf"));

        // Remaining pending tasks should be claimable in a second batch
        let remaining = store.claim_pending(100).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1, "/c.pdf");
    }

    // ── run_extraction_process error handling tests ──

    #[test]
    fn test_process_worker_missing_binary_no_crash() {
        let (task_tx, task_rx) = bounded::<ExtractorMsg>(10);
        let (result_tx, result_rx) = bounded::<DocumentRecord>(10);
        let jobs = Arc::new(super::super::scanner::JobStore::open_in_memory().unwrap());
        let metrics = Arc::new(super::super::metrics::Metrics::new());

        task_tx.send(ExtractorMsg::Extract(vec![make_task(1, "a.pdf")])).unwrap();
        drop(task_tx);

        run_extraction_process(
            Path::new(r"C:\__nonexistent_worker_12345__.exe"),
            &task_rx, &result_tx, &jobs, &metrics,
            None, None, None,
        );

        // Must not crash; no records should arrive at result channel
        let recv_result = result_rx.try_recv();
        assert!(recv_result.is_err(), "no record should be sent for failed worker: {:?}", recv_result);
    }

    fn make_frame_bytes(frame: &WorkerFrame) -> Vec<u8> {
        let data = bincode::serialize(frame).unwrap();
        let crc = frame_crc32(&data);
        let mut out = Vec::with_capacity(8 + data.len());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    #[test]
    fn test_read_frame_success() {
        let frame = WorkerFrame::Success(WorkerOutput {
            path: "/mocked.pdf".into(),
            checksum: "dummy".into(),
            ocr_flag: false,
            text: "hello from mock".into(),
            word_positions: vec![],
            file_extraction_ms: 0,
            page_count: 1,
        });
        let bytes = make_frame_bytes(&frame);
        let mut reader = io::BufReader::new(std::io::Cursor::new(&bytes));
        let parsed = read_frame(&mut reader).unwrap();
        match parsed {
            WorkerFrame::Success(wo) => {
                assert_eq!(wo.path, "/mocked.pdf");
                assert_eq!(wo.text, "hello from mock");
            }
            _ => panic!("Expected Success frame"),
        }
    }

    #[test]
    fn test_read_frame_error() {
        let frame = WorkerFrame::Error {
            path: "/broken.pdf".into(),
            message: "ERROR:/broken.pdf:parse error".into(),
        };
        let bytes = make_frame_bytes(&frame);
        let mut reader = io::BufReader::new(std::io::Cursor::new(&bytes));
        let parsed = read_frame(&mut reader).unwrap();
        match parsed {
            WorkerFrame::Error { path, message } => {
                assert_eq!(path, "/broken.pdf");
                assert!(message.contains("parse error"));
            }
            _ => panic!("Expected Error frame"),
        }
    }

    #[test]
    fn test_read_frame_garbage_returns_error() {
        let garbage = [0u8; 8];
        let mut reader = io::BufReader::new(std::io::Cursor::new(&garbage));
        let result = read_frame(&mut reader);
        assert!(result.is_err(), "garbage frame should produce an error");
    }

    #[test]
    fn test_read_frame_crc_mismatch_returns_error() {
        let frame = WorkerFrame::Success(WorkerOutput {
            path: "/test.pdf".into(),
            checksum: "dummy".into(),
            ocr_flag: false,
            text: "test".into(),
            word_positions: vec![],
            file_extraction_ms: 0,
            page_count: 1,
        });
        let mut bytes = make_frame_bytes(&frame);
        // Corrupt the last CRC byte (flip all bits)
        let last = bytes.len() - 1;
        bytes[last] = !bytes[last];
        let mut reader = io::BufReader::new(std::io::Cursor::new(&bytes));
        let result = read_frame(&mut reader);
        assert!(result.is_err(), "corrupted CRC should error");
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("CRC mismatch"), "error should mention CRC, got: {}", err);
    }

    #[test]
    fn test_read_frame_truncated_returns_error() {
        let frame = WorkerFrame::Success(WorkerOutput {
            path: "/test.pdf".into(),
            checksum: "dummy".into(),
            ocr_flag: false,
            text: "test".into(),
            word_positions: vec![],
            file_extraction_ms: 0,
            page_count: 1,
        });
        let mut bytes = make_frame_bytes(&frame);
        bytes.truncate(bytes.len() - 3); // remove last 3 bytes
        let mut reader = io::BufReader::new(std::io::Cursor::new(&bytes));
        let result = read_frame(&mut reader);
        assert!(result.is_err(), "truncated frame should produce an error");
    }

    // ── Pipeline validation tests ──

    #[test]
    fn test_run_pipeline_requires_worker_path() {
        use crate::metrics::Metrics;
        use crate::scanner::scan_directory;

        let tmp = std::env::temp_dir().join("pdf_extractor_test_req_worker");
        let _ = std::fs::create_dir_all(&tmp);
        let books = tmp.join("books");
        std::fs::create_dir_all(&books).unwrap();
        std::fs::write(books.join("dummy.pdf"), b"dummy").unwrap();

        let jobs = Arc::new(JobStore::open_in_memory().unwrap());
        let metrics = Arc::new(Metrics::new());

        scan_directory(&jobs, &books).unwrap();

        let config = PipelineConfig {
            worker_path: None,
            ..Default::default()
        };

        let result = run_pipeline(jobs, metrics, &books, None, config);
        assert!(result.is_err(), "run_pipeline should error when worker_path=None");
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.to_lowercase().contains("worker_path"),
            "error should mention worker_path, got: {}",
            err
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── Worker binary integration tests ──

    fn parse_worker_frames(data: &[u8]) -> Vec<WorkerFrame> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset + 4 <= data.len() {
            let len = u32::from_le_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]) as usize;
            offset += 4;
            if offset + len + 4 > data.len() {
                break;
            }
            if let Ok(frame) = bincode::deserialize(&data[offset..offset + len]) {
                frames.push(frame);
            }
            offset += len + 4; // skip data + CRC
        }
        frames
    }

    fn spawn_worker_with_stdin(worker: &Path, paths: &[&std::path::Path]) -> std::process::Output {
        let mut child = std::process::Command::new(worker)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn pdf_worker");
        {
            let mut stdin = child.stdin.take().expect("failed to open worker stdin");
            for p in paths {
                let _ = writeln!(stdin, "{}", p.display());
            }
        }
        child.wait_with_output().expect("failed to wait for pdf_worker")
    }

    #[test]
    fn test_worker_binary_with_valid_pdf() {
        let worker = match find_worker_binary() {
            Some(w) => w,
            None => return,
        };
        if !is_pdfium_available() {
            return;
        }

        let tmp = std::env::temp_dir().join("pdf_extractor_test_worker_valid");
        let _ = std::fs::create_dir_all(&tmp);

        let pdf = make_test_pdf(&tmp, "This is a longer English sentence that should be reliably detected by the language detector with enough characters");

        let output = spawn_worker_with_stdin(&worker, &[&pdf]);

        assert!(output.status.success(), "worker exited with code {}: {}\nstdout: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout));

        let frames = parse_worker_frames(&output.stdout);
        assert_eq!(frames.len(), 1, "should produce 1 frame");
        let wo = match &frames[0] {
            WorkerFrame::Success(wo) => wo,
            _ => panic!("expected Success frame, got: {:?}", frames[0]),
        };

        assert_eq!(wo.path, pdf.to_string_lossy(), "path mismatch");
        assert!(!wo.ocr_flag, "text PDF should not be OCR-flagged");
        assert!(wo.text.contains("English sentence"), "extracted text should contain input: {}", wo.text);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn test_worker_binary_with_missing_file() {
        let worker = match find_worker_binary() {
            Some(w) => w,
            None => return,
        };

        let missing = Path::new(r"C:\__pdf_worker_test_nonexistent__\missing.pdf");

        let output = spawn_worker_with_stdin(&worker, &[missing]);

        let frames = parse_worker_frames(&output.stdout);
        assert_eq!(frames.len(), 1, "should produce 1 frame");
        match &frames[0] {
            WorkerFrame::Error { path, message: _ } => {
                assert!(path.contains("missing.pdf"), "error path should mention file, got: {}", path);
            }
            _ => panic!("expected Error frame for missing file"),
        }
    }

    #[test]
    fn test_worker_binary_with_multiple_files() {
        let worker = match find_worker_binary() {
            Some(w) => w,
            None => return,
        };
        if !is_pdfium_available() {
            return;
        }

        let tmp = std::env::temp_dir().join("pdf_extractor_test_worker_batch");
        let _ = std::fs::create_dir_all(&tmp);

        let pdf1 = make_test_pdf(&tmp, "First document text");
        let pdf2 = make_test_pdf(&tmp, "Second document content");
        let missing = tmp.join("nonexistent.pdf");

        let output = spawn_worker_with_stdin(&worker, &[&pdf1, &pdf2, &missing]);

        assert!(output.status.success(), "worker exited with code {}: {}\nstdout: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout));

        let frames = parse_worker_frames(&output.stdout);
        assert_eq!(frames.len(), 3, "should produce 3 frames");

        // First frame: success for pdf1
        match &frames[0] {
            WorkerFrame::Success(wo) => {
                assert_eq!(wo.path, pdf1.to_string_lossy());
                assert!(wo.text.contains("First document"));
            }
            _ => panic!("frame 0 should be Success"),
        }

        // Second frame: success for pdf2
        match &frames[1] {
            WorkerFrame::Success(wo) => {
                assert_eq!(wo.path, pdf2.to_string_lossy());
                assert!(wo.text.contains("Second document"));
            }
            _ => panic!("frame 1 should be Success"),
        }

        // Third frame: error for missing file
        match &frames[2] {
            WorkerFrame::Error { path, message: _ } => {
                assert!(path.contains("nonexistent.pdf"));
            }
            _ => panic!("frame 2 should be Error"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worker_binary_with_no_arguments() {
        let worker = match find_worker_binary() {
            Some(w) => w,
            None => return,
        };

        let output = std::process::Command::new(&worker)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("failed to spawn pdf_worker");

        // In streaming mode with no args and no stdin, the worker processes
        // zero files and exits successfully.
        assert!(
            output.status.success(),
            "streaming worker should exit successfully with no input, got: {:?}",
            output.status.code()
        );
    }

    #[test]
    fn test_worker_binary_with_special_characters() {
        let worker = match find_worker_binary() {
            Some(w) => w,
            None => return,
        };
        if !is_pdfium_available() {
            return;
        }

        let tmp = std::env::temp_dir().join("pdf_extractor_test_worker_special");
        let _ = std::fs::create_dir_all(&tmp);

        // Use WinAnsi-encodable characters (Helvetica standard encoding
        // does not support arbitrary Unicode via raw content streams).
        // PDF extraction will convert these back from the font encoding,
        // so we test symbols and numbers that round-trip correctly.
        let text = "Price: 99.95 USD (discount 15%) #sale!";
        let pdf = make_test_pdf(&tmp, text);

        let output = spawn_worker_with_stdin(&worker, &[&pdf]);

        assert!(output.status.success(), "worker failed for special chars PDF: {}",
            String::from_utf8_lossy(&output.stderr));

        let frames = parse_worker_frames(&output.stdout);
        assert_eq!(frames.len(), 1, "should produce 1 frame");
        let wo = match &frames[0] {
            WorkerFrame::Success(wo) => wo,
            _ => panic!("expected Success frame"),
        };

        assert!(wo.text.contains("99.95"), "should preserve numbers: {}", wo.text);
        assert!(wo.text.contains("discount"), "should preserve words: {}", wo.text);
        assert!(wo.text.contains("USD"), "should preserve uppercase: {}", wo.text);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worker_binary_empty_pdf() {
        let worker = match find_worker_binary() {
            Some(w) => w,
            None => return,
        };
        if !is_pdfium_available() {
            return;
        }

        let tmp = std::env::temp_dir().join("pdf_extractor_test_worker_empty");
        let _ = std::fs::create_dir_all(&tmp);

        // Create a PDF with no text content (whitespace only)
        let pdf = make_test_pdf(&tmp, "   ");

        let output = spawn_worker_with_stdin(&worker, &[&pdf]);

        assert!(output.status.success(), "worker failed for empty-text PDF: {}",
            String::from_utf8_lossy(&output.stderr));

        let frames = parse_worker_frames(&output.stdout);
        assert_eq!(frames.len(), 1, "should produce 1 frame");
        let wo = match &frames[0] {
            WorkerFrame::Success(wo) => wo,
            _ => panic!("expected Success frame"),
        };

        assert_eq!(wo.path, pdf.to_string_lossy());
        assert!(wo.ocr_flag, "whitespace-only PDF should be flagged for OCR");
        assert!(wo.text.is_empty(), "text should be empty, got: {}", wo.text);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // --- Heap greedy distribution ---

    /// Simulates the heap-greedy distribution logic used in the producer thread.
    /// Returns per-worker total MB and per-worker doc count.
    fn simulate_heap_distribution(
        docs: &[(i64, i64)], // (id, file_size_mb)
        num_workers: usize,
        batch_cap: usize,
    ) -> Vec<(i64, usize)> {
        let mut sorted = docs.to_vec();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));

        // Cap at num_workers * batch_cap (matches real pipeline logic)
        let target = num_workers * batch_cap;
        let n = std::cmp::min(target, sorted.len());

        let mut worker_mb: Vec<i64> = vec![0; num_workers];
        let mut worker_count: Vec<usize> = vec![0; num_workers];

        #[derive(Clone, Eq, PartialEq)]
        struct Entry {
            total_mb: i64,
            count: usize,
            idx: usize,
        }
        impl Ord for Entry {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                other.total_mb.cmp(&self.total_mb).then(other.count.cmp(&self.count))
            }
        }
        impl PartialOrd for Entry {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        let mut heap: std::collections::BinaryHeap<Entry> = std::collections::BinaryHeap::new();
        for i in 0..num_workers {
            heap.push(Entry { total_mb: 0, count: 0, idx: i });
        }

        for &(_id, size) in &sorted[..n] {
            let mut entry = loop {
                let e = heap.pop().unwrap();
                if e.count < batch_cap { break e; }
            };
            worker_mb[entry.idx] += size;
            worker_count[entry.idx] += 1;
            entry.total_mb += size;
            entry.count += 1;
            heap.push(entry);
        }

        worker_mb.into_iter().zip(worker_count.into_iter()).collect()
    }

    #[test]
    fn test_heap_distribution_balances_mb() {
        // 4 workers, docs: [100, 80, 60, 40, 20, 10] MB
        // Heap greedy assigns: W0=100, W1=80, W2=60+10=70, W3=40+20=60
        // Round-robin would give: W0=100+40=140, W1=80+20=100, W2=60+10=70, W3=0
        // Heap greedy spread = 100-60 = 40 vs round-robin = 140-0 = 140
        let docs: Vec<(i64, i64)> = vec![
            (1, 100), (2, 80), (3, 60), (4, 40), (5, 20), (6, 10),
        ];
        let result = simulate_heap_distribution(&docs, 4, 30);
        let max_mb = result.iter().map(|(mb, _)| *mb).max().unwrap();
        let min_mb = result.iter().map(|(mb, _)| *mb).filter(|x| *x > 0).min().unwrap();
        // Heap greedy should never produce a worse spread than round-robin (140)
        // With 6 docs across 4 workers, max spread is bounded by the largest doc
        assert!(max_mb - min_mb <= 100, "spread worse than largest doc: max={} min={} delta={}", max_mb, min_mb, max_mb - min_mb);
        // Every worker should have at least 1 doc (6 docs, 4 workers)
        assert!(result.iter().all(|(_, c)| *c >= 1), "all workers should have at least 1 doc");
    }

    #[test]
    fn test_heap_distribution_respects_batch_cap() {
        // 2 workers, 70 docs of 1 MB each, cap=30
        // Should fill W0=30, W1=30, leftover=10
        let docs: Vec<(i64, i64)> = (1..=70).map(|i| (i, 1)).collect();
        let result = simulate_heap_distribution(&docs, 2, 30);
        let total: usize = result.iter().map(|(_, c)| *c).sum();
        assert_eq!(total, 60, "should assign exactly 60 docs (2 workers × 30 cap)");
        for (mb, count) in &result {
            assert!(*count <= 30, "worker exceeded batch cap: count={}", count);
            assert_eq!(*mb, *count as i64, "each doc is 1 MB");
        }
    }

    #[test]
    fn test_heap_distribution_single_worker() {
        let docs: Vec<(i64, i64)> = vec![(1, 50), (2, 30), (3, 20)];
        let result = simulate_heap_distribution(&docs, 1, 30);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 100); // total MB
        assert_eq!(result[0].1, 3);   // doc count
    }

    #[test]
    fn test_heap_distribution_more_docs_than_cap() {
        // 2 workers, cap=30, 100 docs → only 60 assigned
        let docs: Vec<(i64, i64)> = (1..=100).map(|i| (i, 1)).collect();
        let result = simulate_heap_distribution(&docs, 2, 30);
        let total: usize = result.iter().map(|(_, c)| *c).sum();
        assert_eq!(total, 60);
    }

    #[test]
    fn test_reservoir_size_default() {
        let cfg = PipelineConfig::default();
        let workers = cfg.extract_workers();
        assert_eq!(cfg.reservoir_size(), std::cmp::min(200, workers * RESERVOIR_FACTOR));
    }

    #[test]
    fn test_reservoir_size_capped_at_200() {
        let cfg = PipelineConfig {
            num_extract_workers: Some(100),
            ..Default::default()
        };
        assert_eq!(cfg.reservoir_size(), 200);
    }

    // ---------------------------------------------------------------------------
    // flush_batch micro-benchmark
    // ---------------------------------------------------------------------------
    //
    // Generates synthetic DocumentRecords and runs flush_batch, reporting
    // per-phase timings (add / align / store).  Useful for tracking indexing
    // performance changes.
    //
    // Run with:
    //   cargo test -p pdf_extractor -- bench_flush_batch --nocapture
    //
    // Adjust N / WORDS_PER_DOC to match production scale.
    //
    // NOTE: on the reference machine (Ryzen 5950X, NVMe SSD), N=500 with
    // 5000 words/doc produces ~2.5M total word positions and takes ~30-270s
    // depending on the current code path.

    #[test]
    fn bench_flush_batch() {
        const N: usize = 100; // override to 500 for production-scale measurement
        const WORDS_PER_DOC: usize = 5000;
        const INDEXER_THREADS: usize = 4;

        let lorem = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua ut enim ad minim veniam quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat duis aute irure dolor in reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla pariatur excepteur sint occaecat cupidatat non proident sunt in culpa qui officia deserunt mollit anim id est laborum";
        let words: Vec<&str> = lorem.split_whitespace().collect();

        eprintln!("\n=== Generating {} synthetic docs ({} words each, {} total words) ===",
            N, WORDS_PER_DOC, N * WORDS_PER_DOC);

        let gen_start = Instant::now();
        let batch: Vec<DocumentRecord> = (0..N as i64).map(|id| {
            let mut text = String::with_capacity(WORDS_PER_DOC * 7);
            let mut word_positions = Vec::with_capacity(WORDS_PER_DOC);

            for i in 0..WORDS_PER_DOC {
                let word = words[i % words.len()];
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(word);

                let page = 1 + (i / 250) as u32;
                let line = (i % 250) / 8;
                let col = i % 8;
                let x_min = 50.0 + col as f32 * 70.0;
                let y_min = 750.0 - line as f32 * 15.0;
                word_positions.push(crate::extractor::WordPosition {
                    page,
                    x_min,
                    y_min,
                    x_max: x_min + 60.0,
                    y_max: y_min + 12.0,
                    text: word.to_string(),
                });
            }

            DocumentRecord {
                id,
                path: format!("/doc_{}.pdf", id),
                checksum: format!("cs{}", id),
                ocr_flag: false,
                text,
                word_positions,
                file_extraction_ms: 0,
                page_count: 20,
            }
        }).collect();
        eprintln!("Generation: {:.3}s", gen_start.elapsed().as_secs_f64());

        let tmp = tempfile::tempdir().unwrap();
        let indexer = crate::indexer::Indexer::new(tmp.path()).unwrap();
        let writer = indexer.search_index().writer_with_num_threads(INDEXER_THREADS).unwrap();

        let mut done_ids: Vec<(i64, bool, String)> = Vec::new();

        eprintln!("=== Running flush_batch (N={}) ===", N);
        let flush_start = Instant::now();
        flush_batch(&writer, &indexer, &mut done_ids, &batch, None);
        let flush_elapsed = flush_start.elapsed();

        assert_eq!(done_ids.len(), N);

        eprintln!("=== Results ===");
        eprintln!("  Total:    {:.3}s", flush_elapsed.as_secs_f64());
        eprintln!("  Per doc:  {:.3}s", flush_elapsed.as_secs_f64() / N as f64);
        eprintln!("  Docs/s:   {:.1}", N as f64 / flush_elapsed.as_secs_f64());
    }
}
