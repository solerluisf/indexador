use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use serde::{Serialize, Deserialize};

/// A single word's bounding box on a PDF page, plus its word offset
/// within the document's text and the extracted word text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredPosition {
    pub word_offset: usize,
    pub page: u32,
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
    pub word_text: String,
}

/// SQLite-backed store for word-level bounding-box positions.
///
/// Instead of one row per word (which creates tens of millions of rows
/// for large collections), all positions for a document are serialised
/// with **bincode**, compressed with **zstd** (level 3), and stored as
/// a single blob row.
///
/// Schema:
/// ```sql
/// CREATE TABLE IF NOT EXISTS doc_positions (
///     doc_id      INTEGER PRIMARY KEY,
///     blob_data   BLOB NOT NULL
/// );
/// ```
pub struct PositionStore {
    conn: Connection,
}

impl PositionStore {
    /// Open (or create) the position DB at `path`.
    /// Any pre-existing `word_positions` table (old row-per-word schema)
    /// is silently dropped on open.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .context("Failed to open position store database")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;

             CREATE TABLE IF NOT EXISTS doc_positions (
                 doc_id      INTEGER PRIMARY KEY,
                 blob_data   BLOB NOT NULL
             );

             DROP TABLE IF EXISTS word_positions;",
        )
        .context("Failed to initialise position store schema")?;

        Ok(Self { conn })
    }

    /// Open an in-memory position store (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .context("Failed to open in-memory position store")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS doc_positions (
                 doc_id      INTEGER PRIMARY KEY,
                 blob_data   BLOB NOT NULL
             );",
        )
        .context("Failed to initialise in-memory schema")?;
        Ok(Self { conn })
    }

    /// Store positions for a document as a single compressed blob.
    /// Replaces any existing entry for the same `doc_id` in one atomic
    /// UPSERT (no separate DELETE + multi-INSERT transaction needed).
    pub fn store_positions(
        &self,
        doc_id: i64,
        positions: &[(usize, crate::extractor::WordPosition)],
    ) -> Result<()> {
        if positions.is_empty() {
            return Ok(());
        }

        let stored: Vec<StoredPosition> = positions
            .iter()
            .map(|(offset, pos)| StoredPosition {
                word_offset: *offset,
                page: pos.page,
                x_min: pos.x_min,
                y_min: pos.y_min,
                x_max: pos.x_max,
                y_max: pos.y_max,
                word_text: pos.text.clone(),
            })
            .collect();

        let encoded = bincode::serialize(&stored).context("Failed to serialise positions")?;

        // Compress with zstd level 3 (fast).  Level 1 is even faster but
        // compresses less; level 3 gives a good speed/size trade-off for
        // this use case (text bounding boxes compress well).
        let compressed = zstd::bulk::compress(&encoded, 3)
            .map_err(|e| anyhow::anyhow!("zstd compression failed: {}", e))?;

        self.conn
            .execute(
                "INSERT OR REPLACE INTO doc_positions (doc_id, blob_data) VALUES (?1, ?2)",
                rusqlite::params![doc_id, compressed],
            )
            .context("Failed to insert position blob")?;

        Ok(())
    }

    /// Get positions for the given `doc_id` matching the specified
    /// `word_offsets`.  Only offsets that exist in the stored data are
    /// returned (missing offsets are silently omitted).
    ///
    /// The blob is decompressed and deserialised once per call.
    /// For typical documents (a few thousand words) this is faster than
    /// the old multi-row SQL approach because it avoids parsing a
    /// dynamically-built `WHERE word_offset IN (...)` query and scanning
    /// an index over millions of rows.
    pub fn get_positions(
        &self,
        doc_id: i64,
        offsets: &[usize],
    ) -> Result<Vec<StoredPosition>> {
        if offsets.is_empty() {
            return Ok(Vec::new());
        }
        let all = self.load_all_for_doc(doc_id)?;
        let offset_set: std::collections::HashSet<&usize> = offsets.iter().collect();
        Ok(all
            .into_iter()
            .filter(|p| offset_set.contains(&p.word_offset))
            .collect())
    }

    /// Get all positions for `doc_id` where `word_text` equals `term`
    /// (case-insensitive).
    ///
    /// Unlike the old schema, this cannot delegate filtering to SQLite;
    /// instead it decompresses the blob and scans in memory.  For a single
    /// document this is fast — the scan is a simple linear pass over a few
    /// thousand entries.
    pub fn get_positions_by_term(
        &self,
        doc_id: i64,
        term: &str,
    ) -> Result<Vec<StoredPosition>> {
        let all = self.load_all_for_doc(doc_id)?;
        let term_lower = term.to_lowercase();
        Ok(all
            .into_iter()
            .filter(|p| p.word_text.to_lowercase() == term_lower)
            .collect())
    }

    /// Delete all positions for a given doc_id.
    pub fn delete_doc(&self, doc_id: i64) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM doc_positions WHERE doc_id = ?1",
                rusqlite::params![doc_id],
            )
            .context("Failed to delete positions for doc")?;
        Ok(())
    }

    /// Count positions for a doc_id (useful for testing)
    pub fn count_positions(&self, doc_id: i64) -> Result<usize> {
        match self.conn.query_row(
            "SELECT blob_data FROM doc_positions WHERE doc_id = ?1",
            rusqlite::params![doc_id],
            |row| row.get::<_, Vec<u8>>(0),
        ) {
            Ok(blob) => {
                let decompressed = zstd::bulk::decompress(&blob, 1024 * 1024 * 4)
                    .map_err(|e| anyhow::anyhow!("zstd decompression failed: {}", e))?;
                let positions: Vec<StoredPosition> =
                    bincode::deserialize(&decompressed)
                        .context("Failed to deserialise positions")?;
                Ok(positions.len())
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(e) => Err(e).context("Failed to count positions"),
        }
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Load, decompress and deserialise the full position list for a doc.
    fn load_all_for_doc(&self, doc_id: i64) -> Result<Vec<StoredPosition>> {
        let blob: Vec<u8> = self
            .conn
            .query_row(
                "SELECT blob_data FROM doc_positions WHERE doc_id = ?1",
                rusqlite::params![doc_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    anyhow::anyhow!("No positions found for doc_id {}", doc_id)
                }
                other => anyhow::anyhow!("Failed to query positions: {}", other),
            })?;

        let decompressed = zstd::bulk::decompress(&blob, 1024 * 1024 * 8)
            .map_err(|e| anyhow::anyhow!("zstd decompression failed: {}", e))?;

        bincode::deserialize(&decompressed)
            .context("Failed to deserialise position blob")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_positions() -> Vec<(usize, crate::extractor::WordPosition)> {
        vec![
            (
                0,
                crate::extractor::WordPosition {
                    page: 1,
                    x_min: 10.0,
                    y_min: 20.0,
                    x_max: 50.0,
                    y_max: 30.0,
                    text: "hello".into(),
                },
            ),
            (
                2,
                crate::extractor::WordPosition {
                    page: 1,
                    x_min: 60.0,
                    y_min: 20.0,
                    x_max: 100.0,
                    y_max: 30.0,
                    text: "world".into(),
                },
            ),
            (
                5,
                crate::extractor::WordPosition {
                    page: 2,
                    x_min: 10.0,
                    y_min: 40.0,
                    x_max: 50.0,
                    y_max: 50.0,
                    text: "Hello".into(),
                },
            ),
        ]
    }

    #[test]
    fn test_store_and_retrieve() {
        let store = PositionStore::open_in_memory().unwrap();
        store.store_positions(1, &sample_positions()).unwrap();

        let all = store.load_all_for_doc(1).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].word_text, "hello");
    }

    #[test]
    fn test_get_positions_by_offset() {
        let store = PositionStore::open_in_memory().unwrap();
        store.store_positions(1, &sample_positions()).unwrap();

        let results = store.get_positions(1, &[0, 5]).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].word_text, "hello");
        assert_eq!(results[1].word_text, "Hello");
    }

    #[test]
    fn test_get_positions_by_term() {
        let store = PositionStore::open_in_memory().unwrap();
        store.store_positions(1, &sample_positions()).unwrap();

        // Case-insensitive search for "hello" should match both "hello" and "Hello"
        let results = store.get_positions_by_term(1, "hello").unwrap();
        assert_eq!(results.len(), 2);

        // Exact match
        let results = store.get_positions_by_term(1, "world").unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_get_positions_empty() {
        let store = PositionStore::open_in_memory().unwrap();
        store.store_positions(1, &sample_positions()).unwrap();

        let results = store.get_positions(1, &[]).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_delete_doc() {
        let store = PositionStore::open_in_memory().unwrap();
        store.store_positions(1, &sample_positions()).unwrap();
        assert_eq!(store.count_positions(1).unwrap(), 3);

        store.delete_doc(1).unwrap();
        assert_eq!(store.count_positions(1).unwrap(), 0);
    }

    #[test]
    fn test_nonexistent_doc() {
        let store = PositionStore::open_in_memory().unwrap();
        let results = store.get_positions(999, &[0]).unwrap_err();
        assert!(results.to_string().contains("No positions found"));

        assert_eq!(store.count_positions(999).unwrap(), 0);
    }
}
