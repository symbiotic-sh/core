//! Stage 4a — Reconcile (produces patches for drifting artifacts).
//!
//! Only runs when Diagnose = `Salvageable`. For each `Drifting` row in
//! the Staleness report, invokes a `deep`-tier reconciler with the
//! current file content + archeological evidence and collects the
//! resulting unified-diff patch into a `Finding` record.
//!
//! Scope fence: findings whose `evidence_path` is not in the target's
//! `allowed_paths` are dropped at the output filter. An empty allow-list
//! means "everything allowed" (full-run mode).
//!
//! See `docs/design/source-archeology.md` §Stage 4a — Reconcile.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use symbiotic_core::protocol::{ChatMessage, LlmClient};

use super::archeology_types::{
    ArcheologyTarget, Finding, FindingAction, FindingSeverity, FindingSourceStage, GoalAlignment,
    PathPattern, StalenessClass,
};
use super::date::{StalenessReport, StalenessRow};
use super::excavate::{ExcavationReport, Observation};

/// Hard cap on the content excerpt sent to the reconciler's LLM. Keeps
/// token cost bounded even for pathologically large markdown files.
pub const MAX_CONTENT_CHARS: usize = 8_000;

// ── Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ReconcileInput<'a> {
    pub repo_id: &'a str,
    pub path: &'a str,
    pub content: &'a str,
    pub staleness_class: StalenessClass,
    pub evidence: &'a [Observation],
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ReconciledPatch {
    /// Unified diff against `content`. Empty string means "no patch".
    pub diff: String,
    pub severity: FindingSeverity,
    pub rationale: String,
}

// ── LLM seam ───────────────────────────────────────────────────────────

/// `deep`-tier reconciler. Given one drifting artifact, returns a
/// unified-diff patch (or `None` if no patch warranted).
#[async_trait]
pub trait Reconciler: Send + Sync {
    async fn reconcile_artifact(
        &self,
        input: &ReconcileInput<'_>,
    ) -> Result<Option<ReconciledPatch>>;
}

/// Production impl backed by an `LlmClient` (`deep` tier). Retries once
/// on transient failure before returning `Err`.
pub struct LlmReconciler<'a> {
    client: &'a dyn LlmClient,
}

impl<'a> LlmReconciler<'a> {
    pub fn new(client: &'a dyn LlmClient) -> Self {
        Self { client }
    }

    fn build_messages(input: &ReconcileInput<'_>) -> Vec<ChatMessage> {
        let system = "You are the Source Archeology reconciler. A documentation \
                      file is drifting — its current content partially mismatches \
                      the code. Produce a unified-diff patch that corrects the drift. \
                      Reply with exactly one JSON object: {\"diff\": \"<unified diff \
                      or empty>\", \"severity\": \"critical\" | \"high\" | \"medium\" \
                      | \"low\", \"rationale\": \"<1-3 sentences>\"}. If no patch is \
                      warranted, reply with {\"diff\": \"\", \"severity\": \"low\", \
                      \"rationale\": \"<why>\"}. No prose outside the JSON.";

        let evidence_joined = input
            .evidence
            .iter()
            .map(|o| format!("- [{:?}] {}: {}", o.category, o.path, o.evidence))
            .collect::<Vec<_>>()
            .join("\n");

        let content_excerpt: String = input.content.chars().take(MAX_CONTENT_CHARS).collect();

        let user = format!(
            "PATH: {}\nSTALENESS: {:?}\nEVIDENCE:\n{}\n\nCONTENT:\n{}",
            input.path, input.staleness_class, evidence_joined, content_excerpt
        );
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
impl<'a> Reconciler for LlmReconciler<'a> {
    async fn reconcile_artifact(
        &self,
        input: &ReconcileInput<'_>,
    ) -> Result<Option<ReconciledPatch>> {
        let messages = Self::build_messages(input);
        let resp = match self.client.chat(&messages, true).await {
            Ok(r) => r,
            Err(first_err) => match self.client.chat(&messages, true).await {
                Ok(r) => r,
                Err(second_err) => {
                    return Err(anyhow::anyhow!(
                        "LLM reconciler failed twice: {first_err}; then: {second_err}"
                    ));
                }
            },
        };
        let patch: ReconciledPatch = serde_json::from_str(resp.trim()).map_err(|e| {
            anyhow::anyhow!("LLM response is not a ReconciledPatch: {e}; raw={resp}")
        })?;
        if patch.diff.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(patch))
        }
    }
}

// ── Scope fence ────────────────────────────────────────────────────────

/// Check whether `path` is in scope per `allowed_paths`.
///
/// Empty list → allow everything (full-run mode). Non-empty → at least
/// one pattern must match. Patterns use `.gitignore`-flavored glob
/// grammar (leading `**/`, `*` for a single segment, trailing `/` for
/// directory prefix match).
pub fn path_matches_allowed(path: &str, allowed: &[PathPattern]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    allowed.iter().any(|pat| glob_match(&pat.0, path))
}

fn glob_match(pattern: &str, path: &str) -> bool {
    // Minimal matcher good enough for the doc-reconciliation use case:
    // - `**` matches any number of segments (including zero).
    // - `*` matches a single segment (no `/`).
    // - trailing `/` marks directory prefix — pattern `foo/` matches
    //   `foo/x` and `foo/a/b` but not `foo`.
    // - exact segments match literally.
    //
    // Not a full .gitignore implementation; intentionally minimal so
    // behavior is predictable. Patterns encountered in practice are
    // `docs/**`, `*.md`, `CLAUDE.md`, etc.
    if let Some(prefix) = pattern.strip_suffix('/') {
        return path.starts_with(prefix) && path.len() > prefix.len();
    }
    segment_match(
        &pattern.split('/').collect::<Vec<_>>(),
        &path.split('/').collect::<Vec<_>>(),
    )
}

fn segment_match(pat: &[&str], path: &[&str]) -> bool {
    let mut i = 0;
    let mut j = 0;
    while i < pat.len() && j < path.len() {
        match pat[i] {
            "**" => {
                // Try matching ** against 0..=remaining path segments.
                if i + 1 == pat.len() {
                    return true;
                }
                for skip in 0..=(path.len() - j) {
                    if segment_match(&pat[i + 1..], &path[j + skip..]) {
                        return true;
                    }
                }
                return false;
            }
            seg => {
                if !single_segment_match(seg, path[j]) {
                    return false;
                }
                i += 1;
                j += 1;
            }
        }
    }
    // Pattern exhausted but path has more segments → no match unless
    // the trailing pattern segment was `**`.
    if i < pat.len() && pat[i] == "**" {
        return true;
    }
    i == pat.len() && j == path.len()
}

fn single_segment_match(pat: &str, seg: &str) -> bool {
    // Support `*` wildcard within a single segment (e.g. `*.md`).
    if !pat.contains('*') {
        return pat == seg;
    }
    let mut pat_chars = pat.chars().peekable();
    let mut seg_chars = seg.chars().peekable();
    while let Some(p) = pat_chars.next() {
        if p == '*' {
            let rest: String = pat_chars.clone().collect();
            if rest.is_empty() {
                return true;
            }
            // Greedy: find any suffix of seg that matches `rest`.
            let seg_rest: String = seg_chars.clone().collect();
            for start in 0..=seg_rest.len() {
                if single_segment_match(&rest, &seg_rest[start..]) {
                    return true;
                }
            }
            return false;
        }
        match seg_chars.next() {
            Some(s) if s == p => {}
            _ => return false,
        }
    }
    seg_chars.next().is_none()
}

// ── Entry point ────────────────────────────────────────────────────────

pub async fn run(
    target: &ArcheologyTarget,
    clone_root: &Path,
    excavation: &ExcavationReport,
    staleness: &StalenessReport,
    reconciler: &dyn Reconciler,
) -> Result<Vec<Finding>> {
    let drifting_rows: Vec<&StalenessRow> = staleness
        .rows
        .iter()
        .filter(|r| r.class == StalenessClass::Drifting)
        .collect();

    let mut findings: Vec<Finding> = Vec::new();
    for row in drifting_rows {
        if !path_matches_allowed(&row.path, &target.allowed_paths) {
            continue;
        }
        let content = match std::fs::read_to_string(clone_root.join(&row.path)) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let evidence: Vec<Observation> = excavation
            .observations
            .iter()
            .filter(|o| o.path == row.path)
            .cloned()
            .collect();

        let input = ReconcileInput {
            repo_id: &excavation.repo_id,
            path: &row.path,
            content: &content,
            staleness_class: row.class,
            evidence: &evidence,
        };

        let patch = match reconciler.reconcile_artifact(&input).await {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(_) => continue,
        };

        findings.push(Finding {
            id: format!("F-reconcile-{}", uuid::Uuid::new_v4()),
            source_stage: FindingSourceStage::Reconcile,
            severity: patch.severity,
            category: "drift".to_string(),
            evidence_path: row.path.clone(),
            description: patch.rationale,
            proposed_action: FindingAction::Patch { diff: patch.diff },
            goal_alignment: GoalAlignment::InScope,
        });
    }

    Ok(findings)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::ArcheologyMode;
    use crate::source_archeology::date::{StalenessReport, StalenessRow};
    use crate::source_archeology::excavate::{ExcavationReport, ObservationCategory};
    use crate::source_archeology::fixtures::ScriptedReconciler;

    fn target_with(paths: Vec<&str>) -> ArcheologyTarget {
        ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "main".to_string(),
            goal_id: "onboard".to_string(),
            allowed_paths: paths
                .into_iter()
                .map(|p| PathPattern(p.to_string()))
                .collect(),
            mode: ArcheologyMode::default(),
        }
    }

    fn staleness_with(rows: Vec<(&str, StalenessClass)>) -> StalenessReport {
        StalenessReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "abcd".to_string(),
            rows: rows
                .into_iter()
                .map(|(p, c)| StalenessRow {
                    path: p.to_string(),
                    class: c,
                    last_commit_age_days: 100,
                    last_human_touch_age_days: Some(100),
                    wired: true,
                    aspirational: false,
                    evidence: "fixture".to_string(),
                })
                .collect(),
        }
    }

    fn excavation_for(paths: &[&str]) -> ExcavationReport {
        ExcavationReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "abcd".to_string(),
            observations: paths
                .iter()
                .map(|p| Observation {
                    category: ObservationCategory::SignalFile,
                    path: p.to_string(),
                    evidence: "size_bytes=10".to_string(),
                })
                .collect(),
        }
    }

    fn tempdir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let full = dir.path().join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
        }
        dir
    }

    fn sample_patch() -> ReconciledPatch {
        ReconciledPatch {
            diff: "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            severity: FindingSeverity::Medium,
            rationale: "sample".to_string(),
        }
    }

    #[tokio::test]
    async fn reconcile_returns_patches_for_drifting_only() {
        let dir = tempdir_with(&[
            ("docs/a.md", "a"),
            ("docs/b.md", "b"),
            ("docs/c.md", "c"),
            ("docs/d.md", "d"),
        ]);
        let exc = excavation_for(&["docs/a.md", "docs/b.md", "docs/c.md", "docs/d.md"]);
        let sta = staleness_with(vec![
            ("docs/a.md", StalenessClass::Fresh),
            ("docs/b.md", StalenessClass::Drifting),
            ("docs/c.md", StalenessClass::Stale),
            ("docs/d.md", StalenessClass::Aspirational),
        ]);
        let target = target_with(vec![]);
        let reconciler = ScriptedReconciler::new_ok(Some(sample_patch()));
        let findings = run(&target, dir.path(), &exc, &sta, &reconciler)
            .await
            .unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence_path, "docs/b.md");
        assert_eq!(findings[0].source_stage, FindingSourceStage::Reconcile);
    }

    #[tokio::test]
    async fn reconcile_respects_allowed_paths_exact_match() {
        let dir = tempdir_with(&[("docs/a.md", "a"), ("docs/b.md", "b")]);
        let exc = excavation_for(&["docs/a.md", "docs/b.md"]);
        let sta = staleness_with(vec![
            ("docs/a.md", StalenessClass::Drifting),
            ("docs/b.md", StalenessClass::Drifting),
        ]);
        let target = target_with(vec!["docs/a.md"]);
        let reconciler = ScriptedReconciler::new_ok(Some(sample_patch()));
        let findings = run(&target, dir.path(), &exc, &sta, &reconciler)
            .await
            .unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence_path, "docs/a.md");
    }

    #[tokio::test]
    async fn reconcile_respects_allowed_paths_glob() {
        let dir = tempdir_with(&[
            ("docs/a.md", "a"),
            ("docs/sub/b.md", "b"),
            ("CLAUDE.md", "c"),
        ]);
        let exc = excavation_for(&["docs/a.md", "docs/sub/b.md", "CLAUDE.md"]);
        let sta = staleness_with(vec![
            ("docs/a.md", StalenessClass::Drifting),
            ("docs/sub/b.md", StalenessClass::Drifting),
            ("CLAUDE.md", StalenessClass::Drifting),
        ]);
        let target = target_with(vec!["docs/**"]);
        let reconciler = ScriptedReconciler::new_ok(Some(sample_patch()));
        let findings = run(&target, dir.path(), &exc, &sta, &reconciler)
            .await
            .unwrap();
        let paths: Vec<&str> = findings.iter().map(|f| f.evidence_path.as_str()).collect();
        assert!(paths.contains(&"docs/a.md"));
        assert!(paths.contains(&"docs/sub/b.md"));
        assert!(!paths.contains(&"CLAUDE.md"));
    }

    #[tokio::test]
    async fn reconcile_empty_allowed_paths_means_all() {
        let dir = tempdir_with(&[("docs/a.md", "a"), ("CLAUDE.md", "c")]);
        let exc = excavation_for(&["docs/a.md", "CLAUDE.md"]);
        let sta = staleness_with(vec![
            ("docs/a.md", StalenessClass::Drifting),
            ("CLAUDE.md", StalenessClass::Drifting),
        ]);
        let target = target_with(vec![]);
        let reconciler = ScriptedReconciler::new_ok(Some(sample_patch()));
        let findings = run(&target, dir.path(), &exc, &sta, &reconciler)
            .await
            .unwrap();
        assert_eq!(findings.len(), 2);
    }

    #[tokio::test]
    async fn reconcile_skips_on_reconciler_none() {
        let dir = tempdir_with(&[("docs/a.md", "a")]);
        let exc = excavation_for(&["docs/a.md"]);
        let sta = staleness_with(vec![("docs/a.md", StalenessClass::Drifting)]);
        let target = target_with(vec![]);
        let reconciler = ScriptedReconciler::new_ok(None);
        let findings = run(&target, dir.path(), &exc, &sta, &reconciler)
            .await
            .unwrap();
        assert_eq!(findings.len(), 0);
    }

    #[tokio::test]
    async fn reconcile_skips_on_reconciler_err() {
        let dir = tempdir_with(&[("docs/a.md", "a")]);
        let exc = excavation_for(&["docs/a.md"]);
        let sta = staleness_with(vec![("docs/a.md", StalenessClass::Drifting)]);
        let target = target_with(vec![]);
        let reconciler = ScriptedReconciler::new_err();
        let findings = run(&target, dir.path(), &exc, &sta, &reconciler)
            .await
            .unwrap();
        assert_eq!(findings.len(), 0);
    }

    #[tokio::test]
    async fn reconcile_finding_carries_patch_and_severity() {
        let dir = tempdir_with(&[("docs/a.md", "a")]);
        let exc = excavation_for(&["docs/a.md"]);
        let sta = staleness_with(vec![("docs/a.md", StalenessClass::Drifting)]);
        let target = target_with(vec![]);
        let patch = ReconciledPatch {
            diff: "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            severity: FindingSeverity::High,
            rationale: "replace old ref".to_string(),
        };
        let reconciler = ScriptedReconciler::new_ok(Some(patch.clone()));
        let findings = run(&target, dir.path(), &exc, &sta, &reconciler)
            .await
            .unwrap();
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, FindingSeverity::High);
        assert_eq!(f.description, "replace old ref");
        assert_eq!(f.source_stage, FindingSourceStage::Reconcile);
        assert_eq!(f.evidence_path, "docs/a.md");
        match &f.proposed_action {
            FindingAction::Patch { diff } => assert_eq!(diff, &patch.diff),
            other => panic!("expected Patch, got {other:?}"),
        }
    }

    #[test]
    fn path_matches_allowed_glob_semantics() {
        let pat = |s: &str| PathPattern(s.to_string());

        // Empty list matches everything.
        assert!(path_matches_allowed("anything", &[]));
        assert!(path_matches_allowed("docs/a.md", &[]));

        // Exact match.
        assert!(path_matches_allowed("CLAUDE.md", &[pat("CLAUDE.md")]));
        assert!(!path_matches_allowed("AGENTS.md", &[pat("CLAUDE.md")]));

        // `docs/**` matches everything under docs/.
        let docs_star_star = [pat("docs/**")];
        assert!(path_matches_allowed("docs/a.md", &docs_star_star));
        assert!(path_matches_allowed("docs/sub/b.md", &docs_star_star));
        assert!(!path_matches_allowed("src/x.rs", &docs_star_star));
        assert!(!path_matches_allowed("CLAUDE.md", &docs_star_star));

        // `*.md` at root.
        let star_md = [pat("*.md")];
        assert!(path_matches_allowed("CLAUDE.md", &star_md));
        assert!(path_matches_allowed("README.md", &star_md));
        assert!(!path_matches_allowed("docs/x.md", &star_md));
        assert!(!path_matches_allowed("src/x.rs", &star_md));

        // Trailing-slash directory match.
        let docs_slash = [pat("docs/")];
        assert!(path_matches_allowed("docs/a.md", &docs_slash));
        assert!(path_matches_allowed("docs/sub/b.md", &docs_slash));
        assert!(!path_matches_allowed("docs", &docs_slash));
        assert!(!path_matches_allowed("other", &docs_slash));

        // Multiple patterns: any match passes.
        let multi = [pat("CLAUDE.md"), pat("docs/**")];
        assert!(path_matches_allowed("CLAUDE.md", &multi));
        assert!(path_matches_allowed("docs/a.md", &multi));
        assert!(!path_matches_allowed("src/x.rs", &multi));
    }
}
