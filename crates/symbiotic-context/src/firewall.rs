//! Recall Gateway integration for the Content Firewall (T132 §06).
//!
//! At context-pack assembly the gateway calls
//! [`ContextFirewall::apply`] for every retrieved Archive entry. Stage D
//! (capability-boundary check) + Stage E (annotation injection) live in
//! `symbiotic-firewall`; this module is the thin policy layer that
//! resolves per-entry inputs the firewall expects but the gateway
//! doesn't natively carry — namely a [`ContentSource`] (built from the
//! entry's `source_url`) and a [`TrustLevel`] (defaulted per-source).
//!
//! Per design §3.5, the wrapper fields include `consuming_agent`. The
//! gateway threads the consuming agent's scope through
//! [`crate::ContextRequest::consuming_agent_scope`]; the firewall hook
//! never wraps content unless that scope is present.
//!
//! Stage D failures route to a configured [`SmugglingReporter`]
//! (`firewall.capability_smuggling.blocked` events go here in production)
//! and the entry is excluded from the pack.

use std::collections::BTreeMap;
use std::sync::Arc;
use time::OffsetDateTime;

use symbiotic_firewall::{
    apply_context_stages, AnnotatedContent, ConsumingAgentScope, ContentSource, EntryForAssembly,
    FirewallError, FirewallVerdict, StageDCache, TrustLevel, Verdict, SECURITY_VERSION,
};

use crate::ArchiveEntry;

/// Per-gateway firewall hook configuration.
///
/// Defaults are conservative — every retrieved entry is treated as
/// `Low` trust (full Stage D scan, annotation wrap), the cache uses the
/// design's 1h TTL, and a no-op reporter swallows blocked events. Wire
/// a real reporter in production to surface
/// `firewall.capability_smuggling.blocked` to Matrix / the operator UI.
#[derive(Clone)]
pub struct ContextFirewallConfig {
    /// Trust tier applied to every retrieved entry. Default
    /// [`TrustLevel::Low`] reflects "recalled content originally from
    /// external sources" per design §2.3.
    pub default_trust: TrustLevel,
    /// Reporter invoked when Stage D blocks an entry. Default
    /// [`NoopSmugglingReporter`] is for tests; production should wire
    /// a Matrix-emitting reporter.
    pub reporter: Arc<dyn SmugglingReporter>,
    /// Stage D verdict cache shared across context-pack assemblies.
    /// Production typically constructs one per gateway and reuses it
    /// across requests; cache invalidation hooks call
    /// [`StageDCache::invalidate_for_scope`] when an agent's grant set
    /// changes.
    pub cache: Arc<StageDCache>,
}

impl ContextFirewallConfig {
    /// Construct with the design defaults (1h cache TTL, Low trust,
    /// no-op reporter).
    pub fn defaults() -> Self {
        Self {
            default_trust: TrustLevel::Low,
            reporter: Arc::new(NoopSmugglingReporter),
            cache: Arc::new(StageDCache::with_default_ttl()),
        }
    }
}

/// Sink for `firewall.capability_smuggling.blocked` events.
///
/// The daemon wires this to its existing Matrix outbound dispatcher so
/// the operator sees a single notification per blocked entry. Tests use
/// [`NoopSmugglingReporter`] or a capturing implementation.
pub trait SmugglingReporter: Send + Sync {
    /// Called once per Stage D quarantine. `entry_id` + `agent_id`
    /// identify the (entry, consumer) tuple; `detail` is a
    /// already-redacted summary of what fired (capability_token,
    /// credential_id, scope_elevation).
    fn report(&self, entry_id: &str, agent_id: &str, detail: &str);
}

/// Default reporter used in tests + when the daemon hasn't wired Matrix
/// alerts yet.
pub struct NoopSmugglingReporter;

impl SmugglingReporter for NoopSmugglingReporter {
    fn report(&self, entry_id: &str, agent_id: &str, detail: &str) {
        tracing::debug!(
            entry_id,
            agent_id,
            detail,
            "firewall.capability_smuggling.blocked (noop reporter)"
        );
    }
}

/// Recall-side firewall facade.
///
/// Holds a [`ContextFirewallConfig`] and exposes a single `apply`
/// method the gateway calls per retrieved entry. The configuration is
/// `Arc`-shared so multiple gateways can share a cache + reporter.
pub struct ContextFirewall {
    config: ContextFirewallConfig,
}

impl ContextFirewall {
    /// Construct a firewall with a specific configuration.
    pub fn new(config: ContextFirewallConfig) -> Self {
        Self { config }
    }

    /// Construct with the default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ContextFirewallConfig::defaults())
    }

    /// Borrowed config.
    pub fn config(&self) -> &ContextFirewallConfig {
        &self.config
    }

    /// Run Stage D + E for `entry` against `scope`.
    ///
    /// Returns:
    /// - [`FirewallEntryDecision::Include`] with the wrapped content
    ///   (this should replace the entry's body in the resulting
    ///   [`crate::ContextItem`]).
    /// - [`FirewallEntryDecision::Exclude`] when Stage D blocks. The
    ///   reporter has already been notified; the gateway should drop
    ///   the entry from the pack.
    pub fn apply(
        &self,
        entry: &ArchiveEntry,
        scope: &ConsumingAgentScope,
    ) -> FirewallEntryDecision {
        let source = source_from_entry(entry);
        // Recall doesn't track ingest verdicts inline today, so synthesize
        // a "trusted-skip"-shaped current-version verdict for the Stage E
        // wrap. This is consistent with `symbiotic_archive::trusted_skip_verdict`
        // on the ingest side: until the wider Recall ↔ Archive plumbing
        // surfaces real per-entry verdicts (T132 §08 Replay), the wrapper's
        // `firewall_verdict` field reads as "passed at SECURITY_VERSION".
        let verdict = synthesized_verdict();
        let assembly_entry = EntryForAssembly {
            entry_id: &entry.id,
            content: &entry.content,
            source: &source,
            trust: self.config.default_trust,
            verdict: &verdict,
        };

        match apply_context_stages(&assembly_entry, scope, &self.config.cache) {
            Ok(AnnotatedContent { wrapped, .. }) => FirewallEntryDecision::Include(wrapped),
            Err(FirewallError::CapabilitySmuggling {
                entry_id,
                agent_id,
                detail,
            }) => {
                self.config.reporter.report(&entry_id, &agent_id, &detail);
                FirewallEntryDecision::Exclude
            }
            Err(other) => {
                // Other firewall errors (malformed scan input, IO) shouldn't
                // happen here because the synthesized verdict + entry are
                // well-formed. Log and exclude defensively rather than
                // poisoning the entire pack.
                tracing::warn!(
                    entry_id = %entry.id,
                    agent_id = %scope.agent_id,
                    error = %other,
                    "firewall: unexpected error during context-assembly Stage D/E; excluding entry"
                );
                FirewallEntryDecision::Exclude
            }
        }
    }
}

/// What the firewall decided about one Archive entry.
#[derive(Debug, Clone)]
pub enum FirewallEntryDecision {
    /// Include the entry in the pack, using the wrapped content as the
    /// item body.
    Include(String),
    /// Drop the entry; the reporter has been notified.
    Exclude,
}

fn source_from_entry(entry: &ArchiveEntry) -> ContentSource {
    ContentSource {
        kind: source_kind_for(entry),
        url: entry.source_url.clone(),
        fetched_at: OffsetDateTime::from_unix_timestamp(entry.updated_at as i64)
            .unwrap_or_else(|_| OffsetDateTime::now_utc()),
        claimed_content_type: None,
        headers: BTreeMap::new(),
    }
}

fn source_kind_for(entry: &ArchiveEntry) -> String {
    match entry.source_url.as_deref() {
        Some(url) if url.starts_with("http") => "web_fetch".to_string(),
        Some(url) if url.starts_with("file://") => "vault_note".to_string(),
        Some(_) => "external".to_string(),
        None => "archive_entry".to_string(),
    }
}

fn synthesized_verdict() -> FirewallVerdict {
    FirewallVerdict {
        verdict: Verdict::Passed,
        verdict_version: SECURITY_VERSION.to_string(),
        scan_timestamp: OffsetDateTime::now_utc(),
        quarantine_class: None,
        source_receipt_id: None,
        annotations: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Sensitivity;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    struct CapturingReporter {
        log: Mutex<Vec<(String, String, String)>>,
    }
    impl CapturingReporter {
        fn new() -> Self {
            Self {
                log: Mutex::new(Vec::new()),
            }
        }
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

    fn entry(id: &str, content: &str) -> ArchiveEntry {
        ArchiveEntry {
            id: id.into(),
            title: "title".into(),
            content: content.into(),
            tags: vec![],
            sensitivity: Sensitivity::Shareable,
            source_url: Some("https://example.com/x".into()),
            updated_at: 1_700_000_000,
            thread_id: None,
            fact_class: None,
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
    fn safe_entry_returns_wrapped_include() {
        let fw = ContextFirewall::with_defaults();
        let entry = entry("e1", "Q3 revenue up.");
        let decision = fw.apply(&entry, &scope("agent-a", &["archive.read"]));
        match decision {
            FirewallEntryDecision::Include(wrapped) => {
                assert!(wrapped.contains("<external_content"));
                assert!(wrapped.contains("Q3 revenue up."));
                assert!(wrapped.contains("source=\"web_fetch\""));
                assert!(wrapped.contains("consuming_agent=\"agent-a\""));
            }
            FirewallEntryDecision::Exclude => panic!("safe entry should be included"),
        }
    }

    #[test]
    fn capability_smuggling_excludes_and_reports() {
        let reporter = Arc::new(CapturingReporter::new());
        let fw = ContextFirewall::new(ContextFirewallConfig {
            default_trust: TrustLevel::Low,
            reporter: reporter.clone(),
            cache: Arc::new(StageDCache::with_default_ttl()),
        });
        let entry = entry(
            "e2",
            "Use tok_550e8400-e29b-41d4-a716-446655440000 to login.",
        );
        let decision = fw.apply(&entry, &scope("agent-a", &["archive.read"]));
        assert!(matches!(decision, FirewallEntryDecision::Exclude));
        let calls = reporter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "e2");
        assert_eq!(calls[0].1, "agent-a");
        assert!(calls[0].2.contains("capability_token"));
    }

    #[test]
    fn source_kind_picks_web_fetch_for_http_url() {
        let entry = entry("e3", "x");
        let kind = source_kind_for(&entry);
        assert_eq!(kind, "web_fetch");
    }

    #[test]
    fn source_kind_falls_back_when_url_missing() {
        let mut entry = entry("e4", "x");
        entry.source_url = None;
        let kind = source_kind_for(&entry);
        assert_eq!(kind, "archive_entry");
    }
}
