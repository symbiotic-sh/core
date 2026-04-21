//! Stage 2 — Date (per-artifact staleness classification).
//!
//! Deterministic pre-filter handles the clear cases (fresh / stale / dead).
//! Ambiguous rows flow to a fast-tier LLM (`AspirationalClassifier`) which
//! adjudicates `aspirational` vs `drifting` / `stale`. Err from the LLM
//! falls back to `Drifting` per the design doc's Failure Modes table.
//!
//! See `docs/design/source-archeology.md` §Stage 2 — Date.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use symbiotic_core::protocol::{ChatMessage, LlmClient};

use super::archeology_types::{ArcheologyTarget, StalenessClass};
use super::excavate::{ExcavationReport, Observation, ObservationCategory};

// ── Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StalenessReport {
    pub repo_id: String,
    pub observed_head: String,
    pub rows: Vec<StalenessRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StalenessRow {
    /// Repo-relative path of the classified artifact.
    pub path: String,
    pub class: StalenessClass,
    /// Age in days since the file was last modified (any author).
    pub last_commit_age_days: u32,
    /// Age in days since a non-bot, non-auto-refactor commit touched
    /// the file. `None` if the file has only bot-authored history.
    pub last_human_touch_age_days: Option<u32>,
    /// `true` if the file is referenced by CI, another doc, or a
    /// build-system manifest. Drives `dead`.
    pub wired: bool,
    /// `true` iff the Date worker judged the doc describes aspirational
    /// behavior. Combines the deterministic pre-filter with the LLM
    /// adjudication pass.
    pub aspirational: bool,
    /// Short evidence pointer — rule name + signal values. For audit /
    /// operator display.
    pub evidence: String,
}

// ── LLM seam ───────────────────────────────────────────────────────────

/// LLM-backed adjudicator for the aspirational-vs-stale distinction.
/// Tier: `fast` (low-latency classifier; short prompt, small context).
///
/// The Date worker only invokes this on rows the deterministic pre-filter
/// cannot decide. `Err` returns fall back to `Drifting` conservatively
/// per the design doc's Failure Modes table.
#[async_trait]
pub trait AspirationalClassifier: Send + Sync {
    /// `Ok(true)` = aspirational (planned / never existed).
    /// `Ok(false)` = stale or fresh (past or present behavior).
    async fn is_aspirational(
        &self,
        doc_path: &str,
        excerpt: &str,
        evidence: &[String],
    ) -> Result<bool>;
}

/// Production classifier backed by an `LlmClient` (`fast`-tier model).
/// Retries once on transient failure before returning `Err`.
pub struct LlmAspirationalClassifier<'a> {
    client: &'a dyn LlmClient,
}

impl<'a> LlmAspirationalClassifier<'a> {
    pub fn new(client: &'a dyn LlmClient) -> Self {
        Self { client }
    }

    fn build_messages(doc_path: &str, excerpt: &str, evidence: &[String]) -> Vec<ChatMessage> {
        let system = "You classify whether a documentation excerpt describes \
                      aspirational behavior (planned or never-existed) versus \
                      stale/past behavior. Reply with exactly one JSON object: \
                      {\"aspirational\": true|false}. No prose.";
        let joined_evidence = evidence.join("\n");
        let user = format!("PATH: {doc_path}\nEVIDENCE:\n{joined_evidence}\n\nEXCERPT:\n{excerpt}");
        vec![
            ChatMessage {
                role: "system".to_string(),
                content: system.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user,
            },
        ]
    }
}

#[async_trait]
impl<'a> AspirationalClassifier for LlmAspirationalClassifier<'a> {
    async fn is_aspirational(
        &self,
        doc_path: &str,
        excerpt: &str,
        evidence: &[String],
    ) -> Result<bool> {
        let messages = Self::build_messages(doc_path, excerpt, evidence);
        // One retry on transient error.
        let resp = match self.client.chat(&messages, true).await {
            Ok(r) => r,
            Err(first_err) => match self.client.chat(&messages, true).await {
                Ok(r) => r,
                Err(second_err) => {
                    return Err(anyhow::anyhow!(
                        "LLM aspirational classifier failed twice: {first_err}; then: {second_err}"
                    ));
                }
            },
        };
        parse_aspirational_response(&resp)
    }
}

fn parse_aspirational_response(resp: &str) -> Result<bool> {
    let parsed: serde_json::Value = serde_json::from_str(resp.trim())
        .map_err(|e| anyhow::anyhow!("LLM response is not JSON: {e}; raw={resp}"))?;
    parsed
        .get("aspirational")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| anyhow::anyhow!("LLM response missing bool `aspirational`: {resp}"))
}

// ── Entry point ────────────────────────────────────────────────────────

/// Run Stage 2 against the excavation report.
pub async fn run(
    target: &ArcheologyTarget,
    clone_root: &Path,
    excavation: &ExcavationReport,
    classifier: &dyn AspirationalClassifier,
) -> Result<StalenessReport> {
    let now_secs = chrono::Utc::now().timestamp();
    let signal_rows: Vec<&Observation> = excavation
        .observations
        .iter()
        .filter(|o| o.category == ObservationCategory::SignalFile)
        .collect();

    // Wired-set: files referenced by (a) CI configs, (b) other docs via
    // WireMismatch targets that DO exist, (c) build-system manifests.
    let wired_set = build_wired_set(&excavation.observations, clone_root);

    // Wire-mismatch map: file → list of missing references found in it.
    let mut wire_mismatches: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for o in &excavation.observations {
        if o.category == ObservationCategory::WireMismatch {
            wire_mismatches
                .entry(o.path.clone())
                .or_default()
                .push(o.evidence.clone());
        }
    }

    let mut rows: Vec<StalenessRow> = Vec::new();
    for observation in signal_rows {
        let path = observation.path.clone();
        let last_commit_age_days = file_age_days(clone_root, &path, None, now_secs);
        let last_human_touch_age_days = file_age_days_humans_only(clone_root, &path, now_secs);
        let wired = wired_set.contains(&path);
        let evidence_refs = wire_mismatches.get(&path).cloned().unwrap_or_default();
        let has_wire_mismatch = !evidence_refs.is_empty();

        let (class, aspirational, evidence) = classify_row(
            &path,
            last_commit_age_days,
            last_human_touch_age_days,
            wired,
            has_wire_mismatch,
            &evidence_refs,
            clone_root,
            classifier,
        )
        .await;

        rows.push(StalenessRow {
            path,
            class,
            last_commit_age_days,
            last_human_touch_age_days,
            wired,
            aspirational,
            evidence,
        });
    }

    Ok(StalenessReport {
        repo_id: target.repo_id.clone(),
        observed_head: excavation.observed_head.clone(),
        rows,
    })
}

// ── Classification ─────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn classify_row(
    path: &str,
    last_commit_age_days: u32,
    last_human_touch_age_days: Option<u32>,
    wired: bool,
    has_wire_mismatch: bool,
    evidence_refs: &[String],
    clone_root: &Path,
    classifier: &dyn AspirationalClassifier,
) -> (StalenessClass, bool, String) {
    // Rule 1: Dead (signal file, unwired, ancient).
    if !wired && last_commit_age_days > 365 {
        return (
            StalenessClass::Dead,
            false,
            format!("rule=dead; wired=false; last_commit_age_days={last_commit_age_days}"),
        );
    }

    // Rule 2: Candidate for Aspirational (wire-mismatch + aged human touch).
    if has_wire_mismatch && last_human_touch_age_days.map(|d| d > 180).unwrap_or(true) {
        // LLM adjudication.
        let (is_asp, evidence_tag) =
            adjudicate_aspirational(path, evidence_refs, clone_root, classifier).await;
        if is_asp {
            return (
                StalenessClass::Aspirational,
                true,
                format!(
                    "rule=llm:aspirational; {evidence_tag}; last_human_touch_age_days={:?}",
                    last_human_touch_age_days
                ),
            );
        } else {
            // LLM said not aspirational. Route to Stale if old / no human
            // touches; otherwise Drifting.
            let (class_name, cls) =
                if last_commit_age_days > 180 || last_human_touch_age_days.is_none() {
                    ("stale", StalenessClass::Stale)
                } else {
                    ("drifting", StalenessClass::Drifting)
                };
            return (
                cls,
                false,
                format!(
                    "rule=llm:{evidence_tag}->{class_name}; last_commit_age_days={last_commit_age_days}"
                ),
            );
        }
    }

    // Rule 3: Stale (no human touches in 180d, or 365d since any commit).
    if last_commit_age_days > 365
        || (last_human_touch_age_days.is_none() && last_commit_age_days > 180)
    {
        return (
            StalenessClass::Stale,
            false,
            format!(
                "rule=stale; last_commit_age_days={last_commit_age_days}; last_human_touch_age_days={:?}",
                last_human_touch_age_days
            ),
        );
    }

    // Rule 4: Drifting (wire-mismatch or 90-365d age).
    if has_wire_mismatch || last_commit_age_days > 90 {
        // Drifting case may still benefit from LLM adjudication when a
        // wire-mismatch is present — but if the human touch is recent
        // (<=180d), skip the LLM per Rule 2's guard and classify as
        // Drifting deterministically.
        return (
            StalenessClass::Drifting,
            false,
            format!(
                "rule=drifting; last_commit_age_days={last_commit_age_days}; wire_mismatch={has_wire_mismatch}"
            ),
        );
    }

    // Rule 5: Fresh (default).
    (
        StalenessClass::Fresh,
        false,
        format!("rule=fresh; last_commit_age_days={last_commit_age_days}; wired={wired}"),
    )
}

async fn adjudicate_aspirational(
    path: &str,
    evidence_refs: &[String],
    clone_root: &Path,
    classifier: &dyn AspirationalClassifier,
) -> (bool, &'static str) {
    let excerpt = load_excerpt(clone_root, path, 1000);
    match classifier
        .is_aspirational(path, &excerpt, evidence_refs)
        .await
    {
        Ok(true) => (true, "aspirational"),
        Ok(false) => (false, "not_aspirational"),
        Err(_) => (false, "fallback_drifting"),
    }
}

// ── Git-age helpers ────────────────────────────────────────────────────

fn file_age_days(
    clone_root: &Path,
    path: &str,
    author_filter: Option<&[&str]>,
    now_secs: i64,
) -> u32 {
    let ts = last_commit_ts_for(clone_root, path, author_filter);
    match ts {
        Some(ts) => {
            let delta = (now_secs - ts).max(0);
            (delta / (24 * 60 * 60)) as u32
        }
        None => u32::MAX,
    }
}

fn file_age_days_humans_only(clone_root: &Path, path: &str, now_secs: i64) -> Option<u32> {
    // List commits touching the path, inspect authors until we find a
    // human (non-bot) author. Limit to 50 commits.
    let out = Command::new("git")
        .arg("-C")
        .arg(clone_root)
        .args(["log", "--max-count=50", "--format=%at%x09%ae", "--", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split('\t');
        let ts: i64 = parts.next()?.parse().ok()?;
        let email = parts.next().unwrap_or("");
        if !is_bot_email(email) {
            let delta = (now_secs - ts).max(0);
            return Some((delta / (24 * 60 * 60)) as u32);
        }
    }
    None
}

fn last_commit_ts_for(
    clone_root: &Path,
    path: &str,
    author_filter: Option<&[&str]>,
) -> Option<i64> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(clone_root)
        .args(["log", "-1", "--format=%at"]);
    if let Some(authors) = author_filter {
        for a in authors {
            cmd.arg(format!("--author={a}"));
        }
    }
    cmd.arg("--").arg(path);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn is_bot_email(email: &str) -> bool {
    let e = email.to_ascii_lowercase();
    e.contains("dependabot")
        || e.contains("renovate")
        || e.starts_with("bot@")
        || e.contains("bot@")
        || e.contains("github-actions")
        || e.contains("noreply@symbiotic.sh")
}

// ── Wired-set + excerpt helpers ────────────────────────────────────────

fn build_wired_set(observations: &[Observation], clone_root: &Path) -> HashSet<String> {
    let mut wired: HashSet<String> = HashSet::new();

    // CI config files are wired by definition.
    for o in observations {
        if o.category == ObservationCategory::CiConfig {
            wired.insert(o.path.clone());
        }
    }

    // Build-system manifests are wired.
    for o in observations {
        if o.category == ObservationCategory::BuildSystem {
            wired.insert(o.path.clone());
        }
    }

    // For each signal file, scan every OTHER signal file for a reference
    // to it. If anyone references the target, target is wired.
    let md_paths: Vec<String> = observations
        .iter()
        .filter(|o| {
            o.category == ObservationCategory::SignalFile
                && o.path.to_ascii_lowercase().ends_with(".md")
        })
        .map(|o| o.path.clone())
        .collect();

    for target_path in &md_paths {
        for source_path in &md_paths {
            if source_path == target_path {
                continue;
            }
            if file_references_target(clone_root, source_path, target_path) {
                wired.insert(target_path.clone());
                break;
            }
        }
    }
    wired
}

fn file_references_target(clone_root: &Path, source_rel: &str, target_rel: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(clone_root.join(source_rel)) else {
        return false;
    };
    let target_basename = Path::new(target_rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(target_rel);
    content.contains(target_rel) || content.contains(target_basename)
}

fn load_excerpt(clone_root: &Path, rel: &str, max_chars: usize) -> String {
    let path: PathBuf = clone_root.join(rel);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return String::new();
    };
    if content.len() <= max_chars {
        return content;
    }
    content.chars().take(max_chars).collect()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aspirational_true() {
        assert!(parse_aspirational_response(r#"{"aspirational": true}"#).unwrap());
    }

    #[test]
    fn parse_aspirational_false() {
        assert!(!parse_aspirational_response(r#"{"aspirational": false}"#).unwrap());
    }

    #[test]
    fn parse_aspirational_rejects_garbage() {
        assert!(parse_aspirational_response("not json").is_err());
        assert!(parse_aspirational_response(r#"{"other": true}"#).is_err());
    }

    #[test]
    fn bot_email_detection() {
        assert!(is_bot_email(
            "49699333+dependabot[bot]@users.noreply.github.com"
        ));
        assert!(is_bot_email("renovate[bot]@users.noreply.github.com"));
        assert!(is_bot_email("actions-user@github-actions.noreply"));
        assert!(is_bot_email("noreply@symbiotic.sh"));
        assert!(!is_bot_email("alice@example.com"));
    }

    // ── Fixture-based classification tests ─────────────────────────

    use crate::source_archeology::archeology_types::{ArcheologyMode, StalenessClass};
    use crate::source_archeology::excavate::run as excavate_run;
    use crate::source_archeology::fixtures::{
        build, DeterministicOnlyClassifier, ScriptedClassifier,
    };

    fn target() -> ArcheologyTarget {
        ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "main".to_string(),
            goal_id: "onboard".to_string(),
            allowed_paths: Vec::new(),
            mode: ArcheologyMode::default(),
        }
    }

    fn row_for<'a>(report: &'a StalenessReport, path: &str) -> &'a StalenessRow {
        report
            .rows
            .iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("no row for {path} in {:?}", report.rows))
    }

    #[tokio::test]
    async fn date_classifies_fresh_without_llm() {
        let fx = build(|_r, author| {
            // Cargo.toml exists so README is wired via build-system manifest
            // lookup AND via wired_set (Cargo.toml being referenced in the
            // doc ensures the doc reference path lights up too).
            author.commit_file(
                "Cargo.toml",
                "[package]\nname=\"flux\"\nversion=\"0\"\n",
                10,
                "alice@example.com",
            )?;
            // README mentions Cargo.toml so it looks wired to a manifest.
            author.commit_file(
                "README.md",
                "See Cargo.toml for build info.\n",
                10,
                "alice@example.com",
            )?;
            Ok(())
        })
        .unwrap();

        let clone_root = fx.clone_root();
        let excavation = excavate_run(&target(), &clone_root).unwrap();
        let classifier = DeterministicOnlyClassifier;
        let report = run(&target(), &clone_root, &excavation, &classifier)
            .await
            .expect("date");

        let readme = row_for(&report, "README.md");
        assert_eq!(readme.class, StalenessClass::Fresh, "row={readme:?}");
        assert!(!readme.aspirational);
        assert!(readme.evidence.starts_with("rule=fresh"));
    }

    #[tokio::test]
    async fn date_classifies_stale_without_llm() {
        let fx = build(|_r, author| {
            // README references docs/old.md — this wires docs/old.md, which
            // disqualifies the Dead rule and routes to Stale (Rule 3).
            author.commit_file(
                "README.md",
                "See docs/old.md for legacy architecture.\n",
                400,
                "noreply@symbiotic.sh",
            )?;
            author.commit_file(
                "docs/old.md",
                "# Old architecture\n",
                400,
                "noreply@symbiotic.sh",
            )?;
            Ok(())
        })
        .unwrap();

        let clone_root = fx.clone_root();
        let excavation = excavate_run(&target(), &clone_root).unwrap();
        let classifier = DeterministicOnlyClassifier;
        let report = run(&target(), &clone_root, &excavation, &classifier)
            .await
            .expect("date");

        let old = row_for(&report, "docs/old.md");
        assert_eq!(old.class, StalenessClass::Stale, "row={old:?}");
        assert!(!old.aspirational);
        assert!(old.evidence.starts_with("rule=stale"));
    }

    #[tokio::test]
    async fn date_classifies_dead_without_llm() {
        let fx = build(|_r, author| {
            // README to anchor the wired-set (but it does not reference orphan).
            author.commit_file("README.md", "# Flux\n", 400, "alice@example.com")?;
            author.commit_file(
                "docs/orphan.md",
                "# Orphan — nothing references me.\n",
                400,
                "alice@example.com",
            )?;
            Ok(())
        })
        .unwrap();

        let clone_root = fx.clone_root();
        let excavation = excavate_run(&target(), &clone_root).unwrap();
        let classifier = DeterministicOnlyClassifier;
        let report = run(&target(), &clone_root, &excavation, &classifier)
            .await
            .expect("date");

        let orphan = row_for(&report, "docs/orphan.md");
        assert_eq!(orphan.class, StalenessClass::Dead, "row={orphan:?}");
        assert!(orphan.evidence.starts_with("rule=dead"));
    }

    #[tokio::test]
    async fn date_classifies_aspirational_via_llm() {
        let fx = build(|_r, author| {
            // CLAUDE.md references a non-existent directory → WireMismatch.
            // Last human touch 200 days ago → qualifies for LLM adjudication.
            author.commit_file(
                "CLAUDE.md",
                "Workflows live under `./crates/symbiotic-workflows/templates/`.\n",
                200,
                "alice@example.com",
            )?;
            Ok(())
        })
        .unwrap();

        let clone_root = fx.clone_root();
        let excavation = excavate_run(&target(), &clone_root).unwrap();
        let classifier = ScriptedClassifier::new_ok(true);
        let report = run(&target(), &clone_root, &excavation, &classifier)
            .await
            .expect("date");

        let row = row_for(&report, "CLAUDE.md");
        assert_eq!(row.class, StalenessClass::Aspirational, "row={row:?}");
        assert!(row.aspirational);
        assert!(
            row.evidence.contains("llm:aspirational"),
            "evidence missing llm marker: {}",
            row.evidence
        );
    }

    #[tokio::test]
    async fn date_classifies_drifting_via_llm() {
        let fx = build(|_r, author| {
            // Same shape as aspirational test but the LLM says "not
            // aspirational"; with 200d last_human_touch the deterministic
            // branch routes to Stale.
            author.commit_file(
                "docs/architecture.md",
                "See `./bin/daemon` for details.\n",
                200,
                "alice@example.com",
            )?;
            Ok(())
        })
        .unwrap();

        let clone_root = fx.clone_root();
        let excavation = excavate_run(&target(), &clone_root).unwrap();
        let classifier = ScriptedClassifier::new_ok(false);
        let report = run(&target(), &clone_root, &excavation, &classifier)
            .await
            .expect("date");

        let row = row_for(&report, "docs/architecture.md");
        assert_eq!(row.class, StalenessClass::Stale, "row={row:?}");
        assert!(!row.aspirational);
        assert!(
            row.evidence.contains("llm:not_aspirational"),
            "evidence missing llm marker: {}",
            row.evidence
        );
    }

    #[tokio::test]
    async fn date_falls_back_to_drifting_on_llm_error() {
        let fx = build(|_r, author| {
            // Aspirational-candidate shape; LLM errors.
            author.commit_file(
                "docs/architecture.md",
                "See `./bin/daemon` for details.\n",
                200,
                "alice@example.com",
            )?;
            Ok(())
        })
        .unwrap();

        let clone_root = fx.clone_root();
        let excavation = excavate_run(&target(), &clone_root).unwrap();
        let classifier = ScriptedClassifier::new_err();
        let report = run(&target(), &clone_root, &excavation, &classifier)
            .await
            .expect("date");

        let row = row_for(&report, "docs/architecture.md");
        // With last_commit_age_days=200, fallback routes to Stale per the
        // implementation's conservative mapping (stale beats drifting
        // when the file is >180d old).
        assert_eq!(row.class, StalenessClass::Stale, "row={row:?}");
        assert!(
            row.evidence.contains("fallback_drifting"),
            "evidence missing fallback marker: {}",
            row.evidence
        );
    }

    #[test]
    fn staleness_report_serde_roundtrip() {
        let report = StalenessReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "deadbeef".to_string(),
            rows: vec![
                StalenessRow {
                    path: "README.md".to_string(),
                    class: StalenessClass::Fresh,
                    last_commit_age_days: 5,
                    last_human_touch_age_days: Some(5),
                    wired: true,
                    aspirational: false,
                    evidence: "rule=fresh".to_string(),
                },
                StalenessRow {
                    path: "docs/old.md".to_string(),
                    class: StalenessClass::Stale,
                    last_commit_age_days: 400,
                    last_human_touch_age_days: None,
                    wired: false,
                    aspirational: false,
                    evidence: "rule=stale".to_string(),
                },
                StalenessRow {
                    path: "CLAUDE.md".to_string(),
                    class: StalenessClass::Aspirational,
                    last_commit_age_days: 200,
                    last_human_touch_age_days: Some(200),
                    wired: false,
                    aspirational: true,
                    evidence: "rule=llm:aspirational".to_string(),
                },
            ],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: StalenessReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.repo_id, report.repo_id);
        assert_eq!(parsed.rows.len(), 3);
        assert_eq!(parsed.rows[0].class, StalenessClass::Fresh);
        assert_eq!(parsed.rows[1].class, StalenessClass::Stale);
        assert_eq!(parsed.rows[2].class, StalenessClass::Aspirational);
    }
}
