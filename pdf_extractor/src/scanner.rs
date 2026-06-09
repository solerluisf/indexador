use anyhow::{Context, Result};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::collections::HashMap;
use std::path::Path;
#[cfg_attr(not(test), allow(unused_imports))]
use std::path::PathBuf;
use xxhash_rust::xxh3::xxh3_64;

pub struct JobStore {
    pool: Pool<SqliteConnectionManager>,
}

impl JobStore {
    pub fn open(db_path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(db_path)
            .with_init(|conn| conn.execute_batch("PRAGMA busy_timeout = 5000;"));
        let pool = Pool::builder()
            .max_size(8)
            .build(manager)
            .context("Failed to build SQLite connection pool")?;
        let conn = pool.get().context("Failed to get initial connection")?;
        // WAL mode persists in the database file, so it only needs to be set once.
        // In-memory databases are test-only and don't benefit from WAL.
        conn.execute_batch("PRAGMA journal_mode = WAL;")
            .context("Failed to enable WAL journal mode")?;
        Self::init_schema(&conn)?;
        drop(conn);
        Ok(Self { pool })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        // Use a unique temp file per call so pool connections share
        // the same database without cross-test contamination.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pdf_jobstore_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).context("Failed to create temp dir for JobStore")?;
        let db_path = dir.join("test.db");
        Self::open(&db_path)
    }

    fn init_schema(conn: &rusqlite::Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                checksum TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                ocr_flag INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                ocr_attempts INTEGER NOT NULL DEFAULT 0,
                ocr_error TEXT,
                failed_ocr TEXT,
                language TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);",
        )
        .context("Failed to create jobs table")?;

        conn.execute_batch("ALTER TABLE jobs ADD COLUMN ocr_attempts INTEGER NOT NULL DEFAULT 0;").ok();
        conn.execute_batch("ALTER TABLE jobs ADD COLUMN ocr_error TEXT;").ok();
        conn.execute_batch("ALTER TABLE jobs ADD COLUMN failed_ocr TEXT;").ok();
        conn.execute_batch("ALTER TABLE jobs ADD COLUMN language TEXT;").ok();
        conn.execute_batch("ALTER TABLE jobs ADD COLUMN file_modified INTEGER;").ok();
        conn.execute_batch("ALTER TABLE jobs ADD COLUMN file_size INTEGER;").ok();
        Ok(())
    }

    pub fn upsert_file(&self, path: &str, checksum: &str) -> Result<bool> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let mut stmt = conn
            .prepare(
                "INSERT INTO jobs (path, checksum, status)
                 VALUES (?1, ?2, 'pending')
                 ON CONFLICT(path) DO UPDATE SET
                     checksum = excluded.checksum,
                     status = 'pending',
                     ocr_flag = 0,
                     error = NULL
                 WHERE excluded.checksum != jobs.checksum",
            )
            .context("Failed to prepare upsert")?;

        let changed = stmt
            .execute(rusqlite::params![path, checksum])
            .context("Failed to upsert file")?;

        if changed > 0 {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Insert or update a job as pending without reading the file for checksum.
    /// Always sets status to 'pending' regardless of checksum changes.
    /// Returns true if a row was inserted/updated, false if unchanged.
    ///
    /// Used by scan_directory after the fast-path (mtime+size) check succeeds.
    /// The checksum field is left empty — the worker will compute and store it
    /// during extraction. This avoids the double I/O of reading the file once
    /// for a scanner checksum and again for PDFium extraction.
    pub fn mark_pending(&self, path: &str) -> Result<bool> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let changed = conn
            .execute(
                "INSERT INTO jobs (path, checksum, status)
                 VALUES (?1, '', 'pending')
                 ON CONFLICT(path) DO UPDATE SET
                     status = 'pending',
                     ocr_flag = 0,
                     error = NULL",
                rusqlite::params![path],
            )
            .context("Failed to mark pending")?;
        Ok(changed > 0)
    }

    pub fn mark_done(&self, id: i64, ocr_flag: bool) -> Result<()> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        conn.execute(
            "UPDATE jobs SET status = 'done', ocr_flag = ?1 WHERE id = ?2",
            rusqlite::params![ocr_flag as i32, id],
        )
        .context("Failed to mark job done")?;
        Ok(())
    }

    pub fn batch_mark_done(&self, items: &[(i64, bool)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut conn = self.pool.get().context("Failed to get DB connection")?;
        let tx = conn.transaction()?;

        // Group by ocr_flag so a single IN-clause per group suffices.
        let mut false_ids = Vec::new();
        let mut true_ids = Vec::new();
        for &(id, flag) in items {
            if flag { true_ids.push(id); } else { false_ids.push(id); }
        }

        // SQLite default max variables per statement is 999.
        const MAX_VARS: usize = 999;
        for (ids, flag_val) in [(&false_ids, "0"), (&true_ids, "1")] {
            for chunk in ids.chunks(MAX_VARS) {
                let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "UPDATE jobs SET status = 'done', ocr_flag = {flag_val} WHERE id IN ({placeholders})"
                );
                tx.execute(&sql, rusqlite::params_from_iter(chunk.iter().copied()))
                    .context("Failed to batch-mark jobs done")?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    pub fn mark_error(&self, id: i64, error_msg: &str) -> Result<()> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        conn.execute(
            "UPDATE jobs SET status = 'error', error = ?1 WHERE id = ?2",
            rusqlite::params![error_msg, id],
        )
        .context("Failed to mark job error")?;
        Ok(())
    }

    pub fn count_pending(&self) -> Result<i64> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .context("Failed to count pending jobs")?;
        Ok(count)
    }

    #[allow(dead_code)]
    pub fn fetch_pending(&self, limit: i64) -> Result<Vec<(i64, String, String)>> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
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

    pub fn claim_pending(&self, limit: i64) -> Result<Vec<(i64, String, String)>> {
        let mut conn = self.pool.get().context("Failed to get DB connection")?;
        let clamped = if limit < 0 { 0 } else { limit };

        // Use an IMMEDIATE transaction so the SELECT + UPDATE is atomic
        // across pool connections. Without this, concurrent claim_pending
        // calls from different connections could claim the same rows.
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("Failed to begin immediate transaction for claim")?;

        let rows: Vec<(i64, String, String)> = {
            let mut select = tx
                .prepare("SELECT id, path, checksum FROM jobs WHERE status = 'pending' LIMIT ?1")
                .context("Failed to prepare claim select")?;

            let r = select
                .query_map(rusqlite::params![clamped], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .context("Failed to query pending jobs for claim")?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();

            r
        };

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let id_list: Vec<String> = rows.iter().map(|(id, _, _)| id.to_string()).collect();
        let placeholders = id_list.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE jobs SET status = 'extracting' WHERE id IN ({})",
            placeholders
        );
        tx.execute(&sql, rusqlite::params_from_iter(&id_list))
            .context("Failed to mark jobs as extracting")?;

        tx.commit().context("Failed to commit claim transaction")?;
        Ok(rows)
    }

    /// Reset any jobs stuck in 'extracting' status back to 'pending'.
    /// Used at the end of the pipeline to recover tasks lost when the
    /// worker process crashes mid-batch.
    pub fn reprocess_extracting(&self) -> Result<()> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        conn.execute(
            "UPDATE jobs SET status = 'pending', error = NULL WHERE status = 'extracting'",
            [],
        )
        .context("Failed to reprocess extracting jobs")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn mark_extracted(&self, id: i64, ocr_flag: bool) -> Result<()> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        conn.execute(
            "UPDATE jobs SET status = 'extracted', ocr_flag = ?1 WHERE id = ?2",
            rusqlite::params![ocr_flag as i32, id],
        )
        .context("Failed to mark job extracted")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn fetch_extracted(&self, limit: i64) -> Result<Vec<(i64, String, String, bool)>> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let clamped = if limit < 0 { 0 } else { limit };
        let mut stmt = conn
            .prepare(
                "SELECT id, path, checksum, ocr_flag FROM jobs WHERE status = 'extracted' LIMIT ?1",
            )
            .context("Failed to prepare fetch extracted query")?;

        let rows = stmt
            .query_map(rusqlite::params![clamped], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })
            .context("Failed to query extracted jobs")?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.context("Failed to read row")?);
        }
        Ok(result)
    }

    #[allow(dead_code)]
    pub fn count_by_status(&self, status: &str) -> Result<i64> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE status = ?1",
                rusqlite::params![status],
                |row| row.get(0),
            )
            .context("Failed to count jobs by status")?;
        Ok(count)
    }

    pub fn fetch_ocr_needed(&self, limit: i64, max_retries: u32) -> Result<Vec<(i64, String, String)>> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let clamped = if limit < 0 { 0 } else { limit };
        let mut stmt = conn
            .prepare(
                "SELECT id, path, checksum FROM jobs WHERE status = 'done' AND ocr_flag = 1 AND ocr_attempts < ?1 LIMIT ?2",
            )
            .context("Failed to prepare fetch_ocr_needed query")?;

        let rows = stmt
            .query_map(rusqlite::params![max_retries, clamped], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("Failed to query OCR-needed jobs")?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.context("Failed to read row")?);
        }
        Ok(result)
    }

    pub fn mark_ocr_attempt(&self, id: i64, success: bool, ocr_error: Option<&str>, max_retries: u32) -> Result<()> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        if success {
            conn.execute(
                "UPDATE jobs SET ocr_attempts = ocr_attempts + 1, ocr_flag = 0, ocr_error = NULL, failed_ocr = NULL WHERE id = ?1",
                rusqlite::params![id],
            )?;
        } else {
            conn.execute(
                "UPDATE jobs SET ocr_attempts = ocr_attempts + 1, ocr_error = ?1,
                 failed_ocr = CASE WHEN ocr_attempts + 1 >= ?3 THEN datetime('now') ELSE NULL END
                 WHERE id = ?2",
                rusqlite::params![ocr_error, id, max_retries],
            )?;
        }
        Ok(())
    }

    pub fn fetch_failed_ocr(&self) -> Result<Vec<(i64, String, String, Option<String>)>> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let mut stmt = conn
            .prepare(
                "SELECT id, path, checksum, ocr_error FROM jobs WHERE failed_ocr IS NOT NULL ORDER BY failed_ocr DESC",
            )
            .context("Failed to prepare fetch_failed_ocr query")?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .context("Failed to query failed OCR jobs")?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.context("Failed to read row")?);
        }
        Ok(result)
    }

    /// Fetch all jobs with status 'error'.
    pub fn fetch_errored(&self) -> Result<Vec<(i64, String, String, i32, Option<String>)>> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let mut stmt = conn
            .prepare(
                "SELECT id, path, status, ocr_flag, error FROM jobs WHERE status = 'error' ORDER BY id",
            )
            .context("Failed to prepare fetch_errored query")?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i32>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .context("Failed to query errored jobs")?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.context("Failed to read row")?);
        }
        Ok(result)
    }

    pub fn count_ocr_pending(&self, max_retries: u32) -> Result<i64> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE status = 'done' AND ocr_flag = 1 AND ocr_attempts < ?1",
                rusqlite::params![max_retries],
                |row| row.get(0),
            )
            .context("Failed to count OCR-pending jobs")?;
        Ok(count)
    }

    /// Fast-path: returns true if a row exists with matching path, mtime, and size.
    /// Used by scan_directory to skip re-reading+re-checksumming unchanged files.
    pub fn is_file_unchanged(&self, path: &str, modified: u64, size: u64) -> Result<bool> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM jobs WHERE path = ?1 AND file_modified = ?2 AND file_size = ?3",
                rusqlite::params![path, modified, size],
                |_| Ok(true),
            )
            .unwrap_or(false);
        Ok(exists)
    }

    /// Update stored file metadata by job id.
    /// This is called unconditionally (even when the checksum hasn't changed)
    /// so that future scans can use the fast-path.
    pub fn update_file_metadata(&self, id: i64, modified: u64, size: u64) -> Result<()> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        conn.execute(
            "UPDATE jobs SET file_modified = ?1, file_size = ?2 WHERE id = ?3",
            rusqlite::params![modified, size, id],
        )
        .context("Failed to update file metadata")?;
        Ok(())
    }

    /// Get the primary key `id` for a given path, or `None` if not found.
    pub fn get_id_by_path(&self, path: &str) -> Result<Option<i64>> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        match conn.query_row(
            "SELECT id FROM jobs WHERE path = ?1",
            rusqlite::params![path],
            |row| row.get(0),
        ) {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Load all known file paths + metadata into a HashMap for fast in-memory lookups.
    /// Used by scan_directory to avoid per-file SQLite queries during the walk.
    /// Legacy rows with NULL file_modified/file_size are treated as (0, 0),
    /// forcing a re-index on first scan after migration.
    pub fn load_all_metadata(&self) -> Result<HashMap<String, (u64, u64, String)>> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        let mut stmt = conn
            .prepare("SELECT path, file_modified, file_size, status FROM jobs")
            .context("Failed to prepare load_all_metadata query")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<u64>>(1)?.unwrap_or(0),
                    row.get::<_, Option<u64>>(2)?.unwrap_or(0),
                    row.get::<_, String>(3)?,
                ))
            })
            .context("Failed to query all metadata")?;
        let mut map = HashMap::new();
        for row in rows {
            if let Ok((path, modified, size, status)) = row {
                map.insert(path, (modified, size, status));
            }
        }
        Ok(map)
    }

    /// Batch upsert multiple files as pending in a single transaction.
    /// Each entry includes the path alongside its mtime and file size,
    /// so no separate `update_file_metadata` call is needed afterward.
    pub fn batch_mark_pending(&self, files: &[(String, u64, u64)]) -> Result<()> {
        let mut conn = self.pool.get().context("Failed to get DB connection")?;
        let tx = conn
            .transaction()
            .context("Failed to begin transaction for batch_mark_pending")?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO jobs (path, checksum, status, file_modified, file_size)
                     VALUES (?1, '', 'pending', ?2, ?3)
                     ON CONFLICT(path) DO UPDATE SET
                         status = 'pending',
                         ocr_flag = 0,
                         error = NULL,
                         file_modified = excluded.file_modified,
                         file_size = excluded.file_size",
                )
                .context("Failed to prepare batch_mark_pending statement")?;
            for (path, modified, size) in files {
                stmt.execute(rusqlite::params![path, modified, size])
                    .with_context(|| format!("Failed to batch-mark pending: {}", path))?;
            }
        }
        tx.commit()
            .context("Failed to commit batch_mark_pending transaction")?;
        Ok(())
    }

    #[cfg(test)]
    pub fn pragma_value(&self, pragma: &str) -> Result<String> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        conn.query_row(
            &format!("PRAGMA {}", pragma),
            [],
            |row| {
                let v: rusqlite::types::Value = row.get(0)?;
                match v {
                    rusqlite::types::Value::Text(s) => Ok(s),
                    rusqlite::types::Value::Integer(i) => Ok(i.to_string()),
                    rusqlite::types::Value::Real(f) => Ok(f.to_string()),
                    rusqlite::types::Value::Blob(b) => Ok(format!("{:?}", b)),
                    rusqlite::types::Value::Null => Ok(String::new()),
                }
            },
        )
        .context("Failed to query pragma")
    }

    #[cfg(test)]
    pub fn get_job_language(&self, id: i64) -> Result<Option<String>> {
        let conn = self.pool.get().context("Failed to get DB connection")?;
        conn.query_row(
            "SELECT language FROM jobs WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .context("Failed to query job language")
    }
}

pub fn compute_checksum(data: &[u8]) -> String {
    let hash = xxh3_64(data);
    format!("{:016x}", hash)
}

pub fn scan_directory(jobs: &JobStore, dir: &Path) -> Result<u64> {
    use rayon::prelude::*;
    use walkdir::WalkDir;

    if !dir.is_dir() {
        anyhow::bail!("Input path is not a directory: {}", dir.display());
    }

    // Phase 1: load all known file metadata into memory (single SQL query).
    // Replaces per-file is_file_unchanged() round-trips with a HashMap lookup.
    let known = jobs.load_all_metadata()?;

    // Phase 2: walk the directory tree in parallel, check each PDF against the
    // in-memory map, and collect only new/changed files with their stats.
    // The walkdir iterator is Send, so par_bridge() parallelizes the
    // std::fs::metadata() calls across all available CPU cores.
    let changes: Vec<(String, u64, u64)> = WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("pdf"))
                .unwrap_or(false)
        })
        .par_bridge()
        .filter_map(|entry| {
            let path = entry.path();
            let path_str = path.to_string_lossy().to_string();
            let metadata = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(_) => return None,
            };
            let modified = match metadata.modified() {
                Ok(t) => t
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                Err(_) => 0,
            };
            let size = metadata.len();

            // In-memory check: no SQLite round-trip per file.
            // Always re-queue errored or stuck-extracting files regardless
            // of mtime/size so they are retried on subsequent runs.
            if let Some((known_mtime, known_size, known_status)) = known.get(&path_str) {
                if *known_mtime == modified && *known_size == size
                    && known_status != "error" && known_status != "extracting"
                {
                    return None; // unchanged
                }
            }

            Some((path_str, modified, size))
        })
        .collect();

    if changes.is_empty() {
        return Ok(0);
    }

    // Phase 3: batch upsert all new/changed files in a single transaction.
    // The UPSERT handles both the status reset AND metadata update atomically,
    // so no separate get_id_by_path + update_file_metadata is needed.
    jobs.batch_mark_pending(&changes)?;

    Ok(changes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::thread;

    fn unique_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = temp_dir().join(format!("pdf_extractor_test_{}_{}", std::process::id(), id));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- compute_checksum: basic flows ---

    #[test]
    fn test_compute_checksum_consistency() {
        let dir = unique_dir();
        let path = dir.join("test.txt");
        fs::write(&path, b"hello world").unwrap();
        let data = fs::read(&path).unwrap();

        let hash1 = compute_checksum(&data);
        let hash2 = compute_checksum(&data);
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
        let data_a = fs::read(&a).unwrap();
        let data_b = fs::read(&b).unwrap();

        let hash_a = compute_checksum(&data_a);
        let hash_b = compute_checksum(&data_b);
        assert_ne!(hash_a, hash_b);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_compute_checksum_hex_format() {
        let dir = unique_dir();
        let path = dir.join("test.txt");
        fs::write(&path, b"data").unwrap();
        let data = fs::read(&path).unwrap();

        let hash = compute_checksum(&data);
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        fs::remove_dir_all(&dir).unwrap();
    }

    // --- compute_checksum: content-based ---

    #[test]
    fn test_compute_checksum_empty_data() {
        let hash = compute_checksum(b"");
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_compute_checksum_binary_content() {
        let data: Vec<u8> = (0..=255).collect();
        let hash = compute_checksum(&data);
        assert_eq!(hash.len(), 16);

    }

    #[test]
    fn test_compute_checksum_large_data() {
        let data = vec![0xABu8; 10_000_000];
        let hash = compute_checksum(&data);
        assert_eq!(hash.len(), 16);
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

    // --- mark_pending: new no-checksum path ---

    #[test]
    fn test_mark_pending_inserts_new() {
        let store = JobStore::open_in_memory().unwrap();
        let inserted = store.mark_pending("/new.pdf").unwrap();
        assert!(inserted);
        assert_eq!(store.count_pending().unwrap(), 1);
    }

    #[test]
    fn test_mark_pending_always_returns_true_for_existing() {
        let store = JobStore::open_in_memory().unwrap();
        store.mark_pending("/a.pdf").unwrap();

        // Second call should also return true (always sets to pending)
        let inserted = store.mark_pending("/a.pdf").unwrap();
        assert!(inserted, "mark_pending should always return true for existing paths");
        assert_eq!(store.count_pending().unwrap(), 1);
    }

    #[test]
    fn test_mark_pending_resets_error_jobs() {
        let store = JobStore::open_in_memory().unwrap();
        store.mark_pending("/a.pdf").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_error(batch[0].0, "previous error").unwrap();

        // mark_pending should reset to pending
        let inserted = store.mark_pending("/a.pdf").unwrap();
        assert!(inserted);
        assert_eq!(store.count_pending().unwrap(), 1);
    }

    #[test]
    fn test_mark_pending_resets_done_jobs() {
        let store = JobStore::open_in_memory().unwrap();
        store.mark_pending("/a.pdf").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, false).unwrap();

        let inserted = store.mark_pending("/a.pdf").unwrap();
        assert!(inserted);
        assert_eq!(store.count_pending().unwrap(), 1);
    }

    #[test]
    fn test_mark_pending_leaves_empty_checksum() {
        let store = JobStore::open_in_memory().unwrap();
        store.mark_pending("/a.pdf").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        assert_eq!(batch[0].2, "", "mark_pending should store empty checksum");
    }

    // --- JobStore: claim_pending ---

    #[test]
    fn test_claim_pending_claims_and_changes_status() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        store.upsert_file("/b.pdf", "def").unwrap();

        let claimed = store.claim_pending(10).unwrap();
        assert_eq!(claimed.len(), 2);

        let pending = store.count_pending().unwrap();
        assert_eq!(pending, 0, "Claimed jobs should no longer be pending");

        let extracting = store.count_by_status("extracting").unwrap();
        assert_eq!(extracting, 2, "Claimed jobs should be 'extracting'");
    }

    #[test]
    fn test_claim_pending_respects_limit() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        store.upsert_file("/b.pdf", "def").unwrap();
        store.upsert_file("/c.pdf", "ghi").unwrap();

        let claimed = store.claim_pending(2).unwrap();
        assert_eq!(claimed.len(), 2);

        let pending = store.count_pending().unwrap();
        assert_eq!(pending, 1, "One should remain pending");
    }

    #[test]
    fn test_claim_pending_empty_when_none_pending() {
        let store = JobStore::open_in_memory().unwrap();
        let claimed = store.claim_pending(10).unwrap();
        assert!(claimed.is_empty());
    }

    #[test]
    fn test_claim_pending_negative_limit() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let claimed = store.claim_pending(-1).unwrap();
        assert!(claimed.is_empty(), "Negative limit should return empty");
    }

    #[test]
    fn test_claim_pending_does_not_claim_done_or_error() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.fetch_pending(10).unwrap();
        store.mark_done(batch[0].0, false).unwrap();

        store.upsert_file("/b.pdf", "def").unwrap();
        let batch2 = store.fetch_pending(10).unwrap();
        store.mark_error(batch2[0].0, "err").unwrap();

        // Should not claim done or error jobs
        let claimed = store.claim_pending(10).unwrap();
        assert!(claimed.is_empty());
    }

    #[test]
    fn test_claim_pending_only_once() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();

        let first = store.claim_pending(10).unwrap();
        assert_eq!(first.len(), 1);

        let second = store.claim_pending(10).unwrap();
        assert!(second.is_empty(), "Same job should not be claimable twice");
    }

    // --- JobStore: extracted status ---

    #[test]
    fn test_mark_extracted_sets_status() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();

        let batch = store.claim_pending(10).unwrap();
        assert_eq!(batch.len(), 1);
        let id = batch[0].0;

        store.mark_extracted(id, true).unwrap();

        assert_eq!(store.count_by_status("extracting").unwrap(), 0);
        assert_eq!(store.count_by_status("extracted").unwrap(), 1);
    }

    #[test]
    fn test_mark_extracted_nonexistent_id() {
        let store = JobStore::open_in_memory().unwrap();
        let result = store.mark_extracted(999, false);
        assert!(result.is_ok(), "mark_extracted on nonexistent id should not error");
    }

    #[test]
    fn test_fetch_extracted_returns_marked_jobs() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        store.upsert_file("/b.pdf", "def").unwrap();

        let batch = store.claim_pending(10).unwrap();
        assert_eq!(batch.len(), 2);

        store.mark_extracted(batch[0].0, true).unwrap();
        store.mark_extracted(batch[1].0, false).unwrap();

        let extracted = store.fetch_extracted(10).unwrap();
        assert_eq!(extracted.len(), 2);

        let doc1 = extracted.iter().find(|(_, p, _, _)| p.contains("a.pdf")).unwrap();
        assert!(doc1.3, "a.pdf should have ocr_flag=true");

        let doc2 = extracted.iter().find(|(_, p, _, _)| p.contains("b.pdf")).unwrap();
        assert!(!doc2.3, "b.pdf should have ocr_flag=false");
    }

    #[test]
    fn test_fetch_extracted_empty_when_none() {
        let store = JobStore::open_in_memory().unwrap();
        let extracted = store.fetch_extracted(10).unwrap();
        assert!(extracted.is_empty());
    }

    #[test]
    fn test_fetch_extracted_respects_limit() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "a").unwrap();
        store.upsert_file("/b.pdf", "b").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_extracted(batch[0].0, false).unwrap();
        store.mark_extracted(batch[1].0, false).unwrap();

        let limited = store.fetch_extracted(1).unwrap();
        assert_eq!(limited.len(), 1);
    }

    #[test]
    fn test_fetch_extracted_negative_limit() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "a").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_extracted(batch[0].0, false).unwrap();

        let extracted = store.fetch_extracted(-1).unwrap();
        assert!(extracted.is_empty(), "Negative limit should return empty");
    }

    // --- JobStore: count_by_status ---

    #[test]
    fn test_count_by_status_counts_only_matching() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "a").unwrap();  // status = pending
        store.upsert_file("/b.pdf", "b").unwrap();
        let batch = store.claim_pending(10).unwrap();  // status = extracting
        store.mark_extracted(batch[0].0, false).unwrap();  // status = extracted
        store.mark_done(batch[1].0, false).unwrap();  // status = done

        assert_eq!(store.count_by_status("pending").unwrap(), 0);
        assert_eq!(store.count_by_status("extracting").unwrap(), 0);
        assert_eq!(store.count_by_status("extracted").unwrap(), 1);
        assert_eq!(store.count_by_status("done").unwrap(), 1);
    }

    #[test]
    fn test_count_by_status_unknown_status_returns_zero() {
        let store = JobStore::open_in_memory().unwrap();
        let count = store.count_by_status("nonexistent").unwrap();
        assert_eq!(count, 0);
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

    // --- scan_directory: empty subdirectory ---

    #[test]
    fn test_scan_directory_empty_subdirectory() {
        let dir = make_scan_dir();
        fs::create_dir_all(dir.join("empty")).unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 0);
        fs::remove_dir_all(&dir).ok();
    }

    // --- OCR tracking: basic flows ---

    #[test]
    fn test_ocr_mark_success_clears_flag() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, true).unwrap();  // done with ocr_flag=true

        let needed = store.fetch_ocr_needed(10, 2).unwrap();
        assert_eq!(needed.len(), 1, "Should find OCR-needed doc");

        store.mark_ocr_attempt(needed[0].0, true, None, 2).unwrap();
        let after = store.fetch_ocr_needed(10, 2).unwrap();
        assert!(after.is_empty(), "After successful OCR, flag should be cleared");
    }

    #[test]
    fn test_ocr_mark_failure_increments_attempts() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, true).unwrap();

        store.mark_ocr_attempt(batch[0].0, false, Some("ocr failed"), 2).unwrap();
        let needed = store.fetch_ocr_needed(10, 2).unwrap();
        assert_eq!(needed.len(), 1, "Should still be needed (attempts < max_retries)");

        store.mark_ocr_attempt(batch[0].0, false, Some("still failing"), 2).unwrap();
        let after = store.fetch_ocr_needed(10, 2).unwrap();
        assert!(after.is_empty(), "Should stop retrying after max_retries");
    }

    // --- OCR tracking: alternative flows ---

    #[test]
    fn test_ocr_needed_only_matches_ocr_flag() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "a").unwrap();
        store.upsert_file("/b.pdf", "b").unwrap();
        let batch = store.claim_pending(10).unwrap();

        // a.pdf: done with ocr_flag=true, b.pdf: done with ocr_flag=false
        store.mark_done(batch[0].0, true).unwrap();
        store.mark_done(batch[1].0, false).unwrap();

        let needed = store.fetch_ocr_needed(10, 2).unwrap();
        assert_eq!(needed.len(), 1, "Only ocr_flag=true should match");
        assert!(needed[0].1.contains("a.pdf"), "Should be the OCR-needed doc");
    }

    #[test]
    fn test_ocr_needed_respects_limit() {
        let store = JobStore::open_in_memory().unwrap();
        for i in 0..5 {
            store.upsert_file(&format!("/{}.pdf", i), &format!("cs{}", i)).unwrap();
        }
        let batch = store.claim_pending(10).unwrap();
        for (id, _, _) in &batch {
            store.mark_done(*id, true).unwrap();
        }

        let needed = store.fetch_ocr_needed(2, 2).unwrap();
        assert_eq!(needed.len(), 2, "Should respect limit");
    }

    // --- OCR tracking: error flows ---

    #[test]
    fn test_ocr_needed_empty_when_none() {
        let store = JobStore::open_in_memory().unwrap();
        let needed = store.fetch_ocr_needed(10, 2).unwrap();
        assert!(needed.is_empty(), "No jobs → no OCR needed");
    }

    #[test]
    fn test_ocr_needed_negative_limit() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, true).unwrap();

        let needed = store.fetch_ocr_needed(-1, 2).unwrap();
        assert!(needed.is_empty(), "Negative limit should return empty");
    }

    #[test]
    fn test_ocr_count_pending() {
        let store = JobStore::open_in_memory().unwrap();
        assert_eq!(store.count_ocr_pending(2).unwrap(), 0);

        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, true).unwrap();

        assert_eq!(store.count_ocr_pending(2).unwrap(), 1);
    }

    // --- failed_ocr queue ---

    #[test]
    fn test_failed_ocr_not_set_on_success() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, true).unwrap();

        store.mark_ocr_attempt(batch[0].0, true, None, 2).unwrap();
        let failed = store.fetch_failed_ocr().unwrap();
        assert!(failed.is_empty(), "Successful OCR should not create failed_ocr entry");
    }

    #[test]
    fn test_failed_ocr_set_after_max_retries() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, true).unwrap();

        // First failure: attempts=1, max_retries=2 → not yet failed
        store.mark_ocr_attempt(batch[0].0, false, Some("first fail"), 2).unwrap();
        let failed = store.fetch_failed_ocr().unwrap();
        assert!(failed.is_empty(), "Should not fail after 1 attempt with max_retries=2");

        // Second failure: attempts=2 >= max_retries=2 → marked failed
        store.mark_ocr_attempt(batch[0].0, false, Some("second fail"), 2).unwrap();
        let failed = store.fetch_failed_ocr().unwrap();
        assert_eq!(failed.len(), 1, "Should be in failed_ocr after exhausting retries");
        assert_eq!(failed[0].0, batch[0].0, "ID should match");
        assert!(failed[0].1.contains("a.pdf"), "Path should match");
        assert_eq!(failed[0].3.as_deref(), Some("second fail"), "Should have latest error");
    }

    #[test]
    fn test_failed_ocr_cleared_on_retry_success() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, true).unwrap();

        // Exhaust retries
        store.mark_ocr_attempt(batch[0].0, false, Some("fail 1"), 1).unwrap();
        let failed = store.fetch_failed_ocr().unwrap();
        assert_eq!(failed.len(), 1, "Should be failed after 1 attempt with max_retries=1");

        // If OCR subsequently succeeds (e.g. user retried), failed_ocr should clear
        store.mark_ocr_attempt(batch[0].0, true, None, 1).unwrap();
        let failed = store.fetch_failed_ocr().unwrap();
        assert!(failed.is_empty(), "Successful OCR should clear failed_ocr and ocr_error");
    }

    #[test]
    fn test_fetch_failed_ocr_empty_when_none() {
        let store = JobStore::open_in_memory().unwrap();
        let failed = store.fetch_failed_ocr().unwrap();
        assert!(failed.is_empty(), "No jobs → no failed OCR entries");
    }

    // --- language column ---

    #[test]
    fn test_language_default_null() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let batch = store.claim_pending(10).unwrap();
        store.mark_done(batch[0].0, true).unwrap();

        let lang = store.get_job_language(batch[0].0).unwrap();
        assert_eq!(lang, None, "New job should have NULL language");
    }

    #[test]
    fn test_language_stored_and_retrieved() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/b.pdf", "def").unwrap();
        let batch = store.claim_pending(10).unwrap();

        // Direct SQL update to set language (as the pipeline would)
        let conn = store.pool.get().unwrap();
        conn.execute(
            "UPDATE jobs SET language = ?1 WHERE id = ?2",
            rusqlite::params!["eng", batch[0].0],
        ).unwrap();
        drop(conn);

        let lang = store.get_job_language(batch[0].0).unwrap();
        assert_eq!(lang.as_deref(), Some("eng"), "Should retrieve stored language");
    }

    // ── Regression: compute_checksum is now pure &[u8] -> String (no I/O) ──

    #[test]
    fn test_compute_checksum_all_byte_values() {
        let mut data: Vec<u8> = (0..=255).collect();
        let hash = compute_checksum(&data);
        assert_eq!(hash.len(), 16, "hex digest should be 16 chars");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "should be hex");

        // Verify idempotence
        assert_eq!(compute_checksum(&data), hash);

        // Verify single-bit change flips output
        data[0] ^= 1;
        let hash2 = compute_checksum(&data);
        assert_ne!(hash2, hash, "bit flip should change checksum");
    }

    #[test]
    fn test_compute_checksum_idempotent_concurrent() {
        let data = b"concurrent idempotence check data";
        let mut handles = Vec::new();
        for _ in 0..10 {
            let d = data.to_vec();
            handles.push(std::thread::spawn(move || compute_checksum(&d)));
        }
        let hashes: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for h in &hashes {
            assert_eq!(h, &hashes[0], "all concurrent calls should produce identical checksums");
        }
    }

    // ── Regression: scan_directory with inlined walkdir ──

    #[test]
    fn test_scan_directory_pdfs_in_subdirs_and_root() {
        let dir = make_scan_dir();
        fs::write(dir.join("root.pdf"), b"root").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("nested.pdf"), b"nested").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 2, "should discover PDFs in root and subdirectory");
        assert_eq!(store.count_pending().unwrap(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_skips_non_pdf_extensions() {
        let dir = make_scan_dir();
        fs::write(dir.join("alpha.pdf"), b"pdf").unwrap();
        fs::write(dir.join("bravo.txt"), b"text").unwrap();
        fs::write(dir.join("charlie"), b"noext").unwrap();
        fs::write(dir.join("delta.pdf.txt"), b"double").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 1, "only .pdf should be discovered");
        fs::remove_dir_all(&dir).ok();
    }

    // ── Lazy walk: files processed one-at-a-time, not pre-collected ──

    #[test]
    fn test_scan_directory_large_directory_tree() {
        let dir = make_scan_dir();
        // Create 200 directories each with one PDF — tests that the walk
        // processes files without pre-collecting all paths into memory.
        for i in 0..200 {
            let sub = dir.join(format!("sub{}", i));
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("doc.pdf"), b"content").unwrap();
        }
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 200, "all PDFs in subdirectories should be found");
        assert_eq!(store.count_pending().unwrap(), 200);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_deeply_nested_lazy() {
        let dir = make_scan_dir();
        // Create a chain a/b/c/.../doc.pdf to verify the stack-based
        // walk reaches deeply without pre-collecting sibling files.
        let mut current = dir.clone();
        for _ in 0..50 {
            current = current.join("n");
            fs::create_dir_all(&current).unwrap();
        }
        fs::write(current.join("deep.pdf"), b"deep").unwrap();
        // Also add a file at the root — should be found first during walk
        fs::write(dir.join("shallow.pdf"), b"shallow").unwrap();

        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 2, "should find shallow + deeply nested PDF");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_subdirs_only_no_files() {
        let dir = make_scan_dir();
        fs::create_dir_all(dir.join("a")).unwrap();
        fs::create_dir_all(dir.join("b")).unwrap();
        fs::create_dir_all(dir.join("a").join("c")).unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let count = scan_directory(&store, &dir).unwrap();
        assert_eq!(count, 0, "subdirectories with no PDFs should not produce results");
        fs::remove_dir_all(&dir).ok();
    }

    // ── is_file_unchanged tests ──

    #[test]
    fn test_is_file_unchanged_true() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let id = store.get_id_by_path("/a.pdf").unwrap().unwrap();
        store.update_file_metadata(id, 1000, 500).unwrap();

        assert!(store.is_file_unchanged("/a.pdf", 1000, 500).unwrap());
    }

    #[test]
    fn test_is_file_unchanged_false_wrong_mtime() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let id = store.get_id_by_path("/a.pdf").unwrap().unwrap();
        store.update_file_metadata(id, 1000, 500).unwrap();

        assert!(!store.is_file_unchanged("/a.pdf", 2000, 500).unwrap());
    }

    #[test]
    fn test_is_file_unchanged_false_wrong_size() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let id = store.get_id_by_path("/a.pdf").unwrap().unwrap();
        store.update_file_metadata(id, 1000, 500).unwrap();

        assert!(!store.is_file_unchanged("/a.pdf", 1000, 999).unwrap());
    }

    #[test]
    fn test_is_file_unchanged_missing_path() {
        let store = JobStore::open_in_memory().unwrap();
        assert!(!store.is_file_unchanged("/nonexistent.pdf", 1000, 500).unwrap());
    }

    #[test]
    fn test_is_file_unchanged_no_metadata_stored() {
        let store = JobStore::open_in_memory().unwrap();
        // upsert without calling update_file_metadata
        store.upsert_file("/a.pdf", "abc").unwrap();
        // is_file_unchanged checks file_modified IS NULL, so should return false
        assert!(!store.is_file_unchanged("/a.pdf", 1000, 500).unwrap());
    }

    // ── update_file_metadata tests ──

    #[test]
    fn test_update_file_metadata_stores_values() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let id = store.get_id_by_path("/a.pdf").unwrap().unwrap();
        store.update_file_metadata(id, 42, 777).unwrap();

        assert!(store.is_file_unchanged("/a.pdf", 42, 777).unwrap());
    }

    #[test]
    fn test_update_file_metadata_overwrites_previous() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "abc").unwrap();
        let id = store.get_id_by_path("/a.pdf").unwrap().unwrap();
        store.update_file_metadata(id, 10, 100).unwrap();
        store.update_file_metadata(id, 20, 200).unwrap();

        assert!(!store.is_file_unchanged("/a.pdf", 10, 100).unwrap());
        assert!(store.is_file_unchanged("/a.pdf", 20, 200).unwrap());
    }

    // ── scan_directory fast-path tests ──

    #[test]
    fn test_scan_directory_skips_unchanged_files() {
        let dir = make_scan_dir();
        fs::write(dir.join("doc.pdf"), b"same content").unwrap();
        let store = JobStore::open_in_memory().unwrap();

        // First scan: reads and indexes
        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 1, "first scan should detect the PDF");

        // Second scan: mtime + size match → fast-path skip
        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 0, "second scan should skip unchanged file");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_no_file_read_on_rescan() {
        let dir = make_scan_dir();
        fs::write(dir.join("doc.pdf"), b"content").unwrap();
        let store = JobStore::open_in_memory().unwrap();

        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 1);
        assert_eq!(store.count_pending().unwrap(), 1);

        // Second scan on unchanged file: no file read (fast-path skip)
        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 0, "no new scans on unchanged files");
        assert_eq!(store.count_pending().unwrap(), 1, "pending count unchanged");

        // Third scan: still no file read
        let third = scan_directory(&store, &dir).unwrap();
        assert_eq!(third, 0, "still no new scans");
        assert_eq!(store.count_pending().unwrap(), 1, "pending count still unchanged");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_rescans_modified_content() {
        let dir = make_scan_dir();
        let path = dir.join("doc.pdf");
        fs::write(&path, b"original").unwrap();
        let store = JobStore::open_in_memory().unwrap();

        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 1);

        // Modify content (which changes size and mtime)
        std::thread::sleep(std::time::Duration::from_millis(150)); // ensure mtime changes (NTFS granularity ~100ms)
        fs::write(&path, b"modified content").unwrap();

        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 1, "modified file should be re-scanned");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_rescans_same_size_different_content() {
        let dir = make_scan_dir();
        let path = dir.join("doc.pdf");
        fs::write(&path, b"abcdefgh").unwrap();
        let store = JobStore::open_in_memory().unwrap();

        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 1);

        // Same size but different content.
        // Sleep >1s to cross the second-resolution mtime boundary.
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(&path, b"hgfedcba").unwrap();

        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 1, "same-size modified file should be re-scanned");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_touch_same_content() {
        let dir = make_scan_dir();
        let path = dir.join("doc.pdf");
        fs::write(&path, b"content").unwrap();
        let store = JobStore::open_in_memory().unwrap();

        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 1);

        // Touch file (mtime changes, size unchanged, content unchanged).
        // Without a scanner checksum, the file is always re-scanned when
        // mtime differs.  The metadata is updated so the third scan skips.
        // Sleep >1s to cross the second-resolution mtime boundary.
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(&path, b"content").unwrap();

        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 1, "touched file is re-scanned (no checksum dedup at scan time)");

        // Third scan: now mtime+size match → fast-path skip
        let third = scan_directory(&store, &dir).unwrap();
        assert_eq!(third, 0, "third scan should fast-path skip");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_fast_path_does_not_hide_new_files() {
        let dir = make_scan_dir();
        let store = JobStore::open_in_memory().unwrap();

        // Scan empty dir
        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 0);

        // Add a PDF
        fs::write(dir.join("new.pdf"), b"new file").unwrap();

        // Second scan should find it
        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 1, "new file should be detected after fast-path skip");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_mixed_changed_and_unchanged() {
        let dir = make_scan_dir();
        let store = JobStore::open_in_memory().unwrap();

        fs::write(dir.join("stable.pdf"), b"stable").unwrap();
        fs::write(dir.join("volatile.pdf"), b"v1").unwrap();

        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 2);

        // Modify one file.
        // Sleep >1s to cross the second-resolution mtime boundary.
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(dir.join("volatile.pdf"), b"v2").unwrap();

        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 1, "only the modified file should be rescanned");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_is_file_unchanged_zero_values() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/zero.pdf", "cs").unwrap();
        let id = store.get_id_by_path("/zero.pdf").unwrap().unwrap();
        store.update_file_metadata(id, 0, 0).unwrap();
        assert!(store.is_file_unchanged("/zero.pdf", 0, 0).unwrap());
        assert!(!store.is_file_unchanged("/zero.pdf", 0, 1).unwrap(), "different size should not match");
        assert!(!store.is_file_unchanged("/zero.pdf", 1, 0).unwrap(), "different mtime should not match");
    }

    #[test]
    fn test_is_file_unchanged_large_mtime() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/old.pdf", "cs").unwrap();
        let id = store.get_id_by_path("/old.pdf").unwrap().unwrap();
        store.update_file_metadata(id, 9_999_999_999_999, 100).unwrap();
        assert!(store.is_file_unchanged("/old.pdf", 9_999_999_999_999, 100).unwrap());
    }

    #[test]
    fn test_update_file_metadata_nonexistent_id() {
        let store = JobStore::open_in_memory().unwrap();
        // An UPDATE on a non-existent id is a no-op, not an error.
        store.update_file_metadata(999, 42, 100).unwrap();
    }

    #[test]
    fn test_scan_directory_size_zero_file() {
        let dir = make_scan_dir();
        fs::write(dir.join("empty.pdf"), b"").unwrap();
        let store = JobStore::open_in_memory().unwrap();
        let first = scan_directory(&store, &dir).unwrap();
        assert_eq!(first, 1, "zero-size PDF should be detected");
        let second = scan_directory(&store, &dir).unwrap();
        assert_eq!(second, 0, "zero-size PDF should be fast-path skipped on second scan");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_scan_directory_fast_path_after_metadata_update() {
        let dir = make_scan_dir();
        let path = dir.join("doc.pdf");
        fs::write(&path, b"content").unwrap();
        let store = JobStore::open_in_memory().unwrap();

        assert_eq!(scan_directory(&store, &dir).unwrap(), 1);

        let meta = std::fs::metadata(&path).unwrap();
        let modified = meta.modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let size = meta.len();
        assert!(store.is_file_unchanged(
            &path.to_string_lossy(), modified, size
        ).unwrap(), "file should be considered unchanged after first scan");

        fs::remove_dir_all(&dir).ok();
    }

    // ── SQLite WAL mode + busy timeout tests ──

    #[test]
    fn test_wal_mode_enabled_for_file_db() {
        let dir = unique_dir();
        let db_path = dir.join("test_wal.db");
        let store = JobStore::open(&db_path).unwrap();
        let mode = store.pragma_value("journal_mode").unwrap();
        assert_eq!(mode.to_lowercase(), "wal", "file-based DB should use WAL mode, got: {}", mode);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_busy_timeout_set() {
        let store = JobStore::open_in_memory().unwrap();
        let raw = store.pragma_value("busy_timeout").unwrap();
        let timeout: i64 = raw.trim().parse().unwrap();
        assert_eq!(timeout, 5000, "busy_timeout should be 5000ms, got: '{}'", raw);
    }

    #[test]
    fn test_in_memory_db_still_works_with_pragmas() {
        let store = JobStore::open_in_memory().unwrap();
        store.upsert_file("/a.pdf", "cs1").unwrap();
        assert!(store.is_file_unchanged("/a.pdf", 0, 0).is_ok(), "in-memory DB should be functional");
        assert_eq!(store.count_pending().unwrap(), 1);
    }

    #[test]
    fn test_concurrent_mark_done_contention() {
        let store = Arc::new(JobStore::open_in_memory().unwrap());
        let num_jobs: i64 = 50;
        let mut ids = Vec::new();

        for i in 0..num_jobs {
            let path = format!("/concurrent/{}.pdf", i);
            store.upsert_file(&path, &format!("cs{}", i)).unwrap();
            let batch = store.claim_pending(1).unwrap();
            ids.push(batch[0].0);
        }

        let mut handles = Vec::new();
        for id in ids {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                store.mark_done(id, false).unwrap();
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert_eq!(store.count_pending().unwrap(), 0);
        assert_eq!(store.count_by_status("done").unwrap(), num_jobs);
    }

    #[test]
    fn test_concurrent_claim_and_mark_contention() {
        let store = Arc::new(JobStore::open_in_memory().unwrap());
        let num_jobs: i64 = 40;
        let num_threads = 8;
        let jobs_per_thread = num_jobs / num_threads;

        for i in 0..num_jobs {
            store.upsert_file(&format!("/stress/{}.pdf", i), &format!("cs{}", i)).unwrap();
        }

        let mut handles = Vec::new();
        for _ in 0..num_threads {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                for _ in 0..jobs_per_thread {
                    let batch = store.claim_pending(1).unwrap();
                    if !batch.is_empty() {
                        let (id, _, _) = batch[0];
                        store.mark_done(id, false).unwrap();
                    }
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert_eq!(store.count_pending().unwrap(), 0, "all jobs should be claimed");
        assert_eq!(store.count_by_status("done").unwrap(), num_jobs, "all jobs should be done");
    }

    #[test]
    fn test_concurrent_mixed_operations_contention() {
        let store = Arc::new(JobStore::open_in_memory().unwrap());
        let num_jobs: i64 = 30;

        for i in 0..num_jobs {
            store.upsert_file(&format!("/mixed/{}.pdf", i), &format!("cs{}", i)).unwrap();
        }

        let store_clone = Arc::clone(&store);
        let writer = thread::spawn(move || {
            for i in num_jobs..num_jobs + 10 {
                store_clone.upsert_file(&format!("/mixed/new_{}.pdf", i), &format!("cs{}", i)).unwrap();
            }
        });

        let store_clone = Arc::clone(&store);
        let reader = thread::spawn(move || {
            for _ in 0..20 {
                let _ = store_clone.count_pending();
                let _ = store_clone.count_by_status("done");
            }
        });

        let store_clone = Arc::clone(&store);
        let claimer = thread::spawn(move || {
            let mut total = 0;
            loop {
                let batch = store_clone.claim_pending(5).unwrap();
                if batch.is_empty() {
                    break;
                }
                for (id, _, _) in &batch {
                    store_clone.mark_done(*id, false).unwrap();
                }
                total += batch.len();
                if total >= num_jobs as usize {
                    break;
                }
            }
        });

        writer.join().expect("writer panicked");
        reader.join().expect("reader panicked");
        claimer.join().expect("claimer panicked");

        let done = store.count_by_status("done").unwrap();
        let pending = store.count_pending().unwrap();
        assert_eq!(done + pending, num_jobs + 10, "all original + new jobs should be accounted for");
    }
}


