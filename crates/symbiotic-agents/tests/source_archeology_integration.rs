//! T128 §12 — Source Archeology end-to-end integration tests.
//!
//! Each test builds a real tempdir fixture repo, runs the full pipeline
//! via `SourceArcheologyRunner::run_pipeline`, and verifies the verdict
//! branch + emitted artifacts. LLM classifiers are scripted (not real
//! network) so the tests are hermetic + fast.
//!
//! This file intentionally lives in `tests/` so it gets a separate
//! cargo test binary — matches the integration-test convention used
//! elsewhere in the workspace (symbiotic-control-plane, symbiotic-daemon).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use symbiotic_agents::{ArcheologyMode, ArcheologyTarget, HandoffInput};
use symbiotic_agents::{
    AspirationalClassifier, Autonomy, Classifiers, DeclarativeTriager, DiagnosisClassifier,
    DiagnosisProjection, DiagnosisVerdict, FindingAction, FindingDisposition, FindingSeverity,
    FindingSourceStage, HandoffReport, LintVerifier, OperatorQuestion, OrchestratorInput,
    PatchVerifier, ProjectContext, RawDiagnosis, ReconcileInput, ReconciledPatch, Reconciler,
    Reporter, ScaffoldFile, ScaffoldInput, ScaffoldOutput, Scaffolder, SourceArcheologyRunner,
    StageConfigs, TriageContext, VerifyOutcome,
};

// ── Fixture builder (mirrors the crate's internal fixtures, but here
// we're in an integration-test file and can't reach the #[cfg(test)]
// module, so we replicate the essentials). ────────────────────────────

fn seed_repo(recipe: impl FnOnce(&Path)) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
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
    recipe(&root);
    run(&["add", "--", "."]);
    run(&["commit", "-m", "seed"]);
    (dir, root)
}

fn write(root: &Path, rel: &str, body: &str) {
    let full = root.join(rel);
    if let Some(p) = full.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(full, body).unwrap();
}

// ── Scripted classifiers / verifiers (local, since the crate's
// fixtures module is #[cfg(test)] and unreachable from integration
// tests). ──────────────────────────────────────────────────────────────

struct NoopAsp;

#[async_trait]
impl AspirationalClassifier for NoopAsp {
    async fn is_aspirational(&self, _: &str, _: &str, _: &[String]) -> Result<bool> {
        Ok(false)
    }
}

struct ScriptedDiag(Mutex<Option<RawDiagnosis>>);

#[async_trait]
impl DiagnosisClassifier for ScriptedDiag {
    async fn classify(&self, _: &DiagnosisProjection) -> Result<RawDiagnosis> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("scripted diag consumed"))
    }
}

fn diag(verdict: DiagnosisVerdict, confidence: f32) -> ScriptedDiag {
    ScriptedDiag(Mutex::new(Some(RawDiagnosis {
        verdict,
        confidence,
        reasoning: "test".to_string(),
    })))
}

struct ScriptedRec(Option<ReconciledPatch>);

#[async_trait]
impl Reconciler for ScriptedRec {
    async fn reconcile_artifact(&self, _: &ReconcileInput<'_>) -> Result<Option<ReconciledPatch>> {
        Ok(self.0.clone())
    }
}

struct ScriptedSca(ScaffoldOutput);

#[async_trait]
impl Scaffolder for ScriptedSca {
    async fn scaffold(&self, _: &ScaffoldInput<'_>) -> Result<ScaffoldOutput> {
        Ok(self.0.clone())
    }
}

struct ScriptedRep(Option<HandoffReport>);

#[async_trait]
impl Reporter for ScriptedRep {
    async fn generate_report(&self, _: &HandoffInput<'_>) -> Result<HandoffReport> {
        self.0
            .clone()
            .ok_or_else(|| anyhow::anyhow!("rep not configured"))
    }
}

struct AcceptPatch;

#[async_trait]
impl PatchVerifier for AcceptPatch {
    async fn verify(&self, _: &Path, _: &str) -> Result<VerifyOutcome> {
        Ok(VerifyOutcome::Accepted)
    }
}

struct AcceptLint;

#[async_trait]
impl LintVerifier for AcceptLint {
    async fn verify(&self, _: &str, _: &str) -> Result<VerifyOutcome> {
        Ok(VerifyOutcome::Accepted)
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

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
        description: "A test fixture".to_string(),
    }
}

// ── Tests — one per Diagnose verdict ───────────────────────────────────

#[tokio::test]
async fn salvageable_e2e_runs_reconcile_and_writes_checkpoint() {
    let (_dir, root) = seed_repo(|r| {
        write(r, "README.md", "# Flux\n\nSee Cargo.toml for build info.\n");
        write(r, "Cargo.toml", "[package]\nname=\"flux\"\n");
        write(r, "docs/architecture.md", "# Architecture\n");
    });

    let asp = NoopAsp;
    let d = diag(DiagnosisVerdict::Salvageable, 0.90);
    let rec = ScriptedRec(Some(ReconciledPatch {
        diff: "--- a/docs/architecture.md\n+++ b/docs/architecture.md\n@@ -1 +1 @@\n-# Architecture\n+# Architecture (updated)\n".to_string(),
        severity: FindingSeverity::Medium,
        rationale: "drift fix".to_string(),
    }));
    let sca = ScriptedSca(ScaffoldOutput {
        scaffold_files: Vec::new(),
        source_side_patches: Vec::new(),
    });
    let rep = ScriptedRep(None);
    let tri = DeclarativeTriager::default();
    let patch = AcceptPatch;
    let lint = AcceptLint;

    let classifiers = Classifiers {
        aspirational: &asp,
        diagnosis: &d,
        reconciler: &rec,
        scaffolder: &sca,
        reporter: &rep,
        triager: &tri,
        reviewer: None,
        patch_verifier: &patch,
        lint_verifier: &lint,
    };
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
        .expect("pipeline runs")
        .unwrap_full();

    assert_eq!(run.diagnosis.verdict, DiagnosisVerdict::Salvageable);
    assert!(run.handoff.is_none());
    assert!(run.checkpoint_path.exists());
    assert_eq!(
        run.checkpoint.diagnosis_verdict,
        DiagnosisVerdict::Salvageable
    );
    // Findings may be empty if no rows came out as Drifting (fresh seeds
    // are Fresh by default). What matters is the branch ran.
    for f in &run.findings {
        assert_eq!(f.source_stage, FindingSourceStage::Reconcile);
        assert!(matches!(f.proposed_action, FindingAction::Patch { .. }));
    }
}

#[tokio::test]
async fn stale_beyond_salvage_e2e_runs_scaffold_and_writes_checkpoint() {
    let (_dir, root) = seed_repo(|r| {
        write(r, "README.md", "# old project\n");
        write(r, "docs/architecture.md", "# Legacy architecture\n");
    });

    let asp = NoopAsp;
    let d = diag(DiagnosisVerdict::StaleBeyondSalvage, 0.88);
    let rec = ScriptedRec(None);
    let sca = ScriptedSca(ScaffoldOutput {
        scaffold_files: vec![
            ScaffoldFile {
                path: "README.md".to_string(),
                content: "# Flux\n\nRewritten from scaffold.\n".to_string(),
                severity: FindingSeverity::Medium,
                rationale: "root readme rewrite".to_string(),
            },
            ScaffoldFile {
                path: "docs/architecture.md".to_string(),
                content: "# Architecture\n\nFrom scaffold.\n".to_string(),
                severity: FindingSeverity::Medium,
                rationale: "arch rewrite".to_string(),
            },
        ],
        source_side_patches: Vec::new(),
    });
    let rep = ScriptedRep(None);
    let tri = DeclarativeTriager::default();
    let patch = AcceptPatch;
    let lint = AcceptLint;

    let classifiers = Classifiers {
        aspirational: &asp,
        diagnosis: &d,
        reconciler: &rec,
        scaffolder: &sca,
        reporter: &rep,
        triager: &tri,
        reviewer: None,
        patch_verifier: &patch,
        lint_verifier: &lint,
    };
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
        .expect("pipeline runs")
        .unwrap_full();

    assert_eq!(run.diagnosis.verdict, DiagnosisVerdict::StaleBeyondSalvage);
    assert!(run.handoff.is_none());
    assert_eq!(run.findings.len(), 2);
    for f in &run.findings {
        assert_eq!(f.source_stage, FindingSourceStage::Scaffold);
        assert!(matches!(f.proposed_action, FindingAction::NewFile { .. }));
    }
    // Decisions should all be Resolve (deterministic triager + Auto
    // autonomy + Medium severity + InScope).
    assert_eq!(run.decisions.len(), 2);
    for d in &run.decisions {
        assert_eq!(d.disposition, FindingDisposition::Resolve);
    }
}

#[tokio::test]
async fn no_docs_e2e_runs_scaffold_branch() {
    let (_dir, root) = seed_repo(|r| {
        // Intentionally no README / docs — minimal Cargo-only repo.
        write(r, "Cargo.toml", "[package]\nname=\"nodocs\"\n");
    });

    let asp = NoopAsp;
    let d = diag(DiagnosisVerdict::NoDocs, 0.95);
    let rec = ScriptedRec(None);
    let sca = ScriptedSca(ScaffoldOutput {
        scaffold_files: vec![ScaffoldFile {
            path: "README.md".to_string(),
            content: "# Nodocs\n\nInitial scaffold.\n".to_string(),
            severity: FindingSeverity::High,
            rationale: "root readme missing".to_string(),
        }],
        source_side_patches: Vec::new(),
    });
    let rep = ScriptedRep(None);
    let tri = DeclarativeTriager::default();
    let patch = AcceptPatch;
    let lint = AcceptLint;

    let classifiers = Classifiers {
        aspirational: &asp,
        diagnosis: &d,
        reconciler: &rec,
        scaffolder: &sca,
        reporter: &rep,
        triager: &tri,
        reviewer: None,
        patch_verifier: &patch,
        lint_verifier: &lint,
    };
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
        .expect("pipeline runs")
        .unwrap_full();

    assert_eq!(run.diagnosis.verdict, DiagnosisVerdict::NoDocs);
    assert_eq!(run.findings.len(), 1);
    assert_eq!(run.findings[0].source_stage, FindingSourceStage::Scaffold);
}

#[tokio::test]
async fn needs_operator_input_e2e_runs_handoff_branch() {
    let (_dir, root) = seed_repo(|r| {
        write(r, "README.md", "# Ambiguous\n");
    });

    let asp = NoopAsp;
    let d = diag(DiagnosisVerdict::NeedsOperatorInput, 0.40);
    let rec = ScriptedRec(None);
    let sca = ScriptedSca(ScaffoldOutput {
        scaffold_files: Vec::new(),
        source_side_patches: Vec::new(),
    });
    let rep = ScriptedRep(Some(HandoffReport {
        markdown: "# Handoff\n\nAmbiguity report body.".to_string(),
        questions: vec![OperatorQuestion {
            id: "q1".to_string(),
            text: "Which branch should we take?".to_string(),
            reason: "Contradictory signals from Date stage.".to_string(),
            choices: vec![
                "salvage".to_string(),
                "rescaffold".to_string(),
                "skip".to_string(),
            ],
        }],
    }));
    let tri = DeclarativeTriager::default();
    let patch = AcceptPatch;
    let lint = AcceptLint;

    let classifiers = Classifiers {
        aspirational: &asp,
        diagnosis: &d,
        reconciler: &rec,
        scaffolder: &sca,
        reporter: &rep,
        triager: &tri,
        reviewer: None,
        patch_verifier: &patch,
        lint_verifier: &lint,
    };
    let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());
    let checkpoint_root = tempfile::tempdir().unwrap();

    let run = runner
        .run_pipeline(&OrchestratorInput {
            target: &target(),
            clone_root: &root,
            project: &project(),
            triage_ctx: TriageContext {
                autonomy: Autonomy::Semi,
            },
            checkpoint_root: checkpoint_root.path(),
            now: Utc::now(),
        })
        .await
        .expect("pipeline runs")
        .unwrap_full();

    assert_eq!(run.diagnosis.verdict, DiagnosisVerdict::NeedsOperatorInput);
    assert!(run.findings.is_empty());
    assert_eq!(run.decisions.len(), 0);
    let handoff = run.handoff.expect("handoff branch populates report");
    assert_eq!(handoff.questions.len(), 1);
    assert_eq!(handoff.questions[0].choices.len(), 3);
}

#[tokio::test]
async fn second_run_with_unchanged_head_detects_noop() {
    let (_dir, root) = seed_repo(|r| {
        write(r, "README.md", "# Flux\n");
    });
    let checkpoint_root = tempfile::tempdir().unwrap();

    // First run produces an empty-findings NoDocs checkpoint → open_issues empty.
    let run1 = {
        let asp = NoopAsp;
        let d = diag(DiagnosisVerdict::NoDocs, 0.9);
        let rec = ScriptedRec(None);
        let sca = ScriptedSca(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedRep(None);
        let tri = DeclarativeTriager::default();
        let patch = AcceptPatch;
        let lint = AcceptLint;
        let classifiers = Classifiers {
            aspirational: &asp,
            diagnosis: &d,
            reconciler: &rec,
            scaffolder: &sca,
            reporter: &rep,
            triager: &tri,
            reviewer: None,
            patch_verifier: &patch,
            lint_verifier: &lint,
        };
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

    // Second run: same HEAD + no open_issues → orchestrator should return Noop
    // BEFORE invoking any stage classifier. Use a failing-Diag to prove it
    // isn't called: if the stages ran, the test would fail on diag error.
    let outcome2 = {
        let asp = NoopAsp;
        // A ScriptedDiag pre-consumed so any call would fail — proves no
        // stage got invoked.
        let d = ScriptedDiag(Mutex::new(None));
        let rec = ScriptedRec(None);
        let sca = ScriptedSca(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedRep(None);
        let tri = DeclarativeTriager::default();
        let patch = AcceptPatch;
        let lint = AcceptLint;
        let classifiers = Classifiers {
            aspirational: &asp,
            diagnosis: &d,
            reconciler: &rec,
            scaffolder: &sca,
            reporter: &rep,
            triager: &tri,
            reviewer: None,
            patch_verifier: &patch,
            lint_verifier: &lint,
        };
        let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());
        runner
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
    };

    match outcome2 {
        symbiotic_agents::PipelineOutcome::Noop {
            current_head,
            prior_checkpoint,
            ..
        } => {
            assert_eq!(current_head, run1.checkpoint.observed_head);
            assert_eq!(
                prior_checkpoint.observed_head,
                run1.checkpoint.observed_head
            );
        }
        other => panic!("expected Noop on unchanged-head second run, got {other:?}"),
    }
}

#[tokio::test]
async fn second_run_with_modified_tree_does_full_run() {
    let (_dir, root) = seed_repo(|r| {
        write(r, "README.md", "# Flux\n");
    });
    let checkpoint_root = tempfile::tempdir().unwrap();

    // First run.
    let _run1 = {
        let asp = NoopAsp;
        let d = diag(DiagnosisVerdict::NoDocs, 0.9);
        let rec = ScriptedRec(None);
        let sca = ScriptedSca(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedRep(None);
        let tri = DeclarativeTriager::default();
        let patch = AcceptPatch;
        let lint = AcceptLint;
        let classifiers = Classifiers {
            aspirational: &asp,
            diagnosis: &d,
            reconciler: &rec,
            scaffolder: &sca,
            reporter: &rep,
            triager: &tri,
            reviewer: None,
            patch_verifier: &patch,
            lint_verifier: &lint,
        };
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

    // Move HEAD by committing a new file.
    write(&root, "NEW.md", "# New\n");
    let gs = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["add", "--", "."])
        .status()
        .unwrap();
    assert!(gs.success());
    let gc = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["commit", "-m", "second"])
        .status()
        .unwrap();
    assert!(gc.success());

    // Second run: HEAD differs → full run, previous_observed_head populated.
    let run2 = {
        let asp = NoopAsp;
        let d = diag(DiagnosisVerdict::NoDocs, 0.9);
        let rec = ScriptedRec(None);
        let sca = ScriptedSca(ScaffoldOutput {
            scaffold_files: Vec::new(),
            source_side_patches: Vec::new(),
        });
        let rep = ScriptedRep(None);
        let tri = DeclarativeTriager::default();
        let patch = AcceptPatch;
        let lint = AcceptLint;
        let classifiers = Classifiers {
            aspirational: &asp,
            diagnosis: &d,
            reconciler: &rec,
            scaffolder: &sca,
            reporter: &rep,
            triager: &tri,
            reviewer: None,
            patch_verifier: &patch,
            lint_verifier: &lint,
        };
        let runner = SourceArcheologyRunner::new(classifiers, StageConfigs::default());
        runner
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
            .unwrap_full()
    };

    assert!(
        run2.checkpoint.previous_observed_head.is_some(),
        "second run after HEAD move populates previous_observed_head"
    );
}
