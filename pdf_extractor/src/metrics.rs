use chrono::Local;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

pub struct Metrics {
    docs_processed: AtomicU64,
    docs_errored: AtomicU64,
    task_queue_depth: AtomicU64,
    result_queue_depth: AtomicU64,
    indexer_docs_indexed: AtomicU64,
    indexer_last_commit_age: AtomicU64,
    indexer_writer_mem: AtomicU64,
    start: Instant,
    last_log: Mutex<Instant>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            docs_processed: AtomicU64::new(0),
            docs_errored: AtomicU64::new(0),
            task_queue_depth: AtomicU64::new(0),
            result_queue_depth: AtomicU64::new(0),
            indexer_docs_indexed: AtomicU64::new(0),
            indexer_last_commit_age: AtomicU64::new(0),
            indexer_writer_mem: AtomicU64::new(0),
            start: Instant::now(),
            last_log: Mutex::new(Instant::now()),
        }
    }

    pub fn increment_processed(&self) {
        self.docs_processed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_errored(&self) {
        self.docs_errored.fetch_add(1, Ordering::Relaxed);
    }

    pub fn processed(&self) -> u64 {
        self.docs_processed.load(Ordering::Relaxed)
    }

    pub fn errored(&self) -> u64 {
        self.docs_errored.load(Ordering::Relaxed)
    }

    pub fn set_task_queue_depth(&self, depth: u64) {
        self.task_queue_depth.store(depth, Ordering::Relaxed);
    }

    pub fn set_result_queue_depth(&self, depth: u64) {
        self.result_queue_depth.store(depth, Ordering::Relaxed);
    }

    pub fn set_indexer_docs_indexed(&self, val: u64) {
        self.indexer_docs_indexed.store(val, Ordering::Relaxed);
    }

    pub fn set_indexer_last_commit_age(&self, secs: u64) {
        self.indexer_last_commit_age.store(secs, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn set_indexer_writer_mem(&self, bytes: u64) {
        self.indexer_writer_mem.store(bytes, Ordering::Relaxed);
    }

    pub fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    pub fn throughput(&self) -> f64 {
        let secs = self.elapsed_secs();
        if secs > 0.0 {
            self.processed() as f64 / secs
        } else {
            0.0
        }
    }

    pub fn log_summary(&self) {
        let now = Instant::now();
        let since_last = now.duration_since(*self.last_log.lock().unwrap()).as_secs_f64();
        if since_last >= 5.0 {
            *self.last_log.lock().unwrap() = now;
            tracing::info!(
                timestamp = %Local::now().format("%Y-%m-%dT%H:%M:%S"),
                docs_processed = self.processed(),
                docs_errored = self.errored(),
                task_queue_depth = self.task_queue_depth.load(Ordering::Relaxed),
                result_queue_depth = self.result_queue_depth.load(Ordering::Relaxed),
                indexer_docs_indexed = self.indexer_docs_indexed.load(Ordering::Relaxed),
                indexer_last_commit_age_secs = self.indexer_last_commit_age.load(Ordering::Relaxed),
                indexer_writer_mem_bytes = self.indexer_writer_mem.load(Ordering::Relaxed),
                throughput_docs_per_sec = format!("{:.2}", self.throughput()),
                elapsed_secs = format!("{:.1}", self.elapsed_secs()),
                "Metrics snapshot"
            );
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;


    // --- zero state ---

    #[test]
    fn test_metrics_start_at_zero() {
        let m = Metrics::new();
        assert_eq!(m.processed(), 0);
        assert_eq!(m.errored(), 0);
    }

    // --- basic increments ---

    #[test]
    fn test_metrics_increment_processed() {
        let m = Metrics::new();
        m.increment_processed();
        m.increment_processed();
        m.increment_processed();
        assert_eq!(m.processed(), 3);
    }

    #[test]
    fn test_metrics_increment_errored() {
        let m = Metrics::new();
        m.increment_errored();
        assert_eq!(m.errored(), 1);
    }

    #[test]
    fn test_metrics_mixed_increments() {
        let m = Metrics::new();
        m.increment_processed();
        m.increment_errored();
        m.increment_processed();
        assert_eq!(m.processed(), 2);
        assert_eq!(m.errored(), 1);
    }

    // --- queue depth ---

    #[test]
    fn test_metrics_queue_depth_default_zero() {
        let m = Metrics::new();
        assert_eq!(m.task_queue_depth.load(Ordering::Relaxed), 0);
        assert_eq!(m.result_queue_depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_metrics_set_task_queue_depth() {
        let m = Metrics::new();
        m.set_task_queue_depth(42);
        assert_eq!(m.task_queue_depth.load(Ordering::Relaxed), 42);
        m.set_task_queue_depth(0);
        assert_eq!(m.task_queue_depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_metrics_set_result_queue_depth() {
        let m = Metrics::new();
        m.set_result_queue_depth(7);
        assert_eq!(m.result_queue_depth.load(Ordering::Relaxed), 7);
    }

    // --- indexer metrics ---

    #[test]
    fn test_metrics_indexer_defaults() {
        let m = Metrics::new();
        assert_eq!(m.indexer_docs_indexed.load(Ordering::Relaxed), 0);
        assert_eq!(m.indexer_last_commit_age.load(Ordering::Relaxed), 0);
        assert_eq!(m.indexer_writer_mem.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_metrics_set_indexer_docs_indexed() {
        let m = Metrics::new();
        m.set_indexer_docs_indexed(100);
        assert_eq!(m.indexer_docs_indexed.load(Ordering::Relaxed), 100);
        m.set_indexer_docs_indexed(200);
        assert_eq!(m.indexer_docs_indexed.load(Ordering::Relaxed), 200);
    }

    // --- throughput ---

    #[test]
    fn test_metrics_throughput_initial() {
        let m = Metrics::new();
        assert_eq!(m.throughput(), 0.0);
    }

    #[test]
    fn test_metrics_throughput_after_work() {
        let m = Metrics::new();
        m.increment_processed();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let t = m.throughput();
        assert!(t > 0.0);
    }

    #[test]
    fn test_metrics_throughput_increases_with_more_work() {
        let m = Metrics::new();
        std::thread::sleep(std::time::Duration::from_millis(20));
        m.increment_processed();
        let t1 = m.throughput();
        m.increment_processed();
        m.increment_processed();
        let t2 = m.throughput();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_metrics_throughput_with_zero_processed() {
        let m = Metrics::new();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert_eq!(m.throughput(), 0.0);
    }

    // --- elapsed_secs ---

    #[test]
    fn test_metrics_elapsed_secs_increases() {
        let m = Metrics::new();
        let t1 = m.elapsed_secs();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let t2 = m.elapsed_secs();
        assert!(t2 > t1);
    }

    #[test]
    fn test_metrics_elapsed_secs_non_negative() {
        let m = Metrics::new();
        assert!(m.elapsed_secs() >= 0.0);
    }

    // --- log_summary ---

    #[test]
    fn test_log_summary_does_not_log_before_5s() {
        let m = Metrics::new();
        m.increment_processed();
        // last_log is set to now, so log_summary should not log (last was just set)
        // We can't easily test the absence of a log, but we can verify it doesn't panic
        m.log_summary();
        // Wait a tiny bit and call again — still shouldn't log
        m.log_summary();
    }
}
