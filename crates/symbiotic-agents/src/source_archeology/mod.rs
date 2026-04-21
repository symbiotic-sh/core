//! Source Archeology pipeline.
//!
//! Six-stage pipeline for deep repo inspection with triageable findings:
//! `Excavate → Date → Diagnose → {Reconcile | Scaffold | Handoff} → Triage → Verify`.
//!
//! See `docs/design/source-archeology.md` for the full specification.
//!
//! ## Module map
//!
//! - [`archeology_types`] — declarative types (`ArcheologyTarget`, `Finding`, …).
//! - [`excavate`] — Stage 1 worker.
//! - [`date`] — Stage 2 worker + LLM-backed aspirational classifier.
//! - [`diagnose`] — Stage 3 branch classifier.
//! - [`reconcile`] — Stage 4a patch producer.
//! - [`scaffold`] — Stage 4b docs-package producer.
//! - [`handoff`] — Stage 4c operator-report compiler.
//! - `fixtures` — test harness (`#[cfg(test)]` only).

pub mod archeology_types;
pub mod checkpoint;
pub mod contract;
pub mod date;
pub mod diagnose;
pub mod excavate;
pub mod handoff;
pub mod orchestrator;
pub mod reconcile;
pub mod scaffold;
pub mod scaffold_templates;
pub mod triage;
pub mod verify;

#[cfg(test)]
pub mod fixtures;

use anyhow::Result;
use std::path::Path;

pub use archeology_types::{
    ArcheologyError, ArcheologyMode, ArcheologyTarget, Autonomy, Diagnosis, DiagnosisVerdict,
    Finding, FindingAction, FindingDisposition, FindingSeverity, FindingSourceStage, GoalAlignment,
    PathPattern, StalenessClass, TriageDecision,
};
pub use checkpoint::{
    build_checkpoint, find_latest_checkpoint, read_checkpoint, write_checkpoint,
    ArcheologyCheckpoint, AspirationalClaim, CheckpointConfig, DiscrepancyEntry, OpenIssue,
    StalenessRecord, TriageSummary, CHECKPOINT_SCHEMA_VERSION, DEFAULT_DEFER_TTL_DAYS,
};
pub use contract::{
    ArcheologyInput, ArcheologyOutput, LlmConfig, PipelineOutcomeKind, ProjectContextWire,
    StageConfigsWire,
};
pub use date::{AspirationalClassifier, LlmAspirationalClassifier, StalenessReport, StalenessRow};
pub use diagnose::{
    DiagnoseConfig, DiagnosisClassifier, DiagnosisProjection, LlmDiagnosisClassifier, RawDiagnosis,
};
pub use excavate::{ExcavationReport, Observation, ObservationCategory};
pub use handoff::{
    HandoffConfig, HandoffInput, HandoffReport, LlmReporter, OperatorQuestion, Reporter,
};
pub use orchestrator::{OrchestratorInput, PipelineOutcome, PipelineRun};
pub use reconcile::{LlmReconciler, ReconcileInput, ReconciledPatch, Reconciler};
pub use scaffold::{
    LlmScaffolder, ProjectContext, ScaffoldConfig, ScaffoldFile, ScaffoldInput, ScaffoldOutput,
    Scaffolder, SourceSidePatch,
};
pub use triage::{
    DeclarativeTriager, LlmTriager, RawDisposition, Reviewer, TriageConfig, TriageContext, Triager,
};
pub use verify::{
    GitApplyCheckVerifier, LintVerifier, MarkdownlintVerifier, NoopLintVerifier, PatchVerifier,
    VerifyConfig, VerifyOutcome,
};

/// Bundle of stage LLM classifiers passed to the runner. Grouping these
/// into one struct keeps the constructor surface flat as future stages
/// (§08 Triage, peer-review, future verifiers) add more seams — and
/// keeps each classifier reference addressable by name at call sites.
pub struct Classifiers<'a> {
    pub aspirational: &'a dyn AspirationalClassifier,
    pub diagnosis: &'a dyn DiagnosisClassifier,
    pub reconciler: &'a dyn Reconciler,
    pub scaffolder: &'a dyn Scaffolder,
    pub reporter: &'a dyn Reporter,
    pub triager: &'a dyn Triager,
    /// Optional peer reviewer for Stage 5 Triage. `None` disables
    /// peer review (MVP default); post-MVP wiring can supply a second
    /// agent to re-check the primary triager's disposition.
    pub reviewer: Option<&'a dyn Reviewer>,
    pub patch_verifier: &'a dyn PatchVerifier,
    pub lint_verifier: &'a dyn LintVerifier,
}

/// Bundle of per-stage config structs. Each field is `impl Default` so
/// `StageConfigs::default()` is the zero-config path. Per
/// `docs/design/agent-tunables.md`, overrides flow from
/// `RepoManifest.archeology_policy` when that field lands.
#[derive(Debug, Clone, Copy, Default)]
pub struct StageConfigs {
    pub diagnose: DiagnoseConfig,
    pub scaffold: ScaffoldConfig,
    pub handoff: HandoffConfig,
    pub triage: TriageConfig,
    pub verify: VerifyConfig,
}

/// Source Archeology pipeline runner.
///
/// Stateless per-run. Callers supply:
/// 1. An `ArcheologyTarget` describing what to inspect.
/// 2. A filesystem path to the read-only source clone (caller resolves
///    `target.repo_id` → path via the daemon's `RepoRegistry`; keeping
///    that resolution out of this crate preserves the one-way
///    `control-plane → agents` crate dependency).
/// 3. A `Classifiers` bundle — one reference per stage LLM seam.
/// 4. A `StageConfigs` bundle — per-stage tunables per
///    `docs/design/agent-tunables.md`. `StageConfigs::default()` is
///    the zero-config path.
pub struct SourceArcheologyRunner<'a> {
    classifiers: Classifiers<'a>,
    configs: StageConfigs,
}

impl<'a> SourceArcheologyRunner<'a> {
    pub fn new(classifiers: Classifiers<'a>, configs: StageConfigs) -> Self {
        Self {
            classifiers,
            configs,
        }
    }

    /// Stage 1 — Excavate. Deterministic; no LLM.
    pub async fn excavate(
        &self,
        target: &ArcheologyTarget,
        clone_root: &Path,
    ) -> Result<ExcavationReport> {
        excavate::run(target, clone_root)
    }

    /// Stage 2 — Date. Deterministic pre-filter + LLM adjudication on
    /// ambiguous rows.
    pub async fn date(
        &self,
        target: &ArcheologyTarget,
        clone_root: &Path,
        excavation: &ExcavationReport,
    ) -> Result<StalenessReport> {
        date::run(
            target,
            clone_root,
            excavation,
            self.classifiers.aspirational,
        )
        .await
    }

    /// Stage 3 — Diagnose. Aggregates the prior two reports into a single
    /// branch verdict. Applies a confidence floor that forces
    /// `NeedsOperatorInput` when the classifier is uncertain.
    pub async fn diagnose(
        &self,
        excavation: &ExcavationReport,
        staleness: &StalenessReport,
    ) -> Result<Diagnosis> {
        diagnose::run(
            excavation,
            staleness,
            self.classifiers.diagnosis,
            self.configs.diagnose,
        )
        .await
    }

    /// Stage 4a — Reconcile. Produces patches for `Drifting` artifacts,
    /// scoped to `target.allowed_paths`. Returns `Finding` records with
    /// `FindingAction::Patch`; Triage (§08) decides disposition, Verify
    /// (§09) checks applicability before dispatch.
    pub async fn reconcile(
        &self,
        target: &ArcheologyTarget,
        clone_root: &Path,
        excavation: &ExcavationReport,
        staleness: &StalenessReport,
    ) -> Result<Vec<Finding>> {
        reconcile::run(
            target,
            clone_root,
            excavation,
            staleness,
            self.classifiers.reconciler,
        )
        .await
    }

    /// Stage 4b — Scaffold. Produces a fresh docs package (new files
    /// for `{slug}-docs`) plus optional source-side fix findings.
    /// Runs when Diagnose = `StaleBeyondSalvage` or `NoDocs`.
    pub async fn scaffold(
        &self,
        target: &ArcheologyTarget,
        excavation: &ExcavationReport,
        staleness: &StalenessReport,
        diagnosis: &Diagnosis,
        project: &ProjectContext,
    ) -> Result<Vec<Finding>> {
        let input = ScaffoldInput {
            target,
            excavation,
            staleness,
            diagnosis,
            project,
        };
        scaffold::run(&input, self.classifiers.scaffolder, self.configs.scaffold).await
    }

    /// Stage 4c — Handoff. Compiles an operator-facing report +
    /// questions when Diagnose could not confidently pick a branch.
    pub async fn handoff(
        &self,
        target: &ArcheologyTarget,
        excavation: &ExcavationReport,
        staleness: &StalenessReport,
        diagnosis: &Diagnosis,
        project: &ProjectContext,
    ) -> Result<HandoffReport> {
        handoff::run(
            target,
            excavation,
            staleness,
            diagnosis,
            project,
            self.classifiers.reporter,
            self.configs.handoff,
        )
        .await
    }

    /// Stage 5 — Triage. Runs the decision tree across each Finding and
    /// produces `TriageDecision` records. MVP is declarative per design
    /// doc §Stage 5; post-MVP swaps in an LLM-reasoning triager.
    pub async fn triage(
        &self,
        findings: &[Finding],
        ctx: &TriageContext,
    ) -> Result<Vec<TriageDecision>> {
        triage::run(
            findings,
            ctx,
            self.classifiers.triager,
            self.classifiers.reviewer,
            self.configs.triage,
        )
        .await
    }

    /// Stage 6 — Verify. Checks patch applicability + lint on
    /// `Resolve`-disposition decisions; rejects downgrade to `Defer`
    /// with the rejection reason appended to rationale.
    pub async fn verify(
        &self,
        findings: &[Finding],
        decisions: Vec<TriageDecision>,
        clone_root: &Path,
    ) -> Result<Vec<TriageDecision>> {
        verify::run(
            findings,
            decisions,
            clone_root,
            self.classifiers.patch_verifier,
            self.classifiers.lint_verifier,
            self.configs.verify,
        )
        .await
    }
}
