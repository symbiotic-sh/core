//! Stage 10 — Archeology Checkpoint read/write.
//!
//! After each pipeline run we write a fresh `archeology-checkpoint.json`
//! into the triggering goal's artifact folder. Future runs (including
//! `on_drift_detected` re-runs) read the most-recent checkpoint to:
//!
//! - skip Excavate on unchanged repos (HEAD unchanged + no open issues),
//! - carry over `open_issues` (deferred findings eligible for
//!   re-evaluation) across runs,
//! - remember `discrepancy_files` that need a recheck regardless of
//!   whether they were touched since the last run.
//!
//! See `docs/design/source-archeology.md` §Archeology Checkpoint
//! Artifact.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::archeology_types::{
    DiagnosisVerdict, FindingDisposition, StalenessClass, TriageDecision,
};
use super::date::StalenessReport;
use super::excavate::ExcavationReport;

/// Current schema version. Bump when the on-disk shape changes.
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// Default time-to-live for `Defer`-disposition findings before they
/// are pruned from `open_issues` on checkpoint write. 90 days per the
/// design doc §Open Questions.
pub const DEFAULT_DEFER_TTL_DAYS: u32 = 90;

// ── Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArcheologyCheckpoint {
    pub schema_version: u32,
    pub repo_id: String,
    pub run_timestamp: DateTime<Utc>,
    pub observed_head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_observed_head: Option<String>,
    pub diagnosis_verdict: DiagnosisVerdict,
    pub diagnosis_confidence: f32,
    #[serde(default)]
    pub staleness_by_file: std::collections::BTreeMap<String, StalenessRecord>,
    #[serde(default)]
    pub discrepancy_files: Vec<DiscrepancyEntry>,
    #[serde(default)]
    pub aspirational_claims: Vec<AspirationalClaim>,
    #[serde(default)]
    pub open_issues: Vec<OpenIssue>,
    #[serde(default)]
    pub files_analyzed: Vec<String>,
    #[serde(default)]
    pub no_drift_files: Vec<String>,
    pub findings_count: usize,
    pub triage_summary: TriageSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StalenessRecord {
    pub classification: StalenessClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_human_touch_age_days: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscrepancyEntry {
    pub path: String,
    #[serde(default)]
    pub findings: Vec<String>,
    /// `"recheck"` — recheck the file on next run even if unchanged.
    pub status_on_next_run: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AspirationalClaim {
    pub file: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub line_range: String,
    pub claim: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenIssue {
    pub finding_id: String,
    pub disposition: FindingDisposition,
    pub reason: String,
    pub eligible_for_next_run: bool,
    /// RFC3339 timestamp of the run that produced this open issue.
    /// Used by checkpoint pruning to enforce `defer_ttl_days`.
    #[serde(default)]
    pub first_seen: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageSummary {
    #[serde(default)]
    pub resolve: usize,
    #[serde(default)]
    pub defer: usize,
    #[serde(default)]
    pub escalate: usize,
}

// ── Config (per docs/design/agent-tunables.md) ─────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct CheckpointConfig {
    /// Open-issues older than this many days are pruned on checkpoint
    /// write. Default: 90 — matches the design doc's proposal.
    pub defer_ttl_days: u32,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            defer_ttl_days: DEFAULT_DEFER_TTL_DAYS,
        }
    }
}

// ── Build from stage outputs ───────────────────────────────────────────

/// Assemble a fresh checkpoint from the run's stage outputs.
///
/// `previous` is the prior-run checkpoint if one was found (used for
/// `previous_observed_head` + open-issue carryover). `now` is injected
/// for deterministic testing.
pub fn build_checkpoint(
    excavation: &ExcavationReport,
    staleness: &StalenessReport,
    verdict: DiagnosisVerdict,
    confidence: f32,
    decisions: &[TriageDecision],
    previous: Option<&ArcheologyCheckpoint>,
    now: DateTime<Utc>,
    config: CheckpointConfig,
) -> ArcheologyCheckpoint {
    let mut staleness_by_file: std::collections::BTreeMap<String, StalenessRecord> =
        std::collections::BTreeMap::new();
    let mut files_analyzed: Vec<String> = Vec::new();
    let mut no_drift_files: Vec<String> = Vec::new();

    for row in &staleness.rows {
        files_analyzed.push(row.path.clone());
        staleness_by_file.insert(
            row.path.clone(),
            StalenessRecord {
                classification: row.class,
                last_human_touch_age_days: row.last_human_touch_age_days,
                evidence: row.evidence.clone(),
            },
        );
        if matches!(row.class, StalenessClass::Fresh | StalenessClass::Drifting)
            && !row.aspirational
        {
            // Empty bucket for now — the design doc distinguishes "no
            // drift" from "drifting"; Fresh only goes here. Keeping the
            // field populated for audit.
            if row.class == StalenessClass::Fresh {
                no_drift_files.push(row.path.clone());
            }
        }
    }

    let mut triage_summary = TriageSummary::default();
    for d in decisions {
        match d.disposition {
            FindingDisposition::Resolve => triage_summary.resolve += 1,
            FindingDisposition::Defer => triage_summary.defer += 1,
            FindingDisposition::Escalate => triage_summary.escalate += 1,
        }
    }

    // Carry over prior open_issues (TTL-filtered) and fold in this run's
    // Defer-disposition decisions.
    let ttl_cutoff = now - chrono::Duration::days(i64::from(config.defer_ttl_days));
    let mut open_issues: Vec<OpenIssue> = previous
        .map(|p| {
            p.open_issues
                .iter()
                .filter(|oi| {
                    oi.eligible_for_next_run
                        && oi.first_seen.map(|ts| ts >= ttl_cutoff).unwrap_or(true)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // Add this run's Defer findings (dedupe by finding_id).
    for d in decisions {
        if d.disposition == FindingDisposition::Defer
            && !open_issues.iter().any(|oi| oi.finding_id == d.finding_id)
        {
            open_issues.push(OpenIssue {
                finding_id: d.finding_id.clone(),
                disposition: d.disposition,
                reason: d.rationale.clone(),
                eligible_for_next_run: true,
                first_seen: Some(now),
            });
        }
    }

    // aspirational_claims: synthesized from staleness rows tagged
    // aspirational. Evidence string already carries the "why."
    let aspirational_claims: Vec<AspirationalClaim> = staleness
        .rows
        .iter()
        .filter(|r| r.aspirational)
        .map(|r| AspirationalClaim {
            file: r.path.clone(),
            line_range: String::new(),
            claim: r.evidence.clone(),
        })
        .collect();

    ArcheologyCheckpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        repo_id: excavation.repo_id.clone(),
        run_timestamp: now,
        observed_head: excavation.observed_head.clone(),
        previous_observed_head: previous.map(|p| p.observed_head.clone()),
        diagnosis_verdict: verdict,
        diagnosis_confidence: confidence,
        staleness_by_file,
        discrepancy_files: Vec::new(),
        aspirational_claims,
        open_issues,
        files_analyzed,
        no_drift_files,
        findings_count: decisions.len(),
        triage_summary,
    }
}

// ── Disk I/O ───────────────────────────────────────────────────────────

/// Write `checkpoint` to `{dir}/archeology-checkpoint.json`.
/// Creates intermediate directories if needed.
pub fn write_checkpoint(dir: &Path, checkpoint: &ArcheologyCheckpoint) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create_dir_all {dir:?}"))?;
    let path = dir.join("archeology-checkpoint.json");
    let json = serde_json::to_string_pretty(checkpoint).context("serialize checkpoint")?;
    std::fs::write(&path, json).with_context(|| format!("write {path:?}"))?;
    Ok(path)
}

/// Read `archeology-checkpoint.json` from `path`. Returns `Ok(None)` if
/// the file doesn't exist (first-run case).
pub fn read_checkpoint(path: &Path) -> Result<Option<ArcheologyCheckpoint>> {
    match std::fs::read_to_string(path) {
        Ok(body) => {
            let cp: ArcheologyCheckpoint =
                serde_json::from_str(&body).with_context(|| format!("parse {path:?}"))?;
            Ok(Some(cp))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(anyhow::Error::from(err).context(format!("read {path:?}"))),
    }
}

/// Walk a goal-artifacts root to find the most-recent checkpoint file.
/// Expected layout:
/// `{root}/source-archeology/*/archeology-checkpoint.json`
/// — each subdirectory is a per-run timestamp. Returns `None` if no
/// prior run exists (first-run case).
pub fn find_latest_checkpoint(
    source_archeology_root: &Path,
) -> Result<Option<ArcheologyCheckpoint>> {
    if !source_archeology_root.exists() {
        return Ok(None);
    }
    let mut candidates: Vec<(std::ffi::OsString, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(source_archeology_root)
        .with_context(|| format!("read_dir {source_archeology_root:?}"))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let cp_path = entry.path().join("archeology-checkpoint.json");
        if cp_path.exists() {
            candidates.push((entry.file_name(), cp_path));
        }
    }
    // Directory names are timestamps (ISO8601 or similar) — lexicographic
    // sort yields chronological order.
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    let Some((_, latest_path)) = candidates.last() else {
        return Ok(None);
    };
    read_checkpoint(latest_path)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::{FindingDisposition, StalenessClass};
    use crate::source_archeology::date::StalenessRow;
    use crate::source_archeology::excavate::Observation;

    fn sample_excavation() -> ExcavationReport {
        ExcavationReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "abc123".to_string(),
            observations: vec![Observation {
                category: crate::source_archeology::excavate::ObservationCategory::SignalFile,
                path: "README.md".to_string(),
                evidence: "size=100".to_string(),
            }],
        }
    }

    fn sample_staleness() -> StalenessReport {
        StalenessReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "abc123".to_string(),
            rows: vec![
                StalenessRow {
                    path: "README.md".to_string(),
                    class: StalenessClass::Fresh,
                    last_commit_age_days: 10,
                    last_human_touch_age_days: Some(10),
                    wired: true,
                    aspirational: false,
                    evidence: "rule=fresh".to_string(),
                },
                StalenessRow {
                    path: "CLAUDE.md".to_string(),
                    class: StalenessClass::Aspirational,
                    last_commit_age_days: 200,
                    last_human_touch_age_days: Some(200),
                    wired: false,
                    aspirational: true,
                    evidence: "references missing workflows".to_string(),
                },
            ],
        }
    }

    fn decisions(resolves: usize, defers: usize, escalates: usize) -> Vec<TriageDecision> {
        let mut v = Vec::new();
        for i in 0..resolves {
            v.push(TriageDecision {
                finding_id: format!("r-{i}"),
                disposition: FindingDisposition::Resolve,
                rationale: "ok".to_string(),
            });
        }
        for i in 0..defers {
            v.push(TriageDecision {
                finding_id: format!("d-{i}"),
                disposition: FindingDisposition::Defer,
                rationale: "defer-reason".to_string(),
            });
        }
        for i in 0..escalates {
            v.push(TriageDecision {
                finding_id: format!("e-{i}"),
                disposition: FindingDisposition::Escalate,
                rationale: "escalate-reason".to_string(),
            });
        }
        v
    }

    #[test]
    fn checkpoint_roundtrip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cp = build_checkpoint(
            &sample_excavation(),
            &sample_staleness(),
            DiagnosisVerdict::Salvageable,
            0.92,
            &decisions(2, 1, 0),
            None,
            Utc::now(),
            CheckpointConfig::default(),
        );
        let path = write_checkpoint(dir.path(), &cp).unwrap();
        let parsed = read_checkpoint(&path).unwrap().expect("present");
        assert_eq!(parsed.repo_id, "repo:flux");
        assert_eq!(parsed.observed_head, "abc123");
        assert_eq!(parsed.diagnosis_verdict, DiagnosisVerdict::Salvageable);
        assert_eq!(parsed.findings_count, 3);
        assert_eq!(parsed.triage_summary.resolve, 2);
        assert_eq!(parsed.triage_summary.defer, 1);
        assert_eq!(parsed.triage_summary.escalate, 0);
        assert_eq!(parsed.open_issues.len(), 1);
        assert_eq!(parsed.aspirational_claims.len(), 1);
    }

    #[test]
    fn first_run_has_no_previous_head() {
        let cp = build_checkpoint(
            &sample_excavation(),
            &sample_staleness(),
            DiagnosisVerdict::NoDocs,
            0.90,
            &[],
            None,
            Utc::now(),
            CheckpointConfig::default(),
        );
        assert!(cp.previous_observed_head.is_none());
    }

    #[test]
    fn second_run_records_previous_head() {
        let prev = ArcheologyCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            repo_id: "repo:flux".to_string(),
            run_timestamp: Utc::now(),
            observed_head: "old-head".to_string(),
            previous_observed_head: None,
            diagnosis_verdict: DiagnosisVerdict::Salvageable,
            diagnosis_confidence: 0.85,
            staleness_by_file: Default::default(),
            discrepancy_files: Vec::new(),
            aspirational_claims: Vec::new(),
            open_issues: Vec::new(),
            files_analyzed: Vec::new(),
            no_drift_files: Vec::new(),
            findings_count: 0,
            triage_summary: TriageSummary::default(),
        };
        let cp = build_checkpoint(
            &sample_excavation(),
            &sample_staleness(),
            DiagnosisVerdict::Salvageable,
            0.90,
            &[],
            Some(&prev),
            Utc::now(),
            CheckpointConfig::default(),
        );
        assert_eq!(cp.previous_observed_head.as_deref(), Some("old-head"));
    }

    #[test]
    fn open_issues_carry_over_from_previous() {
        let now = Utc::now();
        let prev = ArcheologyCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            repo_id: "repo:flux".to_string(),
            run_timestamp: now - chrono::Duration::days(10),
            observed_head: "old".to_string(),
            previous_observed_head: None,
            diagnosis_verdict: DiagnosisVerdict::Salvageable,
            diagnosis_confidence: 0.85,
            staleness_by_file: Default::default(),
            discrepancy_files: Vec::new(),
            aspirational_claims: Vec::new(),
            open_issues: vec![OpenIssue {
                finding_id: "carry-1".to_string(),
                disposition: FindingDisposition::Defer,
                reason: "stale issue".to_string(),
                eligible_for_next_run: true,
                first_seen: Some(now - chrono::Duration::days(10)),
            }],
            files_analyzed: Vec::new(),
            no_drift_files: Vec::new(),
            findings_count: 1,
            triage_summary: TriageSummary {
                resolve: 0,
                defer: 1,
                escalate: 0,
            },
        };
        let cp = build_checkpoint(
            &sample_excavation(),
            &sample_staleness(),
            DiagnosisVerdict::Salvageable,
            0.90,
            &decisions(0, 1, 0),
            Some(&prev),
            now,
            CheckpointConfig::default(),
        );
        let ids: Vec<&str> = cp
            .open_issues
            .iter()
            .map(|oi| oi.finding_id.as_str())
            .collect();
        assert!(ids.contains(&"carry-1"), "prior open issue preserved");
        assert!(ids.contains(&"d-0"), "new defer added");
    }

    #[test]
    fn ttl_prunes_stale_open_issues() {
        let now = Utc::now();
        let prev = ArcheologyCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            repo_id: "repo:flux".to_string(),
            run_timestamp: now - chrono::Duration::days(100),
            observed_head: "old".to_string(),
            previous_observed_head: None,
            diagnosis_verdict: DiagnosisVerdict::Salvageable,
            diagnosis_confidence: 0.85,
            staleness_by_file: Default::default(),
            discrepancy_files: Vec::new(),
            aspirational_claims: Vec::new(),
            open_issues: vec![OpenIssue {
                finding_id: "ancient".to_string(),
                disposition: FindingDisposition::Defer,
                reason: "ancient".to_string(),
                eligible_for_next_run: true,
                first_seen: Some(now - chrono::Duration::days(100)), // >90d TTL
            }],
            files_analyzed: Vec::new(),
            no_drift_files: Vec::new(),
            findings_count: 1,
            triage_summary: TriageSummary::default(),
        };
        let cp = build_checkpoint(
            &sample_excavation(),
            &sample_staleness(),
            DiagnosisVerdict::Salvageable,
            0.90,
            &[],
            Some(&prev),
            now,
            CheckpointConfig::default(),
        );
        let ids: Vec<&str> = cp
            .open_issues
            .iter()
            .map(|oi| oi.finding_id.as_str())
            .collect();
        assert!(
            !ids.contains(&"ancient"),
            "issues older than defer_ttl_days are pruned"
        );
    }

    #[test]
    fn find_latest_returns_none_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let found = find_latest_checkpoint(&dir.path().join("nonexistent")).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn find_latest_picks_newest_timestamp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("source-archeology");
        std::fs::create_dir_all(root.join("2026-04-17T10-00-00Z")).unwrap();
        std::fs::create_dir_all(root.join("2026-04-18T10-00-00Z")).unwrap();
        std::fs::create_dir_all(root.join("2026-04-16T10-00-00Z")).unwrap();

        let older = ArcheologyCheckpoint {
            schema_version: 1,
            repo_id: "repo:flux".to_string(),
            run_timestamp: Utc::now(),
            observed_head: "older-head".to_string(),
            previous_observed_head: None,
            diagnosis_verdict: DiagnosisVerdict::NoDocs,
            diagnosis_confidence: 0.9,
            staleness_by_file: Default::default(),
            discrepancy_files: Vec::new(),
            aspirational_claims: Vec::new(),
            open_issues: Vec::new(),
            files_analyzed: Vec::new(),
            no_drift_files: Vec::new(),
            findings_count: 0,
            triage_summary: TriageSummary::default(),
        };
        let newer = ArcheologyCheckpoint {
            observed_head: "newer-head".to_string(),
            ..older.clone()
        };
        write_checkpoint(&root.join("2026-04-17T10-00-00Z"), &older).unwrap();
        write_checkpoint(&root.join("2026-04-18T10-00-00Z"), &newer).unwrap();
        write_checkpoint(&root.join("2026-04-16T10-00-00Z"), &older).unwrap();

        let picked = find_latest_checkpoint(&root).unwrap().expect("present");
        assert_eq!(picked.observed_head, "newer-head");
    }
}
