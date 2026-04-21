//! Stage C cache — 1 h TTL LRU keyed by `(source_kind, content_hash)`.
//!
//! Stage C is the only LLM-backed stage; a single call is orders of
//! magnitude more expensive than any deterministic stage. Design §6.5
//! carves out a dedicated cache for Stage C so that:
//!
//! - A repeat scan of identical content (same source kind + same payload)
//!   inside the TTL window skips the LLM call entirely.
//! - Rate-limit / gateway-unavailable results get a shorter TTL (5 min) so
//!   a transient outage doesn't lock a content-hash into `SUSPICIOUS` for a
//!   full hour.
//! - The cache is independent of the Stage A+B cache: those scans are
//!   deterministic across the full firewall version, whereas Stage C's
//!   cache lifetime is tied to freshness rather than version bumps.
//!
//! Keys omit the firewall version on purpose — Stage C's cache is TTL-gated,
//! not version-gated. When a new firewall version ships, operators can opt
//! to flush the Stage C cache, but an old verdict isn't *wrong* the way an
//! old Stage A+B verdict might be (the prompt template + model tier can
//! rev independently of the heuristic rules).

use lru::LruCache;
use sha2::{Digest, Sha256};
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::stages::stage_c_prompt::{ParsedResponse, StageCLabel};

/// Default cache capacity. Chosen to hold ~24 h of Stage C traffic at the
/// ~15 k calls/day cost-model estimate in the design doc (§3.3 notes).
pub const DEFAULT_CAPACITY: usize = 20_000;

/// Default TTL for positive (SAFE / SUSPICIOUS / MALICIOUS) verdicts.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);

/// Shorter TTL used when Stage C falls back to `SUSPICIOUS` because the
/// gateway rate-limited or was unavailable. Avoids a transient outage
/// locking a content hash into a hostile verdict for the full TTL window.
pub const DEFAULT_FAILURE_TTL: Duration = Duration::from_secs(5 * 60);

/// Key for the Stage C cache: `(source_kind, sha256(content))`.
///
/// Unlike the Stage A+B [`crate::CacheKey`], this cache does not include the
/// firewall version. Stage C's freshness is TTL-gated; version-bump
/// invalidation happens via operator-initiated cache flush.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StageCCacheKey {
    pub source_kind: String,
    pub content_hash: String,
}

impl StageCCacheKey {
    /// Construct a key from a raw `(source_kind, payload)` pair.
    pub fn for_payload(source_kind: &str, payload: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        let digest = hasher.finalize();
        Self {
            source_kind: source_kind.to_string(),
            content_hash: hex_encode(&digest),
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

/// A cached Stage C verdict + its expiry.
///
/// Expiry is stored as an `Instant` so wall-clock changes don't move the
/// TTL. Tests use [`ClockSource`] to fast-forward virtual time without
/// waiting on real elapsed duration.
#[derive(Debug, Clone)]
struct Entry {
    response: ParsedResponse,
    expires_at: Instant,
}

/// Abstraction over time so tests can advance a virtual clock instead of
/// sleeping. Production uses [`SystemClock`] which delegates to
/// [`Instant::now`]; tests use [`TestClock`] (see this module's test block).
pub trait ClockSource: Send + Sync {
    /// Current monotonic instant.
    fn now(&self) -> Instant;
}

/// Real-world clock implementation.
pub struct SystemClock;

impl ClockSource for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Thread-safe LRU cache of Stage C verdicts. Entries expire after a
/// configurable TTL (default [`DEFAULT_TTL`]).
pub struct StageCCache {
    inner: Mutex<LruCache<StageCCacheKey, Entry>>,
    ttl: Duration,
    failure_ttl: Duration,
    clock: Box<dyn ClockSource>,
}

impl StageCCache {
    /// Construct with a specific capacity, TTL, failure-TTL, and clock.
    pub fn with_parts(
        capacity: usize,
        ttl: Duration,
        failure_ttl: Duration,
        clock: Box<dyn ClockSource>,
    ) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("cache capacity must be > 0");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            ttl,
            failure_ttl,
            clock,
        }
    }

    /// Default-configured cache.
    pub fn default_capacity() -> Self {
        Self::with_parts(
            DEFAULT_CAPACITY,
            DEFAULT_TTL,
            DEFAULT_FAILURE_TTL,
            Box::new(SystemClock),
        )
    }

    /// Construct with a custom clock (used in tests for deterministic TTL).
    pub fn with_clock(clock: Box<dyn ClockSource>) -> Self {
        Self::with_parts(DEFAULT_CAPACITY, DEFAULT_TTL, DEFAULT_FAILURE_TTL, clock)
    }

    /// Lookup. Returns `None` on miss OR on expired entry. Expired entries
    /// are not actively purged here — they're overwritten on the next
    /// `put`, and the LRU evicts them eventually.
    pub fn get(&self, key: &StageCCacheKey) -> Option<ParsedResponse> {
        let now = self.clock.now();
        let mut guard = self.inner.lock().expect("stage c cache poisoned");
        let entry = guard.get(key)?;
        if entry.expires_at > now {
            Some(entry.response.clone())
        } else {
            None
        }
    }

    /// Insert a positive-outcome verdict (full TTL).
    pub fn put(&self, key: StageCCacheKey, response: ParsedResponse) {
        self.put_with_ttl(key, response, self.ttl);
    }

    /// Insert a failure-fallback verdict (short TTL).
    ///
    /// Use this when the gateway returned `RateLimited` / `Unavailable` and
    /// Stage C fell back to `SUSPICIOUS`. The shorter TTL lets a recovered
    /// gateway produce a real verdict sooner.
    pub fn put_failure_fallback(&self, key: StageCCacheKey, response: ParsedResponse) {
        debug_assert!(
            response.label == StageCLabel::Suspicious,
            "failure fallback must be SUSPICIOUS"
        );
        self.put_with_ttl(key, response, self.failure_ttl);
    }

    fn put_with_ttl(&self, key: StageCCacheKey, response: ParsedResponse, ttl: Duration) {
        let expires_at = self.clock.now() + ttl;
        let mut guard = self.inner.lock().expect("stage c cache poisoned");
        guard.put(
            key,
            Entry {
                response,
                expires_at,
            },
        );
    }

    /// Current entry count (useful for tests + telemetry). Includes
    /// unexpired-but-expiring entries that have yet to be evicted.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("stage c cache poisoned").len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for StageCCache {
    fn default() -> Self {
        Self::default_capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Deterministic clock for TTL assertions.
    pub(crate) struct TestClock {
        inner: StdMutex<Instant>,
    }

    impl TestClock {
        pub fn starting_now() -> Self {
            Self {
                inner: StdMutex::new(Instant::now()),
            }
        }

        pub fn advance(&self, d: Duration) {
            let mut guard = self.inner.lock().expect("test clock poisoned");
            *guard += d;
        }
    }

    impl ClockSource for TestClock {
        fn now(&self) -> Instant {
            *self.inner.lock().expect("test clock poisoned")
        }
    }

    fn parsed(label: StageCLabel, rationale: &str) -> ParsedResponse {
        ParsedResponse {
            label,
            rationale: rationale.to_string(),
        }
    }

    #[test]
    fn key_is_stable_for_same_content() {
        let k1 = StageCCacheKey::for_payload("web_fetch", "hello world");
        let k2 = StageCCacheKey::for_payload("web_fetch", "hello world");
        assert_eq!(k1, k2);
        assert_eq!(k1.content_hash.len(), 64);
    }

    #[test]
    fn key_differs_by_source_kind() {
        let k1 = StageCCacheKey::for_payload("web_fetch", "same");
        let k2 = StageCCacheKey::for_payload("tool_observation", "same");
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_differs_by_content() {
        let k1 = StageCCacheKey::for_payload("web_fetch", "a");
        let k2 = StageCCacheKey::for_payload("web_fetch", "b");
        assert_ne!(k1, k2);
    }

    #[test]
    fn put_then_get_returns_verdict() {
        let cache = StageCCache::default_capacity();
        let key = StageCCacheKey::for_payload("web_fetch", "benign");
        cache.put(key.clone(), parsed(StageCLabel::Safe, "clean"));
        let got = cache.get(&key).expect("hit");
        assert_eq!(got.label, StageCLabel::Safe);
        assert_eq!(got.rationale, "clean");
    }

    #[test]
    fn miss_on_unseen_key() {
        let cache = StageCCache::default_capacity();
        assert!(cache
            .get(&StageCCacheKey::for_payload("web_fetch", "missing"))
            .is_none());
    }

    #[test]
    fn ttl_expiry_clears_entry() {
        let clock = Box::new(TestClock::starting_now());
        // Keep a raw pointer via Arc-alternative: wrap in a custom clock
        // that shares state with an outside handle. Since Box<dyn ClockSource>
        // moves the clock, we instead construct a clock that we can refer to
        // through a module-level static-ish pattern — use a small adapter.
        struct SharedClock(std::sync::Arc<TestClock>);
        impl ClockSource for SharedClock {
            fn now(&self) -> Instant {
                self.0.now()
            }
        }
        let shared = std::sync::Arc::new(TestClock::starting_now());
        let _ = clock;
        let cache = StageCCache::with_parts(
            8,
            Duration::from_secs(60),
            Duration::from_secs(10),
            Box::new(SharedClock(shared.clone())),
        );
        let key = StageCCacheKey::for_payload("web_fetch", "x");
        cache.put(key.clone(), parsed(StageCLabel::Safe, "ok"));
        assert!(cache.get(&key).is_some());

        shared.advance(Duration::from_secs(59));
        assert!(cache.get(&key).is_some());

        shared.advance(Duration::from_secs(2));
        assert!(cache.get(&key).is_none(), "entry should expire past TTL");
    }

    #[test]
    fn failure_fallback_uses_shorter_ttl() {
        struct SharedClock(std::sync::Arc<TestClock>);
        impl ClockSource for SharedClock {
            fn now(&self) -> Instant {
                self.0.now()
            }
        }
        let shared = std::sync::Arc::new(TestClock::starting_now());
        let cache = StageCCache::with_parts(
            8,
            Duration::from_secs(60),
            Duration::from_secs(10),
            Box::new(SharedClock(shared.clone())),
        );
        let key = StageCCacheKey::for_payload("web_fetch", "rate-limited");
        cache.put_failure_fallback(key.clone(), parsed(StageCLabel::Suspicious, "fallback"));
        assert!(cache.get(&key).is_some());

        shared.advance(Duration::from_secs(11));
        assert!(
            cache.get(&key).is_none(),
            "failure-fallback should expire at the shorter failure TTL"
        );
    }

    #[test]
    fn lru_eviction_respects_capacity() {
        let cache = StageCCache::with_parts(
            2,
            Duration::from_secs(3600),
            Duration::from_secs(60),
            Box::new(SystemClock),
        );
        let k1 = StageCCacheKey::for_payload("s", "1");
        let k2 = StageCCacheKey::for_payload("s", "2");
        let k3 = StageCCacheKey::for_payload("s", "3");
        cache.put(k1.clone(), parsed(StageCLabel::Safe, "a"));
        cache.put(k2.clone(), parsed(StageCLabel::Safe, "b"));
        cache.put(k3.clone(), parsed(StageCLabel::Safe, "c"));
        assert!(cache.get(&k1).is_none(), "oldest should evict");
        assert!(cache.get(&k2).is_some());
        assert!(cache.get(&k3).is_some());
    }
}
