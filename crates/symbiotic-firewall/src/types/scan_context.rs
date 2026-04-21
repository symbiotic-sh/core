//! Scan context — the structured input to every firewall scan call.
//!
//! [`ScanContext`] bundles the payload, the provenance, the consuming agent's
//! capability scope, and the call site (which ingest boundary fired). Stages
//! A–C only need source + payload; Stage D additionally needs
//! `consuming_agent_scope` to perform capability-boundary comparisons.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::source::ContentSource;

/// Which ingest / context-assembly boundary fired this scan. Used for
/// routing alerts and for per-boundary telemetry. Free-form string — the
/// firewall crate does not enumerate call sites; callers pass a short
/// identifier (e.g. `"intake.url"`, `"tool.observation"`,
/// `"context_assembly.recall"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallSite(pub String);

impl CallSite {
    /// Construct a call site from anything string-convertible.
    pub fn new<S: Into<String>>(label: S) -> Self {
        CallSite(label.into())
    }

    /// Borrow the underlying label.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<S: Into<String>> From<S> for CallSite {
    fn from(label: S) -> Self {
        CallSite::new(label)
    }
}

/// Capability scope of the agent that will consume the scanned content.
///
/// Stage D (capability-boundary check) compares references found inside the
/// content against this scope. An empty scope means "no broader-scope
/// escalation is permitted" — any capability token embedded in content
/// fails the check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumingAgentScope {
    /// Stable id of the consuming agent (goal id, sub-agent id, etc.).
    /// Used for cache-key construction in Stage D verdict caching.
    pub agent_id: String,
    /// Capability scopes the consuming agent is authorized for (e.g.
    /// `{"archive.read", "tools.web_fetch"}`). Ordered set for stable
    /// cache-key hashing.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub allowed_scopes: BTreeSet<String>,
}

impl ConsumingAgentScope {
    /// Construct a scope with a single agent id and no capabilities — the
    /// strictest possible scope. Useful for ingest-time scans where the
    /// future consumer is unknown (Stage D will re-run at assembly time
    /// with the real scope).
    pub fn minimal<S: Into<String>>(agent_id: S) -> Self {
        Self {
            agent_id: agent_id.into(),
            allowed_scopes: BTreeSet::new(),
        }
    }
}

/// Structured input to every firewall scan call.
///
/// Serializable so it round-trips through opentraces (T131 §02). Does **not**
/// carry the raw payload bytes — those are passed separately to the scan
/// engine to avoid accidental logging of content inside a context envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanContext {
    pub source: ContentSource,
    pub consuming_agent_scope: ConsumingAgentScope,
    pub call_site: CallSite,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn sample_source() -> ContentSource {
        ContentSource {
            kind: "web_fetch".into(),
            url: Some("https://example.com/doc".into()),
            fetched_at: OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("valid timestamp"),
            claimed_content_type: Some("text/html".into()),
            headers: Default::default(),
        }
    }

    #[test]
    fn call_site_from_str() {
        let cs: CallSite = "intake.url".into();
        assert_eq!(cs.as_str(), "intake.url");
    }

    #[test]
    fn consuming_agent_scope_minimal_has_empty_scopes() {
        let scope = ConsumingAgentScope::minimal("agent-1");
        assert_eq!(scope.agent_id, "agent-1");
        assert!(scope.allowed_scopes.is_empty());
    }

    #[test]
    fn scan_context_round_trips() {
        let mut scopes = BTreeSet::new();
        scopes.insert("archive.read".to_string());
        scopes.insert("tools.web_fetch".to_string());
        let ctx = ScanContext {
            source: sample_source(),
            consuming_agent_scope: ConsumingAgentScope {
                agent_id: "agent-42".into(),
                allowed_scopes: scopes,
            },
            call_site: CallSite::new("intake.url"),
        };
        let json = serde_json::to_string(&ctx).expect("serialize");
        let back: ScanContext = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ctx);
    }
}
