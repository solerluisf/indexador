use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{info, warn};
use xxhash_rust::xxh3::xxh3_64;

pub struct JobStore {
    conn: Mutex<Connection>,
}

impl JobStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path).context("Failed to open SQLite database")?;
        Self::from_connection(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                checksum TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                ocr_flag INTEGER NOT NULL DEFAULT 0,
                error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);",
        )
        .context("Failed to create jobs table")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert_file(&self, path: &str, checksum: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "INSERT INTO jobs (path, checksum, status)
                 VALUES (?1, ?2, 'pending')
                 ON CONFLICT(path) DO UPDATE SET
                     checksum = excluded.checksum,
                     status = 'pending',
                     ocr_flag = 0,
                     error = NULL
                 WHERE excluded.checksum != jobs.checksum
                   AND jobs.status IN ('done', 'error')",
            )
            .context("Failed to prepare upsert")?;

        let changed = stmt
            .execute(rusqlite::params![path, checksum])
            .context("Failed to upsert file")?;

        if changed > 0 {
            info!(path, "New or changed file registered");
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn mark_done(&self, id: i64, ocr_flag: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET status = 'done', ocr_flag = ?1 WHERE id = ?2",
            rusqlite::params![ocr_flag as i32, id],
        )
        .context("Failed to mark job done")?;
        Ok(())
    }

    pub fn mark_error(&self, id: i64, error_msg: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET status = 'error', error = ?1 WHERE id = ?2",
            rusqlite::params![error_msg, id],
        )
        .context("Failed to mark job error")?;
        Ok(())
    }

    pub fn count_pending(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .context("Failed to count pending jobs")?;
        Ok(count)
    }

    pub fn fetch_pending(&self, limit: i64) -> Result<Vec<(i64, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let clamped = if limit < 0 { 0 } else { limit };
        let mut stmt = conn
            .prepare(
                "SELECT id, path, checksum FROM jobs WHERE status = 'pending' LIMIT ?1",
            )
            .context("Failed to prepare fetch query")?;

        let rows = stmt
            .query_map(rusqlite::params![clamped], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("Failed to query pending jobs")?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.context("Failed to read row")?);
        }
        Ok(result)
    }
}

pub fn compute_checksum(path: &Path) -> Result<String> {
    let data = std::fs::read(path).context("Failed to read file for checksum")?;
    let hash = xxh3_64(&data);
    Ok(format!("{:016x}", hash))
}

pub fn scan_directory(jobs: &JobStore, dir: &Path) -> Result<u64> {
    let mut scanned = 0u64;
    if !dir.is_dir() {
        anyhow::bail!("Input path is not a directory: {}", dir.display());
    }

    let entries = walkdir(dir)?;
    for path in entries {
        if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            != Some(true)
        {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        match compute_checksum(&path) {
            Ok(checksum) => {
                if jobs.upsert_file(&path_str, &checksum)? {
                    scanned += 1;
                }
            }
            Err(e) => {
                warn!(path = %path_str, error = %e, "Failed to compute checksum, skipping");
            }
        }
    }

    Ok(scanned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = temp_dir().join(format!("pdf_extractor_test_{}", id));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- compute_checksum: basic flows ---

    #[test]
    fn test_compute_checksum_consistency() {
        let dir = unique_dir();
        let path = dir.join("test.txt");
        fs::write(&path, b"hello world").unwrap();

        let hash1 = compute_checksum(&path).unwrap();
        let hash2 = compute_checksum(&path).unwrap();
        assert_eq!(hash1, hash2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_compute_checksum_different_files() {
        let dir = unique_dir();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        fs::write(&a, b"hello").unwrap();
        fs::write(&b, b"world").unwrap();

        let hash_a = compute_checksum(&a).unwrap();
        let hash_b = compute_checksum(&b).unwrap();
        assert_ne!(hash_a, hash_b);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_compute_checksum_hex_format() {
        let dir = unique_dir();
        let path = dir.join("test.txt");
        fs::write(&path, b"data").unwrap();

        let hash = compute_checksum(&path).unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        fs::remove_dir_all(&dir).unwrap();
    }

    // --- compute_checksum: error flows ---

    #[test]
    fn test_compute_checksum_nonexistent_file() {
        let result = compute_checksum(&PathBuf::from(r"C:\THIS_FILE_DOES_NOT_EXIST_12345.pdf"));
        assert!(result.is_err());
    }

    #[test]
    fn test_compute_checksum_empty_file() {
        let dir = unique_dir();
        let path = dir.join("empty.pdf");
        fs::write(&path, b"").unwrap();

        let hash = compute_checksum(&path).unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_compute_checksum_binary_content() {
        let dir = unique_dir();
        let path = dir.join("binary.bin");
        let data: Vec<u8> = (0..=255).collect();
        fs::write(&path, &data).unwrap();

        let hash = compute_checksum(&path).unwrap();
        assert_eq!(hash.len(), 16);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_compute_checksum_large_file() {
        let dir = unique_dir();
        let path = dir.join("large.bin");
        let data = vec![0xABu8; 10_000_000];
        fs::write(&path, &data).unwrap();

        let hash = compute_checksum(&path).unwrap();
        assert_eq!(hash.len(), 16);

        fs::remove_dir_all(&dir).unwrap();
    }

    // --- JobStore: basic flows ---

    #[test]
    fn test_jobstore_insert_and_count() {
        let store = JobStore::open_in_memory().unwrap();

        let inserted = store.upsert_file("/a.pdf", "abc").unwrap();
        assert!(inserted);

        let count = store.count_pending().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_jobstore_multiple_inserts() {
        let store = JobStore::open_in_memory().unwrap();

        store.upsert_file("/a.pdf", "aaa").unwrap();
        store.upsert_file("/b.pdf", "bbb").unwrap();
        store.upsert_file("/c.pdf", "ccc").unwrap();

        assert_eq!(store.count_pending().unwrap(), 3);
    }

    #[test]
    fn test_jobstore_duplicate_skipped() {
        let store = JobStore::open_in_memory().unwrap();

        store.upsert_file("/a.pdf", "abc").unwrap();
        let inserted = store.upsert_file("/a.pdf", "abc").unwrap();
        assert!(!inserted);

        let count = store.count_pending().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_jobstore_mark_done() {
        let store = JobStore::open_in_memory().unwrap();

        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        assert_eq!(batch.len(), 1);

        let (id, _path, _checksum) = &batch[0];
        store.mark_done(*id, false).unwrap();

        let count = store.count_pending().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_jobstore_reprocess_changed() {
        let store = JobStore::open_in_memory().unwrap();

        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        store.mark_done(batch[0].0, false).unwrap();

        let inserted = store.upsert_file("/a.pdf", "def").unwrap();
        assert!(inserted);

        let count = store.count_pending().unwrap();
        assert_eq!(count, 1);
    }

    // --- JobStore: error flows ---

    #[test]
    fn test_jobstore_mark_error() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        let id = batch[0].0;

        store.mark_error(id, "corrupt PDF").unwrap();

        assert_eq!(store.count_pending().unwrap(), 0);
        let batch2 = store.fetch_pending(10).unwrap();
        assert!(batch2.is_empty());
    }

    #[test]
    fn test_jobstore_error_not_reprocessed_with_same_checksum() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        store.mark_error(batch[0].0, "error").unwrap();

        let inserted = store.upsert_file("/a.pdf", "abc").unwrap();
        assert!(!inserted, "Same checksum should not re-insert errored file");
    }

    #[test]
    fn test_jobstore_reprocess_after_error_with_new_checksum() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        store.mark_error(batch[0].0, "error").unwrap();

        let inserted = store.upsert_file("/a.pdf", "def").unwrap();
        assert!(inserted, "New checksum should re-insert errored file");
        assert_eq!(store.count_pending().unwrap(), 1);
    }

    #[test]
    fn test_jobstore_mark_nonexistent_id() {
        let store = JobStore::open_in_memory().unwrap();
        let result = store.mark_done(999, false);
        assert!(result.is_ok(), "mark_done on nonexistent id should not error");
    }

    #[test]
    fn test_jobstore_mark_error_nonexistent_id() {
        let store = JobStore::open_in_memory().unwrap();
        let result = store.mark_error(999, "msg");
        assert!(result.is_ok(), "mark_error on nonexistent id should not error");
    }

    // --- JobStore: fetch_pending edge cases ---

    #[test]
    fn test_jobstore_fetch_pending_limit_zero() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(0).unwrap();
        assert!(batch.is_empty());
    }

    #[test]
    fn test_jobstore_fetch_pending_limit_negative() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(-1).unwrap();
        assert!(batch.is_empty());
    }

    #[test]
    fn test_jobstore_fetch_pending_limit_exceeds_count() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(1000).unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn test_jobstore_fetch_pending_respects_limit() {
        let store = JobStore::open_in_memory().unwrap();
        for i in 0..10 {
            store.upsert_file(&format!("/{}.pdf", i), &format!("{:x}", i)).unwrap();
        }
        let batch = store.fetch_pending(3).unwrap();
        assert_eq!(batch.len(), 3);
    }

    #[test]
    fn test_jobstore_fetch_pending_does_not_return_done() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "a").unwrap();
        store.upsert_file("/b.pdf", "b").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        store.mark_done(batch[0].0, false).unwrap();
        let remaining = store.fetch_pending(10).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    // --- JobStore: count edge cases ---

    #[test]
    fn test_jobstore_count_pending_after_mixed_states() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "a").unwrap();
        store.upsert_file("/b.pdf", "b").unwrap();
        store.upsert_file("/c.pdf", "c").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        store.mark_done(batch[0].0, false).unwrap();
        store.mark_error(batch[1].0, "err").unwrap();
        assert_eq!(store.count_pending().unwrap(), 1);
    }

    #[test]
    fn test_jobstore_count_pending_with_empty_store() {
        let store = JobStore::open_in_memory().unwrap();
        assert_eq!(store.count_pending().unwrap(), 0);
    }

    // --- scan_directory: basic flows via helper ---

    fn make_scan_dir() -> PathBuf {
        unique_dir()
    }

    #[test]
    fn test_scan_directory_empty_dir() {
        let dir = make_scan_dir();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_no_pdfs() {
        let dir = make_scan_dir();
        fs::write(dir.join("readme.txt"), b"hello").unwrap();
        fs::write(dir.join("data.csv"), b"a,b,c").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 0);
        assert_eq!(store.count_pending().unwrap(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_finds_pdfs() {
        let dir = make_scan_dir();
        fs::write(dir.join("doc1.pdf"), b"fake pdf").unwrap();
        fs::write(dir.join("doc2.PDF"), b"another").unwrap();
        fs::write(dir.join("notes.txt"), b"ignored").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 2);
        assert_eq!(store.count_pending().unwrap(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_case_insensitive_extension() {
        let dir = make_scan_dir();
        fs::write(dir.join("a.Pdf"), b"x").unwrap();
        fs::write(dir.join("b.PDF"), b"y").unwrap();
        fs::write(dir.join("c.pDf"), b"z").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_skips_duplicates_on_second_scan() {
        let dir = make_scan_dir();
        fs::write(dir.join("doc.pdf"), b"content").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 1);
        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 0, "Same files should not be rescanned");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_subdirectories() {
        let dir = make_scan_dir();
        fs::create_dir_all(dir.join("sub1")).unwrap();
        fs::create_dir_all(dir.join("sub2")).unwrap();
        fs::write(dir.join("sub1").join("a.pdf"), b"x").unwrap();
        fs::write(dir.join("sub2").join("b.pdf"), b"y").unwrap();
        fs::write(dir.join("root.pdf"), b"z").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_deeply_nested() {
        let dir = make_scan_dir();
        let deep = dir.join("a").join("b").join("c").join("d");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.pdf"), b"found").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 1);
        fs::remove_dir_all(&dir).ok();
    }

    // --- scan_directory: error flows ---

    #[test]
    fn test_scan_directory_nonexistent_path() {
        let store = JobStore::open_in_memory().unwrap();
        let result = scan_directory(&store, &PathBuf::from(r"C:\NONEXISTENT_DIR_12345"));
        assert!(result.is_err());
    }

    #[test]
    fn test_scan_directory_path_is_file() {
        let dir = make_scan_dir();
        let file_path = dir.join("file.txt");
        fs::write(&file_path, b"data").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let result = scan_directory(&store, &file_path);
        assert!(result.is_err());
        fs::remove_dir_all(&dir).ok();
    }

    // --- walkdir: private, tested indirectly via scan_directory ---

    #[test]
    fn test_scan_directory_empty_subdirectory() {
        let dir = make_scan_dir();
        fs::create_dir_all(dir.join("empty")).unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 0);
        fs::remove_dir_all(&dir).ok();
    }
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let read_dir = std::fs::read_dir(&current)
            .with_context(|| format!("Failed to read directory: {}", current.display()))?;
        for entry in read_dir {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}
