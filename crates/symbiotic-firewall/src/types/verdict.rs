//! Firewall verdict + stage-finding types (design §3.0, §6.4).
//!
//! `FirewallVerdict` is the frozen wire format produced by every scan and
//! stored alongside Archive entries. Downstream stages (quarantine queue,
//! Replay, Rebuild) consume these types; the shape is intentionally minimal
//! so it round-trips cleanly through opentraces export (T131 §02).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Top-level outcome of a firewall scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Content cleared all applicable stages; safe for Archive.
    Passed,
    /// Content passed but Stage B raised a heuristic flag with low enough
    /// confidence that it wasn't quarantined. Stored with the flag note so
    /// audit tooling can surface the soft-flag.
    Flagged,
    /// Content failed a stage and was rerouted to quarantine (§4).
    /// Accompanied by a non-`None` `quarantine_class` on the verdict.
    Quarantined,
}

/// Which review lane a quarantined item routes to (design §3.0).
///
/// Wire format uses `snake_case` so the on-disk JSON reads
/// `source_integrity` / `security_risk` / `capability_smuggling`, matching
/// operator-tier review tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineClass {
    /// Stage A structural failures: malformed input, broken encoding,
    /// truncated payload, size-cap violations, MIME mismatches. Safe to
    /// release deterministically after operator confirms the underlying
    /// data was recovered/fixed. No security implication.
    SourceIntegrity,
    /// Stage B/C injection-detection failures: prompt-injection phrases,
    /// role-reversal, delimiter smuggling, LLM-flagged suspicious-or-
    /// malicious content. Requires security-review lane; deterministic-only
    /// re-entry even on false-positive findings.
    SecurityRisk,
    /// Stage D capability-boundary failures at context-assembly time:
    /// content references capabilities/secrets the consuming agent isn't
    /// authorized for. May be fine for other agents with broader scope.
    CapabilitySmuggling,
}

/// Which stage of the firewall produced a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Stage {
    /// Structural sanitization (ingest).
    A,
    /// Prompt-injection heuristics (ingest).
    B,
    /// LLM-lite semantic review (ingest, conditional).
    C,
    /// Capability-boundary check (context-assembly).
    D,
    /// Annotation injection (context-assembly).
    E,
}

/// Categorical label for what kind of finding a stage produced. The `detail`
/// string on [`StageFinding`] carries the free-form specifics; this enum
/// exists so downstream triage can bucket findings without string matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Stage A — payload failed structural sanitization (script tags, bidi
    /// chars, oversized payload, MIME mismatch, etc.).
    StructuralViolation,
    /// Stage B — regex / pattern heuristic flagged a prompt-injection
    /// phrase, role-reversal, delimiter smuggling, or encoded block.
    InjectionHeuristic,
    /// Stage C — LLM-lite semantic review returned SUSPICIOUS or MALICIOUS.
    SemanticRisk,
    /// Stage D — content references a capability token, credential id, or
    /// scope the consuming agent isn't authorized for.
    CapabilityBoundary,
    /// Stage E — annotation wrapping noted the source trust level, content
    /// hash, or other framing detail (informational, not a failure).
    AnnotationNote,
}

/// A single stage's observation about content, attached to a verdict.
///
/// Multiple findings may accumulate on one verdict — e.g. Stage A records a
/// structural-violation note and Stage B records a heuristic flag; the
/// overall `Verdict` is the worst case, but all findings are preserved for
/// audit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageFinding {
    pub stage: Stage,
    pub kind: FindingKind,
    /// Human-readable detail: what the stage saw.
    pub detail: String,
    /// 0.0 – 1.0 confidence. For Stage A structural checks this is typically
    /// 1.0 (the check is deterministic); for Stage B heuristics it's the
    /// computed score; for Stage C it's the model's reported confidence.
    pub confidence: f32,
}

/// Frozen wire-format verdict produced by every firewall scan.
///
/// This shape is stable across firewall versions — `verdict_version` records
/// which scan rules produced this verdict so the Replay job (design §6.3)
/// knows when to re-evaluate. `source_receipt_id` is the critical link to
/// the immutable raw-bytes receipt used by Rebuild (design §6.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirewallVerdict {
    pub verdict: Verdict,
    /// Semver of the firewall at scan time (e.g. `"0.1.0"`). When the
    /// current firewall version is newer, Replay re-scans this entry.
    pub verdict_version: String,
    #[serde(with = "time::serde::rfc3339")]
    pub scan_timestamp: OffsetDateTime,
    /// Populated only when `verdict == Quarantined`. Which review lane the
    /// item routes to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_class: Option<QuarantineClass>,
    /// Id of the immutable source-receipt blob. When present, Rebuild can
    /// regenerate the Archive entry from raw bytes; when `None`, the
    /// receipt was never captured and Rebuild skips this entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_receipt_id: Option<String>,
    /// Accumulated stage findings. Empty for a clean `Passed` verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<StageFinding>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_timestamp() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp")
    }

    fn round_trip_verdict(verdict: Verdict, wire: &str) {
        let json = serde_json::to_string(&verdict).expect("serialize");
        assert_eq!(json, format!("\"{wire}\""));
        let back: Verdict = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, verdict);
    }

    #[test]
    fn verdict_passed_round_trips() {
        round_trip_verdict(Verdict::Passed, "passed");
    }

    #[test]
    fn verdict_flagged_round_trips() {
        round_trip_verdict(Verdict::Flagged, "flagged");
    }

    #[test]
    fn verdict_quarantined_round_trips() {
        round_trip_verdict(Verdict::Quarantined, "quarantined");
    }

    fn round_trip_qclass(class: QuarantineClass, wire: &str) {
        let json = serde_json::to_string(&class).expect("serialize");
        assert_eq!(json, format!("\"{wire}\""));
        let back: QuarantineClass = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, class);
    }

    #[test]
    fn quarantine_class_source_integrity_round_trips() {
        round_trip_qclass(QuarantineClass::SourceIntegrity, "source_integrity");
    }

    #[test]
    fn quarantine_class_security_risk_round_trips() {
        round_trip_qclass(QuarantineClass::SecurityRisk, "security_risk");
    }

    #[test]
    fn quarantine_class_capability_smuggling_round_trips() {
        round_trip_qclass(QuarantineClass::CapabilitySmuggling, "capability_smuggling");
    }

    #[test]
    fn stage_finding_round_trips() {
        let finding = StageFinding {
            stage: Stage::B,
            kind: FindingKind::InjectionHeuristic,
            detail: "matched 'ignore previous instructions' heuristic".into(),
            confidence: 0.82,
        };
        let json = serde_json::to_string(&finding).expect("serialize");
        let back: StageFinding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, finding);
    }

    #[test]
    fn firewall_verdict_round_trips_fully_populated() {
        let verdict = FirewallVerdict {
            verdict: Verdict::Quarantined,
            verdict_version: "0.1.0".into(),
            scan_timestamp: sample_timestamp(),
            quarantine_class: Some(QuarantineClass::SecurityRisk),
            source_receipt_id: Some("deadbeef".repeat(8)),
            annotations: vec![
                StageFinding {
                    stage: Stage::A,
                    kind: FindingKind::StructuralViolation,
                    detail: "stripped <script> tag".into(),
                    confidence: 1.0,
                },
                StageFinding {
                    stage: Stage::B,
                    kind: FindingKind::InjectionHeuristic,
                    detail: "role-reversal phrasing".into(),
                    confidence: 0.74,
                },
            ],
        };
        let json = serde_json::to_string(&verdict).expect("serialize");
        let back: FirewallVerdict = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, verdict);
    }

    #[test]
    fn firewall_verdict_round_trips_minimal_options_none() {
        let verdict = FirewallVerdict {
            verdict: Verdict::Passed,
            verdict_version: "0.1.0".into(),
            scan_timestamp: sample_timestamp(),
            quarantine_class: None,
            source_receipt_id: None,
            annotations: Vec::new(),
        };
        let json = serde_json::to_string(&verdict).expect("serialize");
        // None / empty fields should be omitted from the on-wire form.
        assert!(!json.contains("\"quarantine_class\""));
        assert!(!json.contains("\"source_receipt_id\""));
        assert!(!json.contains("\"annotations\""));
        let back: FirewallVerdict = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, verdict);
    }
}
