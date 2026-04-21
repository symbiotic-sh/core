//! Persistent store for pending (un-embedded) chunks.
//!
//! When an embedding provider is unavailable during intake, the chunk is saved
//! to `PendingChunkStore` for later retry. The daemon periodically drains this
//! store and re-attempts embedding generation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Sensitivity;

/// Maximum number of retry attempts before a pending chunk is discarded.
pub const MAX_RETRY_COUNT: u32 = 5;

/// Errors that can occur when interacting with the pending chunk store.
#[derive(Debug, Error)]
pub enum PendingStoreError {
    /// Failed to read or write the store file.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to serialize or deserialize the store.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// A chunk that failed to embed and is awaiting retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingChunk {
    /// Source document ID (archive entry short-id).
    pub source_id: String,
    /// Chunk index within the document (0-based).
    pub chunk_index: usize,
    /// The chunk's unique ID (for vector index upsert).
    pub chunk_id: String,
    /// The chunk text content.
    pub chunk_text: String,
    /// Sensitivity level inherited from source document.
    pub sensitivity: Sensitivity,
    /// Unix timestamp when this chunk was first queued.
    pub created_at: u64,
    /// Number of retry attempts so far.
    pub retry_count: u32,
}

/// JSON file-backed store for pending embedding chunks.
///
/// Thread-safety: callers must handle synchronization externally (e.g. via
/// `Mutex<PendingChunkStore>` or by restricting access to a single thread).
#[derive(Debug)]
pub struct PendingChunkStore {
    path: PathBuf,
    chunks: Vec<PendingChunk>,
}

impl PendingChunkStore {
    /// Opens (or creates) a pending chunk store at the given path.
    pub fn open(path: &Path) -> Result<Self, PendingStoreError> {
        let chunks = if path.exists() {
            let data = std::fs::read_to_string(path)?;
            if data.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&data)?
            }
        } else {
            Vec::new()
        };
        Ok(Self {
            path: path.to_path_buf(),
            chunks,
        })
    }

    /// Creates an empty in-memory store (for testing).
    pub fn new_in_memory() -> Self {
        Self {
            path: PathBuf::from("/dev/null"),
            chunks: Vec::new(),
        }
    }

    /// Adds a pending chunk to the store.
    pub fn add(&mut self, chunk: PendingChunk) {
        self.chunks.push(chunk);
    }

    /// Takes up to `n` chunks from the front of the store for retry.
    ///
    /// The returned chunks are removed from the store. Call `save()` after
    /// processing to persist the removal.
    pub fn take_batch(&mut self, n: usize) -> Vec<PendingChunk> {
        let count = n.min(self.chunks.len());
        self.chunks.drain(..count).collect()
    }

    /// Removes a specific pending chunk by source_id and chunk_index.
    ///
    /// Returns `true` if the chunk was found and removed.
    pub fn remove(&mut self, source_id: &str, chunk_index: usize) -> bool {
        let before = self.chunks.len();
        self.chunks
            .retain(|c| !(c.source_id == source_id && c.chunk_index == chunk_index));
        self.chunks.len() < before
    }

    /// Returns the number of pending chunks in the store.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Returns `true` if there are no pending chunks.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Returns a read-only slice of all pending chunks.
    pub fn chunks(&self) -> &[PendingChunk] {
        &self.chunks
    }

    /// Re-inserts chunks that failed retry (incrementing their retry_count).
    ///
    /// Chunks that have exceeded `MAX_RETRY_COUNT` are discarded and their
    /// source_id + chunk_index are returned so the caller can log a warning.
    pub fn requeue_failed(&mut self, mut chunks: Vec<PendingChunk>) -> Vec<(String, usize)> {
        let mut discarded = Vec::new();
        for chunk in chunks.drain(..) {
            let mut updated = chunk;
            updated.retry_count += 1;
            if updated.retry_count > MAX_RETRY_COUNT {
                discarded.push((updated.source_id, updated.chunk_index));
            } else {
                self.chunks.push(updated);
            }
        }
        discarded
    }

    /// Persists the current store state to disk.
    pub fn save(&self) -> Result<(), PendingStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&self.chunks)?;
        std::fs::write(&self.path, data)?;
        Ok(())
    }

    /// Returns the default path for the pending chunks file in a data directory.
    pub fn default_path(data_dir: &Path) -> PathBuf {
        data_dir.join("pending_embeddings.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chunk(source_id: &str, index: usize) -> PendingChunk {
        PendingChunk {
            source_id: source_id.to_string(),
            chunk_index: index,
            chunk_id: format!("{source_id}-chunk-{index}"),
            chunk_text: format!("chunk text {index}"),
            sensitivity: Sensitivity::Shareable,
            created_at: 1000,
            retry_count: 0,
        }
    }

    #[test]
    fn add_and_len() {
        let mut store = PendingChunkStore::new_in_memory();
        assert!(store.is_empty());
        store.add(make_chunk("doc1", 0));
        store.add(make_chunk("doc1", 1));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn take_batch_removes_from_front() {
        let mut store = PendingChunkStore::new_in_memory();
        store.add(make_chunk("doc1", 0));
        store.add(make_chunk("doc1", 1));
        store.add(make_chunk("doc1", 2));

        let batch = store.take_batch(2);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].chunk_index, 0);
        assert_eq!(batch[1].chunk_index, 1);
        assert_eq!(store.len(), 1);
        assert_eq!(store.chunks()[0].chunk_index, 2);
    }

    #[test]
    fn take_batch_more_than_available() {
        let mut store = PendingChunkStore::new_in_memory();
        store.add(make_chunk("doc1", 0));

        let batch = store.take_batch(10);
        assert_eq!(batch.len(), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn take_batch_empty_store() {
        let mut store = PendingChunkStore::new_in_memory();
        let batch = store.take_batch(5);
        assert!(batch.is_empty());
    }

    #[test]
    fn remove_by_source_and_index() {
        let mut store = PendingChunkStore::new_in_memory();
        store.add(make_chunk("doc1", 0));
        store.add(make_chunk("doc1", 1));
        store.add(make_chunk("doc2", 0));

        assert!(store.remove("doc1", 0));
        assert_eq!(store.len(), 2);
        // Removing same one again returns false.
        assert!(!store.remove("doc1", 0));
    }

    #[test]
    fn remove_nonexistent() {
        let mut store = PendingChunkStore::new_in_memory();
        store.add(make_chunk("doc1", 0));
        assert!(!store.remove("doc99", 0));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn requeue_failed_increments_retry_count() {
        let mut store = PendingChunkStore::new_in_memory();
        let chunk = make_chunk("doc1", 0);
        let discarded = store.requeue_failed(vec![chunk]);
        assert!(discarded.is_empty());
        assert_eq!(store.len(), 1);
        assert_eq!(store.chunks()[0].retry_count, 1);
    }

    #[test]
    fn requeue_failed_discards_at_max_retries() {
        let mut store = PendingChunkStore::new_in_memory();
        let mut chunk = make_chunk("doc1", 0);
        chunk.retry_count = MAX_RETRY_COUNT; // already at max
        let discarded = store.requeue_failed(vec![chunk]);
        assert_eq!(discarded.len(), 1);
        assert_eq!(discarded[0], ("doc1".to_string(), 0));
        assert!(store.is_empty());
    }

    #[test]
    fn requeue_failed_mixed() {
        let mut store = PendingChunkStore::new_in_memory();
        let mut expired = make_chunk("doc1", 0);
        expired.retry_count = MAX_RETRY_COUNT;
        let fresh = make_chunk("doc2", 0);

        let discarded = store.requeue_failed(vec![expired, fresh]);
        assert_eq!(discarded.len(), 1);
        assert_eq!(store.len(), 1);
        assert_eq!(store.chunks()[0].source_id, "doc2");
        assert_eq!(store.chunks()[0].retry_count, 1);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("symbiotic_test_pending_store");
        let _ = std::fs::remove_dir_all(&dir);
        let path = PendingChunkStore::default_path(&dir);

        let mut store = PendingChunkStore::open(&path).expect("open empty");
        store.add(make_chunk("doc1", 0));
        store.add(make_chunk("doc1", 1));
        store.save().expect("save");

        let loaded = PendingChunkStore::open(&path).expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.chunks()[0].source_id, "doc1");
        assert_eq!(loaded.chunks()[0].chunk_index, 0);
        assert_eq!(loaded.chunks()[1].chunk_index, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let path = std::env::temp_dir().join("nonexistent_pending_store.json");
        let _ = std::fs::remove_file(&path);

        let store = PendingChunkStore::open(&path).expect("open");
        assert!(store.is_empty());
    }

    #[test]
    fn load_empty_file_returns_empty() {
        let dir = std::env::temp_dir().join("symbiotic_test_pending_empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("pending.json");
        std::fs::write(&path, "").expect("write empty");

        let store = PendingChunkStore::open(&path).expect("open");
        assert!(store.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
