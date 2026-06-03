use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const PDF_INDEX_FLAG_OCR: u32 = 1;
pub const PDF_INDEX_FLAG_NO_INDEX: u32 = 2;

#[derive(Debug, Clone)]
pub struct CollectionInfo {
    pub id: i64,
    pub books_folder: String,
    pub label: Option<String>,
    pub data_dir: String,
    pub doc_count: i64,
    pub last_indexed: Option<String>,
    pub created_at: String,
}

pub struct CollectionRegistry {
    conn: Mutex<Connection>,
    base_dir: PathBuf,
}

impl CollectionRegistry {
    pub fn open(base_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(base_dir).context("Failed to create registry directory")?;
        let db_path = base_dir.join("collections.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open collections.db at {}", db_path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS collections (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                books_folder TEXT NOT NULL UNIQUE,
                label       TEXT,
                data_dir    TEXT NOT NULL,
                doc_count   INTEGER DEFAULT 0,
                last_indexed TEXT,
                created_at  TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .context("Failed to create collections table")?;
        Ok(Self {
            conn: Mutex::new(conn),
            base_dir: base_dir.to_path_buf(),
        })
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn add_collection(&self, books_folder: &Path) -> Result<i64> {
        let canonical = std::fs::canonicalize(books_folder)
            .with_context(|| format!("Books folder does not exist: {}", books_folder.display()))?;
        let books_str = canonical.to_string_lossy().to_string();

        let conn = self.conn.lock().unwrap();

        if let Some(id) = conn
            .query_row(
                "SELECT id FROM collections WHERE books_folder = ?1",
                [&books_str],
                |row| row.get::<_, i64>(0),
            )
            .ok()
        {
            return Ok(id);
        }

        let next_id: i64 = conn
            .query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM collections", [], |row| {
                row.get(0)
            })
            .unwrap_or(1);

        let data_dir = self.base_dir.join(next_id.to_string());
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("Failed to create data dir: {}", data_dir.display()))?;
        let data_str = data_dir.to_string_lossy().to_string();

        conn.execute(
            "INSERT INTO collections (id, books_folder, label, data_dir) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![next_id, books_str, books_str, data_str],
        )
        .context("Failed to insert collection")?;

        Ok(next_id)
    }

    pub fn remove_collection(&self, id: i64) -> Result<()> {
        // Look up the data_dir before deleting so we can remove the index on disk
        let data_dir: Option<String> = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT data_dir FROM collections WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .ok()
        };

        {
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM collections WHERE id = ?1", [id])
                .context("Failed to delete collection")?;
        }

        // Remove the index data directory on disk
        if let Some(dir) = data_dir {
            let path = Path::new(&dir);
            if path.exists() {
                std::fs::remove_dir_all(path)
                    .with_context(|| format!("Failed to remove data dir: {}", path.display()))?;
            }
        }

        Ok(())
    }

    pub fn list_collections(&self) -> Result<Vec<CollectionInfo>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, books_folder, label, data_dir, doc_count, last_indexed, created_at FROM collections ORDER BY id")
            .context("Failed to prepare list query")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CollectionInfo {
                    id: row.get(0)?,
                    books_folder: row.get(1)?,
                    label: row.get(2)?,
                    data_dir: row.get(3)?,
                    doc_count: row.get(4)?,
                    last_indexed: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .context("Failed to query collections")?;
        let mut collections = Vec::new();
        for row in rows {
            collections.push(row.context("Failed to read collection row")?);
        }
        Ok(collections)
    }

    pub fn get_collection(&self, id: i64) -> Result<CollectionInfo> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, books_folder, label, data_dir, doc_count, last_indexed, created_at FROM collections WHERE id = ?1",
            [id],
            |row| {
                Ok(CollectionInfo {
                    id: row.get(0)?,
                    books_folder: row.get(1)?,
                    label: row.get(2)?,
                    data_dir: row.get(3)?,
                    doc_count: row.get(4)?,
                    last_indexed: row.get(5)?,
                    created_at: row.get(6)?,
                })
            },
        )
        .with_context(|| format!("Collection with id {} not found", id))
    }

    pub fn update_index_metadata(&self, id: i64, doc_count: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE collections SET doc_count = ?1, last_indexed = datetime('now') WHERE id = ?2",
            rusqlite::params![doc_count as i64, id],
        )
        .context("Failed to update collection metadata")?;
        Ok(())
    }

    pub fn data_dir(&self, id: i64) -> PathBuf {
        self.base_dir.join(id.to_string())
    }

    pub fn db_path(&self, id: i64) -> PathBuf {
        self.data_dir(id).join(".pdf_extractor").join("jobs.db")
    }

    pub fn index_path(&self, id: i64) -> PathBuf {
        self.data_dir(id).join(".pdf_extractor").join("index")
    }

    pub fn output_path(&self, id: i64) -> PathBuf {
        self.data_dir(id).join(".pdf_extractor").join("output.jsonl")
    }

    pub fn log_path(&self, id: i64) -> PathBuf {
        self.data_dir(id).join(".pdf_extractor").join("extractor.log")
    }

    pub fn ensure_data_dirs(&self, id: i64) -> Result<()> {
        std::fs::create_dir_all(self.data_dir(id).join(".pdf_extractor"))
            .context("Failed to create .pdf_extractor data dir")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_dir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pdf_registry_test_{}", n));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_create_registry() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();
        assert!(dir.join("collections.db").exists());
        let list = reg.list_collections().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_add_collection() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();
        let books = test_dir();
        std::fs::create_dir_all(&books).unwrap();

        let id = reg.add_collection(&books).unwrap();
        assert_eq!(id, 1);
        assert!(reg.data_dir(id).exists());
        assert!(reg.data_dir(id).join(".pdf_extractor").exists() == false);

        let list = reg.list_collections().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, 1);
        assert_eq!(list[0].doc_count, 0);
    }

    #[test]
    fn test_add_duplicate_collection() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();
        let books = test_dir();
        std::fs::create_dir_all(&books).unwrap();

        let id1 = reg.add_collection(&books).unwrap();
        let id2 = reg.add_collection(&books).unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_remove_collection() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();
        let books = test_dir();
        std::fs::create_dir_all(&books).unwrap();

        let id = reg.add_collection(&books).unwrap();
        assert_eq!(reg.list_collections().unwrap().len(), 1);
        reg.remove_collection(id).unwrap();
        assert_eq!(reg.list_collections().unwrap().len(), 0);
    }

    #[test]
    fn test_multiple_collections() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();

        for i in 0..3 {
            let books = test_dir();
            std::fs::create_dir_all(&books).unwrap();
            reg.add_collection(&books).unwrap();
        }

        assert_eq!(reg.list_collections().unwrap().len(), 3);
        for coll in reg.list_collections().unwrap() {
            assert!(reg.data_dir(coll.id).exists());
        }
    }

    #[test]
    fn test_update_metadata() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();
        let books = test_dir();
        std::fs::create_dir_all(&books).unwrap();

        let id = reg.add_collection(&books).unwrap();
        reg.update_index_metadata(id, 42).unwrap();
        let coll = reg.get_collection(id).unwrap();
        assert_eq!(coll.doc_count, 42);
        assert!(coll.last_indexed.is_some());
    }

    #[test]
    fn test_get_nonexistent_collection() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();
        assert!(reg.get_collection(999).is_err());
    }

    #[test]
    fn test_add_nonexistent_books_folder() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();
        let nonexistent = test_dir();
        assert!(reg.add_collection(&nonexistent).is_err());
    }

    #[test]
    fn test_ensure_data_dirs() {
        let dir = test_dir();
        let reg = CollectionRegistry::open(&dir).unwrap();
        let books = test_dir();
        std::fs::create_dir_all(&books).unwrap();
        let id = reg.add_collection(&books).unwrap();
        reg.ensure_data_dirs(id).unwrap();
        assert!(reg.db_path(id).parent().unwrap().exists());
        assert!(reg.index_path(id).parent().unwrap().exists());
    }
}
