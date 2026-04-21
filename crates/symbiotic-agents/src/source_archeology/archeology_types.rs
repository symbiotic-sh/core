//! Source Archeology declarative types and validation.
//!
//! An `ArcheologyTarget` is an invocation-scope description of what to
//! archeologically inspect. Not stored on the `RepoManifest` — authored
//! per-invocation (e.g. by T127 Phase 2 or an `on_drift_detected` hook)
//! and passed to the pipeline runner in this crate's `source_archeology`
//! module.
//!
//! This file contains pure declarative types: no pipeline runner, no
//! stage workers, no daemon wiring. The runner + stage workers live in
//! the sibling modules (`source_archeology::{mod, excavate, date, ...}`).
//! Later stages (§04–§09 of T128) slot in alongside.
//!
//! See `docs/design/source-archeology.md` for the full specification.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Core target ────────────────────────────────────────────────────────

/// A single invocation-scope description of what to archeologically inspect.
/// Not stored on the `RepoManifest` — authored per-invocation (e.g. by T127
/// Phase 2 or an `on_drift_detected` hook) and passed to the pipeline runner.
///
/// See `docs/design/source-archeology.md` §Target Type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcheologyTarget {
    /// `repo:{slug}` — resolved against a `RepoManifest` in the same project.
    pub repo_id: String,
    /// Branch at which to read the source tree. Usually
    /// `RepoManifest.source.default_branch`.
    pub base_branch: String,
    /// The driving goal (artifact routing + checkpoint namespace).
    pub goal_id: String,
    /// Scope fence for sandbox write access and verifier enforcement.
    /// `.gitignore`-style globs (same grammar as
    /// `RepoManifest.indexing.exclude_patterns`).
    #[serde(default)]
    pub allowed_paths: Vec<PathPattern>,
    /// Execution mode: full pipeline, dry-run, or checkpoint refresh only.
    #[serde(default)]
    pub mode: ArcheologyMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArcheologyMode {
    /// Run all six stages, write outputs.
    #[default]
    Full,
    /// Excavate → Date → Diagnose → Triage only. No patchsets, no scaffolds,
    /// no push.
    DryRun,
    /// Update `archeology-checkpoint.json` only; emit no findings.
    CheckpointOnly,
}

/// `.gitignore`-style glob used for scope fencing. A thin newtype over String
/// for now; matching logic lives in the pipeline runner (out of scope here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathPattern(pub String);

// ── Staleness + diagnosis ──────────────────────────────────────────────

/// Per-artifact staleness classification (Stage 2 output row).
/// See `docs/design/source-archeology.md` §Stage 2 — Date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StalenessClass {
    Fresh,
    Drifting,
    Stale,
    Aspirational,
    Dead,
}

/// Diagnose verdict (Stage 3 branch classifier).
/// See `docs/design/source-archeology.md` §Stage 3 — Diagnose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosisVerdict {
    /// Docs exist, are mostly accurate, patchable via Reconcile.
    Salvageable,
    /// Docs exist but are so stale / aspirational that patching costs more
    /// than rewriting.
    StaleBeyondSalvage,
    /// No relevant docs found.
    NoDocs,
    /// Confidence below threshold; operator must decide branch.
    NeedsOperatorInput,
}

/// Full Stage 3 output. Verdict + confidence + rationale — confidence lets
/// the runner enforce a conservative floor that forces
/// `NeedsOperatorInput` when the classifier is uncertain; rationale is
/// surfaced in Handoff reports and archived to the checkpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnosis {
    pub verdict: DiagnosisVerdict,
    /// Confidence in [0.0, 1.0]. Below `DiagnoseConfig::confidence_floor`
    /// the verdict is forced to `NeedsOperatorInput` regardless of the
    /// classifier's underlying pick.
    pub confidence: f32,
    /// Short structured rationale: 1-5 sentences the operator can read.
    pub reasoning: String,
}

// ── Finding ────────────────────────────────────────────────────────────

/// A Finding is a single flagged issue Reconcile or Scaffold produced.
/// Triage (Stage 5) assigns a disposition; Verify (Stage 6) double-checks
/// findings with `resolve` disposition before they're dispatched.
///
/// See `docs/design/source-archeology.md` §Stage 5 — Triage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Uuid of this finding. Stable across Triage + Verify.
    pub id: String,
    /// Which source stage produced this finding.
    pub source_stage: FindingSourceStage,
    pub severity: FindingSeverity,
    /// Short category tag, e.g. "drift", "aspirational_claim", "missing_doc",
    /// "broken_reference", "security". Free-form string — not enforced here;
    /// taxonomy evolves with the pipeline.
    pub category: String,
    /// The source path the finding relates to (relative to repo root).
    pub evidence_path: String,
    /// Human-readable description of the finding.
    pub description: String,
    /// Proposed action — what Reconcile/Scaffold wants to do if Triage
    /// approves.
    pub proposed_action: FindingAction,
    /// Goal alignment as seen by the pipeline (Triage decision-tree input).
    #[serde(default)]
    pub goal_alignment: GoalAlignment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSourceStage {
    Reconcile,
    Scaffold,
    /// Source-repo finding surfaced during Scaffold's source-side analysis
    /// (e.g. README references a deleted script). Still flows to Triage.
    ScaffoldSourceSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    /// Hard-coded rule: `critical` always → `escalate` regardless of other
    /// inputs.
    Critical,
    High,
    Medium,
    Low,
}

impl FindingSeverity {
    /// Internal numeric weight for ordering — `Low < Medium < High < Critical`.
    /// Used by `Ord` / `PartialOrd` so `severity ≤ max_severity` thresholds
    /// (e.g. `auto_approve_max_severity`,
    /// `triage_out_of_scope_defer_max_severity`) read the natural way:
    /// `Some(Medium)` covers `Low + Medium`. Declaration order would order
    /// `Critical < High < ... < Low` which reads backward, so we don't
    /// `derive(Ord)`.
    fn rank(self) -> u8 {
        match self {
            FindingSeverity::Low => 0,
            FindingSeverity::Medium => 1,
            FindingSeverity::High => 2,
            FindingSeverity::Critical => 3,
        }
    }
}

impl PartialOrd for FindingSeverity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FindingSeverity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalAlignment {
    InScope,
    #[default]
    OutOfScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FindingAction {
    /// A unified diff against the source repo (Reconcile output).
    Patch { diff: String },
    /// A new file to write into `{slug}-docs` via T127 Auto-Provision
    /// (Scaffold output).
    NewFile { path: String, content: String },
    /// An operator-facing report entry (Handoff or Triage=escalate).
    Report { text: String },
}

/// Triage output — per-finding disposition.
/// See `docs/design/source-archeology.md` §Stage 5 — Triage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageDecision {
    pub finding_id: String,
    pub disposition: FindingDisposition,
    /// Why the tree picked this disposition. Free-form; captured for audit.
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingDisposition {
    Resolve,
    Defer,
    Escalate,
}

/// Operator autonomy level, consumed by Stage 5 Triage as a
/// decision-tree input. Derived from `RepoManifest.autonomy` (or a
/// caller-supplied override at pipeline-orchestration time); future
/// chunks may also fold in thread-signal overrides per the design
/// doc §Stage 5.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Autonomy {
    /// Agent proceeds without per-finding approval; weights Triage
    /// toward `Resolve`.
    Auto,
    /// Default — mixed: per-finding disposition based on severity +
    /// scope + peer review.
    #[default]
    Semi,
    /// Operator wants full control; weights Triage toward `Escalate`.
    Manual,
}

// ── Errors ─────────────────────────────────────────────────────────────

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArcheologyError {
    #[error("invalid repo_id format (expected 'repo:<slug>'): {0}")]
    InvalidRepoId(String),
    #[error("empty base_branch")]
    EmptyBaseBranch,
    #[error("empty goal_id")]
    EmptyGoalId,
}

// ── Validation ─────────────────────────────────────────────────────────

impl ArcheologyTarget {
    /// Load-time schema check. Not a security boundary — that's the
    /// pipeline runner's job. This validates caller-supplied target
    /// shapes early.
    pub fn validate(&self) -> Result<(), ArcheologyError> {
        if !self.repo_id.starts_with("repo:") {
            return Err(ArcheologyError::InvalidRepoId(self.repo_id.clone()));
        }
        if self.base_branch.trim().is_empty() {
            return Err(ArcheologyError::EmptyBaseBranch);
        }
        if self.goal_id.trim().is_empty() {
            return Err(ArcheologyError::EmptyGoalId);
        }
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn minimum_valid_target() -> ArcheologyTarget {
        ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "main".to_string(),
            goal_id: "onboard".to_string(),
            allowed_paths: Vec::new(),
            mode: ArcheologyMode::default(),
        }
    }

    #[test]
    fn archeology_mode_defaults_to_full() {
        assert_eq!(ArcheologyMode::default(), ArcheologyMode::Full);
    }

    #[test]
    fn target_validate_rejects_bad_repo_id_prefix() {
        let mut target = minimum_valid_target();
        target.repo_id = "flux".to_string();
        let err = target.validate().unwrap_err();
        assert_eq!(err, ArcheologyError::InvalidRepoId("flux".to_string()));
    }

    #[test]
    fn target_validate_rejects_empty_base_branch() {
        let mut target = minimum_valid_target();
        target.base_branch = "   ".to_string();
        let err = target.validate().unwrap_err();
        assert_eq!(err, ArcheologyError::EmptyBaseBranch);
    }

    #[test]
    fn target_validate_rejects_empty_goal_id() {
        let mut target = minimum_valid_target();
        target.goal_id = "".to_string();
        let err = target.validate().unwrap_err();
        assert_eq!(err, ArcheologyError::EmptyGoalId);
    }

    #[test]
    fn target_validate_accepts_minimum_valid() {
        let target = minimum_valid_target();
        assert_eq!(target.validate(), Ok(()));
    }

    #[test]
    fn finding_serde_roundtrip() {
        let finding = Finding {
            id: "f-1".to_string(),
            source_stage: FindingSourceStage::Reconcile,
            severity: FindingSeverity::High,
            category: "drift".to_string(),
            evidence_path: "docs/README.md".to_string(),
            description: "Section X references a deleted script.".to_string(),
            proposed_action: FindingAction::Patch {
                diff: "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            },
            goal_alignment: GoalAlignment::InScope,
        };
        let s = serde_yml::to_string(&finding).unwrap();
        let parsed: Finding = serde_yml::from_str(&s).unwrap();
        assert_eq!(parsed.id, finding.id);
        assert_eq!(parsed.source_stage, finding.source_stage);
        assert_eq!(parsed.severity, finding.severity);
        assert_eq!(parsed.category, finding.category);
        assert_eq!(parsed.evidence_path, finding.evidence_path);
        assert_eq!(parsed.description, finding.description);
        assert_eq!(parsed.goal_alignment, finding.goal_alignment);
        match (parsed.proposed_action, finding.proposed_action) {
            (FindingAction::Patch { diff: a }, FindingAction::Patch { diff: b }) => {
                assert_eq!(a, b);
            }
            _ => panic!("expected Patch on both sides of roundtrip"),
        }
    }

    #[test]
    fn finding_action_tagged_serde() {
        let action = FindingAction::NewFile {
            path: "docs/new.md".to_string(),
            content: "# Hello\n".to_string(),
        };
        let s = serde_yml::to_string(&action).unwrap();
        assert!(
            s.contains("kind: new_file"),
            "expected `kind: new_file` in serialized form, got:\n{s}"
        );
    }

    #[test]
    fn diagnosis_verdict_snake_case() {
        let s = serde_yml::to_string(&DiagnosisVerdict::StaleBeyondSalvage).unwrap();
        assert!(
            s.contains("stale_beyond_salvage"),
            "expected snake_case serialization, got: {s}"
        );
    }

    #[test]
    fn staleness_class_has_five_variants() {
        // Exhaustiveness is statically enforced by the `match` below. If a
        // variant is added or removed, this file will fail to compile.
        fn tag(c: StalenessClass) -> &'static str {
            match c {
                StalenessClass::Fresh => "fresh",
                StalenessClass::Drifting => "drifting",
                StalenessClass::Stale => "stale",
                StalenessClass::Aspirational => "aspirational",
                StalenessClass::Dead => "dead",
            }
        }
        assert_eq!(tag(StalenessClass::Fresh), "fresh");
        assert_eq!(tag(StalenessClass::Drifting), "drifting");
        assert_eq!(tag(StalenessClass::Stale), "stale");
        assert_eq!(tag(StalenessClass::Aspirational), "aspirational");
        assert_eq!(tag(StalenessClass::Dead), "dead");
    }

    #[test]
    fn triage_disposition_serde() {
        let s = serde_yml::to_string(&FindingDisposition::Escalate).unwrap();
        assert!(
            s.contains("escalate"),
            "expected 'escalate' in serialized form, got: {s}"
        );
        let parsed: FindingDisposition = serde_yml::from_str(&s).unwrap();
        assert_eq!(parsed, FindingDisposition::Escalate);
    }
}
