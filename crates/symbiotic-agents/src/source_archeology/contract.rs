//! Wire contract — input / output types crossing the
//! daemon ↔ in-container runner boundary for §13b.
//!
//! Both `services/symbiotic-daemon` (writer) and
//! `services/symbiotic-archeology-runner` (reader) depend on this module,
//! so any change here is a hard schema break that the type system catches
//! immediately at both ends.
//!
//! Shape:
//!
//! - [`ArcheologyInput`] — JSON written by the daemon to
//!   `/workspace/input/archeology-input.json`. Carries everything the
//!   runner needs to construct the `OrchestratorInput` for
//!   `SourceArcheologyRunner::run_pipeline`.
//! - [`ArcheologyOutput`] — JSON written by the runner to
//!   `/workspace/output/exit-status.json`. Carries the discriminant of
//!   `PipelineOutcome` plus an error string. Per-stage outputs land in
//!   sibling files (`excavation-report.json`, etc.) — the daemon
//!   reconstructs the full `PipelineRun` by reading each one.
//! - [`PipelineOutcomeKind`] — snake_case discriminant for
//!   [`super::PipelineOutcome`] (`Full` / `Noop` / `Err`).
//! - [`LlmConfig`] — gateway URL + per-tier model + API-key env-var
//!   reference. Deliberately keeps secret material out of band — the
//!   runner reads `api_key_env_var` from its own environment so the
//!   key never lands inside the input JSON.
//!
//! See `tasks/128-source-archeology/13b-sysbox-and-runner-binary.md`
//! for the design + ratification trail.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::archeology_types::ArcheologyTarget;
use super::scaffold::ProjectContext;
use super::triage::TriageContext;

/// Serializable snapshot of [`ProjectContext`]. The daemon-side
/// `ProjectContext` is `#[derive(Debug, Clone)]` only — adding
/// `Serialize`/`Deserialize` would cross the agents ↔ control-plane
/// boundary. Convert at the wire seam instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContextWire {
    pub name: String,
    pub slug: String,
    pub description: String,
}

impl From<&ProjectContext> for ProjectContextWire {
    fn from(ctx: &ProjectContext) -> Self {
        Self {
            name: ctx.name.clone(),
            slug: ctx.slug.clone(),
            description: ctx.description.clone(),
        }
    }
}

impl From<ProjectContextWire> for ProjectContext {
    fn from(wire: ProjectContextWire) -> Self {
        Self {
            name: wire.name,
            slug: wire.slug,
            description: wire.description,
        }
    }
}

/// Input written by the daemon, read by the runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcheologyInput {
    pub target: ArcheologyTarget,
    pub project: ProjectContextWire,
    pub triage_ctx: TriageContext,
    pub stage_configs: StageConfigsWire,
    /// Container-side path to the cloned source repo (read-only mount).
    /// Daemon constructs this typically as `/workspace/repo`.
    pub clone_root_in_container: PathBuf,
    /// Container-side path to the writable checkpoint root.
    /// Daemon constructs this typically as `/workspace/checkpoints`.
    pub checkpoint_root_in_container: PathBuf,
    /// Caller-controlled timestamp — the runner uses this for both
    /// checkpoint dir naming and `OrchestratorInput.now`.
    pub now: DateTime<Utc>,
    /// LLM client wiring. Secrets stay outside the JSON — the runner
    /// reads them from its own environment via `api_key_env_var`.
    pub llm_config: LlmConfig,
}

/// Wire snapshot of [`super::StageConfigs`]. The daemon-side struct is
/// `Copy + Default`, so we mirror by value.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct StageConfigsWire {
    pub diagnose: super::diagnose::DiagnoseConfig,
    pub scaffold: super::scaffold::ScaffoldConfig,
    pub handoff: super::handoff::HandoffConfig,
    pub triage: super::triage::TriageConfig,
    pub verify: super::verify::VerifyConfig,
}

impl From<super::StageConfigs> for StageConfigsWire {
    fn from(s: super::StageConfigs) -> Self {
        Self {
            diagnose: s.diagnose,
            scaffold: s.scaffold,
            handoff: s.handoff,
            triage: s.triage,
            verify: s.verify,
        }
    }
}

impl From<StageConfigsWire> for super::StageConfigs {
    fn from(s: StageConfigsWire) -> Self {
        Self {
            diagnose: s.diagnose,
            scaffold: s.scaffold,
            handoff: s.handoff,
            triage: s.triage,
            verify: s.verify,
        }
    }
}

/// Output written by the runner, read by the daemon.
///
/// The full [`super::PipelineRun`] is reconstructed daemon-side from
/// per-stage JSON files (`excavation-report.json`, `staleness-report.json`,
/// …). This struct only carries the discriminant + an optional error
/// string so the daemon can decide which sibling files to expect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcheologyOutput {
    pub outcome: PipelineOutcomeKind,
    /// Populated only when `outcome == PipelineOutcomeKind::Err`. For
    /// `Full` and `Noop` this is `None`.
    #[serde(default)]
    pub error: Option<String>,
}

/// Snake-case discriminant for [`super::PipelineOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineOutcomeKind {
    /// `PipelineOutcome::Full(_)` — sibling files written under
    /// `/workspace/output/`.
    Full,
    /// `PipelineOutcome::Noop { .. }` — `noop-report.json` written
    /// instead of per-stage outputs.
    Noop,
    /// Pipeline aborted before producing a usable outcome. `error`
    /// carries the operator-facing summary.
    Err,
}

/// LLM client wiring for the in-container runner.
///
/// The runner constructs an `LlmClient` impl from this — typically
/// reading `api_key_env_var` from `std::env` and POSTing chat-completion
/// calls to `gateway_url`. Per-stage tier selection (`fast` vs `deep`)
/// is captured by `model_per_tier`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Gateway base URL — typically the credential-gateway or an
    /// OpenAI-compatible proxy. Empty string means "no LLM" — the runner
    /// falls back to scripted/deterministic stub classifiers (used by
    /// integration tests).
    pub gateway_url: String,
    /// Map from tier name (`"fast"`, `"deep"`) to model ID. Each stage
    /// picks its own tier; missing tiers fall back to `fast`.
    pub model_per_tier: HashMap<String, String>,
    /// Name of the env var the runner should read for the bearer token.
    /// Empty string = no auth header.
    pub api_key_env_var: String,
    /// Optional timeout per request in seconds. `None` = client default.
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::{ArcheologyMode, PathPattern};
    use crate::source_archeology::triage::TriageContext;

    fn sample_input() -> ArcheologyInput {
        let mut models = HashMap::new();
        models.insert("fast".to_string(), "gpt-5-fast".to_string());
        models.insert("deep".to_string(), "gpt-5-deep".to_string());

        ArcheologyInput {
            target: ArcheologyTarget {
                repo_id: "repo:flux".to_string(),
                base_branch: "main".to_string(),
                goal_id: "onboard".to_string(),
                allowed_paths: vec![PathPattern("docs/**".to_string())],
                mode: ArcheologyMode::Full,
            },
            project: ProjectContextWire {
                name: "Flux".to_string(),
                slug: "flux".to_string(),
                description: "test fixture".to_string(),
            },
            triage_ctx: TriageContext::default(),
            stage_configs: StageConfigsWire::default(),
            clone_root_in_container: PathBuf::from("/workspace/repo"),
            checkpoint_root_in_container: PathBuf::from("/workspace/checkpoints"),
            now: DateTime::parse_from_rfc3339("2026-04-19T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            llm_config: LlmConfig {
                gateway_url: "https://gw.example/v1".to_string(),
                model_per_tier: models,
                api_key_env_var: "SYMBIOTIC_LLM_KEY".to_string(),
                request_timeout_secs: Some(45),
            },
        }
    }

    #[test]
    fn archeology_input_roundtrips_through_serde_json() {
        let input = sample_input();
        let json = serde_json::to_string(&input).expect("serialize");
        let parsed: ArcheologyInput = serde_json::from_str(&json).expect("deserialize");
        // Spot-check load-bearing fields end-to-end.
        assert_eq!(parsed.target.repo_id, "repo:flux");
        assert_eq!(parsed.target.allowed_paths.len(), 1);
        assert_eq!(parsed.project.slug, "flux");
        assert_eq!(
            parsed.clone_root_in_container,
            PathBuf::from("/workspace/repo")
        );
        assert_eq!(parsed.llm_config.api_key_env_var, "SYMBIOTIC_LLM_KEY");
        assert_eq!(
            parsed.llm_config.model_per_tier.get("deep"),
            Some(&"gpt-5-deep".to_string())
        );
    }

    #[test]
    fn pipeline_outcome_kind_serializes_snake_case() {
        // Wire-stable snake_case: the daemon parses these literal strings.
        let cases = [
            (PipelineOutcomeKind::Full, "\"full\""),
            (PipelineOutcomeKind::Noop, "\"noop\""),
            (PipelineOutcomeKind::Err, "\"err\""),
        ];
        for (variant, expected) in cases {
            let s = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(s, expected, "variant={variant:?}");
            let parsed: PipelineOutcomeKind = serde_json::from_str(expected).expect("deserialize");
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn archeology_output_omits_error_when_none() {
        // Compact wire form on the success path: `error` field elided
        // when the outcome is Full / Noop.
        let out = ArcheologyOutput {
            outcome: PipelineOutcomeKind::Full,
            error: None,
        };
        let json = serde_json::to_string(&out).expect("serialize");
        let parsed: ArcheologyOutput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.outcome, PipelineOutcomeKind::Full);
        assert!(parsed.error.is_none());
    }
}
