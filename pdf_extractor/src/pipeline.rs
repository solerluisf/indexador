use anyhow::{Context, Result};
use crossbeam_channel::bounded;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
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
use crate::metrics::Metrics;
use crate::ocr;
use crate::output::{DocumentRecord, JsonlWriter};
use crate::scanner::{scan_directory, JobStore};
use crate::worker_ipc::WorkerFrame;

// Channel capacity for DocumentRecord queues.  Each record carries the full
// extracted text (up to several MB for large PDFs), so a small default bounds
// peak memory even when the indexer is the bottleneck.
//   peak memory ≈ (CHANNEL_CAP + INDEXER_BATCH) × avg_text_size
//   = (256 + 500) × text_size
const DEFAULT_CHANNEL_CAPACITY: usize = 256;
const BATCH_SIZE: i64 = 100;
const DEFAULT_INDEXER_BATCH_SIZE: usize = 500;
const DEFAULT_COMMIT_INTERVAL: u64 = 50000;
const DEFAULT_COMMIT_TIMEOUT_SECS: u64 = 30;

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
                std::cmp::min(std::cmp::max(1, cores.saturating_sub(2)), 4)
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
            .unwrap_or_else(|| num_cpus::get())
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
    writer: &JsonlWriter,
    metrics: Arc<Metrics>,
    input: &PathBuf,
    indexer: Option<Arc<Indexer>>,
    config: &PipelineConfig,
) -> Result<()> {
    let pipeline_start = Instant::now();
    let _scanned = scan_directory(&jobs, input)?;

    let pending = jobs.count_pending()?;
    if pending == 0 {
        return Ok(());
    }

    // Worker binary is mandatory — in-thread extraction is unsafe (PDFium is not thread-safe).
    let _worker_path = config
        .worker_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!(
            "worker_path is required: pdf_worker.exe must be deployed alongside the library. \
             Run 'cargo build -p pdf_extractor --bin pdf_worker' to build it."
        ))?;

    let cap = config.channel_cap();
    let (task_tx, task_rx) = bounded::<Vec<ExtractorTask>>(cap);
    let (result_tx, result_rx) = bounded::<DocumentRecord>(cap);

    let num_workers = config.extract_workers();
    let indexer_batch_size = config.indexer_batch();
    let commit_interval = config.commit_int();
    let commit_timeout = config.commit_to();
    let indexer_threads = config.indexer_threads();


    // Producer: claim pending jobs from DB and send batches to task channel.
    // Batching reduces channel lock contention vs one send() per task.
    let producer_jobs = Arc::clone(&jobs);
    let producer_metrics = Arc::clone(&metrics);
    let producer_cancel = config.cancel_flag.clone();
    let producer_handle = {
        thread::spawn(move || {
            loop {
                if producer_cancel.as_ref().map_or(false, |f| f.load(Ordering::Relaxed)) {
                    break;
                }
                match producer_jobs.claim_pending(BATCH_SIZE) {
                    Ok(batch) => {
                        if batch.is_empty() {
                            break;
                        }
                        let tasks: Vec<ExtractorTask> = batch
                            .into_iter()
                            .map(|(id, path, checksum)| ExtractorTask { id, path, checksum })
                            .collect();
                        producer_metrics.set_task_queue_depth(task_tx.len() as u64);
                        if task_tx.send(tasks).is_err() {
                            return;
                        }
                    }
                    Err(_e) => {
                        break;
                    }
                }
            }
        })
    };

    let log_cb = config.log_cb;
    let process_cb = config.process_cb;

    // Workers: extract PDFs via OS process per batch
    // (full process isolation so PDFium cannot corrupt shared state).
    let mut worker_handles = Vec::new();
    for i in 0..num_workers {
        let task_rx = task_rx.clone();
        let result_tx = result_tx.clone();
        let worker_jobs = Arc::clone(&jobs);
        let worker_metrics = Arc::clone(&metrics);
        let worker_path = config.worker_path.clone();
        let wp = worker_path.expect("worker_path validated above");
        let worker_cancel = config.cancel_flag.clone();
        let handle = thread::Builder::new()
            .name(format!("extract-{}", i))
            .spawn(move || {
                run_extraction_process(
                    &wp, &task_rx, &result_tx, &worker_jobs, &worker_metrics,
                    worker_cancel, log_cb, process_cb,
                );
            })
            .expect("Failed to spawn worker thread");
        worker_handles.push(handle);
    }

    drop(task_rx);
    drop(result_tx);

    // Indexer channel
    let (index_tx, index_rx) = bounded::<DocumentRecord>(cap);
    let may_index = indexer.is_some();

    let indexer_handle = match indexer {
        Some(ref idx) => {
            let idx = Arc::clone(idx);
            let metrics_for_indexer = Arc::clone(&metrics);
            let jobs_for_indexer = Arc::clone(&jobs);
            let handle = thread::Builder::new()
                .name("indexer".into())
                .spawn(move || {
                    indexer_thread(&*idx, jobs_for_indexer, index_rx, &metrics_for_indexer, indexer_batch_size, commit_interval, commit_timeout, indexer_threads, log_cb);
                })
                .expect("Failed to spawn indexer thread");
            Some(handle)
        }
        None => None,
    };

    let total_pending = pending as u64;
    let progress_cb = &config.progress_cb;
    let cancel_flag = config.cancel_flag.clone();

    // Writer: consume results and persist
    let is_cancelled = move || cancel_flag.as_ref().map_or(false, |f| f.load(Ordering::Relaxed));

    // Throttle progress callback to at most once per 500 docs to reduce FFI overhead.
    let progress_throttle = 500u64;

    macro_rules! process_record {
        ($record:expr) => {{
            let record = $record;
            metrics.set_result_queue_depth(result_rx.len() as u64);
            if let Err(e) = writer.write_record(&record) {
                jobs.mark_error(record.id, &format!("write failed: {}", e))
                    .ok();
                metrics.increment_errored();
                continue;
            }
            let processed = metrics.increment_processed();

            if may_index {
                let id = record.id;
                let path = record.path.clone();
                if index_tx.send(record).is_err() {
                    log_msg(log_cb, &format!(
                        "[pipeline] indexer channel disconnected for id={} path='{}' — indexer unavailable, will retry on next run",
                        id, path
                    ));
                }
            }

            if let Some(ref cb) = progress_cb {
                let done = processed + metrics.errored();
                if done % progress_throttle == 0 || done >= total_pending {
                    cb(done, total_pending);
                }
            }
            metrics.log_summary();
        }};
    }

    // Phase 1: normal blocking receive, check cancel between records
    for record in &result_rx {
        if is_cancelled() {

            break;
        }
        process_record!(record);
    }

    // Phase 2: drain any records still in-flight from workers
    //
    // Workers may have already extracted PDFs and pushed to result_tx
    // before the producer stopped.  Drain non-blocking until the channel
    // is empty, then join workers and drain one last time in case a
    // worker snuck one more in during the drain.
    if is_cancelled() {
        loop {
            match result_rx.try_recv() {
                Ok(record) => process_record!(record),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }

        // Let workers finish their current extraction, then drain again
        for h in worker_handles {
            h.join().expect("Worker panicked");
        }

        loop {
            match result_rx.try_recv() {
                Ok(record) => process_record!(record),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
    } else {
        for h in worker_handles {
            h.join().expect("Worker panicked");
        }
    }

    drop(index_tx);

    producer_handle.join().expect("Producer panicked");
    if let Some(h) = indexer_handle {
        h.join().expect("Indexer panicked");
    }

    // Reset any tasks still in 'extracting' back to 'pending'.
    // These are PDFs the worker did not get to process before crashing.
    // Must run before the indexer-failed bail so stuck jobs are recoverable.
    jobs.reprocess_extracting()?;

    let processed = metrics.processed();
    let errored = metrics.errored();
    let elapsed = format_uptime(pipeline_start.elapsed());
    log_msg(log_cb, &format!(
        "[pipeline] done — {} total ({:?} processed, {:?} errored) in {}",
        processed + errored, processed, errored, elapsed
    ));

    if metrics.indexer_failed() {
        anyhow::bail!("Indexer failed to initialize — Tantivy index writer could not be created. Check disk space, permissions, and index integrity.");
    }

    Ok(())
}

fn indexer_thread(
    indexer: &Indexer,
    jobs: Arc<JobStore>,
    rx: crossbeam_channel::Receiver<DocumentRecord>,
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

    loop {
        let timeout = Duration::from_secs(commit_timeout);
        let result = rx.recv_timeout(timeout);
        match result {
            Ok(record) => {
                buf.push(record);
                if buf.len() >= batch_size {
                    let batch: Vec<DocumentRecord> = buf.drain(..).collect();
                    flush_batch(&mut writer, indexer, &mut done_ids, &batch, log_cb);
                    doc_count += batch.len() as u64;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                if !buf.is_empty() {
                    let batch: Vec<DocumentRecord> = buf.drain(..).collect();
                    flush_batch(&mut writer, indexer, &mut done_ids, &batch, log_cb);
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
    if writer.commit().is_ok() {
        if !done_ids.is_empty() {
            jobs.batch_mark_done(&done_ids).ok();
            done_ids.clear();
        }
        let total = indexer.metrics().docs_indexed();
        metrics.set_indexer_docs_indexed(total);
        metrics.set_indexer_last_commit_age(0);
    } else {
        log_msg(log_cb, "[indexer_thread] final commit failed — some indexed documents may not be searchable");
    }
}

fn flush_batch(
    writer: &mut tantivy::IndexWriter,
    indexer: &Indexer,
    done_ids: &mut Vec<(i64, bool, String)>,
    batch: &[DocumentRecord],
    log_cb: Option<extern "C" fn(*const u8, u32)>,
) {
    // Phase 1: add documents to Tantivy (fast, in-memory).
    // Collect records that need position storage for phase 2.
    let mut positions_to_store: Vec<(i64, Vec<(usize, crate::extractor::WordPosition)>)> = Vec::new();
    for record in batch {
        if let Err(e) = indexer.search_index().add_document(
            writer,
            record.id,
            &record.path,
            &record.text,
            None,
        ) {
            log_msg(log_cb, &format!(
                "[flush_batch] add_document failed for id={} path='{}': {}",
                record.id, record.path, e
            ));
        } else {
            indexer.metrics().docs_indexed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            done_ids.push((record.id, record.ocr_flag, record.checksum.clone()));

            if !record.word_positions.is_empty() {
                let aligned = align_offsets_to_tantivy(&record.text, &record.word_positions);
                if !aligned.is_empty() {
                    positions_to_store.push((record.id, aligned));
                }
            }
        }
    }

    // Phase 2: store word positions (SQLite I/O).
    if !positions_to_store.is_empty() {
        if let Ok(pos_store) = indexer.position_store.lock() {
            for (id, aligned) in &positions_to_store {
                let _ = pos_store.store_positions(*id, aligned);
            }
        }
    }
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
    output_path: Option<PathBuf>,
    num_workers_override: Option<usize>,
    cancel_flag: Option<Arc<AtomicBool>>,
    log_cb: Option<extern "C" fn(*const u8, u32)>,
) -> Result<u64> {
    let num_workers = resolve_ocr_workers(num_workers_override);
    let max_retries = ocr_config.max_retries;

    let pending = jobs.count_ocr_pending(max_retries)?;
    if pending == 0 {

        return Ok(0);
    }


    // Open JSONL writer if output path provided
    let jsonl_writer = if let Some(ref out_path) = output_path {

        Some(Arc::new(JsonlWriter::new(out_path)?))
    } else {
        None
    };

    let (ocr_tx, ocr_rx) = bounded::<(i64, String, String)>(100);
    let (result_tx, result_rx) = bounded::<(i64, String, String, String)>(100);

    // Clone cancel_flag for the consumer thread; the original is kept
    // for the worker threads below.
    let consumer_cancel = cancel_flag.clone();

    // Consumer thread: process results immediately so mark_ocr_attempt
    // runs as soon as OCR text arrives, preventing producer re-fetches.
    let consumer_jobs = Arc::clone(&jobs);
    let consumer_writer = jsonl_writer.clone();
    let consumer_indexer = indexer.clone();
    let consumer_log_cb = log_cb;
    let consumer_handle = thread::spawn(move || {
        let mut ocr_processed: u64 = 0;
        let mut ocr_errored: u64 = 0;

        // Create Tantivy IndexWriter for OCR re-indexing
        let mut index_writer = consumer_indexer.as_ref().and_then(|idx| {
            let result = idx.search_index().writer_with_num_threads(1);
            if result.is_err() {
                log_msg(consumer_log_cb, "[ocr-consumer] failed to create Tantivy IndexWriter for OCR re-indexing");
            }
            result.ok()
        });

        for (id, path, checksum, ocr_text) in &result_rx {
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

            if let Some(ref writer) = consumer_writer {
                let record = DocumentRecord {
                    id,
                    path: path.clone(),
                    checksum: checksum.clone(),
                    ocr_flag: false,
                    text: ocr_text.clone(),
                    word_positions: Vec::new(),
                };
                if let Err(e) = writer.write_record(&record) {
                    log_msg(consumer_log_cb, &format!(
                        "[ocr-consumer] JSONL write failed for id={}: {}", id, e
                    ));
                }
            }

            // Re-index OCR text into Tantivy (delete stale empty-text doc first)
            if let (Some(idx), Some(w)) = (consumer_indexer.as_ref(), index_writer.as_mut()) {
                let term = tantivy::Term::from_field_u64(idx.search_index().id_field, id as u64);
                w.delete_term(term);
                let math_source = crate::math_tokenizer::extract_math_source(&ocr_text);
                if let Err(e) = idx.search_index().add_document(
                    w, id, &path, &ocr_text,
                    math_source.as_deref(),
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

    // Producer: fetch OCR-needed docs in batches of 100 and drain the
    // entire queue. A local HashSet prevents re-sending an ID that was
    // already dispatched but not yet consumed (consumer thread scheduling
    // lag means the DB may not reflect the attempt yet).
    let producer_jobs = Arc::clone(&jobs);
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

    // OCR workers
    let mut worker_handles = Vec::new();
    let config = ocr::OcrConfig {
        max_retries: ocr_config.max_retries,
        tesseract_path: ocr_config.tesseract_path.clone(),
        max_dim: ocr_config.max_dim,
        language: ocr_config.language.clone(),
    };

    // Create persistent subprocess pool
    let mut pool = ocr::TesseractPool::new(num_workers, &config.tesseract_path, &config.language)
        .context("Failed to create Tesseract subprocess pool")?;

    for i in 0..num_workers {
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
        let worker_cancel = cancel_flag.clone();

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

    // Drop main's channel handles. Workers have their own clones of
    // ocr_rx and result_tx; the producer owns ocr_tx.
    drop(ocr_rx);
    drop(result_tx);

    // Wait for producer to finish (it exits after one batch)
    producer_handle.join().expect("OCR producer panicked");

    // Workers exit when producer drops ocr_tx (channel disconnects)
    for h in worker_handles {
        h.join().expect("OCR worker panicked");
    }

    // All workers done → all result_tx clones dropped → consumer exits
    let (ocr_processed, _ocr_errored) = consumer_handle.join().expect("OCR consumer panicked");


    Ok(ocr_processed)
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
    task_rx: &crossbeam_channel::Receiver<Vec<ExtractorTask>>,
) -> Vec<ExtractorTask> {
    match task_rx.recv() {
        Ok(tasks) => tasks,
        Err(_) => Vec::new(),
    }
}

/// Read one length-prefixed bincode WorkerFrame from a buffered reader.
fn read_frame<R: io::Read>(reader: &mut io::BufReader<R>) -> io::Result<WorkerFrame> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut data = vec![0u8; len];
    reader.read_exact(&mut data)?;
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
    task_rx: &crossbeam_channel::Receiver<Vec<ExtractorTask>>,
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

    'outer: loop {
        batch_count += 1;
        if batch_count % 5 == 0 {
            let mem = proc_mon::working_set_mib(pid);
            report_process(process_cb, &thread_name, pid, "running", mem, "");
        }

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

/// Send a log message to the optional C callback, falling back to stderr.
fn log_msg(log_cb: Option<extern "C" fn(*const u8, u32)>, msg: &str) {
    match log_cb {
        Some(cb) => {
            let bytes = msg.as_bytes();
            cb(bytes.as_ptr(), bytes.len() as u32);
        }
        None => eprintln!("{}", msg),
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
    task_rx: &crossbeam_channel::Receiver<Vec<ExtractorTask>>,
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
        let (tx, rx) = bounded::<Vec<ExtractorTask>>(10);
        drop(tx);
        let batch = collect_batch(&rx);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_collect_batch_single_task() {
        let (tx, rx) = bounded::<Vec<ExtractorTask>>(10);
        tx.send(vec![make_task(1, "a.pdf")]).unwrap();
        drop(tx);
        let batch = collect_batch(&rx);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1);
        assert_eq!(batch[0].path, "a.pdf");
    }

    #[test]
    fn test_collect_batch_multiple_tasks_in_one_batch() {
        let (tx, rx) = bounded::<Vec<ExtractorTask>>(10);
        tx.send(make_batch(&[1, 2, 3, 4, 5], "")).unwrap();
        drop(tx);
        let batch = collect_batch(&rx);
        assert_eq!(batch.len(), 5);
    }

    #[test]
    fn test_collect_batch_blocks_until_first_arrives() {
        let (tx, rx) = bounded::<Vec<ExtractorTask>>(10);
        let tx_clone = tx.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            tx_clone.send(vec![make_task(99, "late.pdf")]).unwrap();
            drop(tx_clone);
        });
        let batch = collect_batch(&rx);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 99);
        handle.join().unwrap();
    }

    #[test]
    fn test_collect_batch_disconnected_returns_partial() {
        let (tx, rx) = bounded::<Vec<ExtractorTask>>(10);
        tx.send(make_batch(&[1, 2, 3], "")).unwrap();
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

        let (task_tx, task_rx) = bounded::<Vec<ExtractorTask>>(10);
        let batch_size: i64 = 2;

        // Simulate the producer: claim pending, batch, send
        let batch = store.claim_pending(batch_size).unwrap();
        assert_eq!(batch.len(), 2, "should claim up to batch_size=2");
        let tasks: Vec<ExtractorTask> = batch
            .into_iter()
            .map(|(id, path, checksum)| ExtractorTask { id, path, checksum })
            .collect();
        task_tx.send(tasks).unwrap();

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
        let (task_tx, task_rx) = bounded::<Vec<ExtractorTask>>(10);
        let (result_tx, result_rx) = bounded::<DocumentRecord>(10);
        let jobs = Arc::new(super::super::scanner::JobStore::open_in_memory().unwrap());
        let metrics = Arc::new(super::super::metrics::Metrics::new());

        task_tx.send(vec![make_task(1, "a.pdf")]).unwrap();
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
        let mut out = Vec::with_capacity(4 + data.len());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
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
    fn test_read_frame_truncated_returns_error() {
        let frame = WorkerFrame::Success(WorkerOutput {
            path: "/test.pdf".into(),
            checksum: "dummy".into(),
            ocr_flag: false,
            text: "test".into(),
            word_positions: vec![],
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
        use crate::output::JsonlWriter;
        use crate::scanner::scan_directory;

        let tmp = std::env::temp_dir().join("pdf_extractor_test_req_worker");
        let _ = std::fs::create_dir_all(&tmp);
        let books = tmp.join("books");
        std::fs::create_dir_all(&books).unwrap();
        std::fs::write(books.join("dummy.pdf"), b"dummy").unwrap();

        let jobs = Arc::new(JobStore::open_in_memory().unwrap());
        let writer = JsonlWriter::new(&tmp.join("out.jsonl")).unwrap();
        let metrics = Arc::new(Metrics::new());

        scan_directory(&jobs, &books).unwrap();

        let config = PipelineConfig {
            worker_path: None,
            ..Default::default()
        };

        let result = run_pipeline(jobs, &writer, metrics, &books, None, &config);
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
            if offset + len > data.len() {
                break;
            }
            if let Ok(frame) = bincode::deserialize(&data[offset..offset + len]) {
                frames.push(frame);
            }
            offset += len;
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
}
