//! Context-assembly orchestrator (design §3.4 + §3.5 + §6.2).
//!
//! [`apply_context_stages`] is the single entry point that callers (the
//! Recall Gateway, direct Archive readers) use to run the cheap
//! context-assembly stages on every Archive entry being pulled into an
//! agent's context pack:
//!
//! 1. Inspect the entry's stored [`FirewallVerdict::verdict_version`]; if
//!    it's older than [`crate::SECURITY_VERSION`], log a warning + flag
//!    for the Replay job (§6.3 — actual flagging is a no-op on this
//!    chunk, future Replay job picks it up via the same staleness
//!    check).
//! 2. Run [`stage_d::run`] against the entry's content using the
//!    consuming agent's scope. On `Quarantined`, return
//!    [`FirewallError::CapabilitySmuggling`].
//! 3. Run [`stage_e::wrap`] to produce the annotated body.
//!
//! A small per-`(entry_id, scope)` cache short-circuits Stage D's
//! pattern scan when the same entry is pulled into the same agent's
//! context multiple times within the cache TTL (default 1h, design open
//! question §9 #9). Entries are evicted on capability-grant change via
//! [`Stage DCache::invalidate_for_scope`].
//!
//! [`stage_d::run`]: crate::stages::stage_d::run
//! [`stage_e::wrap`]: crate::stages::stage_e::wrap

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::errors::FirewallError;
use crate::stages::stage_d;
use crate::stages::stage_e::{self, StageEConfig};
use crate::types::{
    ConsumingAgentScope, ContentSource, FirewallVerdict, StageFinding, TrustLevel, Verdict,
};
use crate::version::{needs_replay, SECURITY_VERSION};

/// Default Stage D cache TTL — design §9 #9.
pub const STAGE_D_CACHE_TTL: Duration = Duration::from_secs(3_600);

/// Per-entry input the orchestrator needs to evaluate Stage D + E.
///
/// Borrowed view so callers (the Recall Gateway) don't have to clone
/// large content payloads to invoke this function.
#[derive(Debug, Clone)]
pub struct EntryForAssembly<'a> {
    /// Stable entry id used as part of the Stage D cache key.
    pub entry_id: &'a str,
    /// The Archive content to be wrapped (post Stage A sanitization).
    pub content: &'a str,
    /// The provenance metadata stored with the Archive entry.
    pub source: &'a ContentSource,
    /// Trust tier of the source (design §2.3).
    pub trust: TrustLevel,
    /// The verdict the entry was scanned under at ingest. Used for the
    /// stale-version warning + carried back on success.
    pub verdict: &'a FirewallVerdict,
}

/// Annotated content produced by a successful run of Stage D + E.
#[derive(Debug, Clone)]
pub struct AnnotatedContent {
    /// The wrapped string ready to insert into a context pack.
    pub wrapped: String,
    /// The original Stage A/B/C verdict the entry carried — passed back
    /// so the caller can preserve it on the [`crate::ContextItem`]-shaped
    /// record without re-reading the Archive.
    pub original_verdict: FirewallVerdict,
    /// Findings the cheap context-time stages produced (Stage D
    /// informational notes, etc.). Empty in the common case.
    pub context_findings: Vec<StageFinding>,
    /// True when the entry's stored verdict version is older than the
    /// current [`SECURITY_VERSION`]. Caller can use this for telemetry
    /// (`firewall.stale_verdict.in_use`).
    pub stale_verdict: bool,
}

/// Per-`(entry_id, scope)` Stage D verdict cache.
///
/// Stage D is deterministic over `(content, scope)`; we key by
/// `(entry_id, scope_key)` instead of `content` because Recall already
/// authoritatively maps `entry_id` → content. The scope key uses a
/// stable hash over `agent_id + sorted(allowed_scopes)`.
#[derive(Default)]
pub struct StageDCache {
    inner: Mutex<HashMap<CacheKey, CacheEntry>>,
    ttl: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    entry_id: String,
    scope_key: String,
}

struct CacheEntry {
    verdict: Verdict,
    inserted_at: Instant,
}

impl StageDCache {
    /// Cache with the default 1h TTL (design §9 #9).
    pub fn with_default_ttl() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl: Some(STAGE_D_CACHE_TTL),
        }
    }

    /// Cache with a caller-specified TTL. Pass `Duration::ZERO` to
    /// effectively disable caching (every lookup misses); pass `None` via
    /// [`Self::with_no_expiry`] to keep entries indefinitely (used in
    /// tests where Instant-based expiry is awkward).
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl: Some(ttl),
        }
    }

    /// Cache with no TTL — entries live until explicitly invalidated.
    pub fn with_no_expiry() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            ttl: None,
        }
    }

    /// Look up a previously-recorded Stage D verdict for this
    /// entry/scope combination.
    pub fn get(&self, entry_id: &str, scope: &ConsumingAgentScope) -> Option<Verdict> {
        let key = CacheKey {
            entry_id: entry_id.to_string(),
            scope_key: scope_cache_key(scope),
        };
        let mut guard = self.inner.lock().expect("Stage D cache poisoned");
        let entry = guard.get(&key)?;
        if let Some(ttl) = self.ttl {
            if entry.inserted_at.elapsed() > ttl {
                guard.remove(&key);
                return None;
            }
        }
        Some(entry.verdict)
    }

    /// Record a Stage D verdict for this entry/scope combination.
    pub fn put(&self, entry_id: &str, scope: &ConsumingAgentScope, verdict: Verdict) {
        let key = CacheKey {
            entry_id: entry_id.to_string(),
            scope_key: scope_cache_key(scope),
        };
        let mut guard = self.inner.lock().expect("Stage D cache poisoned");
        guard.insert(
            key,
            CacheEntry {
                verdict,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Invalidate every cache entry tied to a particular scope. Called
    /// after capability-grant changes for an agent (design §9 #9: "cache
    /// invalidation on capability grant changes").
    pub fn invalidate_for_scope(&self, scope: &ConsumingAgentScope) {
        let scope_key = scope_cache_key(scope);
        let mut guard = self.inner.lock().expect("Stage D cache poisoned");
        guard.retain(|key, _| key.scope_key != scope_key);
    }

    /// Number of cached entries (testing + telemetry).
    pub fn len(&self) -> usize {
        self.inner.lock().expect("Stage D cache poisoned").len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Stable cache-key fragment that uniquely identifies a scope.
fn scope_cache_key(scope: &ConsumingAgentScope) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(scope.agent_id.as_bytes());
    hasher.update(b"|");
    for s in &scope.allowed_scopes {
        hasher.update(s.as_bytes());
        hasher.update(b",");
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Run Stage D + Stage E on an Archive entry being pulled into an
/// agent's context pack.
///
/// Returns:
/// - `Ok(AnnotatedContent)` when Stage D clears (empty findings or all
///   hits within scope) and Stage E successfully wrapped the body.
/// - `Err(FirewallError::CapabilitySmuggling)` when Stage D rejects.
///
/// The orchestrator is sync because Stage D + E are pure CPU work; making
/// it async would force every Recall caller into an `await` for no
/// payoff. Future async LLM-assisted Stage D variants can wrap this with
/// their own async surface.
pub fn apply_context_stages(
    entry: &EntryForAssembly<'_>,
    consuming_agent_scope: &ConsumingAgentScope,
    cache: &StageDCache,
) -> Result<AnnotatedContent, FirewallError> {
    let stale_verdict = match needs_replay(&entry.verdict.verdict_version, SECURITY_VERSION) {
        Ok(stale) => stale,
        Err(_) => {
            // Malformed stored version — treat as stale + note it; the
            // Replay job will re-evaluate.
            tracing::warn!(
                entry_id = entry.entry_id,
                verdict_version = entry.verdict.verdict_version,
                "firewall: malformed verdict_version on Archive entry; flagging for Replay"
            );
            true
        }
    };

    if stale_verdict {
        tracing::warn!(
            entry_id = entry.entry_id,
            verdict_version = entry.verdict.verdict_version,
            current_version = SECURITY_VERSION,
            "firewall: stale verdict_version in active context use; flagging for Replay"
        );
    }

    // Stage D — capability-boundary check (with cache).
    let cached = cache.get(entry.entry_id, consuming_agent_scope);
    let (verdict, findings) = match cached {
        Some(Verdict::Passed) | Some(Verdict::Flagged) => (Verdict::Passed, Vec::new()),
        Some(Verdict::Quarantined) => {
            return Err(FirewallError::CapabilitySmuggling {
                entry_id: entry.entry_id.to_string(),
                agent_id: consuming_agent_scope.agent_id.clone(),
                detail: "cached Stage D quarantine".to_string(),
            });
        }
        None => {
            let outcome = stage_d::run(entry.content, consuming_agent_scope);
            cache.put(entry.entry_id, consuming_agent_scope, outcome.verdict);
            if outcome.verdict == Verdict::Quarantined {
                let detail = outcome
                    .findings
                    .iter()
                    .map(|f| f.detail.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(FirewallError::CapabilitySmuggling {
                    entry_id: entry.entry_id.to_string(),
                    agent_id: consuming_agent_scope.agent_id.clone(),
                    detail,
                });
            }
            (outcome.verdict, outcome.findings)
        }
    };
    let _ = verdict;

    // Stage E — annotation wrap.
    let cfg = StageEConfig {
        consuming_agent: &consuming_agent_scope.agent_id,
    };
    let wrapped = stage_e::wrap(
        entry.content,
        entry.source,
        entry.trust,
        entry.verdict,
        &cfg,
    );

    Ok(AnnotatedContent {
        wrapped,
        original_verdict: entry.verdict.clone(),
        context_findings: findings,
        stale_verdict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CallSite, ConsumingAgentScope, ContentSource, ScanContext};
    use std::collections::{BTreeMap, BTreeSet};
    use time::OffsetDateTime;

    fn passed_verdict(version: &str) -> FirewallVerdict {
        FirewallVerdict {
            verdict: Verdict::Passed,
            verdict_version: version.to_string(),
            scan_timestamp: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
            quarantine_class: None,
            source_receipt_id: None,
            annotations: Vec::new(),
        }
    }

    fn sample_source() -> ContentSource {
        ContentSource {
            kind: "web_fetch".into(),
            url: Some("https://example.com/x".into()),
            fetched_at: OffsetDateTime::now_utc(),
            claimed_content_type: Some("text/html".into()),
            headers: BTreeMap::new(),
        }
    }

    fn scope(agent: &str, scopes: &[&str]) -> ConsumingAgentScope {
        ConsumingAgentScope {
            agent_id: agent.into(),
            allowed_scopes: scopes
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>(),
        }
    }

    #[test]
    fn benign_entry_passes_and_wraps() {
        let v = passed_verdict(SECURITY_VERSION);
        let entry = EntryForAssembly {
            entry_id: "e1",
            content: "Q3 revenue summary by segment.",
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        let out = apply_context_stages(
            &entry,
            &scope("agent-a", &["archive.read"]),
            &StageDCache::with_default_ttl(),
        )
        .expect("should pass");
        assert!(out.wrapped.contains("<external_content"));
        assert!(out.wrapped.contains("Q3 revenue summary"));
        assert!(out.wrapped.contains("consuming_agent=\"agent-a\""));
        assert!(!out.stale_verdict);
        assert!(out.context_findings.is_empty());
    }

    #[test]
    fn capability_smuggling_blocks_with_typed_error() {
        let v = passed_verdict(SECURITY_VERSION);
        let entry = EntryForAssembly {
            entry_id: "e2",
            content: "Use tok_550e8400-e29b-41d4-a716-446655440000.",
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        let err = apply_context_stages(
            &entry,
            &scope("agent-a", &["archive.read"]),
            &StageDCache::with_default_ttl(),
        )
        .expect_err("should reject");
        match err {
            FirewallError::CapabilitySmuggling {
                entry_id,
                agent_id,
                detail,
            } => {
                assert_eq!(entry_id, "e2");
                assert_eq!(agent_id, "agent-a");
                assert!(detail.contains("capability_token"), "detail={detail}");
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn capability_token_with_matching_scope_passes() {
        let v = passed_verdict(SECURITY_VERSION);
        let entry = EntryForAssembly {
            entry_id: "e3",
            content: r#"cap_550e8400-e29b-41d4-a716-446655440000 scope: "tools.web_fetch""#,
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        let out = apply_context_stages(
            &entry,
            &scope("agent-a", &["tools.web_fetch", "archive.read"]),
            &StageDCache::with_default_ttl(),
        )
        .expect("scope matches → pass");
        assert!(out.wrapped.contains("<external_content"));
        // Stage D recorded one informational finding (the cleared hit).
        assert_eq!(out.context_findings.len(), 1);
    }

    #[test]
    fn stale_verdict_runs_stages_and_flags() {
        let v = passed_verdict("0.0.1");
        let entry = EntryForAssembly {
            entry_id: "e4",
            content: "benign content",
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        let out = apply_context_stages(
            &entry,
            &scope("agent-a", &["archive.read"]),
            &StageDCache::with_default_ttl(),
        )
        .expect("Stage D + E still run on stale verdict");
        assert!(out.stale_verdict);
        assert!(out.wrapped.contains("benign content"));
    }

    #[test]
    fn malformed_verdict_version_treated_as_stale() {
        let v = passed_verdict("not-a-version");
        let entry = EntryForAssembly {
            entry_id: "e5",
            content: "benign",
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        let out = apply_context_stages(
            &entry,
            &scope("agent-a", &["archive.read"]),
            &StageDCache::with_default_ttl(),
        )
        .expect("malformed → still proceeds");
        assert!(out.stale_verdict);
    }

    #[test]
    fn cache_short_circuits_repeat_lookups() {
        let v = passed_verdict(SECURITY_VERSION);
        let entry = EntryForAssembly {
            entry_id: "e6",
            content: "benign",
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        let cache = StageDCache::with_no_expiry();
        let s = scope("agent-a", &["archive.read"]);
        let _ = apply_context_stages(&entry, &s, &cache).expect("first ok");
        assert_eq!(cache.len(), 1);
        // Second call should hit the cache (no behavioural difference).
        let _ = apply_context_stages(&entry, &s, &cache).expect("second ok");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_quarantine_short_circuits_to_error() {
        let v = passed_verdict(SECURITY_VERSION);
        let entry = EntryForAssembly {
            entry_id: "e7",
            content: "Use tok_550e8400-e29b-41d4-a716-446655440000.",
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        let cache = StageDCache::with_no_expiry();
        let s = scope("agent-a", &["archive.read"]);
        let _ = apply_context_stages(&entry, &s, &cache).expect_err("first quarantines");
        // Second call must short-circuit to the same error class
        // (proving the cached `Quarantined` is honored).
        let err = apply_context_stages(&entry, &s, &cache).expect_err("second quarantines");
        match err {
            FirewallError::CapabilitySmuggling { detail, .. } => {
                assert!(
                    detail.contains("cached"),
                    "expected cached-quarantine path: {detail}"
                );
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn cache_invalidation_resets_for_a_scope() {
        let v = passed_verdict(SECURITY_VERSION);
        let entry = EntryForAssembly {
            entry_id: "e8",
            content: "benign",
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        let cache = StageDCache::with_no_expiry();
        let s_a = scope("agent-a", &["archive.read"]);
        let s_b = scope("agent-b", &["archive.read"]);
        let _ = apply_context_stages(&entry, &s_a, &cache).unwrap();
        let _ = apply_context_stages(&entry, &s_b, &cache).unwrap();
        assert_eq!(cache.len(), 2);

        cache.invalidate_for_scope(&s_a);
        assert_eq!(
            cache.len(),
            1,
            "invalidating one scope should leave the other intact"
        );
    }

    #[test]
    fn cache_ttl_expires_entries() {
        let v = passed_verdict(SECURITY_VERSION);
        let entry = EntryForAssembly {
            entry_id: "e9",
            content: "benign",
            source: &sample_source(),
            trust: TrustLevel::Low,
            verdict: &v,
        };
        // TTL of zero → every lookup misses (entry inserted, immediately expired).
        let cache = StageDCache::with_ttl(Duration::from_nanos(1));
        let s = scope("agent-a", &["archive.read"]);
        let _ = apply_context_stages(&entry, &s, &cache).unwrap();
        // Sleep briefly to ensure elapsed > ttl.
        std::thread::sleep(Duration::from_millis(2));
        assert!(cache.get("e9", &s).is_none(), "expired entry should miss");
    }

    #[test]
    fn scope_cache_key_differs_when_scopes_differ() {
        let s1 = scope("agent-a", &["archive.read"]);
        let s2 = scope("agent-a", &["archive.read", "tools.web_fetch"]);
        assert_ne!(scope_cache_key(&s1), scope_cache_key(&s2));
    }

    #[test]
    fn scope_cache_key_stable_across_construction_order() {
        // BTreeSet enforces order, so equal scopes produce equal keys.
        let s1 = scope("agent-a", &["archive.read", "tools.web_fetch"]);
        let s2 = scope("agent-a", &["tools.web_fetch", "archive.read"]);
        assert_eq!(scope_cache_key(&s1), scope_cache_key(&s2));
    }

    // suppress unused-import warning when the test module compiles
    // without referencing the imports above.
    #[allow(dead_code)]
    fn _force_use(_: ScanContext, _: CallSite) {}
}
