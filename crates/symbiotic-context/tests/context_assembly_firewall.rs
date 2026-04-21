//! T132 §06 — Recall Gateway × Content Firewall integration.
//!
//! Verifies that wiring a [`ContextFirewall`] onto the gateway and
//! invoking [`RecallGateway::get_context_with_firewall`]:
//!
//! 1. Wraps clean entries' content in `<external_content …>` annotation
//!    (Stage E) before they enter the [`ContextPack`].
//! 2. Excludes entries whose content trips Stage D (capability
//!    smuggling) and reports the block to the configured reporter.
//! 3. Leaves the un-firewalled `get_context` path unchanged so callers
//!    that don't yet thread a scope through still get the same items.
//!
//! No external services; everything is in-memory.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use symbiotic_context::{
    ArchiveEntry, ArchiveProvider, AuditRecord, AuditSink, ContextFirewall, ContextFirewallConfig,
    ContextRequest, ModelClass, Purpose, RecallGateway, Sensitivity, SmugglingReporter,
};
use symbiotic_core::now_unix;
use symbiotic_firewall::{ConsumingAgentScope, StageDCache, TrustLevel};

/// Provider that returns a fixed entry set.
struct FixedProvider {
    entries: Vec<ArchiveEntry>,
}
impl ArchiveProvider for FixedProvider {
    fn list_entries(&self) -> Result<Vec<ArchiveEntry>> {
        Ok(self.entries.clone())
    }
}

#[derive(Default)]
struct CountingAudit {
    records: Mutex<Vec<AuditRecord>>,
}
impl AuditSink for CountingAudit {
    fn record(&self, record: AuditRecord) -> Result<()> {
        self.records.lock().unwrap().push(record);
        Ok(())
    }
}

#[derive(Default)]
struct CapturingReporter {
    log: Mutex<Vec<(String, String, String)>>,
}
impl CapturingReporter {
    fn calls(&self) -> Vec<(String, String, String)> {
        self.log.lock().unwrap().clone()
    }
}
impl SmugglingReporter for CapturingReporter {
    fn report(&self, entry_id: &str, agent_id: &str, detail: &str) {
        self.log
            .lock()
            .unwrap()
            .push((entry_id.into(), agent_id.into(), detail.into()));
    }
}

fn entry(id: &str, title: &str, content: &str) -> ArchiveEntry {
    ArchiveEntry {
        id: id.into(),
        title: title.into(),
        content: content.into(),
        tags: vec!["test".into()],
        sensitivity: Sensitivity::Shareable,
        source_url: Some(format!("https://example.com/{id}")),
        updated_at: now_unix(),
        thread_id: None,
        fact_class: None,
    }
}

fn request(id: &str, query: &str) -> ContextRequest {
    ContextRequest {
        request_id: id.into(),
        query: query.into(),
        model_class: ModelClass::Local,
        purpose: Purpose::Answer,
        sensitivity_max: Sensitivity::Private,
        token_budget: 5_000,
        tags: vec![],
        recency_days: None,
        filter_threads: None,
        disclosure_tier: None,
        class_budget: None,
    }
}

fn scope(agent: &str, scopes: &[&str]) -> ConsumingAgentScope {
    let allowed: std::collections::BTreeSet<String> =
        scopes.iter().map(|s| s.to_string()).collect();
    ConsumingAgentScope {
        agent_id: agent.into(),
        allowed_scopes: allowed,
    }
}

#[test]
fn context_assembly_firewall_wraps_safe_entry_in_pack() {
    let provider = Arc::new(FixedProvider {
        entries: vec![entry("safe-1", "Safe doc", "Q3 revenue grew 14%.")],
    });
    let audit = Arc::new(CountingAudit::default());
    let mut gateway = RecallGateway::new(provider, audit);
    let firewall = Arc::new(ContextFirewall::with_defaults());
    gateway.set_firewall(firewall);

    let pack = gateway
        .get_context_with_firewall(
            &request("ctx-1", "revenue"),
            &scope("researcher-v2", &["archive.read"]),
        )
        .expect("safe entry assembly");

    assert_eq!(pack.items.len(), 1, "safe entry should be included");
    let item = &pack.items[0];
    assert_eq!(item.id, "safe-1");
    assert!(
        item.content.contains("<external_content"),
        "Stage E wrap missing: {}",
        item.content
    );
    assert!(
        item.content.contains("consuming_agent=\"researcher-v2\""),
        "Stage E should record consuming agent"
    );
    assert!(item.content.contains("Q3 revenue grew 14%"));
}

#[test]
fn context_assembly_firewall_blocks_capability_smuggling_and_reports() {
    let provider = Arc::new(FixedProvider {
        entries: vec![
            entry("safe-1", "Safe doc", "Q3 revenue grew 14%."),
            entry(
                "smug-1",
                "Smuggled token",
                "Authenticate via tok_550e8400-e29b-41d4-a716-446655440000.",
            ),
        ],
    });
    let audit = Arc::new(CountingAudit::default());
    let reporter = Arc::new(CapturingReporter::default());
    let mut gateway = RecallGateway::new(provider, audit);
    let firewall = Arc::new(ContextFirewall::new(ContextFirewallConfig {
        default_trust: TrustLevel::Low,
        reporter: reporter.clone(),
        cache: Arc::new(StageDCache::with_default_ttl()),
    }));
    gateway.set_firewall(firewall);

    // Empty query matches every entry under keyword retrieval — keeps
    // the test focused on Stage D's filtering behaviour rather than
    // keyword scoring.
    let pack = gateway
        .get_context_with_firewall(
            &request("ctx-2", ""),
            &scope("researcher-v2", &["archive.read"]),
        )
        .expect("blocked entry assembly should still succeed for the rest of the pack");

    // Safe entry kept; smuggling entry excluded.
    let ids: Vec<&str> = pack.items.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&"safe-1"), "safe entry must remain: {ids:?}");
    assert!(
        !ids.contains(&"smug-1"),
        "smuggling entry must be excluded: {ids:?}"
    );

    let calls = reporter.calls();
    assert_eq!(calls.len(), 1, "exactly one smuggling block reported");
    assert_eq!(calls[0].0, "smug-1");
    assert_eq!(calls[0].1, "researcher-v2");
    assert!(
        calls[0].2.contains("capability_token"),
        "report should mention pattern class: {}",
        calls[0].2
    );
}

#[test]
fn context_assembly_firewall_passes_capability_when_scope_includes_it() {
    // Same content as the smuggling test, but the agent's scope
    // includes the co-located scope hint — Stage D should pass.
    let provider = Arc::new(FixedProvider {
        entries: vec![entry(
            "cap-1",
            "Capability with co-located scope",
            r#"cap_550e8400-e29b-41d4-a716-446655440000 scope: "tools.web_fetch""#,
        )],
    });
    let audit = Arc::new(CountingAudit::default());
    let reporter = Arc::new(CapturingReporter::default());
    let mut gateway = RecallGateway::new(provider, audit);
    gateway.set_firewall(Arc::new(ContextFirewall::new(ContextFirewallConfig {
        default_trust: TrustLevel::Low,
        reporter: reporter.clone(),
        cache: Arc::new(StageDCache::with_default_ttl()),
    })));

    let pack = gateway
        .get_context_with_firewall(
            &request("ctx-3", ""),
            &scope("researcher-v2", &["tools.web_fetch", "archive.read"]),
        )
        .expect("scope holds the capability");

    assert_eq!(pack.items.len(), 1);
    assert!(reporter.calls().is_empty(), "no smuggling block expected");
    assert!(pack.items[0].content.contains("<external_content"));
}

#[test]
fn context_assembly_firewall_inactive_when_scope_not_provided() {
    // Verifies the un-firewalled `get_context` path is unchanged: even
    // with a wired firewall, callers that don't pass a scope skip Stage
    // D + E and the entry goes through as-is.
    let provider = Arc::new(FixedProvider {
        entries: vec![entry("plain", "Plain", "no annotation here")],
    });
    let audit = Arc::new(CountingAudit::default());
    let mut gateway = RecallGateway::new(provider, audit);
    gateway.set_firewall(Arc::new(ContextFirewall::with_defaults()));

    let pack = gateway
        .get_context(&request("ctx-4", "plain"))
        .expect("plain path still works");
    assert_eq!(pack.items.len(), 1);
    assert!(
        !pack.items[0].content.contains("<external_content"),
        "no Stage E wrap when scope not provided"
    );
    assert_eq!(pack.items[0].content, "no annotation here");
}
