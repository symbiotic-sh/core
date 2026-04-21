//! Sandbox seam for the Source Archeology pipeline.
//!
//! `ArcheologySandbox` trait + `InProcessArcheologySandbox` impl + a
//! `verify_write_scope` defense-in-depth verifier. §13b adds a second
//! impl, [`SysboxArcheologySandbox`], that runs the pipeline inside a
//! Sysbox container via `symbiotic-archeology-runner` — both implement
//! the same trait.
//!
//! See `tasks/128-source-archeology/13-sandbox-wiring.md` and §13b for
//! the design + ratification trail.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use symbiotic_agents::source_archeology::contract::{
    ArcheologyInput, ArcheologyOutput, LlmConfig, PipelineOutcomeKind, ProjectContextWire,
    StageConfigsWire,
};
use symbiotic_agents::source_archeology::reconcile::path_matches_allowed;
use symbiotic_agents::source_archeology::{
    ArcheologyCheckpoint, ArcheologyTarget, AspirationalClassifier, Classifiers,
    DiagnosisClassifier, ExcavationReport, Finding, FindingAction, HandoffReport, LintVerifier,
    OrchestratorInput, PatchVerifier, PathPattern, PipelineOutcome, PipelineRun, ProjectContext,
    Reconciler, Reporter, Reviewer, Scaffolder, SourceArcheologyRunner, StageConfigs,
    StalenessReport, TriageContext, TriageDecision, Triager,
};
use symbiotic_trust::AccessBroker;
use symbiotic_vm::manager::VmManager;
use symbiotic_vm::types::{
    BindMount, FileTransfer, NetworkPolicy, TransferDirection, VmCreateRequest, VmId, VmResources,
};
use tracing::warn;

// ── Trait ──────────────────────────────────────────────────────────────

/// Sandbox-wrapped pipeline runner. Implementations decide *where* the
/// pipeline executes (in-process for tests, Sysbox container in
/// production via §13b). All impls return findings filtered through
/// [`verify_write_scope`].
///
/// The `agent_id`, `token_id`, `now` parameters are unused by the
/// in-process impl but required by the Sysbox impl (every `VmManager`
/// method gates on a `CapabilityToken`). Pre-minted-token-in-trait
/// keeps the sandbox stateless and lets the daemon caller mint scoped
/// tokens with the appropriate TTL + scope set.
#[async_trait]
pub trait ArcheologySandbox: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn run_in_sandbox(
        &self,
        target: &ArcheologyTarget,
        clone_root: &Path,
        project: &ProjectContext,
        triage_ctx: TriageContext,
        checkpoint_root: &Path,
        agent_id: &str,
        token_id: &str,
        now: DateTime<Utc>,
    ) -> Result<PipelineOutcome>;
}

// ── In-process impl ────────────────────────────────────────────────────

/// In-process sandbox. Holds owned classifier impls + stage configs,
/// builds a fresh [`SourceArcheologyRunner`] per call. Token params are
/// accepted for trait uniformity but ignored — there is no real VM, so
/// no capability gate to evaluate.
///
/// All eight classifier traits are already `Send + Sync` bounded by the
/// library, so the owned `Box<dyn …>` slots are `Send + Sync` without
/// extra annotation.
pub struct InProcessArcheologySandbox {
    aspirational: Box<dyn AspirationalClassifier>,
    diagnosis: Box<dyn DiagnosisClassifier>,
    reconciler: Box<dyn Reconciler>,
    scaffolder: Box<dyn Scaffolder>,
    reporter: Box<dyn Reporter>,
    triager: Box<dyn Triager>,
    reviewer: Option<Box<dyn Reviewer>>,
    patch_verifier: Box<dyn PatchVerifier>,
    lint_verifier: Box<dyn LintVerifier>,
    stage_configs: StageConfigs,
    config: ArcheologySandboxConfig,
}

impl InProcessArcheologySandbox {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        aspirational: Box<dyn AspirationalClassifier>,
        diagnosis: Box<dyn DiagnosisClassifier>,
        reconciler: Box<dyn Reconciler>,
        scaffolder: Box<dyn Scaffolder>,
        reporter: Box<dyn Reporter>,
        triager: Box<dyn Triager>,
        reviewer: Option<Box<dyn Reviewer>>,
        patch_verifier: Box<dyn PatchVerifier>,
        lint_verifier: Box<dyn LintVerifier>,
        stage_configs: StageConfigs,
        config: ArcheologySandboxConfig,
    ) -> Self {
        Self {
            aspirational,
            diagnosis,
            reconciler,
            scaffolder,
            reporter,
            triager,
            reviewer,
            patch_verifier,
            lint_verifier,
            stage_configs,
            config,
        }
    }

    fn build_classifiers(&self) -> Classifiers<'_> {
        Classifiers {
            aspirational: &*self.aspirational,
            diagnosis: &*self.diagnosis,
            reconciler: &*self.reconciler,
            scaffolder: &*self.scaffolder,
            reporter: &*self.reporter,
            triager: &*self.triager,
            reviewer: self.reviewer.as_deref(),
            patch_verifier: &*self.patch_verifier,
            lint_verifier: &*self.lint_verifier,
        }
    }
}

#[async_trait]
impl ArcheologySandbox for InProcessArcheologySandbox {
    async fn run_in_sandbox(
        &self,
        target: &ArcheologyTarget,
        clone_root: &Path,
        project: &ProjectContext,
        triage_ctx: TriageContext,
        checkpoint_root: &Path,
        _agent_id: &str,
        _token_id: &str,
        now: DateTime<Utc>,
    ) -> Result<PipelineOutcome> {
        let runner = SourceArcheologyRunner::new(self.build_classifiers(), self.stage_configs);
        let input = OrchestratorInput {
            target,
            clone_root,
            project,
            triage_ctx,
            checkpoint_root,
            now,
        };
        let timeout = std::time::Duration::from_secs(self.config.timeout_secs);
        let outcome = tokio::time::timeout(timeout, runner.run_pipeline(&input))
            .await
            .map_err(|_| {
                anyhow!(
                    "archeology pipeline exceeded timeout of {}s",
                    self.config.timeout_secs
                )
            })??;
        Ok(verify_write_scope(outcome, target, &self.config))
    }
}

// ── Config ─────────────────────────────────────────────────────────────

/// Per-task tunables. Per `docs/design/agent-tunables.md`: every
/// threshold + bound on a typed config struct with conservative
/// `Default`, sourceable from `RepoManifest.archeology_policy`.
#[derive(Debug, Clone)]
pub struct ArcheologySandboxConfig {
    /// Max seconds the pipeline may run before abort. Default: 600.
    /// Honored by the in-process impl via `tokio::time::timeout`. The
    /// Sysbox impl will also pass this through to `VmResources` so the
    /// container is killed at the same boundary.
    pub timeout_secs: u64,
    /// Block egress from the Sysbox impl. Default: true. Ignored by
    /// the in-process impl (no network controls).
    pub deny_network: bool,
    /// If true, drop write-scope-violating findings silently. If false,
    /// also emit a `tracing::warn!` per drop. Default: false.
    pub silent_write_scope_violations: bool,
}

impl Default for ArcheologySandboxConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 600,
            deny_network: true,
            silent_write_scope_violations: false,
        }
    }
}

// ── Write-scope verifier ───────────────────────────────────────────────

/// Defense-in-depth: re-checks every Patch + NewFile finding's path
/// against `target.allowed_paths`. The library already enforces this for
/// the **declared `evidence_path`** in
/// `symbiotic_agents::source_archeology::reconcile` (drift findings) and
/// `…::scaffold` (source-side patches); this verifier additionally parses
/// each Patch diff body for `+++ b/<path>` headers — a malicious or
/// buggy stage could craft a diff that touches files outside the
/// finding's declared path.
///
/// `Noop` outcomes pass through unchanged. Empty `allowed_paths` means
/// "no scope fence" — all findings pass through (matches
/// [`path_matches_allowed`] semantics).
pub fn verify_write_scope(
    outcome: PipelineOutcome,
    target: &ArcheologyTarget,
    config: &ArcheologySandboxConfig,
) -> PipelineOutcome {
    let PipelineOutcome::Full(mut run) = outcome else {
        return outcome;
    };
    let kept = filter_findings_by_scope(
        std::mem::take(&mut run.findings),
        &target.allowed_paths,
        config,
    );
    run.findings = kept;
    PipelineOutcome::Full(run)
}

/// Inner verifier — testable in isolation from `PipelineOutcome`
/// construction. Drops findings whose declared `evidence_path`,
/// `NewFile.path`, or `Patch` diff-body `+++ b/<path>` headers fall
/// outside `allowed`. `Report` findings are always kept (no path).
fn filter_findings_by_scope(
    findings: Vec<Finding>,
    allowed: &[PathPattern],
    config: &ArcheologySandboxConfig,
) -> Vec<Finding> {
    let mut kept = Vec::with_capacity(findings.len());
    for finding in findings {
        if finding_within_scope(&finding, allowed) {
            kept.push(finding);
        } else if !config.silent_write_scope_violations {
            warn!(
                finding_id = %finding.id,
                evidence_path = %finding.evidence_path,
                "dropped finding: write-scope verifier rejected one or more paths against target.allowed_paths"
            );
        }
    }
    kept
}

fn finding_within_scope(finding: &Finding, allowed: &[PathPattern]) -> bool {
    if !path_matches_allowed(&finding.evidence_path, allowed) {
        return false;
    }
    match &finding.proposed_action {
        FindingAction::Patch { diff } => diff_paths_in_scope(diff, allowed),
        FindingAction::NewFile { path, .. } => path_matches_allowed(path, allowed),
        FindingAction::Report { .. } => true,
    }
}

/// Extract `+++ b/<path>` headers from a unified diff body and verify
/// each path against `allowed`. Returns `false` on the first miss. The
/// `path_matches_allowed` empty-list semantics (matches everything)
/// flow through — so an empty `allowed` slice causes this function to
/// return `true` regardless of diff content.
fn diff_paths_in_scope(diff: &str, allowed: &[PathPattern]) -> bool {
    diff.lines()
        .filter_map(|line| line.strip_prefix("+++ b/"))
        .all(|path| path_matches_allowed(path, allowed))
}

// ── Sysbox impl (T128 §13b) ────────────────────────────────────────────

/// Container-side mount paths used by the runner. Mirrors the layout in
/// `services/symbiotic-archeology-runner/Dockerfile`.
const VM_REPO_MOUNT: &str = "/workspace/repo";
const VM_CHECKPOINTS_MOUNT: &str = "/workspace/checkpoints";
const VM_RUNNER_BIN_MOUNT: &str = "/usr/local/bin/symbiotic-archeology-runner";
const VM_OUTPUT_DIR: &str = "/workspace/output";
const VM_INPUT_FILE: &str = "/workspace/input/archeology-input.json";

/// Sandbox impl that runs the Source Archeology pipeline inside a
/// Sysbox container via [`symbiotic_vm::manager::VmManager`] +
/// [`symbiotic_vm::backends::sysbox::SysboxBackend`].
///
/// Per §13b ratification:
/// - Runner binary is **bind-mounted**, not baked into the image —
///   `runner_binary_host_path` is the host path resolved at construction.
/// - Network is denied via `NetworkPolicy { deny_all: true, .. }`,
///   which the SysboxBackend cleanup translates into Docker's
///   `network_mode: "none"`.
/// - Source clone mounts read-only at `/workspace/repo`; checkpoint root
///   mounts writable at `/workspace/checkpoints`.
/// - Per-stage outputs are extracted from `/workspace/output/` and
///   reconstructed into a [`PipelineRun`].
/// - LLM wiring (`gateway_url`, model tiers, API-key env-var name) is
///   carried in the input JSON via [`LlmConfig`]; secrets stay outside
///   the JSON via the `api_key_env_var` indirection.
///
/// All findings emitted on the success path flow through
/// [`verify_write_scope`] before return — the in-process and in-sandbox
/// trait impls behave identically at the boundary.
pub struct SysboxArcheologySandbox {
    vm_manager: Arc<Mutex<VmManager>>,
    broker: Arc<Mutex<AccessBroker>>,
    image: String,
    runner_binary_host_path: PathBuf,
    resources: VmResources,
    config: ArcheologySandboxConfig,
    /// LLM gateway wiring passed through to the in-container runner via
    /// the input JSON. Carried on the struct (not per-call) so the daemon
    /// configures it once at sandbox construction.
    llm_config: LlmConfig,
}

impl SysboxArcheologySandbox {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vm_manager: Arc<Mutex<VmManager>>,
        broker: Arc<Mutex<AccessBroker>>,
        image: String,
        runner_binary_host_path: PathBuf,
        resources: VmResources,
        config: ArcheologySandboxConfig,
        llm_config: LlmConfig,
    ) -> Self {
        Self {
            vm_manager,
            broker,
            image,
            runner_binary_host_path,
            resources,
            config,
            llm_config,
        }
    }

    fn build_vm_request(
        &self,
        agent_id: &str,
        host_input_path: &Path,
        clone_root: &Path,
        checkpoint_root: &Path,
    ) -> VmCreateRequest {
        let mut resources = self.resources.clone();
        // Pipeline timeout is the source of truth — propagate to the
        // backend so reap_expired kills the container at the same edge.
        resources.timeout_secs = self.config.timeout_secs;

        VmCreateRequest {
            image: self.image.clone(),
            resources,
            network: NetworkPolicy {
                deny_all: self.config.deny_network,
                ..NetworkPolicy::default()
            },
            inject_files: vec![FileTransfer {
                host_path: host_input_path.display().to_string(),
                vm_path: VM_INPUT_FILE.to_string(),
                direction: TransferDirection::HostToVm,
            }],
            requesting_agent: agent_id.to_string(),
            purpose: "source-archeology".to_string(),
            env: Vec::new(),
            mounts: vec![
                BindMount {
                    host_path: clone_root.display().to_string(),
                    vm_path: VM_REPO_MOUNT.to_string(),
                    read_only: true,
                },
                BindMount {
                    host_path: checkpoint_root.display().to_string(),
                    vm_path: VM_CHECKPOINTS_MOUNT.to_string(),
                    read_only: false,
                },
                BindMount {
                    host_path: self.runner_binary_host_path.display().to_string(),
                    vm_path: VM_RUNNER_BIN_MOUNT.to_string(),
                    read_only: true,
                },
            ],
        }
    }

    /// Build the wire `ArcheologyInput` the runner reads.
    fn build_input(
        &self,
        target: &ArcheologyTarget,
        project: &ProjectContext,
        triage_ctx: TriageContext,
        now: DateTime<Utc>,
    ) -> ArcheologyInput {
        ArcheologyInput {
            target: target.clone(),
            project: ProjectContextWire::from(project),
            triage_ctx,
            stage_configs: StageConfigsWire::from(StageConfigs::default()),
            clone_root_in_container: PathBuf::from(VM_REPO_MOUNT),
            checkpoint_root_in_container: PathBuf::from(VM_CHECKPOINTS_MOUNT),
            now,
            llm_config: self.llm_config.clone(),
        }
    }

    #[allow(clippy::too_many_arguments, clippy::await_holding_lock)]
    async fn run_pipeline_in_container(
        &self,
        request: VmCreateRequest,
        agent_id: &str,
        token_id: &str,
        now_unix: u64,
        host_output_dir: &Path,
    ) -> Result<PipelineOutcome> {
        // The Mutex<VmManager> + Mutex<AccessBroker> are std::sync — to
        // avoid holding their guards across `.await` (and the resulting
        // !Send futures), every VmManager interaction runs inside a
        // `spawn_blocking + block_on` task. Mirrors `create_and_start_vm`
        // / `exec_and_destroy_vm` in swarm_server.rs.
        let mgr = Arc::clone(&self.vm_manager);
        let broker = Arc::clone(&self.broker);
        let agent_id_owned = agent_id.to_string();
        let token_id_owned = token_id.to_string();

        // 1) create() + start()
        let vm_id = {
            let mgr = Arc::clone(&mgr);
            let broker = Arc::clone(&broker);
            let agent_id = agent_id_owned.clone();
            let token_id = token_id_owned.clone();
            let request = request.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(async move {
                    let mut mgr_guard =
                        mgr.lock().map_err(|_| anyhow!("VmManager lock poisoned"))?;
                    let mut broker_guard = broker
                        .lock()
                        .map_err(|_| anyhow!("AccessBroker lock poisoned"))?;
                    let vm_id = mgr_guard
                        .create(request, &mut broker_guard, &token_id, now_unix)
                        .await
                        .with_context(|| "VmManager::create failed")?;
                    mgr_guard
                        .start(
                            &vm_id,
                            &agent_id,
                            &mut broker_guard,
                            &token_id,
                            now_unix + 1,
                        )
                        .await
                        .with_context(|| "VmManager::start failed")?;
                    Ok::<String, anyhow::Error>(vm_id)
                })
            })
            .await
            .map_err(|e| anyhow!("create/start join failed: {e}"))??
        };

        // 2) Run the runner.
        let exec_result = {
            let mgr = Arc::clone(&mgr);
            let broker = Arc::clone(&broker);
            let agent_id = agent_id_owned.clone();
            let token_id = token_id_owned.clone();
            let vm_id = vm_id.clone();
            let cmd = format!(
                "{bin} --input {input} --output-dir {out}",
                bin = VM_RUNNER_BIN_MOUNT,
                input = VM_INPUT_FILE,
                out = VM_OUTPUT_DIR,
            );
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(async move {
                    let mut mgr_guard =
                        mgr.lock().map_err(|_| anyhow!("VmManager lock poisoned"))?;
                    let mut broker_guard = broker
                        .lock()
                        .map_err(|_| anyhow!("AccessBroker lock poisoned"))?;
                    let result = mgr_guard
                        .exec(
                            &vm_id,
                            &cmd,
                            &agent_id,
                            &mut broker_guard,
                            &token_id,
                            now_unix + 2,
                        )
                        .await;
                    Ok::<_, anyhow::Error>(result)
                })
            })
            .await
            .map_err(|e| anyhow!("exec join failed: {e}"))??
        };

        // 3) Extract outputs (best-effort regardless of exec status).
        let extract_result = self
            .extract_outputs(
                &vm_id,
                &agent_id_owned,
                &token_id_owned,
                now_unix,
                host_output_dir,
            )
            .await;

        // 4) Destroy regardless of prior errors.
        let destroy_result = {
            let mgr = Arc::clone(&mgr);
            let broker = Arc::clone(&broker);
            let agent_id = agent_id_owned.clone();
            let token_id = token_id_owned.clone();
            let vm_id = vm_id.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(async move {
                    let mut mgr_guard =
                        mgr.lock().map_err(|_| anyhow!("VmManager lock poisoned"))?;
                    let mut broker_guard = broker
                        .lock()
                        .map_err(|_| anyhow!("AccessBroker lock poisoned"))?;
                    mgr_guard
                        .destroy(
                            &vm_id,
                            &agent_id,
                            &mut broker_guard,
                            &token_id,
                            now_unix + 4,
                        )
                        .await
                })
            })
            .await
            .map_err(|e| anyhow!("destroy join failed: {e}"))?
        };

        if let Err(e) = destroy_result {
            warn!(vm_id = %vm_id, error = %e, "failed to destroy archeology container");
        }

        let exec = exec_result.with_context(|| "VmManager::exec failed")?;
        if exec.exit_code != 0 {
            return Err(anyhow!(
                "archeology runner exited with status {}: stdout={}; stderr={}",
                exec.exit_code,
                exec.stdout.trim(),
                exec.stderr.trim()
            ));
        }
        extract_result?;

        // 5) Reconstruct PipelineRun from the per-stage JSONs.
        reconstruct_outcome(host_output_dir)
    }

    #[allow(clippy::await_holding_lock)]
    async fn extract_outputs(
        &self,
        vm_id: &VmId,
        agent_id: &str,
        token_id: &str,
        now_unix: u64,
        host_output_dir: &Path,
    ) -> Result<()> {
        // Files the runner may write under /workspace/output. The runner
        // always writes exit-status.json; the rest are conditional on
        // outcome variant. transfer_file with VmToHost is best-effort —
        // missing files are not fatal here, but reconstruct_outcome will
        // surface the error if a required file is absent.
        let candidates: &[&str] = &[
            "exit-status.json",
            "excavation-report.json",
            "staleness-report.json",
            "diagnosis.json",
            "findings.json",
            "decisions.json",
            "checkpoint.json",
            "handoff-report.json",
            "noop-report.json",
        ];

        let mgr = Arc::clone(&self.vm_manager);
        let broker = Arc::clone(&self.broker);
        let vm_id = vm_id.clone();
        let agent_id = agent_id.to_string();
        let token_id = token_id.to_string();
        let host_output_dir = host_output_dir.to_path_buf();
        let candidates: Vec<String> = candidates.iter().map(|s| s.to_string()).collect();

        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let mut mgr_guard = mgr.lock().map_err(|_| anyhow!("VmManager lock poisoned"))?;
                let mut broker_guard = broker
                    .lock()
                    .map_err(|_| anyhow!("AccessBroker lock poisoned"))?;
                for filename in &candidates {
                    let host_path = host_output_dir.join(filename);
                    let transfer = FileTransfer {
                        host_path: host_path.display().to_string(),
                        vm_path: format!("{VM_OUTPUT_DIR}/{filename}"),
                        direction: TransferDirection::VmToHost,
                    };
                    if let Err(e) = mgr_guard
                        .transfer_file(
                            &vm_id,
                            &transfer,
                            &agent_id,
                            &mut broker_guard,
                            &token_id,
                            now_unix + 3,
                        )
                        .await
                    {
                        tracing::debug!(
                            vm_id = %vm_id,
                            file = %filename,
                            error = %e,
                            "transfer_file VmToHost failed; file may be absent for this outcome"
                        );
                    }
                }
                Ok::<(), anyhow::Error>(())
            })
        })
        .await
        .map_err(|e| anyhow!("transfer join failed: {e}"))?
    }
}

#[async_trait]
impl ArcheologySandbox for SysboxArcheologySandbox {
    async fn run_in_sandbox(
        &self,
        target: &ArcheologyTarget,
        clone_root: &Path,
        project: &ProjectContext,
        triage_ctx: TriageContext,
        checkpoint_root: &Path,
        agent_id: &str,
        token_id: &str,
        now: DateTime<Utc>,
    ) -> Result<PipelineOutcome> {
        // 1) Materialize the input JSON to a host-side tempdir, plus an
        //    empty output dir for transfer_file extraction.
        let host_io_dir = tempfile::tempdir()
            .with_context(|| "create host-side I/O tempdir for archeology container")?;
        let host_input_path = host_io_dir.path().join("archeology-input.json");
        let input = self.build_input(target, project, triage_ctx, now);
        let input_bytes =
            serde_json::to_vec_pretty(&input).with_context(|| "serialize ArcheologyInput")?;
        std::fs::write(&host_input_path, &input_bytes)
            .with_context(|| format!("write {}", host_input_path.display()))?;
        let host_output_dir = host_io_dir.path().join("output");
        std::fs::create_dir_all(&host_output_dir)
            .with_context(|| format!("create {}", host_output_dir.display()))?;

        // 2) Build VmCreateRequest with the bind mounts + injected
        //    input file.
        let request =
            self.build_vm_request(agent_id, &host_input_path, clone_root, checkpoint_root);

        // 3) Run the container under a single tokio::time::timeout that
        //    matches the same boundary as the in-process impl.
        let now_unix = u64::try_from(now.timestamp().max(0)).unwrap_or(0);
        let timeout = std::time::Duration::from_secs(self.config.timeout_secs);
        let outcome = tokio::time::timeout(
            timeout,
            self.run_pipeline_in_container(request, agent_id, token_id, now_unix, &host_output_dir),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "archeology pipeline exceeded timeout of {}s (container path)",
                self.config.timeout_secs
            )
        })??;

        // Keep the tempdir alive through the borrow above.
        let _ = host_io_dir;

        Ok(verify_write_scope(outcome, target, &self.config))
    }
}

/// Reconstruct a [`PipelineOutcome`] from the per-stage JSON files the
/// runner wrote into `host_output_dir`. `exit-status.json` is always
/// required; the rest are conditional on outcome variant.
fn reconstruct_outcome(host_output_dir: &Path) -> Result<PipelineOutcome> {
    let exit_status: ArcheologyOutput = read_json(&host_output_dir.join("exit-status.json"))
        .with_context(|| "missing or unreadable exit-status.json from archeology container")?;
    match exit_status.outcome {
        PipelineOutcomeKind::Err => Err(anyhow!(
            "archeology runner reported err: {}",
            exit_status
                .error
                .unwrap_or_else(|| "(no detail)".to_string())
        )),
        PipelineOutcomeKind::Noop => {
            let report: NoopReport = read_json(&host_output_dir.join("noop-report.json"))
                .with_context(|| "missing noop-report.json on PipelineOutcomeKind::Noop")?;
            Ok(PipelineOutcome::Noop {
                reason: report.reason,
                current_head: report.current_head,
                prior_checkpoint: Box::new(report.prior_checkpoint),
            })
        }
        PipelineOutcomeKind::Full => {
            let excavation: ExcavationReport =
                read_json(&host_output_dir.join("excavation-report.json"))?;
            let staleness: StalenessReport =
                read_json(&host_output_dir.join("staleness-report.json"))?;
            let diagnosis = read_json(&host_output_dir.join("diagnosis.json"))?;
            let findings: Vec<Finding> = read_json(&host_output_dir.join("findings.json"))?;
            let decisions: Vec<TriageDecision> =
                read_json(&host_output_dir.join("decisions.json"))?;
            let checkpoint: ArcheologyCheckpoint =
                read_json(&host_output_dir.join("checkpoint.json"))?;
            let handoff_path = host_output_dir.join("handoff-report.json");
            let handoff: Option<HandoffReport> = if handoff_path.exists() {
                Some(read_json(&handoff_path)?)
            } else {
                None
            };
            // The container-side checkpoint_path is meaningless on the
            // host. Reconstruct as `host_output_dir/checkpoint.json` so
            // host-side callers can still read it back.
            let checkpoint_path = host_output_dir.join("checkpoint.json");
            Ok(PipelineOutcome::Full(Box::new(PipelineRun {
                excavation,
                staleness,
                diagnosis,
                findings,
                handoff,
                decisions,
                checkpoint,
                checkpoint_path,
            })))
        }
    }
}

#[derive(serde::Deserialize)]
struct NoopReport {
    reason: String,
    current_head: String,
    prior_checkpoint: ArcheologyCheckpoint,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("deserialize {}", path.display()))
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use symbiotic_agents::source_archeology::{
        FindingAction, FindingSeverity, FindingSourceStage, GoalAlignment,
    };

    fn pat(s: &str) -> PathPattern {
        PathPattern(s.to_string())
    }

    fn config_loud() -> ArcheologySandboxConfig {
        ArcheologySandboxConfig::default()
    }

    fn config_silent() -> ArcheologySandboxConfig {
        ArcheologySandboxConfig {
            silent_write_scope_violations: true,
            ..ArcheologySandboxConfig::default()
        }
    }

    fn finding_with(id: &str, evidence_path: &str, action: FindingAction) -> Finding {
        Finding {
            id: id.to_string(),
            source_stage: FindingSourceStage::Reconcile,
            severity: FindingSeverity::Medium,
            category: "drift".to_string(),
            evidence_path: evidence_path.to_string(),
            description: "test finding".to_string(),
            proposed_action: action,
            goal_alignment: GoalAlignment::InScope,
        }
    }

    #[test]
    fn filter_drops_patch_with_out_of_scope_evidence_path() {
        let findings = vec![finding_with(
            "f-1",
            "secrets/cred.env",
            FindingAction::Patch {
                diff: "--- a/secrets/cred.env\n+++ b/secrets/cred.env\n@@ -1 +1 @@\n-old\n+new\n"
                    .to_string(),
            },
        )];
        let kept = filter_findings_by_scope(findings, &[pat("docs/**")], &config_silent());
        assert!(
            kept.is_empty(),
            "out-of-scope evidence_path must drop the finding"
        );
    }

    #[test]
    fn filter_drops_patch_when_diff_body_targets_out_of_scope_path() {
        // The load-bearing case: declared evidence_path is in-scope (the
        // library would let it through), but the diff body's `+++ b/<path>`
        // header targets an out-of-scope file. Verifier catches this.
        let diff = concat!(
            "--- a/docs/in-scope.md\n",
            "+++ b/docs/in-scope.md\n",
            "@@ -1 +1 @@\n",
            "-old\n+new\n",
            "--- a/secrets/cred.env\n",
            "+++ b/secrets/cred.env\n",
            "@@ -1 +1 @@\n",
            "-secret-old\n+secret-new\n",
        );
        let findings = vec![finding_with(
            "f-1",
            "docs/in-scope.md",
            FindingAction::Patch {
                diff: diff.to_string(),
            },
        )];
        let kept = filter_findings_by_scope(findings, &[pat("docs/**")], &config_silent());
        assert!(
            kept.is_empty(),
            "diff body touching out-of-scope path must drop the finding"
        );
    }

    #[test]
    fn filter_drops_new_file_with_out_of_scope_path() {
        let findings = vec![finding_with(
            "f-1",
            "docs/in-scope.md",
            FindingAction::NewFile {
                path: "secrets/leak.md".to_string(),
                content: "anything".to_string(),
            },
        )];
        let kept = filter_findings_by_scope(findings, &[pat("docs/**")], &config_silent());
        assert!(
            kept.is_empty(),
            "NewFile out-of-scope path must drop the finding"
        );
    }

    #[test]
    fn filter_keeps_everything_when_allowed_paths_empty() {
        // Matches `path_matches_allowed` semantics: empty list = no fence.
        let findings = vec![
            finding_with(
                "f-1",
                "anywhere.md",
                FindingAction::NewFile {
                    path: "somewhere/else.md".to_string(),
                    content: "x".to_string(),
                },
            ),
            finding_with(
                "f-2",
                "any/path.md",
                FindingAction::Patch {
                    diff: "+++ b/totally/random.md\n".to_string(),
                },
            ),
        ];
        let kept = filter_findings_by_scope(findings, &[], &config_loud());
        assert_eq!(kept.len(), 2, "empty allowed_paths must keep all findings");
    }

    #[test]
    fn filter_keeps_report_action_regardless_of_path() {
        // Report findings carry no path payload — only evidence_path is
        // checked. With evidence_path in-scope, Report passes.
        let findings = vec![finding_with(
            "f-1",
            "docs/in-scope.md",
            FindingAction::Report {
                text: "operator-facing text".to_string(),
            },
        )];
        let kept = filter_findings_by_scope(findings, &[pat("docs/**")], &config_loud());
        assert_eq!(kept.len(), 1, "in-scope Report finding must be kept");
    }

    /// Compile-time trait-bound check: `InProcessArcheologySandbox`
    /// satisfies `dyn ArcheologySandbox`. Catches regressions where
    /// trait or impl signatures drift apart.
    #[test]
    fn in_process_sandbox_satisfies_trait() {
        fn assert_archeology_sandbox<T: ArcheologySandbox + ?Sized>() {}
        assert_archeology_sandbox::<dyn ArcheologySandbox>();
        assert_archeology_sandbox::<InProcessArcheologySandbox>();
    }

    // ── SysboxArcheologySandbox unit tests (T128 §13b) ────────────────

    #[test]
    fn sysbox_sandbox_satisfies_trait() {
        // Trait-bound smoke check — same shape as the in-process variant.
        fn assert_archeology_sandbox<T: ArcheologySandbox + ?Sized>() {}
        assert_archeology_sandbox::<dyn ArcheologySandbox>();
        assert_archeology_sandbox::<SysboxArcheologySandbox>();
    }

    #[test]
    fn sysbox_sandbox_build_vm_request_shape_matches_design() {
        // T128 §13b — the request the sandbox sends to VmManager must:
        //   - target the configured image
        //   - inject archeology-input.json via FileTransfer (HostToVm)
        //   - bind-mount source clone read-only at /workspace/repo
        //   - bind-mount checkpoint root writable at /workspace/checkpoints
        //   - bind-mount the runner binary read-only at /usr/local/bin/...
        //   - apply NetworkPolicy.deny_all per ArcheologySandboxConfig
        let mgr = Arc::new(Mutex::new(throwaway_manager()));
        let broker = Arc::new(Mutex::new(AccessBroker::new()));
        let sandbox = SysboxArcheologySandbox::new(
            mgr,
            broker,
            "symbiotic-archeology-v1".to_string(),
            PathBuf::from("/host/path/to/symbiotic-archeology-runner"),
            VmResources::default(),
            ArcheologySandboxConfig::default(),
            LlmConfig::default(),
        );

        let host_input = PathBuf::from("/tmp/aa-input/archeology-input.json");
        let clone_root = PathBuf::from("/srv/clones/flux");
        let checkpoint_root = PathBuf::from("/var/symbiotic/checkpoints/flux");
        let request =
            sandbox.build_vm_request("agent-1", &host_input, &clone_root, &checkpoint_root);

        assert_eq!(request.image, "symbiotic-archeology-v1");
        assert_eq!(request.requesting_agent, "agent-1");
        assert_eq!(request.purpose, "source-archeology");
        assert!(
            request.network.deny_all,
            "deny_network=true (default) must propagate to VmCreateRequest"
        );

        // Injected input file.
        assert_eq!(request.inject_files.len(), 1);
        let injected = &request.inject_files[0];
        assert_eq!(injected.host_path, host_input.display().to_string());
        assert_eq!(injected.vm_path, VM_INPUT_FILE);
        assert_eq!(injected.direction, TransferDirection::HostToVm);

        // Bind mounts.
        let by_vm_path: std::collections::HashMap<&str, &BindMount> = request
            .mounts
            .iter()
            .map(|m| (m.vm_path.as_str(), m))
            .collect();
        let repo_mount = by_vm_path
            .get(VM_REPO_MOUNT)
            .expect("repo bind mount missing");
        assert_eq!(repo_mount.host_path, clone_root.display().to_string());
        assert!(repo_mount.read_only, "source clone must mount read-only");

        let cp_mount = by_vm_path
            .get(VM_CHECKPOINTS_MOUNT)
            .expect("checkpoint bind mount missing");
        assert_eq!(cp_mount.host_path, checkpoint_root.display().to_string());
        assert!(!cp_mount.read_only, "checkpoint root must mount writable");

        let runner_mount = by_vm_path
            .get(VM_RUNNER_BIN_MOUNT)
            .expect("runner-binary bind mount missing");
        assert_eq!(
            runner_mount.host_path,
            "/host/path/to/symbiotic-archeology-runner"
        );
        assert!(runner_mount.read_only, "runner binary must mount read-only");

        // Resources timeout matches sandbox config (kill boundary
        // alignment per design §13b).
        assert_eq!(
            request.resources.timeout_secs,
            ArcheologySandboxConfig::default().timeout_secs
        );
    }

    #[test]
    fn sysbox_sandbox_build_input_carries_container_paths() {
        let mgr = Arc::new(Mutex::new(throwaway_manager()));
        let broker = Arc::new(Mutex::new(AccessBroker::new()));
        let sandbox = SysboxArcheologySandbox::new(
            mgr,
            broker,
            "img".to_string(),
            PathBuf::from("/runner"),
            VmResources::default(),
            ArcheologySandboxConfig::default(),
            LlmConfig {
                gateway_url: "https://gw.example/v1".to_string(),
                api_key_env_var: "SYMBIOTIC_LLM_KEY".to_string(),
                ..LlmConfig::default()
            },
        );
        let target = ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "main".to_string(),
            goal_id: "onboard".to_string(),
            allowed_paths: vec![],
            mode: Default::default(),
        };
        let project = ProjectContext {
            name: "Flux".to_string(),
            slug: "flux".to_string(),
            description: String::new(),
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let input = sandbox.build_input(&target, &project, TriageContext::default(), now);
        // Container-side paths land in the input JSON, NOT host paths.
        assert_eq!(input.clone_root_in_container, PathBuf::from(VM_REPO_MOUNT));
        assert_eq!(
            input.checkpoint_root_in_container,
            PathBuf::from(VM_CHECKPOINTS_MOUNT)
        );
        assert_eq!(input.target.repo_id, "repo:flux");
        assert_eq!(input.project.slug, "flux");
        assert_eq!(input.now, now);
        assert_eq!(input.llm_config.api_key_env_var, "SYMBIOTIC_LLM_KEY");
    }

    fn throwaway_manager() -> VmManager {
        use symbiotic_vm::file_bridge::FileBridge;
        use symbiotic_vm::mock_backend::MockBackend;
        let bridge = FileBridge::new(Path::new("/project"), Path::new("/project/data"));
        VmManager::new(
            Box::new(MockBackend::new()),
            Path::new("/project/data/runtime/vm-audit.jsonl"),
            bridge,
        )
    }
}
