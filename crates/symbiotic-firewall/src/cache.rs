//! Scan-result cache (design §6.5).
//!
//! Ingest-time scans are deterministic for a given
//! `(source_kind, content_hash, firewall_version)` triple. Caching avoids
//! re-scanning the same content twice — e.g. a popular API documentation
//! page ingested via multiple call sites.
//!
//! The cache is an in-memory LRU of bounded size. Entries are invalidated
//! automatically when the firewall version changes (because the version is
//! part of the key) — that means a version bump is effectively a cache
//! flush without any explicit purge step. Replay (which runs on older
//! entries in Archive) will re-populate the cache as it re-scans.
//!
//! This module is deliberately free of I/O; a persistent cache would be a
//! natural Phase-2 extension.

use lru::LruCache;
use sha2::{Digest, Sha256};
use std::num::NonZeroUsize;
use std::sync::Mutex;

use crate::types::FirewallVerdict;
use crate::version::SECURITY_VERSION;

/// Cache key: `(source_kind, content_hash, firewall_version)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub source_kind: String,
    pub content_hash: String,
    pub firewall_version: String,
}

impl CacheKey {
    /// Build a cache key from raw inputs. The content hash is computed via
    /// SHA-256 over the payload bytes.
    pub fn for_payload(source_kind: &str, payload: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        let digest = hasher.finalize();
        Self {
            source_kind: source_kind.to_string(),
            content_hash: hex_encode(&digest),
            firewall_version: SECURITY_VERSION.to_string(),
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

const HEX: &[u8] = b"0123456789abcdef";

/// Default cache capacity (design §6.5 suggests 10,000 entries).
pub const DEFAULT_CACHE_CAPACITY: usize = 10_000;

/// Thread-safe LRU cache keyed by [`CacheKey`].
pub struct ScanCache {
    inner: Mutex<LruCache<CacheKey, FirewallVerdict>>,
}

impl ScanCache {
    /// Construct with a specific capacity. Panics if `capacity == 0`.
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("cache capacity must be > 0");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Default-capacity cache.
    pub fn default_capacity() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }

    /// Look up a verdict for this key; `None` on miss.
    pub fn get(&self, key: &CacheKey) -> Option<FirewallVerdict> {
        let mut guard = self.inner.lock().expect("cache mutex poisoned");
        guard.get(key).cloned()
    }

    /// Insert a verdict. Replaces any existing entry.
    pub fn put(&self, key: CacheKey, verdict: FirewallVerdict) {
        let mut guard = self.inner.lock().expect("cache mutex poisoned");
        guard.put(key, verdict);
    }

    /// Current number of entries (useful for tests + telemetry).
    pub fn len(&self) -> usize {
        self.inner.lock().expect("cache mutex poisoned").len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ScanCache {
    fn default() -> Self {
        Self::default_capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Verdict;
    use time::OffsetDateTime;

    fn verdict() -> FirewallVerdict {
        FirewallVerdict {
            verdict: Verdict::Passed,
            verdict_version: SECURITY_VERSION.to_string(),
            scan_timestamp: OffsetDateTime::now_utc(),
            quarantine_class: None,
            source_receipt_id: None,
            annotations: Vec::new(),
        }
    }

    #[test]
    fn cache_roundtrips_verdict() {
        let cache = ScanCache::new(8);
        let key = CacheKey::for_payload("web_fetch", "hello world");
        cache.put(key.clone(), verdict());
        let got = cache.get(&key).expect("cache hit");
        assert_eq!(got.verdict, Verdict::Passed);
    }

    #[test]
    fn cache_miss_on_different_content() {
        let cache = ScanCache::new(8);
        let k1 = CacheKey::for_payload("web_fetch", "aaa");
        let k2 = CacheKey::for_payload("web_fetch", "bbb");
        cache.put(k1.clone(), verdict());
        assert!(cache.get(&k2).is_none());
    }

    #[test]
    fn cache_miss_on_different_source_kind() {
        let cache = ScanCache::new(8);
        let k1 = CacheKey::for_payload("web_fetch", "same");
        let k2 = CacheKey::for_payload("tool_observation", "same");
        cache.put(k1, verdict());
        assert!(cache.get(&k2).is_none());
    }

    #[test]
    fn cache_key_includes_firewall_version() {
        // Directly crafted key with a mismatched version should miss.
        let cache = ScanCache::new(8);
        let k_current = CacheKey::for_payload("web_fetch", "x");
        let k_older = CacheKey {
            firewall_version: "0.0.1".into(),
            ..k_current.clone()
        };
        cache.put(k_older, verdict());
        assert!(cache.get(&k_current).is_none());
    }

    #[test]
    fn cache_evicts_lru() {
        let cache = ScanCache::new(2);
        let k1 = CacheKey::for_payload("s", "1");
        let k2 = CacheKey::for_payload("s", "2");
        let k3 = CacheKey::for_payload("s", "3");
        cache.put(k1.clone(), verdict());
        cache.put(k2.clone(), verdict());
        cache.put(k3.clone(), verdict());
        assert!(cache.get(&k1).is_none(), "k1 should have been evicted");
        assert!(cache.get(&k2).is_some());
        assert!(cache.get(&k3).is_some());
    }

    #[test]
    fn content_hash_is_stable() {
        let k1 = CacheKey::for_payload("s", "hello world");
        let k2 = CacheKey::for_payload("s", "hello world");
        assert_eq!(k1, k2);
        assert_eq!(k1.content_hash.len(), 64);
    }
}
