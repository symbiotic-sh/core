//! Stage 4c — Handoff (compiles operator-facing report + questions).
//!
//! Runs when Diagnose = `NeedsOperatorInput`. Produces a
//! `HandoffReport` (human-readable markdown + structured operator
//! questions) so the operator can resolve the ambiguity the
//! diagnostician couldn't. No automated output — the operator answers,
//! the pipeline re-runs.
//!
//! See `docs/design/source-archeology.md` §Stage 4c — Handoff.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use symbiotic_core::protocol::{ChatMessage, LlmClient};

use super::archeology_types::{ArcheologyTarget, Diagnosis};
use super::date::StalenessReport;
use super::excavate::ExcavationReport;
use super::scaffold::ProjectContext;

// ── Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct HandoffReport {
    /// Markdown body for `archeology-report.md` / Matrix notification.
    pub markdown: String,
    /// Structured operator questions whose answers resolve the
    /// ambiguity. Each question is framed so the answer feeds back
    /// into a future Diagnose re-run.
    pub questions: Vec<OperatorQuestion>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct OperatorQuestion {
    /// Stable identifier — future runs thread answers via this id.
    pub id: String,
    pub text: String,
    pub reason: String,
    /// Empty → free-form answer. Non-empty → Matrix UI renders tappable
    /// chips.
    #[serde(default)]
    pub choices: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct HandoffInput<'a> {
    pub target: &'a ArcheologyTarget,
    pub excavation: &'a ExcavationReport,
    pub staleness: &'a StalenessReport,
    pub diagnosis: &'a Diagnosis,
    pub project: &'a ProjectContext,
    pub config: HandoffConfig,
}

// ── Config (per docs/design/agent-tunables.md) ─────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HandoffConfig {
    /// Max operator questions emitted per run. Default 5 — operators
    /// fatigue under long chains.
    pub max_questions: usize,
    /// Max markdown body length. Default 4000 — operators read these in
    /// Matrix, not as full documents.
    pub max_report_chars: usize,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            max_questions: 5,
            max_report_chars: 4000,
        }
    }
}

// ── LLM seam ───────────────────────────────────────────────────────────

/// `fast`-tier reporter. Compile-and-format; the hard decisions were
/// already made in Diagnose. Latency matters more than depth because
/// the operator is waiting synchronously.
#[async_trait]
pub trait Reporter: Send + Sync {
    async fn generate_report(&self, input: &HandoffInput<'_>) -> Result<HandoffReport>;
}

pub struct LlmReporter<'a> {
    client: &'a dyn LlmClient,
}

impl<'a> LlmReporter<'a> {
    pub fn new(client: &'a dyn LlmClient) -> Self {
        Self { client }
    }

    fn build_messages(input: &HandoffInput<'_>) -> Result<Vec<ChatMessage>> {
        let system = format!(
            "You are the Source Archeology reporter. The diagnostician could not \
             confidently pick a branch — ambiguity is too high or signals contradict. \
             Compile a short human-readable report and operator questions. Reply with \
             exactly one JSON object: {{\"markdown\": \"<report body, max {} chars>\", \
             \"questions\": [{{\"id\": \"<stable-id>\", \"text\": \"<question>\", \
             \"reason\": \"<why the pipeline can't answer this>\", \
             \"choices\": [\"<optional>\", ...]}}]}}. Keep markdown concise — operators \
             read these in Matrix. Frame each question so the answer feeds a future \
             Diagnose re-run. At most {} questions. No prose outside the JSON.",
            input.config.max_report_chars, input.config.max_questions
        );

        let user = serde_json::to_string_pretty(&serde_json::json!({
            "project": {
                "name": input.project.name,
                "slug": input.project.slug,
            },
            "diagnosis": {
                "verdict": input.diagnosis.verdict,
                "confidence": input.diagnosis.confidence,
                "reasoning": input.diagnosis.reasoning,
            },
            "staleness_row_count": input.staleness.rows.len(),
            "observation_count": input.excavation.observations.len(),
        }))?;

        Ok(vec![
            ChatMessage {
                role: "system".to_string(),
                content: system,
            },
            ChatMessage {
                role: "user".to_string(),
                content: user,
            },
        ])
    }
}

#[async_trait]
impl<'a> Reporter for LlmReporter<'a> {
    async fn generate_report(&self, input: &HandoffInput<'_>) -> Result<HandoffReport> {
        let messages = Self::build_messages(input)?;
        let resp = match self.client.chat(&messages, true).await {
            Ok(r) => r,
            Err(first_err) => match self.client.chat(&messages, true).await {
                Ok(r) => r,
                Err(second_err) => {
                    return Err(anyhow::anyhow!(
                        "LLM reporter failed twice: {first_err}; then: {second_err}"
                    ));
                }
            },
        };
        serde_json::from_str(resp.trim())
            .map_err(|e| anyhow::anyhow!("LLM response is not a HandoffReport: {e}; raw={resp}"))
    }
}

// ── Entry point ────────────────────────────────────────────────────────

pub async fn run(
    target: &ArcheologyTarget,
    excavation: &ExcavationReport,
    staleness: &StalenessReport,
    diagnosis: &Diagnosis,
    project: &ProjectContext,
    reporter: &dyn Reporter,
    config: HandoffConfig,
) -> Result<HandoffReport> {
    let input = HandoffInput {
        target,
        excavation,
        staleness,
        diagnosis,
        project,
        config,
    };

    match reporter.generate_report(&input).await {
        Ok(mut report) => {
            // Defensive caps — runner-side enforcement per the
            // agent-tunables rule (config is source of truth).
            report.questions.truncate(config.max_questions);
            if report.markdown.chars().count() > config.max_report_chars {
                let truncated: String = report
                    .markdown
                    .chars()
                    .take(config.max_report_chars)
                    .collect();
                report.markdown = format!(
                    "{truncated}\n\n… [report truncated at {} chars]",
                    config.max_report_chars
                );
            }
            Ok(report)
        }
        Err(err) => Ok(HandoffReport {
            markdown: format!(
                "# Archeology Handoff — {}\n\n## Status\n\nDiagnose verdict: {:?} @ {}. Automated reporter failed: {}.\n\n## Operator Action Required\n\nPlease inspect the repo and provide direction manually.",
                project.name, diagnosis.verdict, diagnosis.confidence, err
            ),
            questions: vec![OperatorQuestion {
                id: "handoff-fallback-01".to_string(),
                text: format!(
                    "Archeology couldn't auto-generate a report for {}. Should this repo be (a) salvaged via Reconcile, (b) re-scaffolded, or (c) skipped?",
                    project.slug
                ),
                reason: format!("Reporter LLM error: {err}"),
                choices: vec![
                    "salvage".to_string(),
                    "rescaffold".to_string(),
                    "skip".to_string(),
                ],
            }],
        }),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::{ArcheologyMode, DiagnosisVerdict};
    use crate::source_archeology::fixtures::ScriptedReporter;

    fn target() -> ArcheologyTarget {
        ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "main".to_string(),
            goal_id: "onboard".to_string(),
            allowed_paths: Vec::new(),
            mode: ArcheologyMode::default(),
        }
    }

    fn reports() -> (ExcavationReport, StalenessReport) {
        (
            ExcavationReport {
                repo_id: "repo:flux".to_string(),
                observed_head: "abcd".to_string(),
                observations: vec![],
            },
            StalenessReport {
                repo_id: "repo:flux".to_string(),
                observed_head: "abcd".to_string(),
                rows: vec![],
            },
        )
    }

    fn diagnosis() -> Diagnosis {
        Diagnosis {
            verdict: DiagnosisVerdict::NeedsOperatorInput,
            confidence: 0.40,
            reasoning: "contradictory signals".to_string(),
        }
    }

    fn project() -> ProjectContext {
        ProjectContext {
            name: "Flux".to_string(),
            slug: "flux".to_string(),
            description: "A test fixture.".to_string(),
        }
    }

    fn sample_questions(n: usize) -> Vec<OperatorQuestion> {
        (0..n)
            .map(|i| OperatorQuestion {
                id: format!("q-{i}"),
                text: format!("question {i}?"),
                reason: format!("reason {i}"),
                choices: vec!["yes".to_string(), "no".to_string()],
            })
            .collect()
    }

    #[tokio::test]
    async fn handoff_returns_report_and_questions_happy_path() {
        let tgt = target();
        let (exc, sta) = reports();
        let diag = diagnosis();
        let proj = project();
        let report = HandoffReport {
            markdown: "# Report\n\n500 char body here.".to_string(),
            questions: sample_questions(3),
        };
        let reporter = ScriptedReporter::new_ok(report.clone());
        let result = run(
            &tgt,
            &exc,
            &sta,
            &diag,
            &proj,
            &reporter,
            HandoffConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.questions.len(), 3);
        assert_eq!(result.markdown, report.markdown);
    }

    #[tokio::test]
    async fn handoff_caps_question_count() {
        let tgt = target();
        let (exc, sta) = reports();
        let diag = diagnosis();
        let proj = project();
        let report = HandoffReport {
            markdown: "short".to_string(),
            questions: sample_questions(10),
        };
        let reporter = ScriptedReporter::new_ok(report);
        let result = run(
            &tgt,
            &exc,
            &sta,
            &diag,
            &proj,
            &reporter,
            HandoffConfig::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.questions.len(), 5, "default cap is 5");
    }

    #[tokio::test]
    async fn handoff_caps_report_length() {
        let tgt = target();
        let (exc, sta) = reports();
        let diag = diagnosis();
        let proj = project();
        let huge_markdown: String = "x".repeat(8000);
        let report = HandoffReport {
            markdown: huge_markdown,
            questions: vec![],
        };
        let reporter = ScriptedReporter::new_ok(report);
        let result = run(
            &tgt,
            &exc,
            &sta,
            &diag,
            &proj,
            &reporter,
            HandoffConfig::default(),
        )
        .await
        .unwrap();
        assert!(
            result.markdown.contains("[report truncated at 4000 chars]"),
            "expected truncation marker, got: {}",
            result.markdown
        );
    }

    #[tokio::test]
    async fn handoff_llm_error_yields_fallback_report() {
        let tgt = target();
        let (exc, sta) = reports();
        let diag = diagnosis();
        let proj = project();
        let reporter = ScriptedReporter::new_err();
        let result = run(
            &tgt,
            &exc,
            &sta,
            &diag,
            &proj,
            &reporter,
            HandoffConfig::default(),
        )
        .await
        .unwrap();
        assert!(result.markdown.contains("Flux"), "project name preserved");
        assert!(
            result.markdown.contains("Automated reporter failed"),
            "fallback marker present"
        );
        assert_eq!(result.questions.len(), 1);
        assert_eq!(result.questions[0].id, "handoff-fallback-01");
        assert_eq!(result.questions[0].choices.len(), 3);
    }

    #[test]
    fn handoff_config_default_values() {
        let c = HandoffConfig::default();
        assert_eq!(c.max_questions, 5);
        assert_eq!(c.max_report_chars, 4000);
    }

    #[test]
    fn handoff_report_serde_roundtrip() {
        let report = HandoffReport {
            markdown: "# Hello".to_string(),
            questions: sample_questions(2),
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: HandoffReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }
}
