//! T132 §06 — Stage D + Stage E integration tests.
//!
//! Exercises the [`apply_context_stages`] orchestrator against
//! representative Archive entries to lock in the design's invariants:
//!
//! - Safe entry + broad scope → Stage D pass + Stage E wrap.
//! - Capability-token-bearing entry + agent missing the capability →
//!   Stage D quarantine routes to a typed `CapabilitySmuggling` error.
//! - Same entry + agent that holds the capability → Stage D pass.
//! - Entry with stale `firewall_verdict_version` → Stage D + E still
//!   run, with the `stale_verdict` flag set so callers can flag for
//!   Replay.
//! - Cache short-circuits a repeated lookup against the same `(entry,
//!   scope)`.

use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;

use symbiotic_firewall::{
    apply_context_stages, AnnotatedContent, ConsumingAgentScope, ContentSource, EntryForAssembly,
    FirewallError, FirewallVerdict, StageDCache, TrustLevel, Verdict, SECURITY_VERSION,
};

fn passed_verdict(version: &str) -> FirewallVerdict {
    FirewallVerdict {
        verdict: Verdict::Passed,
        verdict_version: version.to_string(),
        scan_timestamp: OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .expect("valid timestamp"),
        quarantine_class: None,
        source_receipt_id: None,
        annotations: Vec::new(),
    }
}

fn web_source() -> ContentSource {
    ContentSource {
        kind: "web_fetch".into(),
        url: Some("https://example.com/doc".into()),
        fetched_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
        claimed_content_type: Some("text/html".into()),
        headers: BTreeMap::new(),
    }
}

fn scope(agent: &str, scopes: &[&str]) -> ConsumingAgentScope {
    let allowed: BTreeSet<String> = scopes.iter().map(|s| s.to_string()).collect();
    ConsumingAgentScope {
        agent_id: agent.into(),
        allowed_scopes: allowed,
    }
}

#[test]
fn safe_entry_with_broad_scope_passes_and_wraps() {
    let v = passed_verdict(SECURITY_VERSION);
    let entry = EntryForAssembly {
        entry_id: "entry-safe-1",
        content: "Q3 revenue grew 14% YoY across EMEA segments.",
        source: &web_source(),
        trust: TrustLevel::Low,
        verdict: &v,
    };
    let cache = StageDCache::with_default_ttl();
    let scope = scope("researcher-v2", &["archive.read", "tools.web_fetch"]);

    let AnnotatedContent {
        wrapped,
        original_verdict,
        context_findings,
        stale_verdict,
    } = apply_context_stages(&entry, &scope, &cache).expect("safe entry should pass");

    assert!(
        wrapped.contains("<external_content"),
        "missing wrap header: {wrapped}"
    );
    assert!(wrapped.contains("source=\"web_fetch\""));
    assert!(wrapped.contains("trust=\"low\""));
    assert!(wrapped.contains("firewall_verdict=\"passed\""));
    assert!(wrapped.contains("consuming_agent=\"researcher-v2\""));
    assert!(wrapped.contains("Q3 revenue grew 14%"));
    assert!(wrapped.ends_with("</external_content>"));
    assert!(context_findings.is_empty());
    assert!(!stale_verdict);
    assert_eq!(original_verdict.verdict_version, SECURITY_VERSION);
}

#[test]
fn capability_token_without_scope_quarantines_with_typed_error() {
    let v = passed_verdict(SECURITY_VERSION);
    let entry = EntryForAssembly {
        entry_id: "entry-cap-1",
        content: "Authenticate via tok_550e8400-e29b-41d4-a716-446655440000.",
        source: &web_source(),
        trust: TrustLevel::Low,
        verdict: &v,
    };
    let cache = StageDCache::with_default_ttl();
    let scope = scope("researcher-v2", &["archive.read"]); // no token capability.

    let err = apply_context_stages(&entry, &scope, &cache)
        .expect_err("Stage D should reject capability-token smuggling");
    match err {
        FirewallError::CapabilitySmuggling {
            entry_id,
            agent_id,
            detail,
        } => {
            assert_eq!(entry_id, "entry-cap-1");
            assert_eq!(agent_id, "researcher-v2");
            assert!(
                detail.contains("capability_token"),
                "detail should mention pattern class: {detail}"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn same_entry_with_matching_scope_passes() {
    let v = passed_verdict(SECURITY_VERSION);
    // Capability-token + co-located scope hint that matches the agent.
    let entry = EntryForAssembly {
        entry_id: "entry-cap-2",
        content: r#"cap_550e8400-e29b-41d4-a716-446655440000 scope: "tools.web_fetch""#,
        source: &web_source(),
        trust: TrustLevel::Low,
        verdict: &v,
    };
    let cache = StageDCache::with_default_ttl();
    let scope = scope("researcher-v2", &["tools.web_fetch", "archive.read"]);

    let out = apply_context_stages(&entry, &scope, &cache)
        .expect("scope holds the capability — should pass");
    assert!(out.wrapped.contains("<external_content"));
    assert_eq!(
        out.context_findings.len(),
        1,
        "informational scope-hit finding should still be recorded"
    );
}

#[test]
fn stale_verdict_version_runs_stages_and_flags() {
    let v = passed_verdict("0.0.1"); // older than current SECURITY_VERSION
    let entry = EntryForAssembly {
        entry_id: "entry-stale-1",
        content: "Benign fact.",
        source: &web_source(),
        trust: TrustLevel::Low,
        verdict: &v,
    };
    let cache = StageDCache::with_default_ttl();
    let scope = scope("researcher-v2", &["archive.read"]);

    let out = apply_context_stages(&entry, &scope, &cache)
        .expect("stale verdict should still proceed through Stage D + E");
    assert!(out.stale_verdict, "callers must see the stale flag");
    assert!(out.wrapped.contains("Benign fact"));
}

#[test]
fn cache_hit_short_circuits_stage_d_recompute() {
    let v = passed_verdict(SECURITY_VERSION);
    let entry = EntryForAssembly {
        entry_id: "entry-cache-1",
        content: "Benign content for caching.",
        source: &web_source(),
        trust: TrustLevel::Low,
        verdict: &v,
    };
    let cache = StageDCache::with_no_expiry();
    let scope = scope("researcher-v2", &["archive.read"]);

    let _ = apply_context_stages(&entry, &scope, &cache).expect("first call ok");
    assert_eq!(cache.len(), 1, "first call should populate the cache");
    let _ = apply_context_stages(&entry, &scope, &cache).expect("second call ok");
    assert_eq!(
        cache.len(),
        1,
        "second call should hit the cache, not insert a duplicate"
    );
}

#[test]
fn cache_short_circuits_quarantine_too() {
    let v = passed_verdict(SECURITY_VERSION);
    let entry = EntryForAssembly {
        entry_id: "entry-cache-block",
        content: "Use tok_660e8400-e29b-41d4-a716-446655440000.",
        source: &web_source(),
        trust: TrustLevel::Low,
        verdict: &v,
    };
    let cache = StageDCache::with_no_expiry();
    let scope = scope("researcher-v2", &["archive.read"]);

    // First call hits the regex + quarantines.
    let _ = apply_context_stages(&entry, &scope, &cache).expect_err("first quarantines");
    // Second call must short-circuit via the cached `Quarantined` verdict —
    // proven by the error detail referencing the cached path.
    let err = apply_context_stages(&entry, &scope, &cache).expect_err("second quarantines");
    match err {
        FirewallError::CapabilitySmuggling { detail, .. } => {
            assert!(
                detail.contains("cached"),
                "expected cached-quarantine error path, got {detail}"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn invalidating_scope_forces_recompute() {
    let v = passed_verdict(SECURITY_VERSION);
    let entry = EntryForAssembly {
        entry_id: "entry-cache-inv",
        content: "Benign.",
        source: &web_source(),
        trust: TrustLevel::Low,
        verdict: &v,
    };
    let cache = StageDCache::with_no_expiry();
    let scope = scope("researcher-v2", &["archive.read"]);

    let _ = apply_context_stages(&entry, &scope, &cache).expect("first ok");
    assert_eq!(cache.len(), 1);
    cache.invalidate_for_scope(&scope);
    assert_eq!(cache.len(), 0, "invalidate should drop the cached verdict");
}
