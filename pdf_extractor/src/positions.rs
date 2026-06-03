use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

/// A single word's bounding box on a PDF page, plus its word offset
/// within the document's text and the extracted word text.
#[derive(Debug, Clone, PartialEq)]
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
/// Schema:
/// ```sql
/// CREATE TABLE IF NOT EXISTS word_positions (
///     doc_id       INTEGER NOT NULL,
///     word_offset  INTEGER NOT NULL,
///     page         INTEGER NOT NULL,
///     x_min        REAL NOT NULL,
///     y_min        REAL NOT NULL,
///     x_max        REAL NOT NULL,
///     y_max        REAL NOT NULL,
///     PRIMARY KEY (doc_id, word_offset)
/// );
/// ```
pub struct PositionStore {
    conn: Connection,
}

impl PositionStore {
    /// Open (or create) the position DB at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .context("Failed to open position store database")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS word_positions (
                doc_id       INTEGER NOT NULL,
                word_offset  INTEGER NOT NULL,
                page         INTEGER NOT NULL,
                x_min        REAL NOT NULL,
                y_min        REAL NOT NULL,
                x_max        REAL NOT NULL,
                y_max        REAL NOT NULL,
                word_text    TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (doc_id, word_offset)
            );
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;",
        ).context("Failed to create word_positions table")?;

        // Add word_text column if it doesn't exist (schema migration)
        let _ = conn.execute_batch("ALTER TABLE word_positions ADD COLUMN word_text TEXT NOT NULL DEFAULT ''");

        Ok(Self { conn })
    }

    /// Open an in-memory position store (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .context("Failed to open in-memory position store")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS word_positions (
                doc_id       INTEGER NOT NULL,
                word_offset  INTEGER NOT NULL,
                page         INTEGER NOT NULL,
                x_min        REAL NOT NULL,
                y_min        REAL NOT NULL,
                x_max        REAL NOT NULL,
                y_max        REAL NOT NULL,
                word_text    TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (doc_id, word_offset)
            );",
        ).context("Failed to create word_positions table")?;
        Ok(Self { conn })
    }

    /// Insert positions for a document. Replaces any existing entries
    /// for the given `doc_id`.
    pub fn store_positions(
        &self,
        doc_id: i64,
        positions: &[(usize, crate::extractor::WordPosition)],
    ) -> Result<()> {
        // Delete any existing positions for this doc (dedup / re-index)
        self.conn
            .execute("DELETE FROM word_positions WHERE doc_id = ?1", rusqlite::params![doc_id])
            .context("Failed to delete existing positions")?;

        let mut stmt = self.conn.prepare(
            "INSERT INTO word_positions (doc_id, word_offset, page, x_min, y_min, x_max, y_max, word_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        ).context("Failed to prepare insert statement")?;

        for (offset, pos) in positions {
            stmt.execute(rusqlite::params![
                doc_id,
                *offset as i64,
                pos.page as i64,
                pos.x_min as f64,
                pos.y_min as f64,
                pos.x_max as f64,
                pos.y_max as f64,
                pos.text,
            ]).context("Failed to insert position")?;
        }

        drop(stmt);
        Ok(())
    }

    /// Get all positions for the given `doc_id` and `word_offsets`.
    /// Returns the matching positions in the order of `offsets`.
    /// Missing offsets are silently omitted.
    pub fn get_positions(
        &self,
        doc_id: i64,
        offsets: &[usize],
    ) -> Result<Vec<StoredPosition>> {
        if offsets.is_empty() {
            return Ok(Vec::new());
        }

        // Build a parameterised IN clause
        let placeholders: Vec<String> = offsets.iter().enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect();
        let sql = format!(
            "SELECT word_offset, page, x_min, y_min, x_max, y_max, word_text
             FROM word_positions
             WHERE doc_id = ?1 AND word_offset IN ({})
             ORDER BY word_offset",
            placeholders.join(",")
        );

        let mut stmt = self.conn
            .prepare(&sql)
            .context("Failed to prepare get_positions query")?;

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::with_capacity(1 + offsets.len());
        params.push(Box::new(doc_id));
        for o in offsets {
            params.push(Box::new(*o as i64));
        }
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let results = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(StoredPosition {
                word_offset: row.get::<_, i64>(0)? as usize,
                page: row.get::<_, i64>(1)? as u32,
                x_min: row.get::<_, f64>(2)? as f32,
                y_min: row.get::<_, f64>(3)? as f32,
                x_max: row.get::<_, f64>(4)? as f32,
                y_max: row.get::<_, f64>(5)? as f32,
                word_text: row.get::<_, String>(6)?,
            })
        }).context("Failed to query positions")?;

        let mut positions: Vec<StoredPosition> = Vec::new();
        for r in results {
            positions.push(r.context("Failed to read position row")?);
        }
        Ok(positions)
    }

    /// Get all positions for the given `doc_id` where the word text
    /// matches `term` (case-insensitive). Uses `word_text` column.
    /// This avoids the offset-alignment issue between Tantivy's tokenizer
    /// and the word-position extractor.
    pub fn get_positions_by_term(
        &self,
        doc_id: i64,
        term: &str,
    ) -> Result<Vec<StoredPosition>> {
        let mut stmt = self.conn
            .prepare(
                "SELECT word_offset, page, x_min, y_min, x_max, y_max, word_text
                 FROM word_positions
                 WHERE doc_id = ?1 AND LOWER(word_text) LIKE '%' || LOWER(?2) || '%'
                 ORDER BY page, word_offset"
            )
            .context("Failed to prepare get_positions_by_term query")?;

        let results = stmt.query_map(rusqlite::params![doc_id, term], |row| {
            Ok(StoredPosition {
                word_offset: row.get::<_, i64>(0)? as usize,
                page: row.get::<_, i64>(1)? as u32,
                x_min: row.get::<_, f64>(2)? as f32,
                y_min: row.get::<_, f64>(3)? as f32,
                x_max: row.get::<_, f64>(4)? as f32,
                y_max: row.get::<_, f64>(5)? as f32,
                word_text: row.get::<_, String>(6)?,
            })
        }).context("Failed to query positions by term")?;

        let mut positions: Vec<StoredPosition> = Vec::new();
        for r in results {
            positions.push(r.context("Failed to read position row")?);
        }
        Ok(positions)
    }

    /// Delete all positions for a given doc_id.
    pub fn delete_doc(&self, doc_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM word_positions WHERE doc_id = ?1", rusqlite::params![doc_id])
            .context("Failed to delete positions for doc")?;
        Ok(())
    }

    /// Count positions for a doc_id (useful for testing)
    pub fn count_positions(&self, doc_id: i64) -> Result<usize> {
        let count: i64 = self.conn
            .query_row(
                "SELECT COUNT(*) FROM word_positions WHERE doc_id = ?1",
                rusqlite::params![doc_id],
                |row| row.get(0),
            )
            .context("Failed to count positions")?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::WordPosition;

    #[test]
    fn test_store_and_retrieve_positions() {
        let store = PositionStore::open_in_memory().unwrap();
        let positions = vec![
            (0usize, WordPosition { page: 1, x_min: 100.0, y_min: 700.0, x_max: 120.0, y_max: 712.0, text: String::new() }),
            (1usize, WordPosition { page: 1, x_min: 130.0, y_min: 700.0, x_max: 155.0, y_max: 712.0, text: String::new() }),
            (2usize, WordPosition { page: 2, x_min: 50.0, y_min: 600.0, x_max: 80.0, y_max: 612.0, text: String::new() }),
        ];

        store.store_positions(42, &positions).unwrap();
        assert_eq!(store.count_positions(42).unwrap(), 3);

        let retrieved = store.get_positions(42, &[0, 1, 2]).unwrap();
        assert_eq!(retrieved.len(), 3);
        assert_eq!(retrieved[0].word_offset, 0);
        assert_eq!(retrieved[0].page, 1);
        assert_eq!(retrieved[0].x_min, 100.0);
        assert_eq!(retrieved[1].word_offset, 1);
        assert_eq!(retrieved[1].page, 1);
        assert_eq!(retrieved[2].word_offset, 2);
        assert_eq!(retrieved[2].page, 2);
    }

    #[test]
    fn test_store_replaces_existing() {
        let store = PositionStore::open_in_memory().unwrap();
        let positions1 = vec![
            (0usize, WordPosition { page: 1, x_min: 100.0, y_min: 700.0, x_max: 120.0, y_max: 712.0, text: String::new() }),
        ];
        let positions2 = vec![
            (0usize, WordPosition { page: 1, x_min: 200.0, y_min: 600.0, x_max: 220.0, y_max: 612.0, text: String::new() }),
        ];

        store.store_positions(1, &positions1).unwrap();
        assert_eq!(store.count_positions(1).unwrap(), 1);

        // Re-store with same doc_id should replace
        store.store_positions(1, &positions2).unwrap();
        assert_eq!(store.count_positions(1).unwrap(), 1);
        let retrieved = store.get_positions(1, &[0]).unwrap();
        assert_eq!(retrieved[0].x_min, 200.0);
    }

    #[test]
    fn test_get_positions_with_subset_of_offsets() {
        let store = PositionStore::open_in_memory().unwrap();
        let positions = vec![
            (0usize, WordPosition { page: 1, x_min: 10.0, y_min: 700.0, x_max: 30.0, y_max: 712.0, text: String::new() }),
            (1usize, WordPosition { page: 1, x_min: 40.0, y_min: 700.0, x_max: 60.0, y_max: 712.0, text: String::new() }),
            (2usize, WordPosition { page: 1, x_min: 70.0, y_min: 700.0, x_max: 90.0, y_max: 712.0, text: String::new() }),
        ];

        store.store_positions(1, &positions).unwrap();

        let retrieved = store.get_positions(1, &[0, 2]).unwrap();
        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0].word_offset, 0);
        assert_eq!(retrieved[1].word_offset, 2);
    }

    #[test]
    fn test_get_positions_empty_offsets() {
        let store = PositionStore::open_in_memory().unwrap();
        let retrieved = store.get_positions(1, &[]).unwrap();
        assert!(retrieved.is_empty());
    }

    #[test]
    fn test_get_positions_missing_offset_omitted() {
        let store = PositionStore::open_in_memory().unwrap();
        let positions = vec![
            (0usize, WordPosition { page: 1, x_min: 10.0, y_min: 700.0, x_max: 30.0, y_max: 712.0, text: String::new() }),
        ];
        store.store_positions(1, &positions).unwrap();

        // Offset 5 doesn't exist; should return 1 result for offset 0
        let retrieved = store.get_positions(1, &[0, 5]).unwrap();
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].word_offset, 0);
    }

    #[test]
    fn test_delete_doc_removes_all() {
        let store = PositionStore::open_in_memory().unwrap();
        let positions = vec![
            (0usize, WordPosition { page: 1, x_min: 10.0, y_min: 700.0, x_max: 30.0, y_max: 712.0, text: String::new() }),
        ];
        store.store_positions(1, &positions).unwrap();
        assert_eq!(store.count_positions(1).unwrap(), 1);

        store.delete_doc(1).unwrap();
        assert_eq!(store.count_positions(1).unwrap(), 0);
    }

    #[test]
    fn test_multiple_docs_independent() {
        let store = PositionStore::open_in_memory().unwrap();
        let pos_a = vec![(0usize, WordPosition { page: 1, x_min: 10.0, y_min: 700.0, x_max: 30.0, y_max: 712.0, text: String::new() })];
        let pos_b = vec![(0usize, WordPosition { page: 2, x_min: 50.0, y_min: 600.0, x_max: 70.0, y_max: 612.0, text: String::new() })];

        store.store_positions(1, &pos_a).unwrap();
        store.store_positions(2, &pos_b).unwrap();

        assert_eq!(store.get_positions(1, &[0]).unwrap()[0].page, 1);
        assert_eq!(store.get_positions(2, &[0]).unwrap()[0].page, 2);
    }

    #[test]
    fn test_count_zero_for_new_doc() {
        let store = PositionStore::open_in_memory().unwrap();
        assert_eq!(store.count_positions(999).unwrap(), 0);
    }
}
