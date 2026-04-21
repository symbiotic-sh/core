//! `symbiotic-archeology-runner` — in-container entry point for the
//! Source Archeology pipeline (T128 §13b).
//!
//! Reads `archeology-input.json` from `--input`, constructs a
//! `SourceArcheologyRunner`, runs `run_pipeline`, and writes per-stage
//! JSON outputs + `exit-status.json` under `--output-dir`.
//!
//! The daemon side (`SysboxArcheologySandbox`) bind-mounts this binary
//! into the container at `/usr/local/bin/symbiotic-archeology-runner`
//! and starts it as the container entrypoint.
//!
//! See `tasks/128-source-archeology/13b-sysbox-and-runner-binary.md`.
//!
//! ## Wire contract
//!
//! Input JSON is [`symbiotic_agents::source_archeology::ArcheologyInput`].
//! Output JSON is [`symbiotic_agents::source_archeology::ArcheologyOutput`]
//! at `exit-status.json`, plus per-stage sibling JSONs:
//!
//! - `excavation-report.json` — [`ExcavationReport`]
//! - `staleness-report.json` — [`StalenessReport`]
//! - `diagnosis.json` — [`Diagnosis`]
//! - `findings.json` — `Vec<Finding>`
//! - `decisions.json` — `Vec<TriageDecision>`
//! - `handoff-report.json` — [`HandoffReport`] (only when `Diagnosis = NeedsOperatorInput`)
//! - `checkpoint.json` — [`ArcheologyCheckpoint`]
//! - `noop-report.json` — `{ reason, current_head, prior_checkpoint }`
//!   (written instead of per-stage outputs on `PipelineOutcome::Noop`)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::Parser;
use serde::Serialize;
use symbiotic_agents::source_archeology::{
    contract::{ArcheologyInput, ArcheologyOutput, LlmConfig, PipelineOutcomeKind},
    AspirationalClassifier, Classifiers, DeclarativeTriager, DiagnosisClassifier,
    GitApplyCheckVerifier, LintVerifier, LlmAspirationalClassifier, LlmDiagnosisClassifier,
    LlmReconciler, LlmReporter, LlmScaffolder, MarkdownlintVerifier, OrchestratorInput,
    PatchVerifier, PipelineOutcome, ProjectContext, Reconciler, Reporter, Scaffolder,
    SourceArcheologyRunner, StageConfigs, Triager,
};
use symbiotic_core::protocol::{ChatMessage, LlmClient};
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "symbiotic-archeology-runner")]
#[command(about = "Run the Source Archeology pipeline inside a Sysbox container")]
struct Args {
    /// Path to `archeology-input.json` (typically
    /// `/workspace/input/archeology-input.json` inside the container).
    #[arg(long)]
    input: PathBuf,
    /// Directory under which per-stage outputs + `exit-status.json` are
    /// written (typically `/workspace/output/`).
    #[arg(long)]
    output_dir: PathBuf,
}

#[tokio::main]
async fn main() {
    init_tracing();

    let args = Args::parse();
    let exit_code = match run(&args).await {
        Ok(code) => code,
        Err(err) => {
            error!(error = %err, "runner aborted before pipeline could complete");
            // Best-effort: write an exit-status.json so the daemon side
            // sees a structured error instead of an empty output dir.
            let _ = std::fs::create_dir_all(&args.output_dir);
            let _ = write_json(
                &args.output_dir.join("exit-status.json"),
                &ArcheologyOutput {
                    outcome: PipelineOutcomeKind::Err,
                    error: Some(format!("{err:#}")),
                },
            );
            1
        }
    };
    std::process::exit(exit_code);
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("symbiotic_archeology_runner=info,warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run(args: &Args) -> Result<i32> {
    let input = read_input(&args.input)?;
    info!(
        target = %input.target.repo_id,
        goal = %input.target.goal_id,
        "starting archeology pipeline"
    );

    std::fs::create_dir_all(&args.output_dir)
        .with_context(|| format!("failed to create output dir {}", args.output_dir.display()))?;

    let project: ProjectContext = input.project.clone().into();
    let stage_configs: StageConfigs = input.stage_configs.into();

    // ── Build classifier set ───────────────────────────────────────────
    let llm_client: Arc<dyn LlmClient> = build_llm_client(&input.llm_config)?;
    let aspirational = LlmAspirationalClassifier::new(llm_client.as_ref());
    let diagnosis = LlmDiagnosisClassifier::new(llm_client.as_ref());
    let reconciler = LlmReconciler::new(llm_client.as_ref());
    let scaffolder = LlmScaffolder::new(llm_client.as_ref());
    let reporter = LlmReporter::new(llm_client.as_ref());
    let triager = DeclarativeTriager::default();
    let patch_verifier = GitApplyCheckVerifier;
    let lint_verifier = MarkdownlintVerifier::autodiscover();

    let classifiers = Classifiers {
        aspirational: &aspirational as &dyn AspirationalClassifier,
        diagnosis: &diagnosis as &dyn DiagnosisClassifier,
        reconciler: &reconciler as &dyn Reconciler,
        scaffolder: &scaffolder as &dyn Scaffolder,
        reporter: &reporter as &dyn Reporter,
        triager: &triager as &dyn Triager,
        reviewer: None,
        patch_verifier: &patch_verifier as &dyn PatchVerifier,
        lint_verifier: &lint_verifier as &dyn LintVerifier,
    };

    let runner = SourceArcheologyRunner::new(classifiers, stage_configs);

    let orchestrator_input = OrchestratorInput {
        target: &input.target,
        clone_root: input.clone_root_in_container.as_path(),
        project: &project,
        triage_ctx: input.triage_ctx,
        checkpoint_root: input.checkpoint_root_in_container.as_path(),
        now: input.now,
    };

    let outcome = runner.run_pipeline(&orchestrator_input).await?;
    write_outputs(&args.output_dir, &outcome)?;

    let exit_code = match &outcome {
        PipelineOutcome::Full(_) | PipelineOutcome::Noop { .. } => 0,
    };
    Ok(exit_code)
}

fn read_input(path: &Path) -> Result<ArcheologyInput> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read input JSON at {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "input JSON at {} is not a valid ArcheologyInput",
            path.display()
        )
    })
}

fn write_outputs(output_dir: &Path, outcome: &PipelineOutcome) -> Result<()> {
    match outcome {
        PipelineOutcome::Full(run) => {
            write_json(&output_dir.join("excavation-report.json"), &run.excavation)?;
            write_json(&output_dir.join("staleness-report.json"), &run.staleness)?;
            write_json(&output_dir.join("diagnosis.json"), &run.diagnosis)?;
            write_json(&output_dir.join("findings.json"), &run.findings)?;
            write_json(&output_dir.join("decisions.json"), &run.decisions)?;
            write_json(&output_dir.join("checkpoint.json"), &run.checkpoint)?;
            if let Some(handoff) = &run.handoff {
                write_json(&output_dir.join("handoff-report.json"), handoff)?;
            }
            write_json(
                &output_dir.join("exit-status.json"),
                &ArcheologyOutput {
                    outcome: PipelineOutcomeKind::Full,
                    error: None,
                },
            )?;
            Ok(())
        }
        PipelineOutcome::Noop {
            reason,
            current_head,
            prior_checkpoint,
        } => {
            #[derive(Serialize)]
            struct NoopReport<'a> {
                reason: &'a str,
                current_head: &'a str,
                prior_checkpoint: &'a symbiotic_agents::source_archeology::ArcheologyCheckpoint,
            }
            write_json(
                &output_dir.join("noop-report.json"),
                &NoopReport {
                    reason,
                    current_head,
                    prior_checkpoint,
                },
            )?;
            write_json(
                &output_dir.join("exit-status.json"),
                &ArcheologyOutput {
                    outcome: PipelineOutcomeKind::Noop,
                    error: None,
                },
            )?;
            Ok(())
        }
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

// ── LLM client wiring ──────────────────────────────────────────────────

fn build_llm_client(config: &LlmConfig) -> Result<Arc<dyn LlmClient>> {
    if config.gateway_url.trim().is_empty() {
        warn!(
            "no LLM gateway configured (gateway_url empty); falling back to NoopLlmClient — \
             stages requiring an LLM will return their conservative-fallback output"
        );
        return Ok(Arc::new(NoopLlmClient));
    }
    Ok(Arc::new(HttpLlmClient::new(config.clone())?))
}

/// Minimal LLM client. Posts an OpenAI-compatible chat-completion
/// request to `LlmConfig.gateway_url` + `/chat/completions`. Reads the
/// bearer token from the env var named in `LlmConfig.api_key_env_var`.
///
/// Tier selection is currently coarse: every call uses
/// `model_per_tier["fast"]` (or the first available model). Future
/// chunk: per-stage tier routing.
struct HttpLlmClient {
    http: reqwest::Client,
    endpoint: String,
    bearer: Option<String>,
    model: String,
}

impl HttpLlmClient {
    fn new(config: LlmConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder();
        if let Some(secs) = config.request_timeout_secs {
            builder = builder.timeout(std::time::Duration::from_secs(secs));
        }
        let http = builder.build().context("build reqwest client")?;
        let endpoint = format!(
            "{}/chat/completions",
            config.gateway_url.trim_end_matches('/')
        );
        let bearer = if config.api_key_env_var.trim().is_empty() {
            None
        } else {
            std::env::var(&config.api_key_env_var).ok()
        };
        let model = config
            .model_per_tier
            .get("fast")
            .or_else(|| config.model_per_tier.get("deep"))
            .or_else(|| config.model_per_tier.values().next())
            .cloned()
            .ok_or_else(|| anyhow!("LlmConfig.model_per_tier is empty"))?;
        Ok(Self {
            http,
            endpoint,
            bearer,
            model,
        })
    }
}

#[async_trait]
impl LlmClient for HttpLlmClient {
    async fn chat(&self, messages: &[ChatMessage], json_mode: bool) -> Result<String> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
        });
        if json_mode {
            body["response_format"] = serde_json::json!({ "type": "json_object" });
        }
        let mut req = self.http.post(&self.endpoint).json(&body);
        if let Some(bearer) = &self.bearer {
            req = req.bearer_auth(bearer);
        }
        let resp = req
            .send()
            .await
            .context("LLM gateway request failed to send")?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("read LLM gateway response body")?;
        if !status.is_success() {
            return Err(anyhow!("LLM gateway returned {status}: {text}"));
        }
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("LLM gateway response is not JSON: {text}"))?;
        let content = parsed
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("LLM gateway response missing choices.0.message.content"))?
            .to_string();
        Ok(content)
    }
}

/// Fallback used when no gateway is configured. Returns an error from
/// `chat()` — every classifier already has a conservative-fallback path
/// that handles client errors (Diagnose escalates, Triage falls back to
/// declarative, etc.), so the pipeline still makes forward progress.
struct NoopLlmClient;

#[async_trait]
impl LlmClient for NoopLlmClient {
    async fn chat(&self, _: &[ChatMessage], _: bool) -> Result<String> {
        Err(anyhow!(
            "no LLM client configured; runner started without a gateway_url"
        ))
    }
}
