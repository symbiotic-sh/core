//! Test-only fixture harness for Source Archeology Stages 1+2.
//!
//! `Fixture` wraps a `tempfile::TempDir` with a pre-committed fixture repo.
//! Tests compose fixtures via `build(recipe)`; recipes write files and
//! commit them with deterministic `GIT_AUTHOR_DATE` + `GIT_COMMITTER_DATE`
//! so the classifier's age signals are reproducible regardless of the
//! test runner's wall clock.
//!
//! `DeterministicOnlyClassifier` panics if the LLM path is invoked — used
//! to guard "LLM must not be called" test cases.
//!
//! `ScriptedClassifier` returns a pre-configured answer (or `Err`) and is
//! used to exercise the LLM-adjudication branches without network I/O.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tempfile::TempDir;

use super::archeology_types::Finding;
use super::date::AspirationalClassifier;
use super::diagnose::{DiagnosisClassifier, DiagnosisProjection, RawDiagnosis};
use super::handoff::{HandoffInput, HandoffReport, Reporter};
use super::reconcile::{ReconcileInput, ReconciledPatch, Reconciler};
use super::scaffold::{ScaffoldInput, ScaffoldOutput, Scaffolder};
use super::triage::{RawDisposition, Reviewer, TriageContext, Triager};
use super::verify::{LintVerifier, PatchVerifier, VerifyOutcome};

pub(crate) struct Fixture {
    pub repo_dir: TempDir,
}

impl Fixture {
    pub fn clone_root(&self) -> PathBuf {
        self.repo_dir.path().to_path_buf()
    }
}

/// Build a fixture repo. The recipe gets a repo root and an `Author`
/// helper bound to it. Recipes commit with fully deterministic timestamps.
pub(crate) fn build<R>(recipe: R) -> Result<Fixture>
where
    R: FnOnce(&Path, &Author) -> Result<()>,
{
    let repo_dir = tempfile::tempdir().context("mktemp")?;
    let path = repo_dir.path();

    run_git(path, &["init", "--initial-branch=main"])?;
    run_git(path, &["config", "user.name", "Fixture Author"])?;
    run_git(path, &["config", "user.email", "fixture@example.com"])?;

    let author = Author {
        repo: path.to_path_buf(),
    };
    recipe(path, &author)?;

    Ok(Fixture { repo_dir })
}

/// Wall-clock-independent commit helper. Every call that writes a file
/// should flow through `commit_file` so both author and committer dates
/// are pinned.
pub(crate) struct Author {
    repo: PathBuf,
}

impl Author {
    /// Write `contents` to `rel`, stage, and commit dated `days_ago` days
    /// ago. `author_email` lets tests simulate bot vs. human authorship
    /// (bot detection uses email-domain heuristics).
    pub fn commit_file(
        &self,
        rel: &str,
        contents: &str,
        days_ago: u32,
        author_email: &str,
    ) -> Result<()> {
        let full = self.repo.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, contents)?;

        let iso = timestamp_iso(days_ago);
        run_git(&self.repo, &["add", "--", rel])?;

        let status = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args([
                "-c",
                &format!("user.email={author_email}"),
                "-c",
                "user.name=Fixture Author",
                "commit",
                "-m",
                &format!("fixture: {rel} @{days_ago}d"),
            ])
            .env("GIT_AUTHOR_DATE", &iso)
            .env("GIT_COMMITTER_DATE", &iso)
            .env("GIT_AUTHOR_EMAIL", author_email)
            .env("GIT_COMMITTER_EMAIL", author_email)
            .env("GIT_AUTHOR_NAME", "Fixture Author")
            .env("GIT_COMMITTER_NAME", "Fixture Author")
            .status()
            .context("git commit")?;
        if !status.success() {
            anyhow::bail!("git commit failed for {rel}");
        }
        Ok(())
    }

    /// Create a symlink that points to a non-existent target, to exercise
    /// the parse-error / read-failure path in the wire-mismatch scan.
    pub fn create_dangling_symlink(&self, rel: &str) -> Result<()> {
        let full = self.repo.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("does-not-exist-target", &full)?;
        }
        #[cfg(not(unix))]
        {
            let _ = full;
            anyhow::bail!("dangling symlink fixture only supported on Unix");
        }
        Ok(())
    }
}

fn run_git(repo: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .with_context(|| format!("spawn git {args:?}"))?;
    if !status.success() {
        anyhow::bail!("git {args:?} failed with {status}");
    }
    Ok(())
}

fn timestamp_iso(days_ago: u32) -> String {
    let now = chrono::Utc::now();
    let then = now - chrono::Duration::days(days_ago as i64);
    then.to_rfc3339()
}

// ── Mock classifiers ───────────────────────────────────────────────────

pub(crate) struct DeterministicOnlyClassifier;

#[async_trait]
impl AspirationalClassifier for DeterministicOnlyClassifier {
    async fn is_aspirational(&self, _: &str, _: &str, _: &[String]) -> Result<bool> {
        panic!("LLM path invoked in a deterministic-only test case");
    }
}

pub(crate) struct ScriptedClassifier {
    pub verdict: std::sync::Mutex<Option<Result<bool>>>,
}

impl ScriptedClassifier {
    pub fn new_ok(value: bool) -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Ok(value))),
        }
    }
    pub fn new_err() -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("scripted_err")))),
        }
    }
}

#[async_trait]
impl AspirationalClassifier for ScriptedClassifier {
    async fn is_aspirational(&self, _: &str, _: &str, _: &[String]) -> Result<bool> {
        let mut guard = self.verdict.lock().expect("mutex");
        guard
            .take()
            .unwrap_or(Err(anyhow::anyhow!("ScriptedClassifier already consumed")))
    }
}

pub(crate) struct ScriptedDiagnosisClassifier {
    pub verdict: std::sync::Mutex<Option<Result<RawDiagnosis>>>,
}

impl ScriptedDiagnosisClassifier {
    pub fn new_ok(raw: RawDiagnosis) -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Ok(raw))),
        }
    }
    pub fn new_err() -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("scripted_err")))),
        }
    }
}

#[async_trait]
impl DiagnosisClassifier for ScriptedDiagnosisClassifier {
    async fn classify(&self, _: &DiagnosisProjection) -> Result<RawDiagnosis> {
        let mut guard = self.verdict.lock().expect("mutex");
        guard.take().unwrap_or(Err(anyhow::anyhow!(
            "ScriptedDiagnosisClassifier already consumed"
        )))
    }
}

pub(crate) struct ScriptedReconciler {
    pub verdict: std::sync::Mutex<Option<Result<Option<ReconciledPatch>>>>,
}

impl ScriptedReconciler {
    pub fn new_ok(patch: Option<ReconciledPatch>) -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Ok(patch))),
        }
    }
    pub fn new_err() -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("scripted_err")))),
        }
    }
}

pub(crate) struct ScriptedScaffolder {
    pub verdict: std::sync::Mutex<Option<Result<ScaffoldOutput>>>,
}

impl ScriptedScaffolder {
    pub fn new_ok(output: ScaffoldOutput) -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Ok(output))),
        }
    }
    pub fn new_err() -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("scripted_err")))),
        }
    }
}

#[async_trait]
impl Scaffolder for ScriptedScaffolder {
    async fn scaffold(&self, _: &ScaffoldInput<'_>) -> Result<ScaffoldOutput> {
        let mut guard = self.verdict.lock().expect("mutex");
        guard
            .take()
            .unwrap_or(Err(anyhow::anyhow!("ScriptedScaffolder already consumed")))
    }
}

pub(crate) struct ScriptedReporter {
    pub verdict: std::sync::Mutex<Option<Result<HandoffReport>>>,
}

impl ScriptedReporter {
    pub fn new_ok(report: HandoffReport) -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Ok(report))),
        }
    }
    pub fn new_err() -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("scripted_err")))),
        }
    }
}

#[async_trait]
impl Reporter for ScriptedReporter {
    async fn generate_report(&self, _: &HandoffInput<'_>) -> Result<HandoffReport> {
        let mut guard = self.verdict.lock().expect("mutex");
        guard
            .take()
            .unwrap_or(Err(anyhow::anyhow!("ScriptedReporter already consumed")))
    }
}

pub(crate) struct ScriptedTriager {
    pub verdict: std::sync::Mutex<Option<Result<RawDisposition>>>,
}

impl ScriptedTriager {
    /// Reserved for post-MVP tests that need a ScriptedTriager return
    /// value. MVP uses `DeclarativeTriager` directly, so this helper
    /// is currently only paired with `new_err()` in fixture plumbing.
    #[allow(dead_code)]
    pub fn new_ok(d: RawDisposition) -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Ok(d))),
        }
    }
    pub fn new_err() -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("scripted_err")))),
        }
    }
}

#[async_trait]
impl Triager for ScriptedTriager {
    async fn triage(&self, _: &Finding, _: &TriageContext) -> Result<RawDisposition> {
        // Clone-based (triage may be called N times in one test).
        let guard = self.verdict.lock().expect("mutex");
        match guard.as_ref() {
            Some(Ok(d)) => Ok(d.clone()),
            Some(Err(e)) => Err(anyhow::anyhow!("{e}")),
            None => Err(anyhow::anyhow!("ScriptedTriager unconfigured")),
        }
    }
}

pub(crate) struct ScriptedReviewer {
    pub verdict: std::sync::Mutex<Option<Result<RawDisposition>>>,
}

impl ScriptedReviewer {
    pub fn new_ok(d: RawDisposition) -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Ok(d))),
        }
    }
    /// Reserved for tests that need a reviewer-erroring branch.
    /// Triager-error path is covered by `ScriptedTriager::new_err`;
    /// reviewer-error is a symmetric branch worth keeping available.
    #[allow(dead_code)]
    pub fn new_err() -> Self {
        Self {
            verdict: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("scripted_err")))),
        }
    }
}

#[async_trait]
impl Reviewer for ScriptedReviewer {
    async fn review(
        &self,
        _: &Finding,
        _: &TriageContext,
        _: &RawDisposition,
    ) -> Result<RawDisposition> {
        let guard = self.verdict.lock().expect("mutex");
        match guard.as_ref() {
            Some(Ok(d)) => Ok(d.clone()),
            Some(Err(e)) => Err(anyhow::anyhow!("{e}")),
            None => Err(anyhow::anyhow!("ScriptedReviewer unconfigured")),
        }
    }
}

#[async_trait]
impl Reconciler for ScriptedReconciler {
    async fn reconcile_artifact(&self, _: &ReconcileInput<'_>) -> Result<Option<ReconciledPatch>> {
        // This mock returns the same verdict repeatedly — reconcile::run
        // can invoke it multiple times within one test (one per drifting
        // row). Clone rather than take-once.
        let guard = self.verdict.lock().expect("mutex");
        match guard.as_ref() {
            Some(Ok(opt)) => Ok(opt.clone()),
            Some(Err(e)) => Err(anyhow::anyhow!("{e}")),
            None => Err(anyhow::anyhow!("ScriptedReconciler unconfigured")),
        }
    }
}

// Cloneable outcome — `VerifyOutcome` is Clone by construction.
pub(crate) struct ScriptedPatchVerifier {
    pub outcome: Result<VerifyOutcome>,
}

impl ScriptedPatchVerifier {
    pub fn new_accept() -> Self {
        Self {
            outcome: Ok(VerifyOutcome::Accepted),
        }
    }
    pub fn new_reject(reason: &str) -> Self {
        Self {
            outcome: Ok(VerifyOutcome::Rejected {
                reason: reason.to_string(),
            }),
        }
    }
    pub fn new_err() -> Self {
        Self {
            outcome: Err(anyhow::anyhow!("scripted_err")),
        }
    }
}

#[async_trait]
impl PatchVerifier for ScriptedPatchVerifier {
    async fn verify(&self, _: &Path, _: &str) -> Result<VerifyOutcome> {
        match &self.outcome {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}

pub(crate) struct ScriptedLintVerifier {
    pub outcome: Result<VerifyOutcome>,
}

impl ScriptedLintVerifier {
    pub fn new_accept() -> Self {
        Self {
            outcome: Ok(VerifyOutcome::Accepted),
        }
    }
    pub fn new_reject(reason: &str) -> Self {
        Self {
            outcome: Ok(VerifyOutcome::Rejected {
                reason: reason.to_string(),
            }),
        }
    }
    /// Paired with `new_accept` / `new_reject` for symmetry with other
    /// scripted fixtures. Not currently exercised in §09 tests.
    #[allow(dead_code)]
    pub fn new_err() -> Self {
        Self {
            outcome: Err(anyhow::anyhow!("scripted_err")),
        }
    }
}

#[async_trait]
impl LintVerifier for ScriptedLintVerifier {
    async fn verify(&self, _: &str, _: &str) -> Result<VerifyOutcome> {
        match &self.outcome {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}
