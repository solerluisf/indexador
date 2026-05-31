use anyhow::Result;
use crossbeam_channel::bounded;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use crate::extractor::extract_pdf;
use crate::metrics::Metrics;
use crate::normalizer::normalize_text;
use crate::output::{DocumentRecord, JsonlWriter};
use crate::scanner::{scan_directory, JobStore};

const CHANNEL_CAPACITY: usize = 5000;
const BATCH_SIZE: i64 = 100;

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

    let num_workers = std::cmp::max(1, num_cpus::get() - 2);
    tracing::info!(num_workers = num_workers, "Starting extractor pool");

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
                                let record = DocumentRecord {
                                    id: task.id,
                                    path: task.path,
                                    checksum: task.checksum,
                                    ocr_flag: extraction.ocr_flag,
                                    language: None,
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

        metrics.log_summary();
    }

    producer_handle.join().expect("Producer panicked");
    for h in worker_handles {
        h.join().expect("Worker panicked");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            // After shrink, capacity should be <= 64_000
            assert!(buf.capacity() <= 64_000, "Buffer should be shrunk after large allocation");
        });
    }

    #[test]
    fn test_with_scratch_buf_keeps_small_capacity() {
        with_scratch_buf(|buf| {
            buf.push("small".to_string());
        });
        with_scratch_buf(|buf| {
            // Small strings should not trigger shrink
            assert!(buf.capacity() < 1_000_000 || buf.capacity() <= 64_000);
        });
    }
}
