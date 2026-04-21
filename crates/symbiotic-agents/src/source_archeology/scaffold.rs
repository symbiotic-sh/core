//! Stage 4b — Scaffold (produces fresh docs package for `{slug}-docs`).
//!
//! Runs when Diagnose = `StaleBeyondSalvage` or `NoDocs`. Generates new
//! documentation artifacts from archeological evidence, routed for
//! delivery into a separate `{slug}-docs` repo via T127's Auto-Provision
//! capability (when that lands). Also emits *source-side* findings —
//! issues uncovered against the source repo during scaffold analysis
//! (e.g. "README references a deleted script") — which flow into Triage
//! alongside the scaffold output.
//!
//! See `docs/design/source-archeology.md` §Stage 4b — Scaffold.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use symbiotic_core::protocol::{ChatMessage, LlmClient};

use super::archeology_types::{
    ArcheologyTarget, Diagnosis, Finding, FindingAction, FindingSeverity, FindingSourceStage,
    GoalAlignment,
};
use super::date::StalenessReport;
use super::excavate::ExcavationReport;
use super::reconcile::path_matches_allowed;

// ── Types ──────────────────────────────────────────────────────────────

/// Context about the project the scaffolder is generating docs for.
/// Caller-supplied: T127 Phase 2 resolves the project registry; keeps
/// `symbiotic-agents` free of control-plane coupling.
#[derive(Debug, Clone)]
pub struct ProjectContext {
    /// Human-readable name (e.g. "Flux").
    pub name: String,
    /// Kebab-case slug (e.g. "flux" — feeds `{slug}-docs`).
    pub slug: String,
    /// One-sentence description, or empty string if unknown.
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct ScaffoldInput<'a> {
    pub target: &'a ArcheologyTarget,
    pub excavation: &'a ExcavationReport,
    pub staleness: &'a StalenessReport,
    pub diagnosis: &'a Diagnosis,
    pub project: &'a ProjectContext,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ScaffoldOutput {
    /// New files destined for `{slug}-docs`. Paths relative to that
    /// repo's root.
    pub scaffold_files: Vec<ScaffoldFile>,
    /// Patches against the source repo — broken refs, stale commands,
    /// etc. Capped per-run via `ScaffoldConfig.max_source_side_findings`.
    pub source_side_patches: Vec<SourceSidePatch>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ScaffoldFile {
    pub path: String,
    pub content: String,
    pub severity: FindingSeverity,
    pub rationale: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SourceSidePatch {
    pub path: String,
    pub diff: String,
    pub severity: FindingSeverity,
    pub rationale: String,
}

// ── Config (per docs/design/agent-tunables.md) ─────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScaffoldConfig {
    /// Max source-side findings emitted per run. Default 20 keeps Triage
    /// bounded under pathological scaffold outputs. Sourceable from
    /// `RepoManifest.archeology_policy.scaffold` when that field lands.
    pub max_source_side_findings: usize,
}

impl Default for ScaffoldConfig {
    fn default() -> Self {
        Self {
            max_source_side_findings: 20,
        }
    }
}

// ── LLM seam ───────────────────────────────────────────────────────────

/// `deep`-tier scaffolder. Produces (a) scaffold files for `{slug}-docs`
/// and (b) source-side patches against the source repo.
#[async_trait]
pub trait Scaffolder: Send + Sync {
    async fn scaffold(&self, input: &ScaffoldInput<'_>) -> Result<ScaffoldOutput>;
}

pub struct LlmScaffolder<'a> {
    client: &'a dyn LlmClient,
}

impl<'a> LlmScaffolder<'a> {
    pub fn new(client: &'a dyn LlmClient) -> Self {
        Self { client }
    }

    fn build_messages(input: &ScaffoldInput<'_>) -> Result<Vec<ChatMessage>> {
        let system = "You are the Source Archeology scaffolder. The repository's \
                      docs are missing or too stale to salvage — produce a fresh \
                      documentation package from the archeological evidence. \
                      Reply with exactly one JSON object: \
                      {\"scaffold_files\": [{\"path\": \"<rel>\", \"content\": \
                      \"<full body>\", \"severity\": \"critical|high|medium|low\", \
                      \"rationale\": \"<why this file>\"}, ...], \
                      \"source_side_patches\": [{\"path\": \"<source-rel>\", \
                      \"diff\": \"<unified>\", \"severity\": \"critical|high|medium|low\", \
                      \"rationale\": \"<what's broken>\"}, ...]}. \
                      Target scaffold layout: README.md, CLAUDE.md (if agentic signal), \
                      AGENTS.md (if role map detectable), docs/architecture.md, \
                      docs/build.md, docs/test.md, docs/deploy.md as warranted by the \
                      detected build/CI signals. No prose outside the JSON.";

        let user = serde_json::to_string_pretty(&serde_json::json!({
            "project": {
                "name": input.project.name,
                "slug": input.project.slug,
                "description": input.project.description,
            },
            "diagnosis": {
                "verdict": input.diagnosis.verdict,
                "confidence": input.diagnosis.confidence,
                "reasoning": input.diagnosis.reasoning,
            },
            "excavation": {
                "repo_id": input.excavation.repo_id,
                "observed_head": input.excavation.observed_head,
                "observation_count": input.excavation.observations.len(),
            },
            "staleness": {
                "row_count": input.staleness.rows.len(),
            },
        }))?;

        Ok(vec![
            ChatMessage {
                role: "system".to_string(),
                content: system.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user,
            },
        ])
    }
}

#[async_trait]
impl<'a> Scaffolder for LlmScaffolder<'a> {
    async fn scaffold(&self, input: &ScaffoldInput<'_>) -> Result<ScaffoldOutput> {
        let messages = Self::build_messages(input)?;
        let resp = match self.client.chat(&messages, true).await {
            Ok(r) => r,
            Err(first_err) => match self.client.chat(&messages, true).await {
                Ok(r) => r,
                Err(second_err) => {
                    return Err(anyhow::anyhow!(
                        "LLM scaffolder failed twice: {first_err}; then: {second_err}"
                    ));
                }
            },
        };
        serde_json::from_str(resp.trim())
            .map_err(|e| anyhow::anyhow!("LLM response is not a ScaffoldOutput: {e}; raw={resp}"))
    }
}

// ── Entry point ────────────────────────────────────────────────────────

pub async fn run(
    input: &ScaffoldInput<'_>,
    scaffolder: &dyn Scaffolder,
    config: ScaffoldConfig,
) -> Result<Vec<Finding>> {
    let output = match scaffolder.scaffold(input).await {
        Ok(o) => o,
        Err(_) => return Ok(Vec::new()),
    };

    let mut findings: Vec<Finding> = Vec::new();

    for file in output.scaffold_files {
        findings.push(Finding {
            id: format!("F-scaffold-{}", uuid::Uuid::new_v4()),
            source_stage: FindingSourceStage::Scaffold,
            severity: file.severity,
            category: "scaffold_new_file".to_string(),
            evidence_path: file.path.clone(),
            description: file.rationale,
            proposed_action: FindingAction::NewFile {
                path: file.path,
                content: file.content,
            },
            goal_alignment: GoalAlignment::InScope,
        });
    }

    let cap = config.max_source_side_findings;
    for patch in output.source_side_patches.into_iter().take(cap) {
        if !path_matches_allowed(&patch.path, &input.target.allowed_paths) {
            continue;
        }
        findings.push(Finding {
            id: format!("F-scaffold-src-{}", uuid::Uuid::new_v4()),
            source_stage: FindingSourceStage::ScaffoldSourceSide,
            severity: patch.severity,
            category: "scaffold_source_fix".to_string(),
            evidence_path: patch.path,
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
    use crate::source_archeology::archeology_types::{
        ArcheologyMode, DiagnosisVerdict, PathPattern,
    };
    use crate::source_archeology::fixtures::ScriptedScaffolder;

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

    fn project() -> ProjectContext {
        ProjectContext {
            name: "Flux".to_string(),
            slug: "flux".to_string(),
            description: "A test fixture.".to_string(),
        }
    }

    fn minimal_reports() -> (ExcavationReport, StalenessReport) {
        let exc = ExcavationReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "abcd".to_string(),
            observations: vec![],
        };
        let sta = StalenessReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "abcd".to_string(),
            rows: vec![],
        };
        (exc, sta)
    }

    fn diagnosis() -> Diagnosis {
        Diagnosis {
            verdict: DiagnosisVerdict::NoDocs,
            confidence: 0.95,
            reasoning: "no signal files".to_string(),
        }
    }

    fn sample_scaffold_files(n: usize) -> Vec<ScaffoldFile> {
        (0..n)
            .map(|i| ScaffoldFile {
                path: format!("docs/f{i}.md"),
                content: format!("# File {i}\n"),
                severity: FindingSeverity::Medium,
                rationale: format!("seed file {i}"),
            })
            .collect()
    }

    fn sample_source_patches(n: usize, path_prefix: &str) -> Vec<SourceSidePatch> {
        (0..n)
            .map(|i| SourceSidePatch {
                path: format!("{path_prefix}p{i}.md"),
                diff: format!("--- a\n+++ b\n@@\n-x{i}\n+y{i}\n"),
                severity: FindingSeverity::Low,
                rationale: format!("patch {i}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn scaffold_emits_new_file_findings() {
        let target = target_with(vec![]);
        let (exc, sta) = minimal_reports();
        let diag = diagnosis();
        let proj = project();
        let input = ScaffoldInput {
            target: &target,
            excavation: &exc,
            staleness: &sta,
            diagnosis: &diag,
            project: &proj,
        };
        let scaffolder = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: sample_scaffold_files(3),
            source_side_patches: vec![],
        });
        let findings = run(&input, &scaffolder, ScaffoldConfig::default())
            .await
            .unwrap();
        assert_eq!(findings.len(), 3);
        for f in &findings {
            assert_eq!(f.source_stage, FindingSourceStage::Scaffold);
            assert!(matches!(f.proposed_action, FindingAction::NewFile { .. }));
            assert_eq!(f.category, "scaffold_new_file");
        }
    }

    #[tokio::test]
    async fn scaffold_emits_source_side_patch_findings() {
        let target = target_with(vec![]);
        let (exc, sta) = minimal_reports();
        let diag = diagnosis();
        let proj = project();
        let input = ScaffoldInput {
            target: &target,
            excavation: &exc,
            staleness: &sta,
            diagnosis: &diag,
            project: &proj,
        };
        let scaffolder = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: vec![],
            source_side_patches: sample_source_patches(2, "docs/"),
        });
        let findings = run(&input, &scaffolder, ScaffoldConfig::default())
            .await
            .unwrap();
        assert_eq!(findings.len(), 2);
        for f in &findings {
            assert_eq!(f.source_stage, FindingSourceStage::ScaffoldSourceSide);
            assert!(matches!(f.proposed_action, FindingAction::Patch { .. }));
            assert_eq!(f.category, "scaffold_source_fix");
        }
    }

    #[tokio::test]
    async fn scaffold_source_side_cap_enforced() {
        let target = target_with(vec![]);
        let (exc, sta) = minimal_reports();
        let diag = diagnosis();
        let proj = project();
        let input = ScaffoldInput {
            target: &target,
            excavation: &exc,
            staleness: &sta,
            diagnosis: &diag,
            project: &proj,
        };
        let scaffolder = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: vec![],
            source_side_patches: sample_source_patches(50, "docs/"),
        });
        let findings = run(&input, &scaffolder, ScaffoldConfig::default())
            .await
            .unwrap();
        assert_eq!(findings.len(), 20);
    }

    #[tokio::test]
    async fn scaffold_source_side_respects_allowed_paths() {
        let target = target_with(vec!["docs/**"]);
        let (exc, sta) = minimal_reports();
        let diag = diagnosis();
        let proj = project();
        let input = ScaffoldInput {
            target: &target,
            excavation: &exc,
            staleness: &sta,
            diagnosis: &diag,
            project: &proj,
        };
        let patches = vec![
            SourceSidePatch {
                path: "docs/x.md".to_string(),
                diff: "d".to_string(),
                severity: FindingSeverity::Low,
                rationale: "r".to_string(),
            },
            SourceSidePatch {
                path: "src/x.rs".to_string(),
                diff: "d".to_string(),
                severity: FindingSeverity::Low,
                rationale: "r".to_string(),
            },
        ];
        let scaffolder = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: vec![],
            source_side_patches: patches,
        });
        let findings = run(&input, &scaffolder, ScaffoldConfig::default())
            .await
            .unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence_path, "docs/x.md");
    }

    #[tokio::test]
    async fn scaffold_llm_error_yields_empty() {
        let target = target_with(vec![]);
        let (exc, sta) = minimal_reports();
        let diag = diagnosis();
        let proj = project();
        let input = ScaffoldInput {
            target: &target,
            excavation: &exc,
            staleness: &sta,
            diagnosis: &diag,
            project: &proj,
        };
        let scaffolder = ScriptedScaffolder::new_err();
        let findings = run(&input, &scaffolder, ScaffoldConfig::default())
            .await
            .unwrap();
        assert_eq!(findings.len(), 0);
    }

    #[tokio::test]
    async fn scaffold_empty_output_yields_empty() {
        let target = target_with(vec![]);
        let (exc, sta) = minimal_reports();
        let diag = diagnosis();
        let proj = project();
        let input = ScaffoldInput {
            target: &target,
            excavation: &exc,
            staleness: &sta,
            diagnosis: &diag,
            project: &proj,
        };
        let scaffolder = ScriptedScaffolder::new_ok(ScaffoldOutput {
            scaffold_files: vec![],
            source_side_patches: vec![],
        });
        let findings = run(&input, &scaffolder, ScaffoldConfig::default())
            .await
            .unwrap();
        assert_eq!(findings.len(), 0);
    }

    #[tokio::test]
    async fn scaffold_finding_preserves_severity_and_rationale() {
        let target = target_with(vec![]);
        let (exc, sta) = minimal_reports();
        let diag = diagnosis();
        let proj = project();
        let input = ScaffoldInput {
            target: &target,
            excavation: &exc,
            staleness: &sta,
            diagnosis: &diag,
            project: &proj,
        };
        let output = ScaffoldOutput {
            scaffold_files: vec![ScaffoldFile {
                path: "README.md".to_string(),
                content: "# Flux\n".to_string(),
                severity: FindingSeverity::Critical,
                rationale: "root readme missing".to_string(),
            }],
            source_side_patches: vec![SourceSidePatch {
                path: "docs/old.md".to_string(),
                diff: "diff body".to_string(),
                severity: FindingSeverity::High,
                rationale: "broken ref".to_string(),
            }],
        };
        let scaffolder = ScriptedScaffolder::new_ok(output);
        let findings = run(&input, &scaffolder, ScaffoldConfig::default())
            .await
            .unwrap();
        assert_eq!(findings.len(), 2);
        let new_file = findings
            .iter()
            .find(|f| f.evidence_path == "README.md")
            .unwrap();
        assert_eq!(new_file.severity, FindingSeverity::Critical);
        assert_eq!(new_file.description, "root readme missing");
        let patch = findings
            .iter()
            .find(|f| f.evidence_path == "docs/old.md")
            .unwrap();
        assert_eq!(patch.severity, FindingSeverity::High);
        assert_eq!(patch.description, "broken ref");
    }

    #[test]
    fn scaffold_config_default_is_20() {
        assert_eq!(ScaffoldConfig::default().max_source_side_findings, 20);
    }
}
