//! Pipeline orchestrator — ties all seven stages together.
//!
//! `run_pipeline()` runs Excavate → Date → Diagnose →
//! {Reconcile | Scaffold | Handoff} → Triage → Verify → Checkpoint
//! for a single `ArcheologyTarget`. Stage errors propagate; stage-level
//! conservative fallbacks (Err → Escalate, Err → Defer, empty output)
//! are handled inside each stage, not here.
//!
//! See `docs/design/source-archeology.md` for the full pipeline spec.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::archeology_types::{
    ArcheologyTarget, Diagnosis, DiagnosisVerdict, Finding, TriageDecision,
};
use super::checkpoint::{
    build_checkpoint, find_latest_checkpoint, write_checkpoint, ArcheologyCheckpoint,
};
use super::date::StalenessReport;
use super::excavate::ExcavationReport;
use super::handoff::HandoffReport;
use super::scaffold::ProjectContext;
use super::triage::TriageContext;
use super::SourceArcheologyRunner;

// ── Types ──────────────────────────────────────────────────────────────

/// Pipeline outcome. Either the full 7-stage run produced output, or
/// the pipeline short-circuited because the prior checkpoint matched
/// the repo's current HEAD AND had no open issues (noop detection per
/// design doc §How the next run uses it).
#[derive(Debug)]
pub enum PipelineOutcome {
    /// Full run produced typed outputs from all stages. Boxed because
    /// `PipelineRun` is large and a sibling `Noop` variant is tiny —
    /// keeps the enum compact on the stack.
    Full(Box<PipelineRun>),
    Noop {
        reason: String,
        current_head: String,
        /// Boxed to equalize variant sizes — `ArcheologyCheckpoint`
        /// carries a moderate-sized `staleness_by_file` map.
        prior_checkpoint: Box<ArcheologyCheckpoint>,
    },
}

impl PipelineOutcome {
    pub fn as_full(&self) -> Option<&PipelineRun> {
        match self {
            Self::Full(r) => Some(r.as_ref()),
            Self::Noop { .. } => None,
        }
    }
    pub fn is_noop(&self) -> bool {
        matches!(self, Self::Noop { .. })
    }
    /// Panics if the outcome is `Noop`. Ergonomic for tests + callers
    /// that know the pipeline cannot noop (e.g. first run on a fresh
    /// checkpoint root).
    pub fn unwrap_full(self) -> PipelineRun {
        match self {
            Self::Full(r) => *r,
            Self::Noop { reason, .. } => panic!("expected Full, got Noop: {reason}"),
        }
    }
}

/// Full pipeline output. Some fields are branch-conditional.
#[derive(Debug)]
pub struct PipelineRun {
    pub excavation: ExcavationReport,
    pub staleness: StalenessReport,
    pub diagnosis: Diagnosis,
    /// Non-empty when Diagnose = Salvageable (Reconcile) or
    /// StaleBeyondSalvage / NoDocs (Scaffold). Empty on
    /// NeedsOperatorInput.
    pub findings: Vec<Finding>,
    /// Populated only when Diagnose = NeedsOperatorInput (Handoff
    /// branch). None otherwise.
    pub handoff: Option<HandoffReport>,
    pub decisions: Vec<TriageDecision>,
    pub checkpoint: ArcheologyCheckpoint,
    pub checkpoint_path: PathBuf,
}

pub struct OrchestratorInput<'a> {
    pub target: &'a ArcheologyTarget,
    pub clone_root: &'a Path,
    pub project: &'a ProjectContext,
    pub triage_ctx: TriageContext,
    /// Root under which per-run checkpoint dirs are created:
    /// `{checkpoint_root}/source-archeology/{timestamp}/`.
    pub checkpoint_root: &'a Path,
    /// Caller-controlled timestamp for deterministic tests.
    pub now: DateTime<Utc>,
}

// ── Orchestrator method on the runner ──────────────────────────────────

impl<'a> SourceArcheologyRunner<'a> {
    /// Run the full pipeline end-to-end. Returns `Noop` if the prior
    /// checkpoint shows the same HEAD and has no open issues.
    pub async fn run_pipeline(&self, input: &OrchestratorInput<'_>) -> Result<PipelineOutcome> {
        // Noop detection — check before Excavate.
        let archeology_root = input.checkpoint_root.join("source-archeology");
        let prior = find_latest_checkpoint(&archeology_root).unwrap_or(None);
        let current_head = current_git_head(input.clone_root);
        if let (Some(head), Some(p)) = (current_head.as_deref(), prior.as_ref()) {
            if head == p.observed_head && p.open_issues.is_empty() {
                return Ok(PipelineOutcome::Noop {
                    reason: format!(
                        "head unchanged ({head}) and prior checkpoint has no open issues"
                    ),
                    current_head: head.to_string(),
                    prior_checkpoint: Box::new(p.clone()),
                });
            }
        }

        // Stage 1 — Excavate.
        let excavation = self.excavate(input.target, input.clone_root).await?;

        // Stage 2 — Date.
        let staleness = self
            .date(input.target, input.clone_root, &excavation)
            .await?;

        // Stage 3 — Diagnose.
        let diagnosis = self.diagnose(&excavation, &staleness).await?;

        // Stage 4 — branch on verdict.
        let (findings, handoff) = match diagnosis.verdict {
            DiagnosisVerdict::Salvageable => {
                let f = self
                    .reconcile(input.target, input.clone_root, &excavation, &staleness)
                    .await?;
                (f, None)
            }
            DiagnosisVerdict::StaleBeyondSalvage | DiagnosisVerdict::NoDocs => {
                let f = self
                    .scaffold(
                        input.target,
                        &excavation,
                        &staleness,
                        &diagnosis,
                        input.project,
                    )
                    .await?;
                (f, None)
            }
            DiagnosisVerdict::NeedsOperatorInput => {
                let h = self
                    .handoff(
                        input.target,
                        &excavation,
                        &staleness,
                        &diagnosis,
                        input.project,
                    )
                    .await?;
                (Vec::new(), Some(h))
            }
        };

        // Stage 5 — Triage (only if there are findings to triage).
        let decisions = if findings.is_empty() {
            Vec::new()
        } else {
            self.triage(&findings, &input.triage_ctx).await?
        };

        // Stage 6 — Verify (only if Triage produced non-empty decisions).
        let verified_decisions = if decisions.is_empty() {
            decisions
        } else {
            self.verify(&findings, decisions, input.clone_root).await?
        };

        // Stage 10 — checkpoint build + write (`prior` was already
        // resolved at the top of the function for noop detection).
        let checkpoint_dir = input
            .checkpoint_root
            .join("source-archeology")
            .join(format_timestamp_dir(input.now));
        let checkpoint = build_checkpoint(
            &excavation,
            &staleness,
            diagnosis.verdict,
            diagnosis.confidence,
            &verified_decisions,
            prior.as_ref(),
            input.now,
            Default::default(),
        );
        let checkpoint_path = write_checkpoint(&checkpoint_dir, &checkpoint)?;

        Ok(PipelineOutcome::Full(Box::new(PipelineRun {
            excavation,
            staleness,
            diagnosis,
            findings,
            handoff,
            decisions: verified_decisions,
            checkpoint,
            checkpoint_path,
        })))
    }
}

fn current_git_head(clone_root: &Path) -> Option<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(clone_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

// ── Helpers ────────────────────────────────────────────────────────────

fn format_timestamp_dir(ts: DateTime<Utc>) -> String {
    // Filesystem-safe RFC3339 (colons replaced with hyphens for Windows
    // compatibility in path names, even though our current targets are
    // Unix-only).
    ts.format("%Y-%m-%dT%H-%M-%SZ").to_string()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::{
        ArcheologyMode, Autonomy, FindingAction, FindingDisposition, FindingSeverity,
        FindingSourceStage, GoalAlignment, StalenessClass,
    };
    use crate::source_archeology::date::StalenessRow;
    use crate::source_archeology::diagnose::RawDiagnosis;
    use crate::source_archeology::excavate::{Observation, ObservationCategory};
    use crate::source_archeology::fixtures::{
        DeterministicOnlyClassifier, ScriptedDiagnosisClassifier, ScriptedLintVerifier,
        ScriptedPatchVerifier, ScriptedReconciler, ScriptedReporter, ScriptedScaffolder,
    };
    use crate::source_archeology::handoff::OperatorQuestion;
    use crate::source_archeology::reconcile::ReconciledPatch;
    use crate::source_archeology::scaffold::{ScaffoldFile, ScaffoldOutput};
    use crate::source_archeology::triage::DeclarativeTriager;
    use crate::source_archeology::{Classifiers, StageConfigs};

    fn target() -> ArcheologyTarget {
        ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "main".to_string(),
            goal_id: "onboard".to_string(),
            allowed_paths: Vec::new(),
            mode: ArcheologyMode::default(),
        }
    }

    fn project() -> ProjectContext {
        ProjectContext {
            name: "Flux".to_string(),
            slug: "flux".to_string(),
            description: "A test fixture.".to_string(),
        }
    }

    /// Build a tempdir with a git repo seed so excavate/date can run
    /// without failing on missing HEAD. Returns (temp, root).
    fn seed_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        use std::process::Command;
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.name", "T"]);
        run(&["config", "user.email", "t@example.com"]);
        std::fs::write(root.join("README.md"), "# Flux\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"flux\"\n").unwrap();
        run(&["add", "--", "."]);
        run(&["commit", "-m", "seed"]);
        (dir, root)
    }

    fn build_classifiers<'a>(
        asp: &'a dyn crate::source_archeology::AspirationalClassifier,
        diag: &'a dyn crate::source_archeology::DiagnosisClassifier,
        rec: &'a dyn crate::source_archeology::Reconciler,
        sca: &'a dyn crate::source_archeology::Scaffolder,
        rep: &'a dyn crate::source_archeology::Reporter,
        tri: &'a dyn crate::source_archeology::Triager,
        patch: &'a dyn crate::source_archeology::PatchVerifier,
        lint: &'a dyn crate::source_archeology::LintVerifier,
    ) -> Classifiers<'a> {
        Classifiers {
            aspirational: asp,
            diagnosis: diag,
            reconciler: rec,
            scaffolder: sca,
            reporter: rep,
            triager: tri,
            reviewer: None,
            patch_verifier: patch,
            lint_verifier: lint,
        }
    }

    fn sample_patch() -> ReconciledPatch {
        ReconciledPatch {
            diff: "--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-# Flux\n+# Flux (updated)\n"
                .to_string(),
            severity: FindingSeverity::Medium,
            rationale: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn salvageable_runs_reconcile_branch() {
        let (_dir, root) = seed_repo();
        let asp = DeterministicOnlyClassifier;
        let diag = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::Salvageable,
            confidence: 0.9,
            reasoning: "mostly fresh".to_string(),
        });
        let rec = ScriptedReconciler::new_ok(Some(sample_patch()));
        let sca = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedReporter::new_err();
        let tri = DeclarativeTriager::default();
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_accept();
        let classifiers = build_classifiers(&asp, &diag, &rec, &sca, &rep, &tri, &patch, &lint);
        let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());

        let checkpoint_root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let run = runner
            .run_pipeline(&OrchestratorInput {
                target: &target(),
                clone_root: &root,
                project: &project(),
                triage_ctx: TriageContext {
                    autonomy: Autonomy::Auto,
                },
                checkpoint_root: checkpoint_root.path(),
                now,
            })
            .await
            .unwrap()
            .unwrap_full();

        assert_eq!(run.diagnosis.verdict, DiagnosisVerdict::Salvageable);
        assert!(run.handoff.is_none());
        // README.md in the seeded repo gets an observation but isn't
        // Drifting (recent commit), so Reconcile may produce zero
        // findings. Just assert the branch was taken.
        assert!(run.checkpoint_path.exists());
    }

    #[tokio::test]
    async fn stale_beyond_salvage_runs_scaffold_branch() {
        let (_dir, root) = seed_repo();
        let asp = DeterministicOnlyClassifier;
        let diag = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::StaleBeyondSalvage,
            confidence: 0.9,
            reasoning: "all stale".to_string(),
        });
        let rec = ScriptedReconciler::new_ok(None);
        let sca = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: vec![ScaffoldFile {
                path: "README.md".to_string(),
                content: "# Flux (scaffolded)\n".to_string(),
                severity: FindingSeverity::Medium,
                rationale: "scaffolded readme".to_string(),
            }],
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedReporter::new_err();
        let tri = DeclarativeTriager::default();
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_accept();
        let classifiers = build_classifiers(&asp, &diag, &rec, &sca, &rep, &tri, &patch, &lint);
        let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());

        let checkpoint_root = tempfile::tempdir().unwrap();
        let run = runner
            .run_pipeline(&OrchestratorInput {
                target: &target(),
                clone_root: &root,
                project: &project(),
                triage_ctx: TriageContext {
                    autonomy: Autonomy::Auto,
                },
                checkpoint_root: checkpoint_root.path(),
                now: Utc::now(),
            })
            .await
            .unwrap()
            .unwrap_full();

        assert_eq!(run.diagnosis.verdict, DiagnosisVerdict::StaleBeyondSalvage);
        assert!(run.handoff.is_none());
        assert_eq!(run.findings.len(), 1);
        assert_eq!(run.findings[0].source_stage, FindingSourceStage::Scaffold);
        assert!(matches!(
            run.findings[0].proposed_action,
            FindingAction::NewFile { .. }
        ));
    }

    #[tokio::test]
    async fn no_docs_runs_scaffold_branch() {
        let (_dir, root) = seed_repo();
        let asp = DeterministicOnlyClassifier;
        let diag = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::NoDocs,
            confidence: 0.95,
            reasoning: "empty".to_string(),
        });
        let rec = ScriptedReconciler::new_ok(None);
        let sca = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedReporter::new_err();
        let tri = DeclarativeTriager::default();
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_accept();
        let classifiers = build_classifiers(&asp, &diag, &rec, &sca, &rep, &tri, &patch, &lint);
        let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());

        let checkpoint_root = tempfile::tempdir().unwrap();
        let run = runner
            .run_pipeline(&OrchestratorInput {
                target: &target(),
                clone_root: &root,
                project: &project(),
                triage_ctx: TriageContext {
                    autonomy: Autonomy::Auto,
                },
                checkpoint_root: checkpoint_root.path(),
                now: Utc::now(),
            })
            .await
            .unwrap()
            .unwrap_full();

        assert_eq!(run.diagnosis.verdict, DiagnosisVerdict::NoDocs);
        assert!(run.handoff.is_none());
    }

    #[tokio::test]
    async fn needs_operator_input_runs_handoff_branch() {
        let (_dir, root) = seed_repo();
        let asp = DeterministicOnlyClassifier;
        let diag = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::NeedsOperatorInput,
            confidence: 0.50,
            reasoning: "contradictory".to_string(),
        });
        let rec = ScriptedReconciler::new_ok(None);
        let sca = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedReporter::new_ok(HandoffReport {
            markdown: "# Handoff report".to_string(),
            questions: vec![OperatorQuestion {
                id: "q1".to_string(),
                text: "what do?".to_string(),
                reason: "ambiguous".to_string(),
                choices: vec!["salvage".to_string(), "rescaffold".to_string()],
            }],
        });
        let tri = DeclarativeTriager::default();
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_accept();
        let classifiers = build_classifiers(&asp, &diag, &rec, &sca, &rep, &tri, &patch, &lint);
        let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());

        let checkpoint_root = tempfile::tempdir().unwrap();
        let run = runner
            .run_pipeline(&OrchestratorInput {
                target: &target(),
                clone_root: &root,
                project: &project(),
                triage_ctx: TriageContext {
                    autonomy: Autonomy::Auto,
                },
                checkpoint_root: checkpoint_root.path(),
                now: Utc::now(),
            })
            .await
            .unwrap()
            .unwrap_full();

        assert_eq!(run.diagnosis.verdict, DiagnosisVerdict::NeedsOperatorInput);
        assert!(run.findings.is_empty());
        let handoff = run.handoff.expect("handoff branch populates report");
        assert_eq!(handoff.questions.len(), 1);
        assert_eq!(run.decisions.len(), 0, "Triage/Verify skipped on handoff");
    }

    #[tokio::test]
    async fn checkpoint_is_written_and_readable() {
        let (_dir, root) = seed_repo();
        let asp = DeterministicOnlyClassifier;
        let diag = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::NoDocs,
            confidence: 0.9,
            reasoning: "r".to_string(),
        });
        let rec = ScriptedReconciler::new_ok(None);
        let sca = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedReporter::new_err();
        let tri = DeclarativeTriager::default();
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_accept();
        let classifiers = build_classifiers(&asp, &diag, &rec, &sca, &rep, &tri, &patch, &lint);
        let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());

        let checkpoint_root = tempfile::tempdir().unwrap();
        let run = runner
            .run_pipeline(&OrchestratorInput {
                target: &target(),
                clone_root: &root,
                project: &project(),
                triage_ctx: TriageContext::default(),
                checkpoint_root: checkpoint_root.path(),
                now: Utc::now(),
            })
            .await
            .unwrap()
            .unwrap_full();

        assert!(run.checkpoint_path.exists());
        let parsed = crate::source_archeology::checkpoint::read_checkpoint(&run.checkpoint_path)
            .unwrap()
            .expect("exists");
        assert_eq!(parsed.repo_id, "repo:flux");
        assert_eq!(parsed.diagnosis_verdict, DiagnosisVerdict::NoDocs);
    }

    #[tokio::test]
    async fn second_run_with_unchanged_head_detects_noop() {
        let (_dir, root) = seed_repo();
        let checkpoint_root = tempfile::tempdir().unwrap();

        // First run.
        let first = {
            let asp = DeterministicOnlyClassifier;
            let diag = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
                verdict: DiagnosisVerdict::NoDocs,
                confidence: 0.9,
                reasoning: "r".to_string(),
            });
            let rec = ScriptedReconciler::new_ok(None);
            let sca = ScriptedScaffolder::new_ok(ScaffoldOutput {
                scaffold_files: Vec::new(),
                source_side_patches: Vec::new(),
            });
            let rep = ScriptedReporter::new_err();
            let tri = DeclarativeTriager::default();
            let patch = ScriptedPatchVerifier::new_accept();
            let lint = ScriptedLintVerifier::new_accept();
            let classifiers = build_classifiers(&asp, &diag, &rec, &sca, &rep, &tri, &patch, &lint);
            let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());
            runner
                .run_pipeline(&OrchestratorInput {
                    target: &target(),
                    clone_root: &root,
                    project: &project(),
                    triage_ctx: TriageContext::default(),
                    checkpoint_root: checkpoint_root.path(),
                    now: Utc::now() - chrono::Duration::seconds(2),
                })
                .await
                .unwrap()
                .unwrap_full()
        };

        // Second run: same HEAD + no open_issues → should noop WITHOUT
        // calling any stage classifier. Use a consumed Diag (None) to
        // prove no stage runs.
        let asp = DeterministicOnlyClassifier;
        let diag = ScriptedDiagnosisClassifier::new_err(); // would fail if invoked
        let rec = ScriptedReconciler::new_ok(None);
        let sca = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedReporter::new_err();
        let tri = DeclarativeTriager::default();
        let patch = ScriptedPatchVerifier::new_accept();
        let lint = ScriptedLintVerifier::new_accept();
        let classifiers = build_classifiers(&asp, &diag, &rec, &sca, &rep, &tri, &patch, &lint);
        let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());
        let outcome = runner
            .run_pipeline(&OrchestratorInput {
                target: &target(),
                clone_root: &root,
                project: &project(),
                triage_ctx: TriageContext::default(),
                checkpoint_root: checkpoint_root.path(),
                now: Utc::now(),
            })
            .await
            .unwrap();

        match outcome {
            PipelineOutcome::Noop {
                current_head,
                prior_checkpoint,
                ..
            } => {
                assert_eq!(current_head, first.checkpoint.observed_head);
                assert_eq!(
                    prior_checkpoint.observed_head,
                    first.checkpoint.observed_head
                );
            }
            other => panic!("expected Noop on same-head second run, got {other:?}"),
        }

        // Silence clippy on paired-mate types that future chunks will
        // exercise; keeping the imports live for the rest of the module.
        let _ = GoalAlignment::default();
        let _: FindingDisposition = FindingDisposition::Resolve;
        let _ = ObservationCategory::SignalFile;
        let _: Option<&Finding> = None;
        let _: Option<Observation> = None;
        let _: Option<StalenessClass> = None;
        let _: Option<StalenessRow> = None;
    }
}
