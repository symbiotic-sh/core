//! Stage 3 — Diagnose (repo-wide branch verdict).
//!
//! Aggregates Stage 1 + Stage 2 outputs into one of four branch
//! verdicts (`Salvageable | StaleBeyondSalvage | NoDocs |
//! NeedsOperatorInput`). A `deep`-tier LLM receives a bounded projection
//! of the reports (not the full reports — token cost would scale with
//! repo size); the runner applies a conservative confidence floor that
//! forces `NeedsOperatorInput` when the classifier is uncertain.
//!
//! See `docs/design/source-archeology.md` §Stage 3 — Diagnose.

use std::collections::BTreeMap;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use symbiotic_core::protocol::{ChatMessage, LlmClient};

use super::archeology_types::{Diagnosis, DiagnosisVerdict};
use super::date::StalenessReport;
use super::excavate::{ExcavationReport, ObservationCategory};

/// Maximum number of wire-mismatch evidence strings to include in the
/// LLM projection. Keeps token cost bounded.
pub const WIRE_MISMATCH_SAMPLE_CAP: usize = 20;

/// Maximum classified paths (with their `StalenessClass`) sent to the
/// classifier.
pub const CLASSIFIED_PATHS_CAP: usize = 50;

// ── Config ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DiagnoseConfig {
    /// Below this, the classifier's verdict is overridden to
    /// `NeedsOperatorInput`. Default: 0.80 — conservative-by-default per
    /// `docs/design/agent-tunables.md`: prefer escalating to the operator
    /// over picking the wrong branch. Sourceable from
    /// `RepoManifest.archeology_policy` when that field lands (future
    /// T126 chunk).
    pub confidence_floor: f32,
}

impl Default for DiagnoseConfig {
    fn default() -> Self {
        Self {
            confidence_floor: 0.80,
        }
    }
}

// ── Projection ─────────────────────────────────────────────────────────

/// Compact LLM input packaged from the Excavation + Staleness reports.
/// Caps on sample sizes keep token cost O(1) in repo size.
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosisProjection {
    pub repo_id: String,
    pub observed_head: String,
    /// Observation counts by category (serialized category name).
    pub observation_counts: BTreeMap<String, usize>,
    /// Staleness-row counts by class (serialized class name).
    pub staleness_counts: BTreeMap<String, usize>,
    /// Up to `WIRE_MISMATCH_SAMPLE_CAP` wire-mismatch evidence strings.
    pub wire_mismatch_samples: Vec<String>,
    /// Up to `CLASSIFIED_PATHS_CAP` (path, staleness-class) pairs.
    pub classified_paths: Vec<(String, String)>,
    /// `true` iff at least one of README*, LICENSE*, CLAUDE.md, AGENTS.md,
    /// CONTEXT.md appeared in the excavation signal-file list.
    pub has_any_top_signal: bool,
}

pub fn build_projection(
    excavation: &ExcavationReport,
    staleness: &StalenessReport,
) -> DiagnosisProjection {
    let mut observation_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut wire_mismatch_samples: Vec<String> = Vec::new();
    let mut has_any_top_signal = false;

    for obs in &excavation.observations {
        let key = serde_json::to_value(obs.category)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{:?}", obs.category));
        *observation_counts.entry(key).or_default() += 1;

        if obs.category == ObservationCategory::WireMismatch
            && wire_mismatch_samples.len() < WIRE_MISMATCH_SAMPLE_CAP
        {
            wire_mismatch_samples.push(format!("{}: {}", obs.path, obs.evidence));
        }

        if obs.category == ObservationCategory::SignalFile && is_top_signal(&obs.path) {
            has_any_top_signal = true;
        }
    }

    let mut staleness_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut classified_paths: Vec<(String, String)> = Vec::new();
    for row in &staleness.rows {
        let key = serde_json::to_value(row.class)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{:?}", row.class));
        *staleness_counts.entry(key.clone()).or_default() += 1;
        if classified_paths.len() < CLASSIFIED_PATHS_CAP {
            classified_paths.push((row.path.clone(), key));
        }
    }

    DiagnosisProjection {
        repo_id: excavation.repo_id.clone(),
        observed_head: excavation.observed_head.clone(),
        observation_counts,
        staleness_counts,
        wire_mismatch_samples,
        classified_paths,
        has_any_top_signal,
    }
}

fn is_top_signal(path: &str) -> bool {
    if path.contains('/') {
        return false;
    }
    let upper = path.to_ascii_uppercase();
    upper.starts_with("README")
        || upper.starts_with("LICENSE")
        || matches!(
            path,
            "CLAUDE.md" | "AGENTS.md" | "CONTEXT.md" | "NAMING-CANON.md"
        )
}

// ── LLM seam ───────────────────────────────────────────────────────────

/// Classifier's raw output, before the confidence floor is applied.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RawDiagnosis {
    pub verdict: DiagnosisVerdict,
    pub confidence: f32,
    pub reasoning: String,
}

/// `deep`-tier classifier for Stage 3. Takes a compact projection and
/// produces a branch verdict.
#[async_trait]
pub trait DiagnosisClassifier: Send + Sync {
    async fn classify(&self, projection: &DiagnosisProjection) -> Result<RawDiagnosis>;
}

/// Production impl backed by an `LlmClient` (`deep` tier).
/// Retries once on transient failure before returning `Err`.
pub struct LlmDiagnosisClassifier<'a> {
    client: &'a dyn LlmClient,
}

impl<'a> LlmDiagnosisClassifier<'a> {
    pub fn new(client: &'a dyn LlmClient) -> Self {
        Self { client }
    }

    fn build_messages(projection: &DiagnosisProjection) -> Result<Vec<ChatMessage>> {
        let system = "You are the Source Archeology diagnostician. You see a compact \
                      summary of a repository's observations and doc-staleness \
                      classifications. Decide which branch the downstream pipeline \
                      should take. Reply with exactly one JSON object: \
                      {\"verdict\": \"salvageable\" | \"stale_beyond_salvage\" | \
                      \"no_docs\" | \"needs_operator_input\", \
                      \"confidence\": <float 0.0-1.0>, \
                      \"reasoning\": \"<1-5 sentences>\"}. No prose outside the JSON. \
                      Verdict guidance: salvageable = docs exist and most are fresh or \
                      drifting (reconciliation improves them); stale_beyond_salvage = \
                      docs exist but most are stale or aspirational (rewriting costs \
                      less than patching); no_docs = little or no documentation present; \
                      needs_operator_input = ambiguity is high. Be conservative: \
                      prefer needs_operator_input over picking wrong.";
        let user = serde_json::to_string_pretty(projection)?;
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
impl<'a> DiagnosisClassifier for LlmDiagnosisClassifier<'a> {
    async fn classify(&self, projection: &DiagnosisProjection) -> Result<RawDiagnosis> {
        let messages = Self::build_messages(projection)?;
        let resp = match self.client.chat(&messages, true).await {
            Ok(r) => r,
            Err(first_err) => match self.client.chat(&messages, true).await {
                Ok(r) => r,
                Err(second_err) => {
                    return Err(anyhow::anyhow!(
                        "LLM diagnosis classifier failed twice: {first_err}; then: {second_err}"
                    ));
                }
            },
        };
        parse_diagnosis_response(&resp)
    }
}

fn parse_diagnosis_response(resp: &str) -> Result<RawDiagnosis> {
    serde_json::from_str(resp.trim())
        .map_err(|e| anyhow::anyhow!("LLM response is not a valid RawDiagnosis: {e}; raw={resp}"))
}

// ── Entry point ────────────────────────────────────────────────────────

pub async fn run(
    excavation: &ExcavationReport,
    staleness: &StalenessReport,
    classifier: &dyn DiagnosisClassifier,
    config: DiagnoseConfig,
) -> Result<Diagnosis> {
    let projection = build_projection(excavation, staleness);
    let raw = match classifier.classify(&projection).await {
        Ok(r) => r,
        Err(err) => {
            return Ok(Diagnosis {
                verdict: DiagnosisVerdict::NeedsOperatorInput,
                confidence: 0.0,
                reasoning: format!("llm_error: {err}"),
            });
        }
    };

    if raw.confidence < config.confidence_floor
        && raw.verdict != DiagnosisVerdict::NeedsOperatorInput
    {
        return Ok(Diagnosis {
            verdict: DiagnosisVerdict::NeedsOperatorInput,
            confidence: raw.confidence,
            reasoning: format!(
                "forced_by_confidence_floor: original_verdict={:?}; threshold={}; reasoning={}",
                raw.verdict, config.confidence_floor, raw.reasoning
            ),
        });
    }

    Ok(Diagnosis {
        verdict: raw.verdict,
        confidence: raw.confidence,
        reasoning: raw.reasoning,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::StalenessClass;
    use crate::source_archeology::date::{StalenessReport, StalenessRow};
    use crate::source_archeology::excavate::{ExcavationReport, Observation, ObservationCategory};
    use crate::source_archeology::fixtures::ScriptedDiagnosisClassifier;

    fn excavation_with(
        signal_files: &[&str],
        wire_mismatches: &[(&str, &str)],
    ) -> ExcavationReport {
        let mut observations: Vec<Observation> = signal_files
            .iter()
            .map(|p| Observation {
                category: ObservationCategory::SignalFile,
                path: p.to_string(),
                evidence: "size_bytes=100".to_string(),
            })
            .collect();
        for (p, evidence) in wire_mismatches {
            observations.push(Observation {
                category: ObservationCategory::WireMismatch,
                path: p.to_string(),
                evidence: format!("missing={evidence}"),
            });
        }
        ExcavationReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "abcd1234".to_string(),
            observations,
        }
    }

    fn staleness_with(rows: Vec<(&str, StalenessClass)>) -> StalenessReport {
        StalenessReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "abcd1234".to_string(),
            rows: rows
                .into_iter()
                .map(|(path, class)| StalenessRow {
                    path: path.to_string(),
                    class,
                    last_commit_age_days: 10,
                    last_human_touch_age_days: Some(10),
                    wired: true,
                    aspirational: matches!(class, StalenessClass::Aspirational),
                    evidence: "fixture".to_string(),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn diagnose_salvageable_happy_path() {
        let exc = excavation_with(&["README.md", "docs/architecture.md"], &[]);
        let sta = staleness_with(vec![
            ("README.md", StalenessClass::Fresh),
            ("docs/architecture.md", StalenessClass::Drifting),
        ]);
        let classifier = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::Salvageable,
            confidence: 0.85,
            reasoning: "mostly fresh/drifting".to_string(),
        });
        let diag = run(&exc, &sta, &classifier, DiagnoseConfig::default())
            .await
            .unwrap();
        assert_eq!(diag.verdict, DiagnosisVerdict::Salvageable);
        assert!((diag.confidence - 0.85).abs() < f32::EPSILON);
        assert_eq!(diag.reasoning, "mostly fresh/drifting");
    }

    #[tokio::test]
    async fn diagnose_stale_beyond_salvage_happy_path() {
        let exc = excavation_with(&["README.md"], &[("README.md", "./legacy-script.sh")]);
        let sta = staleness_with(vec![
            ("README.md", StalenessClass::Stale),
            ("docs/old.md", StalenessClass::Aspirational),
        ]);
        let classifier = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::StaleBeyondSalvage,
            confidence: 0.80,
            reasoning: "mostly stale + aspirational".to_string(),
        });
        let diag = run(&exc, &sta, &classifier, DiagnoseConfig::default())
            .await
            .unwrap();
        assert_eq!(diag.verdict, DiagnosisVerdict::StaleBeyondSalvage);
    }

    #[tokio::test]
    async fn diagnose_no_docs_happy_path() {
        let exc = excavation_with(&[], &[]);
        let sta = staleness_with(vec![]);
        let classifier = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::NoDocs,
            confidence: 0.90,
            reasoning: "no signal files found".to_string(),
        });
        let diag = run(&exc, &sta, &classifier, DiagnoseConfig::default())
            .await
            .unwrap();
        assert_eq!(diag.verdict, DiagnosisVerdict::NoDocs);
    }

    #[tokio::test]
    async fn diagnose_needs_operator_input_happy_path() {
        let exc = excavation_with(&["README.md"], &[]);
        let sta = staleness_with(vec![("README.md", StalenessClass::Drifting)]);
        // Classifier returns NeedsOperatorInput at sub-floor confidence
        // (0.40). Since the verdict is already NeedsOperatorInput, the
        // floor logic does NOT re-force — original verdict + confidence
        // pass through.
        let classifier = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::NeedsOperatorInput,
            confidence: 0.40,
            reasoning: "contradictory signals".to_string(),
        });
        let diag = run(&exc, &sta, &classifier, DiagnoseConfig::default())
            .await
            .unwrap();
        assert_eq!(diag.verdict, DiagnosisVerdict::NeedsOperatorInput);
        assert!((diag.confidence - 0.40).abs() < f32::EPSILON);
        assert_eq!(diag.reasoning, "contradictory signals");
    }

    #[tokio::test]
    async fn diagnose_confidence_floor_forces_needs_operator_input() {
        let exc = excavation_with(&["README.md"], &[]);
        let sta = staleness_with(vec![("README.md", StalenessClass::Fresh)]);
        // Classifier says Salvageable @ 0.40 (below default floor 0.60).
        // Runner forces NeedsOperatorInput + preserves the low confidence
        // + annotates reasoning.
        let classifier = ScriptedDiagnosisClassifier::new_ok(RawDiagnosis {
            verdict: DiagnosisVerdict::Salvageable,
            confidence: 0.40,
            reasoning: "kinda leaning salvageable".to_string(),
        });
        let diag = run(&exc, &sta, &classifier, DiagnoseConfig::default())
            .await
            .unwrap();
        assert_eq!(diag.verdict, DiagnosisVerdict::NeedsOperatorInput);
        assert!(
            diag.reasoning.contains("forced_by_confidence_floor"),
            "reasoning missing floor marker: {}",
            diag.reasoning
        );
        assert!(
            diag.reasoning.contains("Salvageable"),
            "reasoning missing original verdict: {}",
            diag.reasoning
        );
    }

    #[tokio::test]
    async fn diagnose_llm_error_falls_back_to_needs_operator_input() {
        let exc = excavation_with(&["README.md"], &[]);
        let sta = staleness_with(vec![("README.md", StalenessClass::Fresh)]);
        let classifier = ScriptedDiagnosisClassifier::new_err();
        let diag = run(&exc, &sta, &classifier, DiagnoseConfig::default())
            .await
            .unwrap();
        assert_eq!(diag.verdict, DiagnosisVerdict::NeedsOperatorInput);
        assert!((diag.confidence - 0.0).abs() < f32::EPSILON);
        assert!(
            diag.reasoning.starts_with("llm_error:"),
            "reasoning missing llm_error marker: {}",
            diag.reasoning
        );
    }

    #[test]
    fn diagnose_projection_counts_are_correct() {
        let exc = excavation_with(
            &["README.md", "CLAUDE.md", "docs/architecture.md"],
            &[
                ("README.md", "./missing-script.sh"),
                ("README.md", "./also-missing.sh"),
            ],
        );
        let sta = staleness_with(vec![
            ("README.md", StalenessClass::Fresh),
            ("CLAUDE.md", StalenessClass::Aspirational),
            ("docs/architecture.md", StalenessClass::Drifting),
        ]);
        let p = build_projection(&exc, &sta);

        assert_eq!(p.repo_id, "repo:flux");
        assert_eq!(p.observation_counts.get("signal_file"), Some(&3));
        assert_eq!(p.observation_counts.get("wire_mismatch"), Some(&2));
        assert_eq!(p.staleness_counts.get("fresh"), Some(&1));
        assert_eq!(p.staleness_counts.get("aspirational"), Some(&1));
        assert_eq!(p.staleness_counts.get("drifting"), Some(&1));
        assert_eq!(p.wire_mismatch_samples.len(), 2);
        assert_eq!(p.classified_paths.len(), 3);
        assert!(
            p.has_any_top_signal,
            "README.md + CLAUDE.md are top signals"
        );
    }

    #[test]
    fn diagnosis_serde_roundtrip() {
        let diag = Diagnosis {
            verdict: DiagnosisVerdict::StaleBeyondSalvage,
            confidence: 0.77,
            reasoning: "test".to_string(),
        };
        let json = serde_json::to_string(&diag).unwrap();
        assert!(
            json.contains("\"stale_beyond_salvage\""),
            "snake_case expected: {json}"
        );
        let parsed: Diagnosis = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.verdict, DiagnosisVerdict::StaleBeyondSalvage);
        assert!((parsed.confidence - 0.77).abs() < f32::EPSILON);
    }
}
