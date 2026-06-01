use anyhow::{Context, Result};
use crossbeam_channel::bounded;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use crate::lang;
use std::time::{Duration, Instant};

use crate::extractor::extract_pdf;
use crate::indexer::Indexer;
use crate::metrics::Metrics;
use crate::normalizer::normalize_text;
use crate::ocr;
use crate::output::{DocumentRecord, JsonlWriter};
use crate::scanner::{scan_directory, JobStore};

const CHANNEL_CAPACITY: usize = 5000;
const BATCH_SIZE: i64 = 100;
const DEFAULT_INDEXER_BATCH_SIZE: usize = 500;
const DEFAULT_COMMIT_INTERVAL: u64 = 5000;
const DEFAULT_COMMIT_TIMEOUT_SECS: u64 = 30;

pub struct PipelineConfig {
    pub num_extract_workers: Option<usize>,
    pub indexer_batch_size: Option<usize>,
    pub commit_interval: Option<u64>,
    pub commit_timeout: Option<u64>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            num_extract_workers: None,
            indexer_batch_size: None,
            commit_interval: None,
            commit_timeout: None,
        }
    }
}

impl PipelineConfig {
    pub fn extract_workers(&self) -> usize {
        self.num_extract_workers
            .map(|v| std::cmp::max(1, v))
            .unwrap_or_else(|| std::cmp::max(1, num_cpus::get() - 2))
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
    let scanned = scan_directory(&jobs, input)?;
    tracing::info!(scanned = scanned, "Directory scan complete");

    let pending = jobs.count_pending()?;
    if pending == 0 {
        tracing::info!("No pending jobs to process");
        return Ok(());
    }
    tracing::info!(pending = pending, "Jobs pending extraction");

    let (task_tx, task_rx) = bounded::<ExtractorTask>(CHANNEL_CAPACITY);
    let (result_tx, result_rx) = bounded::<DocumentRecord>(CHANNEL_CAPACITY);

    let num_workers = config.extract_workers();
    let indexer_batch_size = config.indexer_batch();
    let commit_interval = config.commit_int();
    let commit_timeout = config.commit_to();
    tracing::info!(num_workers = num_workers, indexer_batch = indexer_batch_size, commit_interval = commit_interval, "Starting extractor pool");

    // Producer: claim pending jobs from DB and send to task channel
    let producer_jobs = Arc::clone(&jobs);
    let producer_metrics = Arc::clone(&metrics);
    let producer_handle = {
        thread::spawn(move || loop {
            match producer_jobs.claim_pending(BATCH_SIZE) {
                Ok(batch) => {
                    if batch.is_empty() {
                        break;
                    }
                    for (id, path, checksum) in batch {
                        producer_metrics.set_task_queue_depth(task_tx.len() as u64);
                        if task_tx
                            .send(ExtractorTask { id, path, checksum })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Producer failed to claim jobs");
                    break;
                }
            }
        })
    };

    // Workers: extract PDFs in parallel
    let mut worker_handles = Vec::new();
    for i in 0..num_workers {
        let task_rx = task_rx.clone();
        let result_tx = result_tx.clone();
        let worker_jobs = Arc::clone(&jobs);
        let worker_metrics = Arc::clone(&metrics);
        let handle = thread::Builder::new()
            .name(format!("extract-{}", i))
            .spawn(move || {
                for task in &task_rx {
                    with_scratch_buf(|_buf| {
                        let path_obj = PathBuf::from(&task.path);
                        match extract_pdf(&path_obj) {
                            Ok(extraction) => {
                                let normalized = if !extraction.ocr_flag {
                                    normalize_text(&extraction.text)
                                } else {
                                    String::new()
                                };
                                let math_source =
                                    crate::math_tokenizer::extract_math_source(&extraction.text);
                                let record = DocumentRecord {
                                    id: task.id,
                                    path: task.path,
                                    checksum: task.checksum,
                                    ocr_flag: extraction.ocr_flag,
                                    language: lang::detect_language(&extraction.text),
                                    math_source,
                                    text: normalized,
                                };
                                if result_tx.send(record).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    id = task.id,
                                    path = %task.path,
                                    error = %e,
                                    "Extraction failed"
                                );
                                worker_jobs
                                    .mark_error(task.id, &format!("{}", e))
                                    .ok();
                                worker_metrics.increment_errored();
                            }
                        }
                    });
                }
            })
            .expect("Failed to spawn worker thread");
        worker_handles.push(handle);
    }

    drop(task_rx);
    drop(result_tx);

    // Indexer channel
    let (index_tx, index_rx) = bounded::<DocumentRecord>(CHANNEL_CAPACITY);
    let may_index = indexer.is_some();

    let indexer_handle = match indexer {
        Some(ref idx) => {
            let idx = Arc::clone(idx);
            let metrics_for_indexer = Arc::clone(&metrics);
            let handle = thread::Builder::new()
                .name("indexer".into())
                .spawn(move || {
                    indexer_thread(&*idx, index_rx, &metrics_for_indexer, indexer_batch_size, commit_interval, commit_timeout);
                })
                .expect("Failed to spawn indexer thread");
            Some(handle)
        }
        None => None,
    };

    // Writer: consume results and persist
    for record in &result_rx {
        metrics.set_result_queue_depth(result_rx.len() as u64);

        if let Err(e) = writer.write_record(&record) {
            tracing::error!(
                id = record.id,
                path = %record.path,
                error = %e,
                "Failed to write record"
            );
            jobs.mark_error(record.id, &format!("write failed: {}", e))
                .ok();
            metrics.increment_errored();
            continue;
        }
        jobs.mark_done(record.id, record.ocr_flag).ok();
        metrics.increment_processed();

        // Send to indexer if available
        if may_index && index_tx.send(record).is_err() {
            tracing::warn!("Indexer channel closed, dropping record");
        }

        metrics.log_summary();
    }

    drop(index_tx);

    producer_handle.join().expect("Producer panicked");
    for h in worker_handles {
        h.join().expect("Worker panicked");
    }
    if let Some(h) = indexer_handle {
        h.join().expect("Indexer panicked");
    }

    Ok(())
}

fn indexer_thread(
    indexer: &Indexer,
    rx: crossbeam_channel::Receiver<DocumentRecord>,
    metrics: &Metrics,
    batch_size: usize,
    commit_interval: u64,
    commit_timeout: u64,
) {
    let index_writer = match indexer.search_index().writer() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "Failed to create index writer in indexer thread");
            return;
        }
    };

    let mut buf: Vec<DocumentRecord> = Vec::with_capacity(batch_size);
    let mut doc_count: u64 = 0;
    let mut last_commit = Instant::now();
    let writer = std::sync::Mutex::new(index_writer);

    loop {
        let timeout = Duration::from_secs(commit_timeout);
        let result = rx.recv_timeout(timeout);
        match result {
            Ok(record) => {
                buf.push(record);
                if buf.len() >= batch_size {
                    let batch: Vec<DocumentRecord> = buf.drain(..).collect();
                    flush_batch(&writer, indexer, &batch);
                    doc_count += batch.len() as u64;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                if !buf.is_empty() {
                    let batch: Vec<DocumentRecord> = buf.drain(..).collect();
                    flush_batch(&writer, indexer, &batch);
                }
                break;
            }
        }

        let should_commit = !buf.is_empty()
            && (doc_count >= commit_interval
                || last_commit.elapsed() > Duration::from_secs(commit_timeout));
        if should_commit {
            if let Ok(mut w) = writer.lock() {
                if w.commit().is_ok() {
                    let total = indexer.metrics().docs_indexed();
                    metrics.set_indexer_docs_indexed(total);
                    metrics.set_indexer_last_commit_age(0);
                    tracing::info!(docs_indexed = total, "Index committed");
                    last_commit = Instant::now();
                    doc_count = 0;
                }
            }
        }

        // Update age between commits so the 5s snapshot sees the real value
        metrics.set_indexer_last_commit_age(last_commit.elapsed().as_secs());
    }

    // Final commit
    if let Ok(mut w) = writer.lock() {
        w.commit().ok();
        let total = indexer.metrics().docs_indexed();
        metrics.set_indexer_docs_indexed(total);
        metrics.set_indexer_last_commit_age(0);
    };
}

fn flush_batch(
    writer: &std::sync::Mutex<tantivy::IndexWriter>,
    indexer: &Indexer,
    batch: &[DocumentRecord],
) {
    let mut w = match writer.lock() {
        Ok(w) => w,
        Err(_) => return,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for record in batch {
        let math_source = record.math_source.as_deref().unwrap_or("");
        if let Err(e) = indexer.search_index().add_document_with_ts(
            &mut w,
            record.id,
            &record.path,
            &record.checksum,
            &record.text,
            &record.text,
            record.language.as_deref().unwrap_or(""),
            math_source,
            now,
        ) {
            tracing::error!(id = record.id, error = %e, "Failed to index document");
        }
    }
}

/// Resolve the effective number of OCR worker threads.
/// If an override is given, clamps to at least 1.
/// Otherwise auto-detects as `max(1, cores/2 - 1)`.
fn resolve_ocr_workers(override_val: Option<usize>) -> usize {
    override_val
        .map(|v| std::cmp::max(1, v))
        .unwrap_or_else(|| std::cmp::max(1, num_cpus::get() / 2 - 1))
}

pub fn run_ocr_post_processing(
    jobs: Arc<JobStore>,
    ocr_config: &ocr::OcrConfig,
    output_path: Option<PathBuf>,
    num_workers_override: Option<usize>,
) -> Result<u64> {
    let num_workers = resolve_ocr_workers(num_workers_override);
    let max_retries = ocr_config.max_retries;
    let ocr_language = ocr_config.language.clone();

    let pending = jobs.count_ocr_pending(max_retries)?;
    if pending == 0 {
        tracing::info!("No documents pending OCR");
        return Ok(0);
    }
    tracing::info!(pending = pending, ocr_workers = num_workers, "Starting OCR post-processing");

    // Open JSONL writer if output path provided
    let jsonl_writer = if let Some(ref out_path) = output_path {
        tracing::info!(output = %out_path.display(), "OCR output will be written to JSONL");
        Some(Arc::new(JsonlWriter::new(out_path)?))
    } else {
        None
    };

    let (ocr_tx, ocr_rx) = bounded::<(i64, String, String)>(100);
    let (result_tx, result_rx) = bounded::<(i64, String, String, String)>(100);

    // Consumer thread: process results immediately so mark_ocr_attempt
    // runs as soon as OCR text arrives, preventing producer re-fetches.
    let consumer_jobs = Arc::clone(&jobs);
    let consumer_writer = jsonl_writer.clone();
    let consumer_handle = thread::spawn(move || {
        let mut ocr_processed: u64 = 0;
        let mut ocr_errored: u64 = 0;
        for (id, path, checksum, ocr_text) in &result_rx {
            if ocr_text.is_empty() {
                consumer_jobs.mark_ocr_attempt(id, false, Some("OCR returned empty text"), max_retries).ok();
                ocr_errored += 1;
                continue;
            }
            ocr_processed += 1;
            consumer_jobs.mark_ocr_attempt(id, true, None, max_retries).ok();

            if let Some(ref writer) = consumer_writer {
                let math_source =
                    crate::math_tokenizer::extract_math_source(ocr_text.as_str());
                let record = DocumentRecord {
                    id,
                    path: path.clone(),
                    checksum: checksum.clone(),
                    ocr_flag: false,
                    language: Some(ocr_language.clone()),
                    math_source,
                    text: ocr_text.clone(),
                };
                if let Err(e) = writer.write_record(&record) {
                    tracing::error!(error = %e, id = id, "Failed to write OCR output record");
                }
            }
        }
        (ocr_processed, ocr_errored)
    });

    // Producer: fetch OCR-needed docs and send each unique job ID
    // exactly once. A local HashSet prevents re-sending even if the
    // database hasn't been updated yet (consumer thread scheduling lag).
    // The producer exits after one batch — in-flight jobs are processed
    // by workers and the consumer; remaining pending jobs wait for the
    // next CLI invocation.
    let producer_jobs = Arc::clone(&jobs);
    let producer_handle = thread::spawn(move || {
        let mut sent_ids = HashSet::new();
        match producer_jobs.fetch_ocr_needed(100, max_retries) {
            Ok(batch) => {
                for (id, path, checksum) in deduplicate_ocr_batch(batch, &mut sent_ids) {
                    if ocr_tx.send((id, path, checksum)).is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "OCR producer failed to fetch jobs");
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

        let handle = thread::Builder::new()
            .name(format!("ocr-{}", i))
            .spawn(move || {
                for (id, path_str, checksum) in &rx {
                    let path_obj = PathBuf::from(&path_str);
                    let ocr_result = run_single_ocr(&path_obj, &cfg, Some(&mut worker));
                    match ocr_result {
                        Ok(text) if !text.is_empty() => {
                            tx.send((id, path_str, checksum, text)).ok();
                        }
                        _ => {
                            tracing::warn!(id = id, "OCR failed after retries");
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
    let (ocr_processed, ocr_errored) = consumer_handle.join().expect("OCR consumer panicked");

    tracing::info!(ocr_processed = ocr_processed, ocr_errored = ocr_errored, "OCR post-processing complete");
    Ok(ocr_processed)
}

/// Attempt OCR on a PDF by extracting pages as images, preprocessing, and running Tesseract.
/// Falls back gracefully if Tesseract is not available.
///
/// When `worker` is `Some`, uses a persistent worker process (avoids per-call `tesseract.exe` spawn).
/// When `None`, falls back to a fresh `Command::new(tesseract_path)` for each call.
fn run_single_ocr(path: &Path, config: &ocr::OcrConfig, mut worker: Option<&mut ocr::WorkerProcess>) -> Result<String> {
    // First try to extract text natively (in case PDF has text but was not extracted before)
    if let Ok(result) = extract_pdf(path) {
        if !result.text.is_empty() && !result.ocr_flag {
            return Ok(result.text);
        }
    }

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
                    Err(e) => {
                        tracing::warn!(page = page_num, error = %e, "Failed to preprocess page");
                        continue;
                    }
                };
                let text = match &mut worker {
                    Some(w) => w.process(&preprocessed),
                    None => ocr::run_tesseract(&preprocessed, &config.tesseract_path, &config.language),
                };
                let text = match text {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(page = page_num, error = %e, "Tesseract failed on page");
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
                tracing::warn!(page = page_num, "No renderer available for this page");
                continue;
            }
            Err(e) => {
                tracing::warn!(page = page_num, error = %e, "Failed to render page");
                continue;
            }
        }
    }

    if full_text.is_empty() {
        anyhow::bail!("No text extracted from any page of {}", path.display());
    }
    Ok(full_text)
}

/// Try to determine the number of pages in a PDF using available tools.
/// Tries `mutool info` and `pdfinfo`, falling back to 1.
fn get_pdf_page_count(pdf_path: &Path) -> Result<u32> {
    if let Ok(output) = std::process::Command::new("mutool")
        .args(["info"])
        .arg(pdf_path)
        .output()
    {
        if let Some(n) = extract_page_count_from_stdout(&String::from_utf8_lossy(&output.stdout)) {
            return Ok(n);
        }
    }

    if let Ok(output) = std::process::Command::new("pdfinfo")
        .arg(pdf_path)
        .output()
    {
        if let Some(n) = extract_page_count_from_stdout(&String::from_utf8_lossy(&output.stdout)) {
            return Ok(n);
        }
    }

    Ok(1)
}

/// Extract the page count from the stdout of `mutool info` or `pdfinfo`.
/// Returns `None` if the output does not contain a valid `Pages: N` line.
fn extract_page_count_from_stdout(stdout: &str) -> Option<u32> {
    let re = regex::Regex::new(r"(?m)^Pages:\s+(\d+)\s*$").unwrap();
    re.captures(stdout)
        .and_then(|caps| caps[1].parse::<u32>().ok())
        .filter(|&n| n > 0)
}

/// Try to render a single page of a PDF to an image using available system tools.
/// `page_num` is 1-indexed.
/// Returns Ok(true) if rendering succeeded, Ok(false) if no renderer available.
fn render_pdf_page(pdf_path: &Path, page_num: u32, output_image: &Path) -> Result<bool> {
    // Try `mutool draw` (MuPDF)
    let page_str = page_num.to_string();
    if let Ok(output) = std::process::Command::new("mutool")
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

    // Try `pdftoppm` (poppler)
    let ppm_path = output_image.with_extension("ppm");
    if let Ok(output) = std::process::Command::new("pdftoppm")
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
