//! SQLite-backed vector index using `sqlite-vec` for ANN search.
//!
//! Replaces the brute-force JSON vector index with a `vec0` virtual table
//! backed by `sqlite-vec`. Embeddings are stored as raw float bytes and
//! queried via cosine distance KNN.

use std::path::{Path, PathBuf};
use std::sync::Once;

use anyhow::{anyhow, Result};
use rusqlite::Connection;

use crate::Sensitivity;

/// Default embedding dimensions (nomic-embed-text).
pub const DEFAULT_EMBEDDING_DIM: usize = 768;

/// Register the sqlite-vec extension globally (once per process).
///
/// Must be called before opening any connection that needs vec0 tables.
/// Safe to call multiple times — only the first call has effect.
fn ensure_sqlite_vec_loaded() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(
            #[allow(clippy::missing_transmute_annotations)]
            std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ()),
        ));
    });
}

/// Convert a `Sensitivity` to an integer for SQLite storage.
///
/// Ordering matches the `Ord` impl: Shareable(0) < Restricted(1) < Private(2).
fn sensitivity_to_i32(s: Sensitivity) -> i32 {
    match s {
        Sensitivity::Shareable => 0,
        Sensitivity::Restricted => 1,
        Sensitivity::Private => 2,
    }
}

/// Reinterpret a `&[f32]` slice as raw bytes for sqlite-vec.
///
/// sqlite-vec expects embedding vectors as contiguous little-endian f32 bytes.
/// This is a zero-copy view on native-endian platforms (all supported targets).
fn f32_slice_as_bytes(v: &[f32]) -> &[u8] {
    // SAFETY: f32 is `repr(C)`, contiguous in memory. The lifetime of the
    // returned slice is tied to the input slice.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

/// A search result from the vector index.
#[derive(Debug, Clone)]
pub struct VectorSearchResult {
    pub entry_id: String,
    pub similarity: f32,
}

/// SQLite-backed vector index for cosine similarity search.
///
/// Stores embeddings in a `vec0` virtual table with a companion metadata
/// table for entry IDs and sensitivity levels. All writes are immediately
/// persisted — no explicit save step needed.
pub struct VectorIndex {
    conn: Connection,
    dimensions: usize,
}

impl std::fmt::Debug for VectorIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorIndex")
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}

impl VectorIndex {
    /// Open (or create) a vector index backed by a SQLite file.
    pub fn open(path: &Path, dimensions: usize) -> Result<Self> {
        ensure_sqlite_vec_loaded();
        let conn = Connection::open(path)
            .map_err(|e| anyhow!("failed to open vector index at {}: {e}", path.display()))?;
        let index = Self { conn, dimensions };
        index.initialize()?;
        Ok(index)
    }

    /// Create an in-memory vector index (useful for testing).
    pub fn open_in_memory(dimensions: usize) -> Result<Self> {
        ensure_sqlite_vec_loaded();
        let conn = Connection::open_in_memory()
            .map_err(|e| anyhow!("failed to open in-memory vector index: {e}"))?;
        let index = Self { conn, dimensions };
        index.initialize()?;
        Ok(index)
    }

    /// Initialize the schema: metadata table + vec0 virtual table.
    fn initialize(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vec_entry_map (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id TEXT NOT NULL UNIQUE,
                sensitivity INTEGER NOT NULL DEFAULT 2
            );",
        )?;

        let create_vec = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_entries USING vec0(
                embedding float[{dim}] distance_metric=cosine
            );",
            dim = self.dimensions
        );
        self.conn.execute_batch(&create_vec)?;

        Ok(())
    }

    /// Inserts or updates an embedding for an entry.
    pub fn upsert(&self, entry_id: &str, embedding: &[f32], sensitivity: Sensitivity) {
        if let Err(e) = self.upsert_inner(entry_id, embedding, sensitivity) {
            tracing::warn!(
                error = %e,
                entry_id,
                "vector_index: failed to upsert embedding"
            );
        }
    }

    fn upsert_inner(
        &self,
        entry_id: &str,
        embedding: &[f32],
        sensitivity: Sensitivity,
    ) -> Result<()> {
        if embedding.len() != self.dimensions {
            return Err(anyhow!(
                "embedding dimension mismatch: expected {}, got {}",
                self.dimensions,
                embedding.len()
            ));
        }

        let sens_int = sensitivity_to_i32(sensitivity);
        let bytes = f32_slice_as_bytes(embedding);

        // Check if entry already exists.
        let existing_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM vec_entry_map WHERE entry_id = ?1",
                [entry_id],
                |row| row.get(0),
            )
            .ok();

        if let Some(row_id) = existing_id {
            // Update existing: replace embedding and sensitivity.
            self.conn.execute(
                "UPDATE vec_entry_map SET sensitivity = ?1 WHERE id = ?2",
                rusqlite::params![sens_int, row_id],
            )?;
            // vec0 uses DELETE + INSERT for updates (no UPDATE support).
            self.conn
                .execute("DELETE FROM vec_entries WHERE rowid = ?1", [row_id])?;
            self.conn.execute(
                "INSERT INTO vec_entries(rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![row_id, bytes],
            )?;
        } else {
            // Insert new entry into metadata table.
            self.conn.execute(
                "INSERT INTO vec_entry_map(entry_id, sensitivity) VALUES (?1, ?2)",
                rusqlite::params![entry_id, sens_int],
            )?;
            let row_id = self.conn.last_insert_rowid();
            self.conn.execute(
                "INSERT INTO vec_entries(rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![row_id, bytes],
            )?;
        }

        Ok(())
    }

    /// Searches the index using cosine similarity against the query embedding.
    ///
    /// Returns results sorted by descending similarity, filtered by sensitivity.
    /// Only entries with sensitivity <= `max_sensitivity` are included.
    pub fn search(
        &self,
        query_embedding: &[f32],
        max_sensitivity: Sensitivity,
        top_k: usize,
    ) -> Vec<VectorSearchResult> {
        self.search_inner(query_embedding, max_sensitivity, top_k)
            .unwrap_or_default()
    }

    fn search_inner(
        &self,
        query_embedding: &[f32],
        max_sensitivity: Sensitivity,
        top_k: usize,
    ) -> Result<Vec<VectorSearchResult>> {
        if query_embedding.len() != self.dimensions {
            return Err(anyhow!(
                "query dimension mismatch: expected {}, got {}",
                self.dimensions,
                query_embedding.len()
            ));
        }

        let max_sens_int = sensitivity_to_i32(max_sensitivity);
        let bytes = f32_slice_as_bytes(query_embedding);

        // Over-fetch from KNN to account for sensitivity filtering.
        let fetch_k = (top_k * 3).max(20) as i64;

        let mut stmt = self.conn.prepare(
            "SELECT m.entry_id, v.distance, m.sensitivity
             FROM (
                 SELECT rowid, distance
                 FROM vec_entries
                 WHERE embedding MATCH ?1 AND k = ?2
                 ORDER BY distance
             ) v
             JOIN vec_entry_map m ON m.id = v.rowid
             WHERE m.sensitivity <= ?3
             ORDER BY v.distance",
        )?;

        let results: Vec<VectorSearchResult> = stmt
            .query_map(rusqlite::params![bytes, fetch_k, max_sens_int], |row| {
                let entry_id: String = row.get(0)?;
                let distance: f64 = row.get(1)?;
                // Cosine distance → cosine similarity: similarity = 1 - distance.
                let similarity = (1.0 - distance) as f32;
                Ok(VectorSearchResult {
                    entry_id,
                    similarity,
                })
            })?
            .filter_map(|r| r.ok())
            .filter(|r| r.similarity > 0.0)
            .take(top_k)
            .collect();

        Ok(results)
    }

    /// Returns the number of entries in the index.
    pub fn len(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM vec_entry_map", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or(0) as usize
    }

    /// Returns true if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the default path for the vector index database in a data directory.
    pub fn default_path(data_dir: &Path) -> PathBuf {
        data_dir.join("vector_index.db")
    }

    /// Returns the embedding dimensions this index was created with.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_index() -> VectorIndex {
        VectorIndex::open_in_memory(3).expect("open in-memory index")
    }

    #[test]
    fn upsert_and_search() {
        let index = test_index();
        index.upsert("e1", &[1.0, 0.0, 0.0], Sensitivity::Shareable);
        index.upsert("e2", &[0.0, 1.0, 0.0], Sensitivity::Shareable);
        index.upsert("e3", &[0.9, 0.1, 0.0], Sensitivity::Shareable);

        let results = index.search(&[1.0, 0.0, 0.0], Sensitivity::Private, 10);
        assert!(!results.is_empty());
        // e1 should have highest similarity (identical vector)
        assert_eq!(results[0].entry_id, "e1");
    }

    #[test]
    fn upsert_replaces_existing() {
        let index = test_index();
        index.upsert("e1", &[1.0, 0.0, 0.0], Sensitivity::Shareable);
        assert_eq!(index.len(), 1);

        index.upsert("e1", &[0.0, 1.0, 0.0], Sensitivity::Restricted);
        assert_eq!(index.len(), 1);

        // Search should now match the updated embedding
        let results = index.search(&[0.0, 1.0, 0.0], Sensitivity::Restricted, 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].entry_id, "e1");
    }

    #[test]
    fn filters_by_sensitivity() {
        let index = test_index();
        index.upsert("public", &[1.0, 0.0, 0.0], Sensitivity::Shareable);
        index.upsert("private", &[1.0, 0.0, 0.0], Sensitivity::Private);

        let results = index.search(&[1.0, 0.0, 0.0], Sensitivity::Shareable, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_id, "public");

        let results = index.search(&[1.0, 0.0, 0.0], Sensitivity::Private, 10);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn respects_top_k() {
        let index = test_index();
        for i in 0..10 {
            let emb = [1.0 - (i as f32 * 0.05), i as f32 * 0.05, 0.0];
            index.upsert(&format!("e{i}"), &emb, Sensitivity::Shareable);
        }

        let results = index.search(&[1.0, 0.0, 0.0], Sensitivity::Private, 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn empty_index_returns_empty_results() {
        let index = test_index();
        assert!(index.is_empty());

        let results = index.search(&[1.0, 0.0, 0.0], Sensitivity::Private, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn len_and_is_empty() {
        let index = test_index();
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);

        index.upsert("e1", &[1.0, 0.0, 0.0], Sensitivity::Shareable);
        assert!(!index.is_empty());
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn dimension_mismatch_is_handled() {
        let index = test_index(); // 3 dimensions
                                  // Upsert with wrong dimensions — should log warning, not panic
        index.upsert("bad", &[1.0, 0.0], Sensitivity::Shareable);
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn file_backed_persistence() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("test_vec.db");

        // Write
        {
            let index = VectorIndex::open(&path, 3).expect("open");
            index.upsert("e1", &[1.0, 2.0, 3.0], Sensitivity::Restricted);
            assert_eq!(index.len(), 1);
        }

        // Re-open and verify
        {
            let index = VectorIndex::open(&path, 3).expect("reopen");
            assert_eq!(index.len(), 1);

            let results = index.search(&[1.0, 2.0, 3.0], Sensitivity::Private, 10);
            assert!(!results.is_empty());
            assert_eq!(results[0].entry_id, "e1");
        }
    }
}
