//! Content hash tracking for deduplication in the distillery pipeline.
//!
//! Computes SHA-256 content hashes at intake to prevent reprocessing identical
//! content. Tracks hash-to-record mappings and detects near-duplicates
//! (e.g., same URL fetched at different times with minor changes).
//! See `docs/design/distillery-pipeline.md` §Content Hash Tracking.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Hash computation
// ---------------------------------------------------------------------------

/// Compute the SHA-256 hash of content, returning it as a hex string.
pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Compute a normalized hash that strips whitespace variations for
/// near-duplicate detection. This catches cases like the same URL fetched
/// at different times with minor formatting changes.
pub fn normalized_hash(content: &str) -> String {
    let normalized: String = content
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .to_lowercase();
    content_hash(&normalized)
}

// ---------------------------------------------------------------------------
// Dedup result
// ---------------------------------------------------------------------------

/// Outcome of a deduplication check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DedupResult {
    /// Content is new — proceed with pipeline processing.
    New,
    /// Exact duplicate of a previously processed record.
    ExactDuplicate {
        /// The record ID of the previously processed content.
        original_record_id: String,
    },
    /// Near-duplicate (same normalized content, different raw content).
    /// The pipeline should proceed but flag the result.
    NearDuplicate {
        /// The record ID of the similar previously processed content.
        original_record_id: String,
        /// The exact hash of the original content.
        original_hash: String,
    },
}

impl DedupResult {
    /// Returns true if the content should be skipped (exact duplicate).
    pub fn should_skip(&self) -> bool {
        matches!(self, DedupResult::ExactDuplicate { .. })
    }

    /// Returns true if the content is new or a near-duplicate that should
    /// still be processed.
    pub fn should_process(&self) -> bool {
        !self.should_skip()
    }
}

// ---------------------------------------------------------------------------
// Hash record
// ---------------------------------------------------------------------------

/// Metadata stored alongside a content hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashRecord {
    /// The record ID associated with this content.
    pub record_id: String,
    /// The exact SHA-256 hash.
    pub exact_hash: String,
    /// The normalized hash for near-duplicate detection.
    pub normalized_hash: String,
    /// Source URL that produced this content.
    pub source_url: String,
    /// When this hash was first seen (RFC 3339 timestamp).
    pub first_seen: String,
}

// ---------------------------------------------------------------------------
// Content hash store (in-memory, thread-safe)
// ---------------------------------------------------------------------------

/// In-memory content hash store for deduplication.
///
/// Thread-safe via `RwLock`. In production, this would be backed by a
/// persistent store (SQLite). The in-memory version is suitable for
/// single-pipeline runs and testing.
#[derive(Debug, Clone)]
pub struct ContentHashStore {
    /// Map from exact hash -> record metadata.
    exact_hashes: Arc<RwLock<HashMap<String, HashRecord>>>,
    /// Map from normalized hash -> record metadata.
    normalized_hashes: Arc<RwLock<HashMap<String, HashRecord>>>,
}

impl ContentHashStore {
    /// Create a new empty hash store.
    pub fn new() -> Self {
        Self {
            exact_hashes: Arc::new(RwLock::new(HashMap::new())),
            normalized_hashes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check if content has been seen before.
    ///
    /// Returns `DedupResult::ExactDuplicate` if the exact hash matches,
    /// `DedupResult::NearDuplicate` if only the normalized hash matches,
    /// or `DedupResult::New` if neither matches.
    pub fn check(&self, content: &str) -> DedupResult {
        let exact = content_hash(content);
        let normalized = normalized_hash(content);

        // Check exact match first
        if let Ok(hashes) = self.exact_hashes.read() {
            if let Some(record) = hashes.get(&exact) {
                return DedupResult::ExactDuplicate {
                    original_record_id: record.record_id.clone(),
                };
            }
        }

        // Check normalized match
        if let Ok(hashes) = self.normalized_hashes.read() {
            if let Some(record) = hashes.get(&normalized) {
                return DedupResult::NearDuplicate {
                    original_record_id: record.record_id.clone(),
                    original_hash: record.exact_hash.clone(),
                };
            }
        }

        DedupResult::New
    }

    /// Record a content hash after successful processing.
    ///
    /// Should be called after the pipeline completes successfully to register
    /// this content as "seen."
    pub fn record(
        &self,
        content: &str,
        record_id: &str,
        source_url: &str,
    ) -> Result<(), DedupError> {
        let exact = content_hash(content);
        let normalized = normalized_hash(content);
        let now = chrono::Utc::now().to_rfc3339();

        let record = HashRecord {
            record_id: record_id.to_string(),
            exact_hash: exact.clone(),
            normalized_hash: normalized.clone(),
            source_url: source_url.to_string(),
            first_seen: now,
        };

        self.exact_hashes
            .write()
            .map_err(|_| DedupError::LockPoisoned)?
            .insert(exact, record.clone());

        self.normalized_hashes
            .write()
            .map_err(|_| DedupError::LockPoisoned)?
            .insert(normalized, record);

        Ok(())
    }

    /// Remove a hash record (e.g., on rollback).
    pub fn remove(&self, content: &str) -> Result<(), DedupError> {
        let exact = content_hash(content);
        let normalized = normalized_hash(content);

        self.exact_hashes
            .write()
            .map_err(|_| DedupError::LockPoisoned)?
            .remove(&exact);

        self.normalized_hashes
            .write()
            .map_err(|_| DedupError::LockPoisoned)?
            .remove(&normalized);

        Ok(())
    }

    /// Returns the number of tracked hashes (exact).
    pub fn len(&self) -> usize {
        self.exact_hashes.read().map(|h| h.len()).unwrap_or(0)
    }

    /// Returns true if no hashes are tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up a record by its exact content hash.
    pub fn get_by_hash(&self, hash: &str) -> Option<HashRecord> {
        self.exact_hashes
            .read()
            .ok()
            .and_then(|hashes| hashes.get(hash).cloned())
    }
}

impl Default for ContentHashStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the deduplication system.
#[derive(Debug, thiserror::Error)]
pub enum DedupError {
    #[error("internal lock poisoned")]
    LockPoisoned,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 produces 64 hex chars
    }

    #[test]
    fn content_hash_differs_for_different_content() {
        let h1 = content_hash("hello");
        let h2 = content_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn normalized_hash_ignores_whitespace() {
        let h1 = normalized_hash("hello   world");
        let h2 = normalized_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn normalized_hash_ignores_case() {
        let h1 = normalized_hash("Hello World");
        let h2 = normalized_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn normalized_hash_ignores_newlines() {
        let h1 = normalized_hash("hello\n  world\n");
        let h2 = normalized_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn store_check_returns_new_for_empty() {
        let store = ContentHashStore::new();
        assert_eq!(store.check("some content"), DedupResult::New);
    }

    #[test]
    fn store_detects_exact_duplicate() {
        let store = ContentHashStore::new();
        store
            .record("same content", "rec-1", "https://example.com")
            .expect("record");

        let result = store.check("same content");
        assert!(result.should_skip());
        match result {
            DedupResult::ExactDuplicate { original_record_id } => {
                assert_eq!(original_record_id, "rec-1");
            }
            _ => panic!("expected ExactDuplicate"),
        }
    }

    #[test]
    fn store_detects_near_duplicate() {
        let store = ContentHashStore::new();
        store
            .record("Hello  World", "rec-1", "https://example.com")
            .expect("record");

        // Same normalized content but different raw content
        let result = store.check("hello world");
        match result {
            DedupResult::NearDuplicate {
                original_record_id, ..
            } => {
                assert_eq!(original_record_id, "rec-1");
            }
            _ => panic!("expected NearDuplicate, got {:?}", result),
        }
    }

    #[test]
    fn near_duplicate_should_process() {
        let result = DedupResult::NearDuplicate {
            original_record_id: "rec-1".to_string(),
            original_hash: "abc".to_string(),
        };
        assert!(result.should_process());
        assert!(!result.should_skip());
    }

    #[test]
    fn exact_duplicate_should_skip() {
        let result = DedupResult::ExactDuplicate {
            original_record_id: "rec-1".to_string(),
        };
        assert!(result.should_skip());
        assert!(!result.should_process());
    }

    #[test]
    fn new_should_process() {
        assert!(DedupResult::New.should_process());
        assert!(!DedupResult::New.should_skip());
    }

    #[test]
    fn store_remove_works() {
        let store = ContentHashStore::new();
        store
            .record("content to remove", "rec-1", "https://example.com")
            .expect("record");
        assert_eq!(store.len(), 1);

        store.remove("content to remove").expect("remove");
        assert_eq!(store.len(), 0);
        assert_eq!(store.check("content to remove"), DedupResult::New);
    }

    #[test]
    fn store_len_and_is_empty() {
        let store = ContentHashStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        store.record("a", "rec-1", "url1").expect("record");
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);

        store.record("b", "rec-2", "url2").expect("record");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn store_get_by_hash() {
        let store = ContentHashStore::new();
        store
            .record("test content", "rec-1", "https://example.com")
            .expect("record");

        let hash = content_hash("test content");
        let record = store.get_by_hash(&hash).expect("should find record");
        assert_eq!(record.record_id, "rec-1");
        assert_eq!(record.source_url, "https://example.com");
    }

    #[test]
    fn store_get_by_hash_returns_none_for_unknown() {
        let store = ContentHashStore::new();
        assert!(store.get_by_hash("nonexistent").is_none());
    }

    #[test]
    fn store_is_thread_safe() {
        let store = ContentHashStore::new();
        let store2 = store.clone();

        // Simulate concurrent access
        store.record("content-a", "rec-1", "url-a").expect("record");
        let result = store2.check("content-a");
        assert!(result.should_skip());
    }

    #[test]
    fn dedup_result_serde_round_trip() {
        let cases = vec![
            DedupResult::New,
            DedupResult::ExactDuplicate {
                original_record_id: "rec-1".to_string(),
            },
            DedupResult::NearDuplicate {
                original_record_id: "rec-2".to_string(),
                original_hash: "abc123".to_string(),
            },
        ];

        for case in cases {
            let json = serde_json::to_string(&case).expect("serialize");
            let parsed: DedupResult = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(case, parsed);
        }
    }
}
