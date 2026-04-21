//! T128 §14 — Bundle schema v3 emission for Source Archeology.
//!
//! Packages the typed pipeline outputs of a Source Archeology run into an
//! atomically-emitted on-disk bundle rooted at:
//!
//! ```text
//! {archive_root}/operations/projects/{project}/goals/{goal}/artifacts/source-archeology/{timestamp}/
//! ```
//!
//! Atomic write protocol: artifacts are written to a sibling `<run_dir>.tmp/`
//! directory and only `rename`d into place when the entire bundle is on disk.
//! This keeps in-flight bundles invisible to readers and is retry-friendly:
//! a stale `.tmp/` from a crashed prior emit is recursively removed before
//! re-attempting.
//!
//! Caller wiring (sandbox-returned `PipelineRun` -> bundle emit -> notify)
//! is out of scope here — that lands in §13b. This chunk ships
//! `emit_bundle_v3` as a pure function.
//!
//! See `tasks/128-source-archeology/14-bundle-v3-emission.md` for the full
//! ratified design (5 decisions D1-D5).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use symbiotic_agents::source_archeology::{
    DiagnosisVerdict, Finding, FindingAction, FindingSourceStage, PipelineRun,
};
use uuid::Uuid;

/// Top-level schema version stamped into every emitted bundle's
/// `metadata.json`. Bump only on a breaking on-disk shape change.
pub const BUNDLE_SCHEMA_VERSION: u32 = 3;

/// Per-emit caps on bundle size. Defaults match the chunk design (50
/// scaffold files, 200 patches) to keep bundles bounded when the LLM
/// emits an unreasonable number of findings.
#[derive(Debug, Clone, Copy)]
pub struct BundleConfig {
    /// Max scaffold-package file count per run. Findings beyond this cap
    /// are dropped with a `tracing::warn!`.
    pub max_scaffold_files: usize,
    /// Max patchset file count per run. Findings beyond this cap are
    /// dropped with a `tracing::warn!`.
    pub max_patches: usize,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            max_scaffold_files: 50,
            max_patches: 200,
        }
    }
}

/// Caller-supplied context that pairs with a `PipelineRun` to produce
/// `metadata.json`. The fields here are the ones the orchestrator alone
/// doesn't carry — they originate from the goal/run context.
#[derive(Debug, Clone)]
pub struct BundleEmitContext {
    /// Branch the source repo was inspected at.
    pub base_branch: String,
    /// Goal id this run was triggered for.
    pub goal_id: String,
    /// Execution mode label: `"full"`, `"dry_run"`, `"checkpoint_only"`.
    pub mode: String,
    /// Pipeline start time (RFC3339-encoded into metadata).
    pub started_at: DateTime<Utc>,
    /// Pipeline completion time (RFC3339-encoded into metadata).
    pub completed_at: DateTime<Utc>,
}

/// Result of a successful emit. Surfaced for the caller to log / assert
/// against. `findings_dropped_for_path_safety` is intentionally a public
/// signal so operators see drift between LLM-emitted scaffolds and what
/// actually landed on disk.
#[derive(Debug, Clone)]
pub struct BundleSummary {
    pub run_dir: PathBuf,
    pub checkpoint_path: PathBuf,
    pub artifact_count: usize,
    pub scaffold_files_written: usize,
    pub patches_written: usize,
    pub handoff_report_written: bool,
    /// Count of `NewFile`/`Patch` findings dropped at emit time because
    /// their LLM-supplied paths failed safety checks. See
    /// `is_safe_scaffold_path`.
    pub findings_dropped_for_path_safety: usize,
}

// ── Public API ─────────────────────────────────────────────────────────

/// Atomically emit a Source Archeology bundle for `run` into `run_dir`.
///
/// Writes to a `<run_dir>.tmp/` sibling first, then `rename`s into place.
/// A stale `.tmp/` from a prior crashed emit is removed first
/// (retry-friendly). If `run_dir` already exists this returns `Err` —
/// distinct runs should never collide on the timestamp folder, so
/// collisions are caller bugs we want to surface loudly.
///
/// `Noop` outcomes are out of scope — callers must not invoke
/// `emit_bundle_v3` for a noop run (no artifacts to write).
pub fn emit_bundle_v3(
    run: &PipelineRun,
    run_dir: &Path,
    run_id: Uuid,
    ctx: &BundleEmitContext,
    config: &BundleConfig,
) -> Result<BundleSummary> {
    // Atomic-write protocol step 1: compute sibling tmp dir.
    let tmp_dir = run_dir.with_extension("tmp");

    // Step 2: clear any stale .tmp/ from a prior crashed emit.
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)
            .with_context(|| format!("remove stale tmp dir {tmp_dir:?}"))?;
    }

    // Step 3: fresh tmp dir.
    std::fs::create_dir_all(&tmp_dir).with_context(|| format!("create tmp dir {tmp_dir:?}"))?;

    // Step 4: write artifacts. Routing is per Diagnosis verdict.
    let mut artifact_count: usize = 0;
    let mut scaffold_files_written: usize = 0;
    let mut patches_written: usize = 0;
    let mut handoff_report_written = false;
    let mut findings_dropped_for_path_safety: usize = 0;

    let verdict = run.diagnosis.verdict;

    // Always-emitted core artifacts (excluding NeedsOperatorInput which
    // takes a different shape per the routing table).
    match verdict {
        DiagnosisVerdict::Salvageable
        | DiagnosisVerdict::StaleBeyondSalvage
        | DiagnosisVerdict::NoDocs => {
            write_json(&tmp_dir.join("excavation.json"), &run.excavation)?;
            artifact_count += 1;
            write_json(&tmp_dir.join("staleness.json"), &run.staleness)?;
            artifact_count += 1;
            write_json(&tmp_dir.join("diagnosis.json"), &run.diagnosis)?;
            artifact_count += 1;
            write_json(&tmp_dir.join("findings.json"), &run.findings)?;
            artifact_count += 1;
            write_json(&tmp_dir.join("triage-decisions.json"), &run.decisions)?;
            artifact_count += 1;
        }
        DiagnosisVerdict::NeedsOperatorInput => {
            write_json(&tmp_dir.join("excavation.json"), &run.excavation)?;
            artifact_count += 1;
            write_json(&tmp_dir.join("staleness.json"), &run.staleness)?;
            artifact_count += 1;
            write_json(&tmp_dir.join("diagnosis.json"), &run.diagnosis)?;
            artifact_count += 1;
            // Handoff branch: write the operator-facing markdown report.
            if let Some(handoff) = run.handoff.as_ref() {
                let report_path = tmp_dir.join("archeology-report.md");
                std::fs::write(&report_path, &handoff.markdown)
                    .with_context(|| format!("write {report_path:?}"))?;
                handoff_report_written = true;
                artifact_count += 1;
            }
        }
    }

    // Verdict-conditional sub-bundles: scaffold-package/ and patchset/.
    match verdict {
        DiagnosisVerdict::Salvageable => {
            // Reconcile findings -> patchset/ only.
            let (written, dropped) = write_patches(&tmp_dir, &run.findings, config.max_patches)?;
            patches_written += written;
            findings_dropped_for_path_safety += dropped;
            artifact_count += written;
        }
        DiagnosisVerdict::StaleBeyondSalvage | DiagnosisVerdict::NoDocs => {
            // Scaffold findings split: NewFile -> scaffold-package/,
            // Patch (source-side) -> patchset/.
            let (sw, sd) =
                write_scaffold_files(&tmp_dir, &run.findings, config.max_scaffold_files)?;
            scaffold_files_written += sw;
            findings_dropped_for_path_safety += sd;
            artifact_count += sw;

            let (pw, pd) = write_patches(&tmp_dir, &run.findings, config.max_patches)?;
            patches_written += pw;
            findings_dropped_for_path_safety += pd;
            artifact_count += pw;
        }
        DiagnosisVerdict::NeedsOperatorInput => {
            // No patchset / scaffold-package on the handoff branch.
        }
    }

    // Always: checkpoint mirror + metadata.
    write_json(&tmp_dir.join("archeology-checkpoint.json"), &run.checkpoint)?;
    artifact_count += 1;

    let metadata = build_metadata(run, run_id, ctx, findings_dropped_for_path_safety);
    write_json(&tmp_dir.join("metadata.json"), &metadata)?;
    artifact_count += 1;

    // Step 5: collision check on the destination — fail-loud.
    if run_dir.exists() {
        return Err(anyhow!(
            "bundle run_dir already exists (timestamp collision is a caller bug): {:?}",
            run_dir
        ));
    }

    // Ensure parent dir exists so rename can land.
    if let Some(parent) = run_dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create parent {parent:?}"))?;
    }

    // Step 6: atomic promotion.
    std::fs::rename(&tmp_dir, run_dir)
        .with_context(|| format!("rename {tmp_dir:?} -> {run_dir:?}"))?;

    Ok(BundleSummary {
        run_dir: run_dir.to_path_buf(),
        checkpoint_path: run_dir.join("archeology-checkpoint.json"),
        artifact_count,
        scaffold_files_written,
        patches_written,
        handoff_report_written,
        findings_dropped_for_path_safety,
    })
}

// ── Path-safety helpers ────────────────────────────────────────────────

/// Validate an LLM-supplied scaffold-package-relative path. Rejects:
///
/// - Empty paths.
/// - Paths starting with `/` (absolute).
/// - Any segment equal to `..` (traversal).
/// - NUL bytes.
/// - Any segment matching Windows reserved device names
///   (`CON`/`PRN`/`AUX`/`NUL`/`COM[0-9]`/`LPT[0-9]`, case-insensitive).
///
/// Returning `false` here means the caller (`emit_bundle_v3`) drops the
/// finding with a warning. Bundles still emit — defense-in-depth, not a
/// hard abort. The architectural fix (key-based scaffold contract) is
/// queued as §06b.
pub fn is_safe_scaffold_path(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.starts_with('/') {
        return false;
    }
    if path.contains('\0') {
        return false;
    }
    for segment in path.split('/') {
        if segment == ".." {
            return false;
        }
        if is_windows_reserved(segment) {
            return false;
        }
    }
    true
}

fn is_windows_reserved(segment: &str) -> bool {
    let upper = segment.to_ascii_uppercase();
    // Strip any extension for the reserved-name check (e.g. `CON.txt`
    // is also reserved on Windows). We compare against the stem.
    let stem = upper.split('.').next().unwrap_or("");
    matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && stem.starts_with("COM") && stem.as_bytes()[3].is_ascii_digit())
        || (stem.len() == 4 && stem.starts_with("LPT") && stem.as_bytes()[3].is_ascii_digit())
}

/// Sanitize a finding's `evidence_path` into a single safe filename
/// component for `<finding-id>__<sanitized>.patch`. Replaces filesystem
/// separators / NUL with `_`, truncates to 100 chars, and falls back to
/// `unknown` if the result is empty.
pub fn sanitize_for_patch_filename(evidence: &str) -> String {
    let mut out = String::with_capacity(evidence.len());
    for ch in evidence.chars() {
        match ch {
            '/' | '\\' | ':' | '\0' => out.push('_'),
            _ => out.push(ch),
        }
    }
    if out.len() > 100 {
        out.truncate(100);
    }
    if out.is_empty() {
        return "unknown".to_string();
    }
    out
}

// ── Internal writers ───────────────────────────────────────────────────

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let body =
        serde_json::to_string_pretty(value).with_context(|| format!("serialize {path:?}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create parent {parent:?}"))?;
    }
    std::fs::write(path, body).with_context(|| format!("write {path:?}"))?;
    Ok(())
}

/// Write `NewFile` findings into `<tmp_dir>/scaffold-package/{path}`,
/// honoring the path-safety rules and the per-run cap. Returns
/// (written_count, dropped_for_safety_count).
fn write_scaffold_files(
    tmp_dir: &Path,
    findings: &[Finding],
    max_files: usize,
) -> Result<(usize, usize)> {
    let scaffold_root = tmp_dir.join("scaffold-package");
    let mut written: usize = 0;
    let mut dropped: usize = 0;

    for finding in findings {
        if !matches!(finding.source_stage, FindingSourceStage::Scaffold) {
            continue;
        }
        let FindingAction::NewFile { path, content } = &finding.proposed_action else {
            continue;
        };

        if !is_safe_scaffold_path(path) {
            tracing::warn!(
                finding_id = %finding.id,
                path = %path,
                "dropping NewFile finding with unsafe scaffold path"
            );
            dropped += 1;
            continue;
        }

        if written >= max_files {
            tracing::warn!(
                finding_id = %finding.id,
                path = %path,
                cap = max_files,
                "dropping NewFile finding — bundle scaffold cap reached"
            );
            continue;
        }

        let dest = scaffold_root.join(path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create scaffold parent {parent:?}"))?;
        }
        std::fs::write(&dest, content).with_context(|| format!("write scaffold file {dest:?}"))?;
        written += 1;
    }

    Ok((written, dropped))
}

/// Write `Patch` findings into `<tmp_dir>/patchset/{filename}`, honoring
/// the per-run cap. Filename is `<finding.id>__<sanitized-evidence>.patch`
/// per ratified D2. Returns (written_count, dropped_for_safety_count).
///
/// `dropped_for_safety_count` is always 0 today — the patch path is
/// derived from the (sanitized) finding evidence and lands inside
/// `patchset/` regardless of the original evidence value, so there is
/// no upstream path-injection vector. Returned as a tuple to keep the
/// interface symmetric with `write_scaffold_files`.
fn write_patches(
    tmp_dir: &Path,
    findings: &[Finding],
    max_patches: usize,
) -> Result<(usize, usize)> {
    let patchset_root = tmp_dir.join("patchset");
    let mut written: usize = 0;
    let dropped: usize = 0;

    for finding in findings {
        let FindingAction::Patch { diff } = &finding.proposed_action else {
            continue;
        };

        if written >= max_patches {
            tracing::warn!(
                finding_id = %finding.id,
                cap = max_patches,
                "dropping Patch finding — bundle patch cap reached"
            );
            continue;
        }

        if written == 0 {
            std::fs::create_dir_all(&patchset_root)
                .with_context(|| format!("create patchset dir {patchset_root:?}"))?;
        }

        let sanitized = sanitize_for_patch_filename(&finding.evidence_path);
        let filename = format!("{}__{}.patch", finding.id, sanitized);
        let dest = patchset_root.join(filename);
        std::fs::write(&dest, diff).with_context(|| format!("write patch {dest:?}"))?;
        written += 1;
    }

    Ok((written, dropped))
}

/// Build the `metadata.json` payload from the run + caller context.
fn build_metadata(
    run: &PipelineRun,
    run_id: Uuid,
    ctx: &BundleEmitContext,
    findings_dropped_for_path_safety: usize,
) -> serde_json::Value {
    let triage_summary = json!({
        "resolve": run.checkpoint.triage_summary.resolve,
        "defer": run.checkpoint.triage_summary.defer,
        "escalate": run.checkpoint.triage_summary.escalate,
    });

    json!({
        "bundle_schema_version": BUNDLE_SCHEMA_VERSION,
        "run_id": run_id.to_string(),
        "repo_id": run.checkpoint.repo_id,
        "base_branch": ctx.base_branch,
        "goal_id": ctx.goal_id,
        "mode": ctx.mode,
        "started_at": ctx.started_at.to_rfc3339(),
        "completed_at": ctx.completed_at.to_rfc3339(),
        "outcome_kind": "full",
        "diagnosis_verdict": diagnosis_verdict_str(run.diagnosis.verdict),
        "findings_count": run.findings.len(),
        "triage_summary": triage_summary,
        "findings_dropped_for_path_safety": findings_dropped_for_path_safety,
    })
}

fn diagnosis_verdict_str(v: DiagnosisVerdict) -> &'static str {
    match v {
        DiagnosisVerdict::Salvageable => "salvageable",
        DiagnosisVerdict::StaleBeyondSalvage => "stale_beyond_salvage",
        DiagnosisVerdict::NoDocs => "no_docs",
        DiagnosisVerdict::NeedsOperatorInput => "needs_operator_input",
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;
    use symbiotic_agents::source_archeology::{
        ArcheologyCheckpoint, Diagnosis, ExcavationReport, Finding, FindingAction,
        FindingDisposition, FindingSeverity, FindingSourceStage, GoalAlignment, HandoffReport,
        OperatorQuestion, StalenessReport, TriageDecision, TriageSummary,
        CHECKPOINT_SCHEMA_VERSION,
    };

    // ── Fixtures ───────────────────────────────────────────────────────

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 19, 10, 0, 0).unwrap()
    }

    fn ctx() -> BundleEmitContext {
        BundleEmitContext {
            base_branch: "main".to_string(),
            goal_id: "goal-onboard".to_string(),
            mode: "full".to_string(),
            started_at: now(),
            completed_at: now() + chrono::Duration::seconds(30),
        }
    }

    fn empty_excavation() -> ExcavationReport {
        ExcavationReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "deadbeefcafe".to_string(),
            observations: Vec::new(),
        }
    }

    fn empty_staleness() -> StalenessReport {
        StalenessReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "deadbeefcafe".to_string(),
            rows: Vec::new(),
        }
    }

    fn checkpoint(verdict: DiagnosisVerdict, summary: TriageSummary) -> ArcheologyCheckpoint {
        ArcheologyCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            repo_id: "repo:flux".to_string(),
            run_timestamp: now(),
            observed_head: "deadbeefcafe".to_string(),
            previous_observed_head: None,
            diagnosis_verdict: verdict,
            diagnosis_confidence: 0.9,
            staleness_by_file: Default::default(),
            discrepancy_files: Vec::new(),
            aspirational_claims: Vec::new(),
            open_issues: Vec::new(),
            files_analyzed: Vec::new(),
            no_drift_files: Vec::new(),
            findings_count: 0,
            triage_summary: summary,
        }
    }

    fn diagnosis(v: DiagnosisVerdict) -> Diagnosis {
        Diagnosis {
            verdict: v,
            confidence: 0.9,
            reasoning: "test".to_string(),
        }
    }

    fn make_run(
        verdict: DiagnosisVerdict,
        findings: Vec<Finding>,
        decisions: Vec<TriageDecision>,
        handoff: Option<HandoffReport>,
    ) -> PipelineRun {
        let summary = {
            let mut s = TriageSummary::default();
            for d in &decisions {
                match d.disposition {
                    FindingDisposition::Resolve => s.resolve += 1,
                    FindingDisposition::Defer => s.defer += 1,
                    FindingDisposition::Escalate => s.escalate += 1,
                }
            }
            s
        };
        PipelineRun {
            excavation: empty_excavation(),
            staleness: empty_staleness(),
            diagnosis: diagnosis(verdict),
            findings,
            handoff,
            decisions,
            checkpoint: checkpoint(verdict, summary),
            checkpoint_path: PathBuf::from("/dev/null"),
        }
    }

    fn patch_finding(id: &str, evidence: &str) -> Finding {
        Finding {
            id: id.to_string(),
            source_stage: FindingSourceStage::Reconcile,
            severity: FindingSeverity::Medium,
            category: "drift".to_string(),
            evidence_path: evidence.to_string(),
            description: "test patch".to_string(),
            proposed_action: FindingAction::Patch {
                diff: format!("--- a/{evidence}\n+++ b/{evidence}\n@@ -1 +1 @@\n-old\n+new\n"),
            },
            goal_alignment: GoalAlignment::InScope,
        }
    }

    fn scaffold_finding(id: &str, path: &str, content: &str) -> Finding {
        Finding {
            id: id.to_string(),
            source_stage: FindingSourceStage::Scaffold,
            severity: FindingSeverity::Medium,
            category: "missing_doc".to_string(),
            evidence_path: path.to_string(),
            description: "scaffolded doc".to_string(),
            proposed_action: FindingAction::NewFile {
                path: path.to_string(),
                content: content.to_string(),
            },
            goal_alignment: GoalAlignment::InScope,
        }
    }

    fn source_side_patch_finding(id: &str, evidence: &str) -> Finding {
        Finding {
            id: id.to_string(),
            source_stage: FindingSourceStage::ScaffoldSourceSide,
            severity: FindingSeverity::Medium,
            category: "broken_reference".to_string(),
            evidence_path: evidence.to_string(),
            description: "source-side fix".to_string(),
            proposed_action: FindingAction::Patch {
                diff: format!("--- a/{evidence}\n+++ b/{evidence}\n@@ -1 +1 @@\n-x\n+y\n"),
            },
            goal_alignment: GoalAlignment::InScope,
        }
    }

    fn triage_resolve(finding_id: &str) -> TriageDecision {
        TriageDecision {
            finding_id: finding_id.to_string(),
            disposition: FindingDisposition::Resolve,
            rationale: "test".to_string(),
        }
    }

    // ── Tests ──────────────────────────────────────────────────────────

    #[test]
    fn salvageable_writes_findings_triage_and_patchset() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("2026-04-19T10-00-00Z");

        let findings = vec![
            patch_finding("f-1", "docs/A.md"),
            patch_finding("f-2", "docs/B.md"),
            patch_finding("f-3", "docs/C.md"),
        ];
        let decisions = vec![
            triage_resolve("f-1"),
            triage_resolve("f-2"),
            triage_resolve("f-3"),
        ];
        let run = make_run(DiagnosisVerdict::Salvageable, findings, decisions, None);

        let summary = emit_bundle_v3(
            &run,
            &run_dir,
            Uuid::nil(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap();

        assert!(run_dir.join("metadata.json").exists());
        assert!(run_dir.join("findings.json").exists());
        assert!(run_dir.join("triage-decisions.json").exists());
        assert!(run_dir.join("excavation.json").exists());
        assert!(run_dir.join("staleness.json").exists());
        assert!(run_dir.join("diagnosis.json").exists());
        assert!(run_dir.join("archeology-checkpoint.json").exists());

        let patchset = run_dir.join("patchset");
        assert!(patchset.is_dir());
        assert!(patchset.join("f-1__docs_A.md.patch").exists());
        assert!(patchset.join("f-2__docs_B.md.patch").exists());
        assert!(patchset.join("f-3__docs_C.md.patch").exists());

        assert_eq!(summary.patches_written, 3);
        assert_eq!(summary.scaffold_files_written, 0);
        assert!(!summary.handoff_report_written);

        // metadata payload sanity
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(run_dir.join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(meta["bundle_schema_version"], 3);
        assert_eq!(meta["diagnosis_verdict"], "salvageable");
        assert_eq!(meta["findings_count"], 3);
    }

    #[test]
    fn no_docs_writes_scaffold_package_with_nested_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("ts");

        let findings = vec![
            scaffold_finding("s-1", "README.md", "# README\n"),
            scaffold_finding("s-2", "docs/getting-started.md", "# Start\n"),
            scaffold_finding("s-3", "docs/architecture/overview.md", "# Arch\n"),
            scaffold_finding("s-4", "CONTRIBUTING.md", "# Contrib\n"),
            scaffold_finding("s-5", "docs/guides/install.md", "# Install\n"),
        ];
        let run = make_run(DiagnosisVerdict::NoDocs, findings, Vec::new(), None);

        let summary = emit_bundle_v3(
            &run,
            &run_dir,
            Uuid::new_v4(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap();

        let pkg = run_dir.join("scaffold-package");
        assert!(pkg.join("README.md").exists());
        assert!(pkg.join("docs/getting-started.md").exists());
        assert!(pkg.join("docs/architecture/overview.md").exists());
        assert!(pkg.join("CONTRIBUTING.md").exists());
        assert!(pkg.join("docs/guides/install.md").exists());
        assert_eq!(summary.scaffold_files_written, 5);
        assert_eq!(summary.patches_written, 0);
        assert_eq!(summary.findings_dropped_for_path_safety, 0);
    }

    #[test]
    fn stale_beyond_salvage_populates_both_sub_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("ts");

        let findings = vec![
            scaffold_finding("s-1", "README.md", "# new readme\n"),
            scaffold_finding("s-2", "docs/intro.md", "# intro\n"),
            source_side_patch_finding("p-1", "src/main.rs"),
            source_side_patch_finding("p-2", "Cargo.toml"),
        ];
        let run = make_run(
            DiagnosisVerdict::StaleBeyondSalvage,
            findings,
            Vec::new(),
            None,
        );

        let summary = emit_bundle_v3(
            &run,
            &run_dir,
            Uuid::new_v4(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap();

        assert!(run_dir.join("scaffold-package/README.md").exists());
        assert!(run_dir.join("scaffold-package/docs/intro.md").exists());
        assert!(run_dir.join("patchset/p-1__src_main.rs.patch").exists());
        assert!(run_dir.join("patchset/p-2__Cargo.toml.patch").exists());
        assert_eq!(summary.scaffold_files_written, 2);
        assert_eq!(summary.patches_written, 2);
    }

    #[test]
    fn needs_operator_input_writes_handoff_and_skips_findings() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("ts");

        let handoff = HandoffReport {
            markdown: "# Handoff\nQuestions follow.".to_string(),
            questions: vec![OperatorQuestion {
                id: "q1".to_string(),
                text: "branch?".to_string(),
                reason: "ambiguous".to_string(),
                choices: vec!["salvage".to_string(), "rescaffold".to_string()],
            }],
        };
        let run = make_run(
            DiagnosisVerdict::NeedsOperatorInput,
            Vec::new(),
            Vec::new(),
            Some(handoff),
        );

        let summary = emit_bundle_v3(
            &run,
            &run_dir,
            Uuid::new_v4(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap();

        assert!(run_dir.join("archeology-report.md").exists());
        assert!(!run_dir.join("findings.json").exists());
        assert!(!run_dir.join("triage-decisions.json").exists());
        assert!(!run_dir.join("patchset").exists());
        assert!(!run_dir.join("scaffold-package").exists());
        assert!(summary.handoff_report_written);
        assert_eq!(summary.scaffold_files_written, 0);
        assert_eq!(summary.patches_written, 0);
    }

    #[test]
    fn unsafe_scaffold_paths_are_dropped_with_count() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("ts");

        let findings = vec![
            scaffold_finding("s-good", "docs/ok.md", "ok\n"),
            scaffold_finding("s-bad-1", "../../etc/passwd", "leak\n"),
            scaffold_finding("s-bad-2", "/abs/leak", "leak\n"),
        ];
        let run = make_run(DiagnosisVerdict::NoDocs, findings, Vec::new(), None);

        let summary = emit_bundle_v3(
            &run,
            &run_dir,
            Uuid::new_v4(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap();

        assert!(run_dir.join("scaffold-package/docs/ok.md").exists());
        assert_eq!(summary.scaffold_files_written, 1);
        assert_eq!(summary.findings_dropped_for_path_safety, 2);
        // No traversal escape: confirm passwd file did not land outside the run_dir.
        let escaped = run_dir
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("etc/passwd"));
        if let Some(p) = escaped {
            assert!(
                !p.exists(),
                "path traversal must not have escaped the bundle"
            );
        }
    }

    #[test]
    fn emit_twice_into_same_dir_is_caller_bug_but_stale_tmp_is_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("ts");

        let run = make_run(DiagnosisVerdict::NoDocs, Vec::new(), Vec::new(), None);

        // First emit succeeds.
        emit_bundle_v3(
            &run,
            &run_dir,
            Uuid::new_v4(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap();

        // Second emit into the same run_dir must fail-loud.
        let err = emit_bundle_v3(
            &run,
            &run_dir,
            Uuid::new_v4(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "expected fail-loud collision error, got: {err}"
        );

        // Now: simulate a stale `.tmp/` from a crashed prior emit, into a
        // fresh run_dir. The emit should clean it and succeed.
        let run_dir_2 = tmp.path().join("ts-2");
        let stale_tmp = tmp.path().join("ts-2.tmp");
        std::fs::create_dir_all(&stale_tmp).unwrap();
        std::fs::write(stale_tmp.join("garbage.txt"), b"junk").unwrap();
        emit_bundle_v3(
            &run,
            &run_dir_2,
            Uuid::new_v4(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap();
        assert!(!stale_tmp.exists(), "stale tmp must be cleaned");
        assert!(run_dir_2.join("metadata.json").exists());
    }

    #[test]
    fn atomic_visibility_only_flips_at_rename_time() {
        // We can't observe the atomic transition from a test thread
        // mid-call (the function is sync). Instead, verify the post-
        // condition: after a successful emit, run_dir exists and the
        // sibling .tmp/ does not.
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("ts");
        let tmp_sibling = tmp.path().join("ts.tmp");

        // Pre-condition: neither exists.
        assert!(!run_dir.exists());
        assert!(!tmp_sibling.exists());

        let run = make_run(DiagnosisVerdict::NoDocs, Vec::new(), Vec::new(), None);
        emit_bundle_v3(
            &run,
            &run_dir,
            Uuid::new_v4(),
            &ctx(),
            &BundleConfig::default(),
        )
        .unwrap();

        // Post-condition: run_dir exists, .tmp/ does not.
        assert!(run_dir.exists(), "run_dir must exist after emit");
        assert!(run_dir.join("metadata.json").exists());
        assert!(
            !tmp_sibling.exists(),
            ".tmp sibling must be renamed (not lingering) after emit"
        );
    }

    #[test]
    fn scaffold_cap_drops_overflow_findings() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("ts");

        let findings: Vec<Finding> = (0..100)
            .map(|i| scaffold_finding(&format!("s-{i}"), &format!("docs/f-{i}.md"), "x\n"))
            .collect();
        let run = make_run(DiagnosisVerdict::NoDocs, findings, Vec::new(), None);

        let cfg = BundleConfig {
            max_scaffold_files: 50,
            max_patches: 200,
        };
        let summary = emit_bundle_v3(&run, &run_dir, Uuid::new_v4(), &ctx(), &cfg).unwrap();
        assert_eq!(summary.scaffold_files_written, 50);
        // Overflow drops are NOT path-safety drops — `findings_dropped_for_path_safety`
        // stays 0 here. Cap drops are observable through `scaffold_files_written < 100`.
        assert_eq!(summary.findings_dropped_for_path_safety, 0);

        // Spot-check: the first 50 land, the rest do not.
        let pkg = run_dir.join("scaffold-package");
        assert!(pkg.join("docs/f-0.md").exists());
        assert!(pkg.join("docs/f-49.md").exists());
        assert!(!pkg.join("docs/f-50.md").exists());
    }
}
