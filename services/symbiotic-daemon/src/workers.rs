use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use log::info;
use symbiotic_archive::{ArchiveSensitivity, FileArchiveStore, StoreRequest};
use symbiotic_context::{
    ArchiveEntry as ContextArchiveEntry, ArchiveProvider, AuditRecord, AuditSink,
    Sensitivity as ContextSensitivity,
};
use symbiotic_core::intake::{
    idempotency_key, normalize_tags, normalize_url, IntakeBatchResult, IntakeItemResult,
    IntakeRequest, IntakeRoute, IntakeSource, IntakeStatus, IntakeSummary,
};
use symbiotic_domains::{DomainQueueStore, QueueKind};
use symbiotic_intake::{
    FetchedContent, IntakeStore, ReviewQueue, Sensitivity, SensitivityClassifier,
};
use symbiotic_matrix::intake::IntakeExecutor;
use symbiotic_queue::{now_unix, EnqueueRequest, QueueBackend};
use symbiotic_workflows::{StepExecutor, StepResult, StepStatus, WorkflowContext};
use url::Url;

use crate::{
    encode_ingest_payload, harden_dir_permissions, harden_file_permissions, IngestPayload,
};

pub(crate) struct QueueIntakeExecutor {
    pub(crate) queue: Arc<dyn QueueBackend>,
}

impl IntakeExecutor for QueueIntakeExecutor {
    fn execute(&self, request: IntakeRequest) -> Result<IntakeBatchResult> {
        let tags = normalize_tags(&request.tags);
        let mut items = Vec::new();
        let run_id = format!("run_{}", now_unix());

        for url in request.urls {
            let dedupe_key = idempotency_key(&url, &tags);
            let payload = encode_ingest_payload(&IngestPayload {
                url: url.as_str().to_string(),
                tags: tags.clone(),
                source: request.source.clone(),
                run_id: run_id.clone(),
            });

            let enqueued = self.queue.enqueue(EnqueueRequest {
                type_name: "ingest.fetch".to_string(),
                payload,
                idempotency_key: format!("ingest:{dedupe_key}"),
                max_attempts: 5,
                next_run_at: now_unix(),
                force: false,
            });

            match enqueued {
                Ok(outcome) => items.push(IntakeItemResult {
                    input: url.as_str().to_string(),
                    normalized_url: Some(url),
                    status: IntakeStatus::Ingested,
                    route: IntakeRoute::Archive,
                    review_queued: false,
                    review_job_id: None,
                    idempotency_key: Some(dedupe_key),
                    record_id: Some(outcome.job_id),
                    error: None,
                }),
                Err(err) => items.push(IntakeItemResult {
                    input: url.as_str().to_string(),
                    normalized_url: Some(url),
                    status: IntakeStatus::QueueFailed,
                    route: IntakeRoute::Archive,
                    review_queued: false,
                    review_job_id: None,
                    idempotency_key: Some(dedupe_key),
                    record_id: None,
                    error: Some(err.to_string()),
                }),
            }
        }

        Ok(IntakeBatchResult {
            run_id,
            summary: summarize_items(&items),
            items,
        })
    }
}

pub(crate) struct IntakeNormalizeExecutor;

impl StepExecutor for IntakeNormalizeExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        let mut outputs = std::collections::HashMap::new();
        if let Some(raw_url) = resolve_workflow_url(step, ctx) {
            let normalized = normalize_url(&raw_url)
                .with_context(|| format!("invalid workflow url `{}`", raw_url))?;
            outputs.insert("normalized_url".to_string(), normalized.to_string());
            outputs.insert("intake_status".to_string(), "normalized".to_string());
        } else {
            outputs.insert("intake_status".to_string(), "skipped".to_string());
        }

        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs,
            error: None,
        })
    }
}

pub(crate) struct IntakeDedupeExecutor {
    pub(crate) store: Arc<ArchiveIntakeStore>,
}

impl StepExecutor for IntakeDedupeExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        let mut outputs = std::collections::HashMap::new();
        let Some(raw_url) = resolve_workflow_url(step, ctx) else {
            outputs.insert("dedupe_status".to_string(), "skipped".to_string());
            return Ok(StepResult {
                step_id: step.id.clone(),
                status: StepStatus::Success,
                outputs,
                error: None,
            });
        };

        let normalized = normalize_url(&raw_url)
            .with_context(|| format!("invalid workflow url `{}`", raw_url))?;
        let tags = resolve_workflow_tags(step, ctx);
        let dedupe_key = idempotency_key(&normalized, &tags);
        let exists = self.store.exists(&dedupe_key)?;
        outputs.insert("dedupe_key".to_string(), dedupe_key);
        outputs.insert("normalized_url".to_string(), normalized.to_string());
        outputs.insert(
            "dedupe_status".to_string(),
            if exists {
                "duplicate".to_string()
            } else {
                "new".to_string()
            },
        );
        if !tags.is_empty() {
            outputs.insert("tags".to_string(), tags.join(","));
        }

        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs,
            error: None,
        })
    }
}

pub(crate) struct IngestFetchExecutor {
    pub(crate) queue: Arc<dyn QueueBackend>,
}

impl StepExecutor for IngestFetchExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        let mut outputs = std::collections::HashMap::new();
        if matches!(ctx.outputs.get("dedupe_status"), Some(status) if status == "duplicate") {
            outputs.insert("ingest_status".to_string(), "duplicate".to_string());
            return Ok(StepResult {
                step_id: step.id.clone(),
                status: StepStatus::Success,
                outputs,
                error: None,
            });
        }

        let Some(raw_url) = resolve_workflow_url(step, ctx) else {
            outputs.insert("ingest_status".to_string(), "skipped".to_string());
            return Ok(StepResult {
                step_id: step.id.clone(),
                status: StepStatus::Success,
                outputs,
                error: None,
            });
        };

        let normalized = normalize_url(&raw_url)
            .with_context(|| format!("invalid workflow url `{}`", raw_url))?;
        let source = resolve_workflow_source(step, ctx)?;
        let tags = resolve_workflow_tags(step, ctx);
        let run_id = step
            .config
            .get("run_id")
            .cloned()
            .or_else(|| ctx.inputs.get("run_id").cloned())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("run_{}", now_unix()));
        let dedupe_key = idempotency_key(&normalized, &tags);
        let payload = encode_ingest_payload(&IngestPayload {
            url: normalized.to_string(),
            tags: tags.clone(),
            source,
            run_id: run_id.clone(),
        });
        let enqueue = self.queue.enqueue(EnqueueRequest {
            type_name: "ingest.fetch".to_string(),
            payload,
            idempotency_key: format!("ingest:{dedupe_key}"),
            max_attempts: 5,
            next_run_at: now_unix(),
            force: false,
        })?;

        outputs.insert("ingest_status".to_string(), "queued".to_string());
        outputs.insert("ingest_job_id".to_string(), enqueue.job_id);
        outputs.insert("run_id".to_string(), run_id);
        outputs.insert("dedupe_key".to_string(), dedupe_key);
        if !tags.is_empty() {
            outputs.insert("tags".to_string(), tags.join(","));
        }

        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs,
            error: None,
        })
    }
}

pub(crate) struct ArchiveStoreExecutor;

impl StepExecutor for ArchiveStoreExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        let mut outputs = std::collections::HashMap::new();
        if let Some(record_id) = ctx.outputs.get("record_id").cloned() {
            outputs.insert("record_id".to_string(), record_id);
            outputs.insert("store_status".to_string(), "record_available".to_string());
        } else if ctx.outputs.contains_key("ingest_job_id") {
            outputs.insert("store_status".to_string(), "queued_via_ingest".to_string());
        } else {
            outputs.insert("store_status".to_string(), "skipped".to_string());
        }

        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs,
            error: None,
        })
    }
}

pub(crate) struct ArchiveReviewEnqueueExecutor {
    pub(crate) queue: Arc<dyn QueueBackend>,
}

impl StepExecutor for ArchiveReviewEnqueueExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        let mut outputs = std::collections::HashMap::new();
        if let Some(record_id) = ctx.outputs.get("record_id").cloned() {
            let run_id = ctx
                .outputs
                .get("run_id")
                .or_else(|| ctx.inputs.get("run_id"));
            let payload = crate::encode_review_payload(&record_id, run_id.map(String::as_str));
            let outcome = self.queue.enqueue(EnqueueRequest {
                type_name: "archive.review.enqueue".to_string(),
                payload,
                idempotency_key: format!("review:{record_id}:{}", ctx.run_id),
                max_attempts: 3,
                next_run_at: now_unix(),
                force: false,
            })?;
            outputs.insert("review_status".to_string(), "queued".to_string());
            outputs.insert("review_job_id".to_string(), outcome.job_id);
            outputs.insert("record_id".to_string(), record_id);
        } else if ctx.outputs.contains_key("ingest_job_id") {
            outputs.insert("review_status".to_string(), "queued_via_ingest".to_string());
        } else {
            outputs.insert("review_status".to_string(), "skipped".to_string());
        }

        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs,
            error: None,
        })
    }
}

pub(crate) struct QueueReviewAdapter {
    pub(crate) queue: Arc<dyn QueueBackend>,
    pub(crate) max_attempts: u32,
    /// Set before calling `pipeline.process()` so the review enqueue job
    /// carries the same `run_id` as the originating intake submission.
    pub(crate) current_run_id: Mutex<Option<String>>,
}

impl QueueReviewAdapter {
    pub(crate) fn set_run_id(&self, run_id: &str) {
        *self.current_run_id.lock().unwrap() = Some(run_id.to_string());
    }

    pub(crate) fn clear_run_id(&self) {
        *self.current_run_id.lock().unwrap() = None;
    }
}

impl ReviewQueue for QueueReviewAdapter {
    fn enqueue(&self, record_id: &str) -> Result<String> {
        let run_id = self.current_run_id.lock().unwrap().clone();
        let payload = crate::encode_review_payload(record_id, run_id.as_deref());
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "archive.review.enqueue".to_string(),
            payload,
            idempotency_key: format!("review:{record_id}"),
            max_attempts: self.max_attempts,
            next_run_at: now_unix(),
            force: false,
        })?;
        Ok(outcome.job_id)
    }
}

pub(crate) struct DomainQueueExecutor {
    pub(crate) store: Arc<DomainQueueStore>,
}

impl StepExecutor for DomainQueueExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        let domain = step
            .config
            .get("domain")
            .cloned()
            .or_else(|| ctx.inputs.get("domain").cloned())
            .ok_or_else(|| anyhow!("domain.queue.pull requires domain"))?;
        let queue_kind = match step.config.get("queue").map(|value| value.as_str()) {
            Some("backlog") => Some(QueueKind::Backlog),
            Some("active") => Some(QueueKind::Active),
            Some("all") | None => None,
            Some(other) => {
                return Err(anyhow!("invalid queue kind {other}"));
            }
        };

        let tasks = self.store.list_tasks(&domain, queue_kind)?;
        let mut outputs = std::collections::HashMap::new();
        outputs.insert("task_count".to_string(), tasks.len().to_string());
        outputs.insert(
            "task_ids".to_string(),
            tasks
                .iter()
                .map(|task| task.id.clone())
                .collect::<Vec<_>>()
                .join(","),
        );
        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs,
            error: None,
        })
    }
}

pub(crate) struct ArchiveIntakeStore {
    pub(crate) archive_store: Arc<FileArchiveStore>,
    pub(crate) vault_store: Arc<FileArchiveStore>,
}

impl IntakeStore for ArchiveIntakeStore {
    fn exists(&self, idempotency_key: &str) -> Result<bool> {
        Ok(self.archive_store.exists_idempotency_key(idempotency_key)?
            || self.vault_store.exists_idempotency_key(idempotency_key)?)
    }

    fn store_archive_url(
        &self,
        url: &Url,
        content: &FetchedContent,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String> {
        // T12: Prefer the resolved title from FetchedContent (set by
        // the pipeline via LLM or HTML fallback) over the raw URL.
        let title_hint = content
            .title
            .clone()
            .or_else(|| content.html_title.clone())
            .unwrap_or_else(|| url.as_str().to_string());
        let outcome = self.archive_store.store(StoreRequest {
            title_hint: Some(title_hint),
            content: content.markdown.clone(),
            source_url: Some(url.as_str().to_string()),
            tags: tags.to_vec(),
            sensitivity: ArchiveSensitivity::Shareable,
            idempotency_key: idempotency_key.to_string(),
            firewall_verdict: Some(firewall_verdict),
        })?;
        Ok(outcome.record_id)
    }

    fn store_archive_note(
        &self,
        note: &str,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String> {
        let outcome = self.archive_store.store(StoreRequest {
            title_hint: Some("Note".to_string()),
            content: note.to_string(),
            source_url: None,
            tags: tags.to_vec(),
            sensitivity: ArchiveSensitivity::Shareable,
            idempotency_key: idempotency_key.to_string(),
            firewall_verdict: Some(firewall_verdict),
        })?;
        Ok(outcome.record_id)
    }

    fn store_vault_note(
        &self,
        note: &str,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String> {
        let outcome = self.vault_store.store(StoreRequest {
            title_hint: Some("Vault Note".to_string()),
            content: note.to_string(),
            source_url: None,
            tags: tags.to_vec(),
            sensitivity: ArchiveSensitivity::Private,
            idempotency_key: idempotency_key.to_string(),
            firewall_verdict: Some(firewall_verdict),
        })?;
        Ok(outcome.record_id)
    }

    fn update_title(&self, record_id: &str, new_title: &str) -> Result<bool> {
        // Try archive first, then vault. Returns true if either store had
        // a matching record that was updated.
        if self.archive_store.update_title(record_id, new_title)? {
            return Ok(true);
        }
        self.vault_store.update_title(record_id, new_title)
    }
}

pub(crate) struct ArchiveContextProvider {
    pub(crate) archive_store: Arc<FileArchiveStore>,
    pub(crate) vault_store: Arc<FileArchiveStore>,
}

impl ArchiveProvider for ArchiveContextProvider {
    fn list_entries(&self) -> Result<Vec<ContextArchiveEntry>> {
        let mut entries = Vec::new();
        for doc in self.archive_store.list()? {
            entries.push(map_archive_doc_to_context(doc));
        }
        for doc in self.vault_store.list()? {
            entries.push(map_archive_doc_to_context(doc));
        }
        Ok(entries)
    }
}

pub(crate) struct FileAuditSink {
    log_file: PathBuf,
    lock: Mutex<()>,
}

impl FileAuditSink {
    pub(crate) fn open(log_file: PathBuf) -> Result<Self> {
        if let Some(parent) = log_file.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create audit directory {}", parent.display())
            })?;
            harden_dir_permissions(parent, 0o700)?;
        }
        if !log_file.exists() {
            fs::File::create(&log_file)
                .with_context(|| format!("failed to create audit log {}", log_file.display()))?;
        }
        harden_file_permissions(&log_file, 0o600)?;
        Ok(Self {
            log_file,
            lock: Mutex::new(()),
        })
    }
}

impl AuditSink for FileAuditSink {
    fn record(&self, record: AuditRecord) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("audit lock poisoned"))?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_file)
            .with_context(|| format!("failed to open audit log {}", self.log_file.display()))?;
        writeln!(
            file,
            "{}\t{}\t{:?}\t{}\t{}",
            record.request_id,
            record.retrieval_mode,
            record.model_class,
            record.items_returned,
            record.redaction_applied
        )
        .with_context(|| format!("failed to write audit log {}", self.log_file.display()))?;
        Ok(())
    }
}

pub(crate) fn map_archive_doc_to_context(
    doc: symbiotic_archive::ArchiveDocument,
) -> ContextArchiveEntry {
    ContextArchiveEntry {
        id: doc.record_id,
        title: doc.title,
        content: doc.content,
        tags: doc.tags,
        sensitivity: match doc.sensitivity {
            ArchiveSensitivity::Shareable => ContextSensitivity::Shareable,
            ArchiveSensitivity::Restricted => ContextSensitivity::Restricted,
            ArchiveSensitivity::Private => ContextSensitivity::Private,
        },
        source_url: doc.source_url,
        updated_at: doc.updated_at,
        thread_id: None,
        fact_class: None,
    }
}

pub(crate) struct DaemonSensitivityClassifier;

impl SensitivityClassifier for DaemonSensitivityClassifier {
    fn classify_note(&self, note: &str) -> Sensitivity {
        let lowercase = note.to_ascii_lowercase();
        if lowercase.contains("password=")
            || lowercase.contains("api_key")
            || lowercase.contains("-----begin private key-----")
        {
            Sensitivity::High
        } else if lowercase.contains("secret")
            || lowercase.contains("ssn")
            || lowercase.contains("credit card")
        {
            Sensitivity::Medium
        } else {
            Sensitivity::Low
        }
    }
}

fn resolve_workflow_url(
    step: &symbiotic_workflows::WorkflowStep,
    ctx: &WorkflowContext,
) -> Option<String> {
    step.config
        .get("url")
        .cloned()
        .or_else(|| ctx.outputs.get("normalized_url").cloned())
        .or_else(|| ctx.outputs.get("url").cloned())
        .or_else(|| ctx.inputs.get("url").cloned())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_workflow_tags(
    step: &symbiotic_workflows::WorkflowStep,
    ctx: &WorkflowContext,
) -> Vec<String> {
    let raw = step
        .config
        .get("tags")
        .cloned()
        .or_else(|| ctx.outputs.get("tags").cloned())
        .or_else(|| ctx.inputs.get("tags").cloned())
        .unwrap_or_default();
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

fn resolve_workflow_source(
    step: &symbiotic_workflows::WorkflowStep,
    ctx: &WorkflowContext,
) -> Result<IntakeSource> {
    let raw = step
        .config
        .get("source")
        .cloned()
        .or_else(|| ctx.inputs.get("source").cloned())
        .unwrap_or_else(|| "api".to_string());
    match raw.trim().to_ascii_lowercase().as_str() {
        "cli" => Ok(IntakeSource::Cli),
        "matrix" => Ok(IntakeSource::Matrix),
        "share" => Ok(IntakeSource::Share),
        "bookmarks" => Ok(IntakeSource::Bookmarks),
        "notes" => Ok(IntakeSource::Notes),
        "api" => Ok(IntakeSource::Api),
        other => Err(anyhow!("unsupported workflow intake source `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// Self-improve workflow executors
// ---------------------------------------------------------------------------

/// Executor for `goal.plan` steps.
///
/// Reads the workflow context and produces a plan outline that subsequent
/// agent steps can consume. Outputs the plan text and step listing into
/// the workflow context for downstream executors.
#[allow(dead_code)] // Wired by goal runner in WS3
pub(crate) struct GoalPlanExecutor {
    /// Directory containing goal artifacts (plan files persisted here).
    pub(crate) goals_dir: PathBuf,
}

impl StepExecutor for GoalPlanExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        info!(
            "goal.plan step_id={} workflow_id={} run_id={}",
            step.id, ctx.workflow_id, ctx.run_id
        );

        let mut outputs = HashMap::new();
        outputs.insert("step_status".to_string(), "running".to_string());

        // Collect the list of agent roles that will execute after this step.
        // The workflow template encodes this as subsequent steps with agent_role set.
        // We expose the planned roles so downstream context compilation can use them.
        let planned_roles = ["planner", "coder", "reviewer", "security-analyst"];
        outputs.insert("planned_roles".to_string(), planned_roles.join(","));
        outputs.insert("plan_run_id".to_string(), ctx.run_id.clone());

        // Persist plan artifact to goals directory.
        let goal_dir = self.goals_dir.join(&ctx.run_id);
        if let Err(e) = fs::create_dir_all(&goal_dir) {
            return Ok(StepResult {
                step_id: step.id.clone(),
                status: StepStatus::Failed,
                outputs,
                error: Some(format!("failed to create goal dir: {e}")),
            });
        }
        let plan_text = format!(
            "Goal: {}\nRun: {}\nPlanned roles: {}\nStatus: planned",
            ctx.workflow_id,
            ctx.run_id,
            planned_roles.join(", ")
        );
        let plan_file = goal_dir.join("plan.md");
        if let Err(e) = fs::write(&plan_file, &plan_text) {
            return Ok(StepResult {
                step_id: step.id.clone(),
                status: StepStatus::Failed,
                outputs,
                error: Some(format!("failed to write plan file: {e}")),
            });
        }

        outputs.insert(
            "plan_artifact".to_string(),
            plan_file.to_string_lossy().to_string(),
        );
        outputs.insert("step_status".to_string(), "completed".to_string());

        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs,
            error: None,
        })
    }
}

/// Executor for `agent.execute` steps.
///
/// Supports two execution backends:
/// - **ReAct (default)**: Internal ReAct loop via `ProviderRouterLlmClient` +
///   built-in tools (recall, archive, queue). Works with any completion provider
///   registered in the `ProviderRouter` (Ollama local, OpenAI cloud, Anthropic).
/// - **CLI (opt-in)**: External CLI agents (Claude Code / Codex CLI) dispatched
///   via raw env vars. Selected via `agent_backend: AgentBackend::Cli`.
pub(crate) struct AgentExecuteExecutor {
    /// Directory for per-goal artifacts (agent outputs persisted here).
    pub(crate) goals_dir: PathBuf,
    /// Repo root for context compiler.
    pub(crate) repo_root: PathBuf,
    /// Provider router for sensitivity-aware LLM dispatch (ReAct backend).
    pub(crate) provider_router: Arc<symbiotic_providers::ProviderRouter>,
    /// Role registry for resolving agent role configs.
    pub(crate) role_registry: Arc<symbiotic_agent_config::RoleRegistry>,
    /// Shared identity content (SOUL.md) for agent system prompts.
    pub(crate) identity_content: Arc<Mutex<Option<String>>>,
    /// Execution backend selection.
    pub(crate) agent_backend: crate::AgentBackend,
    /// Archive store for RecallTool and ArchiveTool backends.
    pub(crate) archive_store: Arc<symbiotic_archive::FileArchiveStore>,
    /// Queue backend for QueueTool backend.
    pub(crate) queue: Arc<dyn symbiotic_queue::QueueBackend>,
    /// Shared vector index for semantic recall (None = text-only fallback).
    pub(crate) vector_index: Option<Arc<Mutex<symbiotic_context::vector_index::VectorIndex>>>,
    /// Access broker for capability checking (None = allow-all).
    pub(crate) broker: Option<Arc<Mutex<symbiotic_trust::AccessBroker>>>,
    /// Shared bridge session store for pending question/plan artifacts emitted
    /// by sandboxed agent runners via the JSON-RPC bridge.
    pub(crate) bridge_session_store: Arc<Mutex<crate::bridge_interactions::BridgeSessionStore>>,
    /// Path to the Unix domain socket for the LLM Gateway.
    pub(crate) llm_gateway_socket: Option<String>,
    /// Sandbox manager for spawning isolated agent processes.
    pub(crate) sandbox_manager: Option<Arc<Mutex<symbiotic_vm::manager::VmManager>>>,
    /// Backend for the `dispatch_agent` tool — lets a running agent spawn a
    /// sub-agent via this same executor. Stored lazily because the backend
    /// holds a `Weak<AgentExecuteExecutor>` back to `self`, so it can only be
    /// constructed after the `Arc::new(AgentExecuteExecutor { .. })` site.
    /// Empty by default; wired up in `SymbioticDaemon::new` when the executor
    /// is built.
    pub(crate) dispatch_backend:
        std::sync::OnceLock<Arc<dyn symbiotic_agents::builtin_tools::DispatchAgentBackend>>,
    /// Test-only runner harness mode for exercising the runner library seam
    /// without shelling out to the external binary.
    #[cfg(test)]
    pub(crate) runner_harness_mode: RunnerHarnessMode,
}

/// Aggregated artefacts produced by a single agent execution (ReAct loop or
/// sandbox/runner variant).
///
/// Added for T130 §04a to carry the Inquisitor's `pending_group` alongside
/// the existing `pending_question` + `pending_plan` handles without forcing
/// every call-site into a wider tuple.
///
/// Kept `pub(crate)` so sibling modules (goal pipeline wiring, tests) can
/// construct + inspect it while staying out of the public daemon API.
#[derive(Default)]
pub(crate) struct AgentExecutionResult {
    pub output: String,
    pub status: String,
    pub pending_question: Option<symbiotic_agents::builtin_tools::PendingQuestion>,
    /// New in §04a — populated by the batch Inquisitor (flag-on path) when the
    /// agent fires `ask_user_group`. `None` under flag-off or when the agent
    /// chose the sequential `ask_user` path.
    pub pending_group: Option<symbiotic_core::types::question_group::QuestionGroup>,
    pub pending_plan: Option<symbiotic_agents::builtin_tools::ProposedPlan>,
    pub pending_auth_request: Option<crate::auth_jobs::PendingAuthRequest>,
    pub context_packet: Option<symbiotic_agent_runner::ExecutionContextPacket>,
    pub checkpoint_artifact: Option<symbiotic_agent_runner::ExecutionCheckpointArtifact>,
}

impl AgentExecutionResult {
    /// Build a failure result with no artefacts.
    fn failed(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            status: "failed".to_string(),
            ..Self::default()
        }
    }
}

#[allow(clippy::await_holding_lock)]
async fn run_sandbox_agent_execution(
    mgr: &Arc<Mutex<symbiotic_vm::manager::VmManager>>,
    role: &str,
    agent_id_owned: &str,
    goal_owned: &str,
    context_owned: &str,
    gateway_token_owned: &str,
    system_prompt: &str,
    model_label: Option<&str>,
    thread_id: Option<&str>,
    max_iterations: usize,
) -> Result<symbiotic_vm::types::ExecResult, anyhow::Error> {
    let req = symbiotic_vm::types::VmCreateRequest {
        image: "symbiotic-agent-v1".to_string(),
        resources: symbiotic_vm::types::VmResources::default(),
        network: symbiotic_vm::types::NetworkPolicy::default(),
        inject_files: vec![],
        requesting_agent: agent_id_owned.to_string(),
        purpose: format!("Agent role: {role}"),
        env: vec![],
        mounts: vec![],
    };

    let mut dummy_broker = symbiotic_trust::AccessBroker::new();

    let vm_id = {
        let mut guard = mgr.lock().unwrap();
        guard.create(req, &mut dummy_broker, "internal", 0).await?
    };
    {
        let mut guard = mgr.lock().unwrap();
        guard
            .start(&vm_id, agent_id_owned, &mut dummy_broker, "internal", 0)
            .await?;
    }

    let mut cmd = format!(
        "/usr/local/bin/symbiotic-agent-runner --socket /tmp/nucleus.sock --gateway-token '{}' --goal '{}' --context '{}' --agent-id '{}' --workspace /workspace --role '{}' --sandbox-type 'vm_sandbox' --max-iterations '{}'",
        gateway_token_owned, goal_owned, context_owned, agent_id_owned, role, max_iterations
    );
    if !system_prompt.is_empty() {
        cmd.push_str(&format!(" --system-prompt '{}'", system_prompt));
    }
    if let Some(model_label) = model_label {
        cmd.push_str(&format!(" --model-label '{}'", model_label));
    }
    if let Some(thread_id) = thread_id.filter(|value| !value.is_empty()) {
        cmd.push_str(&format!(" --thread-id '{}'", thread_id));
    }

    let exec_res = {
        let mut guard = mgr.lock().unwrap();
        guard
            .exec(
                &vm_id,
                &cmd,
                agent_id_owned,
                &mut dummy_broker,
                "internal",
                0,
            )
            .await?
    };

    let _: Result<(), anyhow::Error> = {
        let mut guard = mgr.lock().unwrap();
        guard
            .destroy(&vm_id, agent_id_owned, &mut dummy_broker, "internal", 0)
            .await
    };

    Ok(exec_res)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerHarnessMode {
    Process,
    InProcess,
}

impl AgentExecuteExecutor {
    /// Resolve an [`AgentExecConfig`] from the role registry + identity content.
    ///
    /// When the resolved role is allowed to dispatch sub-agents (declares the
    /// `agent.dispatch` capability), the daemon injects an authoritative roster
    /// of the OTHER registered roles into the system prompt at resolve time.
    /// This removes the need to hard-code available specialists in the
    /// orchestrator's TOML — installing a new role in `config/agents/` makes
    /// it instantly discoverable to every dispatcher without prompt edits.
    fn resolve_role_config(
        &self,
        role: Option<&str>,
    ) -> Option<symbiotic_agents::executor::AgentExecConfig> {
        let role_name = role?;
        let resolved = self.role_registry.resolve(role_name).ok()?;

        let identity = self
            .identity_content
            .lock()
            .ok()
            .and_then(|guard| guard.clone());

        let mut system_prompt = resolved.system_prompt;
        if resolved
            .required_capabilities
            .iter()
            .any(|cap| cap == "agent.dispatch")
        {
            let roster = self.render_sub_agent_roster(role_name);
            if !roster.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&roster);
            }
        }

        // Universal JSON-output discipline, appended to every role.
        //
        // Empirically (tested against gemma4:e4b in Ollama) the model emits
        // literal 0x0A newlines inside JSON string values when writing
        // multi-line content (e.g. a markdown memo in a `file_write` call),
        // even with Ollama's `format: "json"` enabled. That's invalid JSON
        // and the ReAct parser's repair path only catches it ~sometimes.
        //
        // The cheap, high-leverage fix is prompt-side: explicitly tell every
        // agent to escape control characters. A direct A/B probe against
        // Ollama showed the "strict prompt" variant produced valid JSON
        // with zero raw newlines on the first try. We inject the rule here
        // once, at resolve time, so every role benefits without touching
        // TOMLs.
        system_prompt.push_str(
            "\n\n## Output format (universal)\n\n\
Every response is exactly ONE JSON object. Two rules the parser depends on:\n\
\n\
1. Inside any string value, escape newlines as the TWO characters `\\n` \
(backslash followed by `n`). Escape tabs as `\\t`. Never emit a literal \
0x0A newline byte inside a string — strict JSON forbids it and the \
response will be dropped.\n\
2. Do NOT wrap the JSON in markdown code fences (no ```json, no ```). Do \
NOT append any trailing tokens after the closing `}`.\n\
\n\
If you need to emit a multi-line markdown memo as the `content` field of a \
`file_write` or `archive` call, put the entire memo in one string with \
every line-break written as `\\n`.\n",
        );

        Some(symbiotic_agents::executor::AgentExecConfig {
            system_prompt: Some(system_prompt),
            max_iterations: resolved.max_iterations,
            identity_context: identity,
            handoff_dir: None,
            agent_id: None,
            role: None,
            redact_output: true,
        })
    }

    /// Render an authoritative list of every OTHER registered role (excluding
    /// `self_role` so an orchestrator isn't invited to dispatch itself) as a
    /// Markdown section that can be appended to a dispatcher's system prompt.
    ///
    /// The registry is the source of truth — if a user drops a new
    /// `config/agents/<name>.toml`, the next orchestrator invocation sees it
    /// automatically. No Rust changes, no prompt edits.
    fn render_sub_agent_roster(&self, self_role: &str) -> String {
        let mut entries: Vec<(String, String)> = self
            .role_registry
            .names()
            .into_iter()
            .filter(|n| *n != self_role)
            .filter_map(|n| {
                self.role_registry
                    .get(n)
                    .map(|role| (n.to_string(), role.description.clone()))
            })
            .collect();
        if entries.is_empty() {
            return String::new();
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = String::from(
            "## Available sub-agent roles (auto-generated from role registry)\n\n\
             Dispatch these via the `dispatch_agent` tool. Roster is injected at \
             resolve time — installing a new role file makes it appear here \
             automatically.\n\n",
        );
        for (name, description) in entries {
            out.push_str(&format!("- **{name}** — {description}\n"));
        }
        out
    }

    /// Execute a one-shot agent goal outside a workflow context.
    ///
    /// Issues capability tokens for the role, prepares a workspace directory,
    /// and dispatches to the configured `agent_backend` (React in-process by
    /// default). Used by the `symbiotic agent run` CLI command to let a user
    /// drive the researcher (or any registered role) against their own Archive
    /// without going through Matrix.
    pub(crate) fn run_agent_goal(&self, role: &str, goal: &str) -> AgentExecutionResult {
        // `{unix_ts:010}_{role}` — chronologically sortable by design,
        // used directly as the trace-folder name, capability-token subject,
        // and workspace-dir name. Fixed-width timestamp prefix keeps
        // lexical sort == chronological sort.
        let agent_id = format!(
            "{:010}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            role,
        );

        if let Some(ref broker) = self.broker {
            let role_caps = self
                .role_registry
                .resolve(role)
                .map(|r| r.required_capabilities.clone())
                .unwrap_or_default();
            if !role_caps.is_empty() {
                info!(
                    "issuing {} capability tokens for CLI agent_id={} role={} scopes={:?}",
                    role_caps.len(),
                    agent_id,
                    role,
                    role_caps
                );
                if let Ok(mut guard) = broker.lock() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    for scope in &role_caps {
                        let token = symbiotic_trust::CapabilityToken {
                            token_id: format!(
                                "tok_{:x}",
                                crate::events::simple_hash(&format!(
                                    "{}:{}:{}",
                                    agent_id, scope, now
                                ))
                            ),
                            subject: agent_id.clone(),
                            trust_level: crate::tool_adapters::scope_to_trust_level(scope),
                            scopes: [scope.to_ascii_lowercase()].into_iter().collect(),
                            expires_at: now + 3600,
                            one_time: false,
                            consumed: false,
                            goal_scope: None,
                        };
                        guard.issue_token(token);
                    }
                }
            }
        }

        let goal_dir = self.goals_dir.join(&agent_id);
        let _ = std::fs::create_dir_all(&goal_dir);

        // Only the ambient role context. `run_agent_with_config` appends
        // "Goal: {goal}" to the user message itself, so including the goal
        // here would duplicate it in every turn's prompt.
        let task_description = format!(
            "Role: {role}\n\nUse your available tools (especially `recall`) \
             to answer the goal based on the Archive. Cite which Archive \
             entries grounded your answer."
        );

        match self.agent_backend {
            crate::AgentBackend::React => {
                self.execute_react(role, &task_description, goal, &agent_id)
            }
            crate::AgentBackend::Runner => {
                self.execute_runner(role, &task_description, goal, &agent_id, None, None)
            }
            crate::AgentBackend::Cli => AgentExecutionResult::failed(
                "AgentBackend::Cli is not supported for one-shot CLI goals; set SYMBIOTIC_AGENT_BACKEND=react or runner",
            ),
        }
    }
}

impl StepExecutor for AgentExecuteExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        let role = step.agent_role.as_deref().unwrap_or("unknown");
        info!(
            "agent.execute step_id={} role={} workflow_id={} run_id={}",
            step.id, role, ctx.workflow_id, ctx.run_id
        );

        let mut outputs = HashMap::new();
        let output_key = format!("{}_status", step.id);
        outputs.insert(output_key.clone(), "running".to_string());

        let agent_id = format!("agent_{}_{}", role, &ctx.run_id);
        outputs.insert(format!("{}_agent_id", step.id), agent_id.clone());
        outputs.insert(format!("{}_role", step.id), role.to_string());

        // Issue capability tokens for this ad-hoc agent_id so workspace tools
        // (file_write, shell_exec, etc.) can pass capability checks.
        // The role's required_capabilities define the scopes it needs.
        if let Some(ref broker) = self.broker {
            let mut role_caps = self
                .role_registry
                .resolve(role)
                .map(|r| r.required_capabilities.clone())
                .unwrap_or_default();
            if let Some(extra_caps) = step.config.get("required_capabilities") {
                for scope in extra_caps
                    .split(',')
                    .map(str::trim)
                    .filter(|scope| !scope.is_empty())
                    .map(|scope| scope.to_ascii_lowercase())
                {
                    if !role_caps.iter().any(|existing| existing == &scope) {
                        role_caps.push(scope);
                    }
                }
            }
            if !role_caps.is_empty() {
                info!(
                    "issuing {} capability tokens for agent_id={} role={} scopes={:?}",
                    role_caps.len(),
                    agent_id,
                    role,
                    role_caps
                );
                if let Ok(mut guard) = broker.lock() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    for scope in &role_caps {
                        let token = symbiotic_trust::CapabilityToken {
                            token_id: format!(
                                "tok_{:x}",
                                crate::events::simple_hash(&format!(
                                    "{}:{}:{}",
                                    agent_id, scope, now
                                ))
                            ),
                            subject: agent_id.clone(),
                            trust_level: crate::tool_adapters::scope_to_trust_level(scope),
                            scopes: [scope.to_ascii_lowercase()].into_iter().collect(),
                            expires_at: now + 3600,
                            one_time: false,
                            consumed: false,
                            goal_scope: Some(ctx.workflow_id.clone()),
                        };
                        guard.issue_token(token);
                    }
                }
            }
        }

        let goal_dir = self.goals_dir.join(&ctx.run_id);
        let _ = fs::create_dir_all(&goal_dir);

        // Read workspace protocol files if they exist (from auto-execute or previous iterations)
        let goal_md = fs::read_to_string(goal_dir.join("GOAL.md")).ok();
        let progress_md = fs::read_to_string(goal_dir.join("Progress.md")).ok();

        let goal_description = ctx
            .outputs
            .get("plan_description")
            .or_else(|| ctx.inputs.get("user_goal"))
            .or_else(|| ctx.inputs.get("user_answer"))
            .cloned()
            .unwrap_or_else(|| format!("workflow run {}", ctx.run_id));
        let thread_id = ctx.inputs.get("thread_id").cloned();

        // Context files are populated by the workflow if needed.
        let context_files: Vec<String> = vec![];

        // Read context file contents and inline them into the task description
        // (the ReAct agent has no file-reading tool, so paths alone are useless).
        let context_section = if context_files.is_empty() {
            "Context: none".to_string()
        } else {
            let mut parts = vec!["## Agent Context".to_string()];
            for path in &context_files {
                let content = fs::read_to_string(path).unwrap_or_default();
                if !content.is_empty() {
                    parts.push(format!("\n{content}"));
                }
            }
            parts.join("\n")
        };
        // Build task description, incorporating workspace files if available
        let mut task_parts = vec![
            format!("Role: {role}"),
            format!("Goal: {goal_description}"),
            format!("Run: {}", ctx.run_id),
            format!(
                "Prior context keys: {}",
                ctx.outputs.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
            context_section,
        ];
        if let Some(ref goal_content) = goal_md {
            task_parts.push(format!("\n## Goal Manifest\n\n{goal_content}"));
        }
        if let Some(ref progress) = progress_md {
            task_parts.push(format!("\n## Previous Progress\n\n{progress}"));
        }
        // Inject workflow inputs so the agent can see user_goal, user_answer, etc.
        let input_entries: Vec<String> = ctx
            .inputs
            .iter()
            .filter(|(k, v)| {
                // Skip template-default placeholder descriptions.
                !v.starts_with("Natural language") && !v.starts_with("JSON array") && !k.is_empty()
            })
            .map(|(k, v)| format!("- **{k}**: {v}"))
            .collect();
        if !input_entries.is_empty() {
            task_parts.push(format!(
                "\n## Workflow Inputs\n\n{}",
                input_entries.join("\n")
            ));
        }
        task_parts.push("\nUse the agent context above as authoritative run context. Complete the goal and produce a clear, concise result.".to_string());
        let task_description = task_parts.join("\n");

        // --- Dispatch to execution backend ---
        let exec_result = match self.agent_backend {
            crate::AgentBackend::React => {
                self.execute_react(role, &task_description, &goal_description, &agent_id)
            }
            crate::AgentBackend::Runner => self.execute_runner(
                role,
                &task_description,
                &goal_description,
                &agent_id,
                Some(&ctx.workflow_id),
                thread_id.as_deref(),
            ),
            crate::AgentBackend::Cli => {
                let system_prompt = match role {
                    "planner" => Some("You are a technical planner. Analyze the codebase and produce a structured improvement plan with prioritized tasks.".to_string()),
                    "coder" => Some("You are a senior software engineer. Implement the changes described in your task, writing clean, tested code.".to_string()),
                    "reviewer" => Some("You are a code reviewer. Review the changes for correctness, style, test coverage, and architectural consistency.".to_string()),
                    "security-analyst" => Some("You are a security analyst. Review the codebase for vulnerabilities, credential leakage, injection risks, and OWASP Top 10 issues.".to_string()),
                    _ => None,
                };
                let timeout_seconds = step
                    .config
                    .get("timeout_seconds")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(600);
                let working_dir = std::env::var("SYMBIOTIC_REPO_WORKSPACE")
                    .unwrap_or_else(|_| self.repo_root.to_string_lossy().to_string());
                let task_request = symbiotic_providers::types::TaskRequest {
                    task: task_description.clone(),
                    system_prompt,
                    working_directory: Some(working_dir),
                    timeout_seconds: Some(timeout_seconds),
                    context_files: context_files.clone(),
                };
                match dispatch_to_provider(role, &task_request) {
                    Ok((output, status)) => AgentExecutionResult {
                        output,
                        status,
                        ..AgentExecutionResult::default()
                    },
                    Err(e) => {
                        info!("agent provider dispatch failed for role={}: {}", role, e);
                        AgentExecutionResult::failed(format!("Agent {role} execution failed: {e}"))
                    }
                }
            }
        };
        let AgentExecutionResult {
            output: agent_output,
            status: provider_status,
            pending_question,
            pending_group,
            pending_plan,
            pending_auth_request,
            context_packet,
            checkpoint_artifact,
        } = exec_result;

        // Persist agent output artifact.
        let artifact_file = goal_dir.join(format!("{}.md", step.id));
        let _ = fs::write(&artifact_file, &agent_output);

        // Write workspace protocol files based on outcome.
        if provider_status == "completed" {
            // Write DONE.md with summary
            let done_content = format!(
                "# Goal Completed\n\n## Summary\n\n{}\n\n## Agent\n\nRole: {}\nRun: {}\n",
                agent_output.chars().take(2000).collect::<String>(),
                role,
                ctx.run_id
            );
            let _ = fs::write(goal_dir.join("DONE.md"), &done_content);
        } else if provider_status == "failed" {
            // Check if agent wrote BLOCKED.md (indicates a blocker vs hard failure)
            if !goal_dir.join("BLOCKED.md").exists() {
                let _ = fs::write(
                    goal_dir.join("BLOCKED.md"),
                    format!("# Blocked\n\nAgent {role} failed during execution.\n\n## Error\n\n{agent_output}\n"),
                );
            }
        }

        // Always update Progress.md with latest state
        let progress = format!(
            "# Progress\n\n## Last Iteration\n\nRole: {}\nStatus: {}\nRun: {}\nStep: {}\n\n## Output\n\n{}\n",
            role,
            provider_status,
            ctx.run_id,
            step.id,
            agent_output.chars().take(4000).collect::<String>()
        );
        let _ = fs::write(goal_dir.join("Progress.md"), &progress);

        outputs.insert(
            format!("{}_artifact", step.id),
            artifact_file.to_string_lossy().to_string(),
        );
        outputs.insert(output_key, provider_status.clone());
        outputs.insert(format!("{}_output", step.id), agent_output);
        if let Some(ref pq) = pending_question {
            outputs.insert("pending_question".to_string(), pq.text.clone());
            if let Some(ref replies) = pq.quick_replies {
                if let Ok(json) = serde_json::to_string(replies) {
                    outputs.insert("pending_question_quick_replies".to_string(), json);
                }
            }
        }
        if let Some(ref group) = pending_group {
            // T130 §04a — surface the drafted `QuestionGroup` to `goals.rs`
            // which will emit the `goal.question_group` Matrix event and
            // register the group in the daemon's `QuestionResolver`.
            if let Ok(json) = serde_json::to_string(group) {
                outputs.insert("pending_question_group".to_string(), json);
            }
        }
        if let Some(ref plan) = pending_plan {
            if let Ok(json) = serde_json::to_string(plan) {
                outputs.insert("pending_plan".to_string(), json);
            }
        }
        if let Some(ref auth_request) = pending_auth_request {
            if let Ok(json) = serde_json::to_string(auth_request) {
                outputs.insert("pending_auth_request".to_string(), json);
            }
        }
        if let Some(ref packet) = context_packet {
            if let Ok(json) = serde_json::to_string(packet) {
                outputs.insert("context_packet".to_string(), json);
            }
        }
        if let Some(ref checkpoint) = checkpoint_artifact {
            if let Ok(json) = serde_json::to_string(checkpoint) {
                outputs.insert("checkpoint_artifact".to_string(), json);
            }
        }

        let step_status = if provider_status == "completed" {
            StepStatus::Success
        } else {
            StepStatus::Failed
        };

        Ok(StepResult {
            step_id: step.id.clone(),
            status: step_status,
            outputs,
            error: if provider_status == "failed" {
                Some(format!("Agent {role} failed"))
            } else {
                None
            },
        })
    }
}

impl AgentExecuteExecutor {
    /// Execute via the internal ReAct loop using `ProviderRouterLlmClient`.
    ///
    /// Returns an [`AgentExecutionResult`] carrying the agent's output plus any
    /// pending artefacts (question, question group, plan). T130 §04a added the
    /// `pending_group` bundle so the Inquisitor can batch-emit a full
    /// `QuestionGroup` via the `ask_user_group` tool when the
    /// `SYMBIOTIC_BATCH_INQUISITOR` flag is on.
    fn execute_react(
        &self,
        role: &str,
        task_description: &str,
        goal_description: &str,
        agent_id: &str,
    ) -> AgentExecutionResult {
        use crate::agents::ProviderRouterLlmClient;
        use crate::goal_pipeline::inquisitor_adapter::InquisitorTools;
        use crate::tool_adapters::{
            DaemonArchiveBackend, DaemonCapabilityChecker, DaemonQueueBackend, DaemonRecallBackend,
        };
        use symbiotic_agents::builtin_tools::{
            ArchiveTool, DispatchAgentTool, GeneratePlanTool, QueueTool, RecallTool,
        };
        use symbiotic_agents::executor::run_agent_with_config;
        use symbiotic_agents::workspace_tools::{
            FileEditTool, FileReadTool, FileWriteTool, ShellExecTool, WorkspaceConfig,
        };
        use symbiotic_core::Sensitivity;

        let mut config = self.resolve_role_config(Some(role)).unwrap_or_default();
        // Populate trace metadata so every LLM call this loop makes is
        // tagged with (agent_id, role, iteration) — provider-layer trace
        // hooks use that to segregate output per agent without a
        // post-processing step.
        config.agent_id = Some(agent_id.to_string());
        config.role = Some(role.to_string());

        let llm_client = ProviderRouterLlmClient::new(
            Arc::clone(&self.provider_router),
            Sensitivity::Shareable,
            format!("agent_execution:{role}"),
        );

        // Run the async ReAct loop from this sync StepExecutor.
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => {
                return AgentExecutionResult::failed(format!(
                    "Agent {role} failed: no tokio runtime"
                ));
            }
        };

        // Build tool backends bridging daemon stores to agent tool traits.
        let caps: Arc<dyn symbiotic_agents::builtin_tools::CapabilityChecker> = match &self.broker {
            Some(broker) => Arc::new(DaemonCapabilityChecker::new(Arc::clone(broker))),
            None => Arc::new(DaemonCapabilityChecker::allow_all()),
        };
        let recall_backend = match (&self.vector_index, &self.provider_router) {
            (Some(vi), router) => Arc::new(DaemonRecallBackend::with_vector_search(
                self.archive_store.clone(),
                Arc::clone(vi),
                Arc::clone(router),
            )),
            _ => Arc::new(DaemonRecallBackend::new(self.archive_store.clone())),
        };
        let archive_backend = Arc::new(DaemonArchiveBackend::new(self.archive_store.clone()));
        let queue_backend = Arc::new(DaemonQueueBackend::new(self.queue.clone()));

        let recall_tool = RecallTool::new(agent_id.to_string(), recall_backend, caps.clone());
        let archive_tool = ArchiveTool::new(agent_id.to_string(), archive_backend, caps.clone());
        let queue_tool = QueueTool::new(agent_id.to_string(), queue_backend, caps.clone());

        // Inquisitor tool-set: always registers `ask_user`, additionally
        // registers `ask_user_group` when `SYMBIOTIC_BATCH_INQUISITOR` is on.
        // Applied to every role so that a workflow step running a non-Inquisitor
        // agent still has `ask_user` available, while the flag-on path unlocks
        // batched emission specifically for the Inquisitor. The adapter is
        // agnostic to the role — the system prompt controls which tool the LLM
        // picks.
        let inquisitor_tools = InquisitorTools::new(Some(agent_id));
        let (generate_plan_tool, pending_plan) = GeneratePlanTool::new();

        // Workspace execution tools — agents can create files, run commands, and
        // edit code within a sandboxed workspace directory scoped to this goal.
        let workspace_config = Arc::new(WorkspaceConfig {
            root: self.goals_dir.join("workspace").join(agent_id),
            max_read_bytes: 10_485_760,
            max_write_bytes: 10_485_760,
            exec_timeout: std::time::Duration::from_secs(300),
            max_output_bytes: 1_048_576,
        });
        // Ensure workspace directory exists before tools try to use it.
        let _ = std::fs::create_dir_all(&workspace_config.root);

        let shell_tool =
            ShellExecTool::new(agent_id.to_string(), workspace_config.clone(), caps.clone());
        let file_read_tool =
            FileReadTool::new(agent_id.to_string(), workspace_config.clone(), caps.clone());
        let file_write_tool =
            FileWriteTool::new(agent_id.to_string(), workspace_config.clone(), caps.clone());
        let file_edit_tool =
            FileEditTool::new(agent_id.to_string(), workspace_config, caps.clone());

        // dispatch_agent tool — only registered when a backend was wired in
        // by the daemon constructor (see `SymbioticDaemon::new`). Keeps sub-agent
        // spawning opt-in per-executor and avoids infinite recursion in tests
        // or harnesses that never set the backend.
        let dispatch_tool_opt = self
            .dispatch_backend
            .get()
            .cloned()
            .map(|backend| DispatchAgentTool::new(agent_id.to_string(), backend, caps.clone()));

        // Tool loadout is purely capability-driven: `required_capabilities`
        // from the role's TOML (or `defaults.rs`) is the single source of
        // truth. Every tool's JSONSchema lands in the system prompt via
        // `format_tools_for_prompt`, so exposing a tool an agent can't
        // actually use is both pointless and expensive — `generate_plan`
        // alone ships ~3 KB of escalation / delivery-window / timezone /
        // SLA schema that only planning roles need. See
        // `docs/design/agent-evolution.md` for why operational policy
        // (timing, escalation) should ultimately migrate from tool-call
        // params to daemon-side event handling.
        let role_caps: std::collections::HashSet<String> = self
            .role_registry
            .resolve(role)
            .map(|r| r.required_capabilities.into_iter().collect())
            .unwrap_or_default();
        let mut tools: Vec<&dyn symbiotic_agents::tools::Tool> = Vec::new();
        if role_caps.contains("archive.read") {
            tools.push(&recall_tool);
        }
        if role_caps.contains("archive.write") {
            tools.push(&archive_tool);
        }
        if role_caps.contains("queue.submit") {
            tools.push(&queue_tool);
        }
        if role_caps.contains("workspace.fs.read") {
            tools.push(&file_read_tool);
        }
        if role_caps.contains("workspace.fs.write") {
            tools.push(&file_write_tool);
            tools.push(&file_edit_tool);
        }
        if role_caps.contains("workspace.fs.exec") {
            tools.push(&shell_tool);
        }
        if role_caps.contains("user.ask") {
            tools.push(&inquisitor_tools.ask_user);
            if let Some(ref group_tool) = inquisitor_tools.ask_user_group {
                tools.push(group_tool);
            }
        }
        if role_caps.contains("plan.propose") {
            tools.push(&generate_plan_tool);
        }
        if role_caps.contains("agent.dispatch") {
            if let Some(ref t) = dispatch_tool_opt {
                tools.push(t);
            }
        }

        let result = std::thread::scope(|s| {
            s.spawn(|| {
                handle.block_on(run_agent_with_config(
                    goal_description,
                    task_description,
                    &tools,
                    &llm_client,
                    &config,
                ))
            })
            .join()
            .expect("agent ReAct thread should not panic")
        });

        let question = inquisitor_tools
            .pending_question
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        let group = inquisitor_tools
            .pending_group
            .as_ref()
            .and_then(|handle| handle.lock().ok().and_then(|guard| guard.clone()));
        let plan = pending_plan.lock().ok().and_then(|guard| guard.clone());

        match result {
            Ok(exec_result) => AgentExecutionResult {
                output: exec_result.output,
                status: "completed".to_string(),
                pending_question: question,
                pending_group: group,
                pending_plan: plan,
                pending_auth_request: None,
                context_packet: None,
                checkpoint_artifact: None,
            },
            Err(e) => AgentExecutionResult {
                output: format!("Agent {role} ReAct execution failed: {e}"),
                status: "failed".to_string(),
                pending_question: question,
                pending_group: group,
                pending_plan: plan,
                pending_auth_request: None,
                context_packet: None,
                checkpoint_artifact: None,
            },
        }
    }

    /// Execute via the decoupled `symbiotic-agent-runner` binary.
    ///
    /// This is Phase 1 of the Nuclear Split (T113). The runner binary is spawned
    /// as a local process and communicates with the Nucleus via a Unix socket.
    fn execute_runner(
        &self,
        role: &str,
        task_description: &str,
        goal_description: &str,
        agent_id: &str,
        goal_scope: Option<&str>,
        thread_id: Option<&str>,
    ) -> AgentExecutionResult {
        let socket_path = match &self.llm_gateway_socket {
            Some(path) => path,
            None => {
                return AgentExecutionResult::failed(
                    "Runner backend requires SYMBIOTIC_LLM_GATEWAY_SOCKET to be set",
                );
            }
        };
        let gateway_token = match self.issue_bridge_session_token(agent_id, goal_scope) {
            Ok(token_id) => token_id,
            Err(e) => {
                return AgentExecutionResult::failed(format!(
                    "Runner backend failed to issue gateway token: {e}"
                ));
            }
        };
        let role_config = self.resolve_role_config(Some(role));
        let max_iterations = role_config
            .as_ref()
            .and_then(|cfg| cfg.max_iterations)
            .unwrap_or(15);
        let system_prompt = role_config
            .as_ref()
            .and_then(|cfg| cfg.system_prompt.clone())
            .unwrap_or_default();
        let model_label: Option<String> = None;

        #[cfg(test)]
        if matches!(self.runner_harness_mode, RunnerHarnessMode::InProcess) {
            return self.execute_runner_inprocess(
                role,
                task_description,
                goal_description,
                agent_id,
                socket_path,
                &gateway_token,
                &system_prompt,
                model_label.as_deref(),
                thread_id,
                max_iterations,
            );
        }

        // --- Phase 2: Sandbox Execution ---
        if let Some(ref mgr) = self.sandbox_manager {
            let handle = match tokio::runtime::Handle::try_current() {
                Ok(h) => h,
                Err(_) => {
                    return AgentExecutionResult::failed(
                        "Failed to get tokio runtime for sandbox execution",
                    );
                }
            };

            let agent_id_owned = agent_id.to_string();
            let goal_owned = goal_description.to_string();
            let context_owned = task_description.to_string();
            let gateway_token_owned = gateway_token.clone();

            info!(
                "executing agent {} (role: {}) in sandbox container",
                agent_id, role
            );

            let result = std::thread::scope(|s| {
                s.spawn(|| {
                    handle.block_on(run_sandbox_agent_execution(
                        mgr,
                        role,
                        &agent_id_owned,
                        &goal_owned,
                        &context_owned,
                        &gateway_token_owned,
                        &system_prompt,
                        model_label.as_deref(),
                        thread_id,
                        max_iterations,
                    ))
                })
                .join()
                .expect("sandbox thread panicked")
            });

            return match result {
                Ok(res) => {
                    let artifacts = self.take_bridge_session_artifacts(&gateway_token);
                    if res.exit_code == 0 {
                        AgentExecutionResult {
                            output: res.stdout,
                            status: "completed".to_string(),
                            pending_question: artifacts.pending_question,
                            pending_group: None,
                            pending_plan: artifacts.pending_plan,
                            pending_auth_request: artifacts.pending_auth_request,
                            context_packet: artifacts.context_packet,
                            checkpoint_artifact: artifacts.checkpoint_artifact,
                        }
                    } else {
                        AgentExecutionResult::failed(format!(
                            "Sandbox execution failed (exit {}):\nSTDOUT: {}\nSTDERR: {}",
                            res.exit_code, res.stdout, res.stderr
                        ))
                    }
                }
                Err(e) => AgentExecutionResult::failed(format!("Sandbox execution error: {e}")),
            };
        }

        // --- Phase 1: Local Process Execution (Fallback) ---
        let runner_bin = self
            .repo_root
            .join("submodules/runtime/target/release/symbiotic-agent-runner");
        // Fallback to debug if release doesn't exist (for development)
        let runner_bin = if runner_bin.exists() {
            runner_bin
        } else {
            self.repo_root
                .join("submodules/runtime/target/debug/symbiotic-agent-runner")
        };

        let mut cmd = std::process::Command::new(&runner_bin);
        cmd.arg("--socket")
            .arg(socket_path)
            .arg("--gateway-token")
            .arg(&gateway_token)
            .arg("--goal")
            .arg(goal_description)
            .arg("--context")
            .arg(task_description)
            .arg("--agent-id")
            .arg(agent_id)
            .arg("--workspace")
            .arg(self.goals_dir.join("workspace").join(agent_id));

        // Resolve system prompt from role config and pass as arg
        if !system_prompt.is_empty() {
            cmd.arg("--system-prompt").arg(&system_prompt);
        }
        cmd.arg("--role")
            .arg(role)
            .arg("--sandbox-type")
            .arg(if self.sandbox_manager.is_some() {
                "vm_sandbox"
            } else {
                "local_process"
            })
            .arg("--max-iterations")
            .arg(max_iterations.to_string());
        if let Some(model_label) = model_label.as_deref() {
            cmd.arg("--model-label").arg(model_label);
        }
        if let Some(thread_id) = thread_id.filter(|value| !value.is_empty()) {
            cmd.arg("--thread-id").arg(thread_id);
        }

        info!(
            "spawning decoupled agent-runner process (agent_id={}, role={}): {:?}",
            agent_id,
            role,
            runner_bin.display()
        );

        let output = match cmd.output() {
            Ok(out) => out,
            Err(e) => {
                return AgentExecutionResult::failed(format!("Failed to spawn agent-runner: {e}"));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let artifacts = self.take_bridge_session_artifacts(&gateway_token);

        if output.status.success() {
            AgentExecutionResult {
                output: stdout,
                status: "completed".to_string(),
                pending_question: artifacts.pending_question,
                pending_group: None,
                pending_plan: artifacts.pending_plan,
                pending_auth_request: artifacts.pending_auth_request,
                context_packet: artifacts.context_packet,
                checkpoint_artifact: artifacts.checkpoint_artifact,
            }
        } else {
            AgentExecutionResult::failed(format!(
                "Agent runner failed (exit {}):\nSTDOUT: {}\nSTDERR: {}",
                output.status.code().unwrap_or(-1),
                stdout,
                stderr
            ))
        }
    }

    #[cfg(test)]
    fn execute_runner_inprocess(
        &self,
        role: &str,
        task_description: &str,
        goal_description: &str,
        agent_id: &str,
        socket_path: &str,
        gateway_token: &str,
        system_prompt: &str,
        model_label: Option<&str>,
        thread_id: Option<&str>,
        max_iterations: usize,
    ) -> AgentExecutionResult {
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            Err(_) => {
                return AgentExecutionResult::failed(
                    "Failed to get tokio runtime for in-process runner execution",
                );
            }
        };

        let bridge = match handle.block_on(symbiotic_agent_runner::BridgeClient::connect_socket(
            std::path::Path::new(socket_path),
            agent_id.to_string(),
            gateway_token.to_string(),
            Some(symbiotic_agent_runner::BridgeRuntimeProfile {
                role: Some(role.to_string()),
                sandbox_type: "in_process".to_string(),
                model_label: model_label.map(str::to_string),
                max_iterations: Some(max_iterations as u32),
                thread_id: thread_id.map(str::to_string),
            }),
        )) {
            Ok(bridge) => bridge,
            Err(e) => {
                return AgentExecutionResult::failed(format!(
                    "Failed to connect in-process runner bridge: {e}"
                ));
            }
        };

        let config = symbiotic_agent_runner::RunnerSessionConfig {
            goal: Some(goal_description.to_string()),
            context: Some(task_description.to_string()),
            agent_id: Some(agent_id.to_string()),
            workspace: self.goals_dir.join("workspace").join(agent_id),
            system_prompt: if system_prompt.is_empty() {
                None
            } else {
                Some(system_prompt.to_string())
            },
            role: Some(role.to_string()),
            sandbox_type: Some("in_process".to_string()),
            model_label: model_label.map(str::to_string),
            thread_id: thread_id.map(str::to_string),
            max_iterations,
            ci_check: None,
            review_pr: false,
            branch: None,
            base_branch: "main".to_string(),
        };

        let result = handle.block_on(symbiotic_agent_runner::run_with_bridge(config, bridge));
        let artifacts = self.take_bridge_session_artifacts(gateway_token);

        match result {
            Ok(Some(result)) => AgentExecutionResult {
                output: result.output,
                status: "completed".to_string(),
                pending_question: artifacts.pending_question,
                pending_group: None,
                pending_plan: artifacts.pending_plan,
                pending_auth_request: artifacts.pending_auth_request,
                context_packet: artifacts.context_packet,
                checkpoint_artifact: artifacts.checkpoint_artifact,
            },
            Ok(None) => {
                AgentExecutionResult::failed("Runner completed without a direct execution result")
            }
            Err(e) => {
                AgentExecutionResult::failed(format!("In-process runner execution failed: {e}"))
            }
        }
    }

    fn take_bridge_session_artifacts(
        &self,
        gateway_token: &str,
    ) -> crate::bridge_interactions::BridgeSessionArtifacts {
        self.bridge_session_store
            .lock()
            .map(|mut store| store.take_artifacts(gateway_token))
            .unwrap_or_default()
    }

    fn issue_bridge_session_token(
        &self,
        agent_id: &str,
        goal_scope: Option<&str>,
    ) -> Result<String> {
        let broker = self
            .broker
            .as_ref()
            .ok_or_else(|| anyhow!("runner backend requires capability broker"))?;
        let mut guard = broker
            .lock()
            .map_err(|_| anyhow!("access broker lock poisoned"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let token_id = format!(
            "bridge_{:x}",
            crate::events::simple_hash(&format!(
                "{}:{}:{}:{nonce}",
                agent_id,
                goal_scope.unwrap_or("global"),
                now
            ))
        );
        guard.issue_token(symbiotic_trust::CapabilityToken {
            token_id: token_id.clone(),
            subject: agent_id.to_string(),
            trust_level: symbiotic_trust::AgentTrustLevel::ReadOnly,
            scopes: ["bridge.connect".to_string(), "llm.chat".to_string()]
                .into_iter()
                .collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: goal_scope.map(str::to_string),
        });
        Ok(token_id)
    }
}

/// Dispatch a task to a real agent provider.
///
/// Runs the full submit → poll → result lifecycle synchronously via
/// `tokio::runtime::Handle::block_on`.
fn dispatch_to_provider(
    role: &str,
    request: &symbiotic_providers::types::TaskRequest,
) -> Result<(String, String)> {
    use symbiotic_providers::{ClaudeCodeProvider, CodexProvider, ProviderAuth};

    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow!("no tokio runtime for provider dispatch"))?;

    // Enforce role-to-provider contract:
    // - coder => Codex CLI (OPENAI_API_KEY)
    // - planner/reviewer/security => Claude Code CLI (ANTHROPIC_API_KEY)
    let result = std::thread::scope(|_| {
        handle.block_on(async {
            if role == "coder" {
                let key = std::env::var("OPENAI_API_KEY")
                    .map_err(|_| anyhow!("OPENAI_API_KEY is required for coder role"))?;
                let provider = CodexProvider::new(ProviderAuth::ApiKey(key));
                run_agent_lifecycle(&provider, request).await
            } else {
                let key = std::env::var("ANTHROPIC_API_KEY")
                    .map_err(|_| anyhow!("ANTHROPIC_API_KEY is required for role={role}"))?;
                let provider = ClaudeCodeProvider::new(ProviderAuth::ApiKey(key));
                run_agent_lifecycle(&provider, request).await
            }
        })
    });

    result
}

/// Run the submit → poll → result lifecycle for an agent provider.
async fn run_agent_lifecycle(
    provider: &(impl symbiotic_providers::AgentProvider + Sync),
    request: &symbiotic_providers::types::TaskRequest,
) -> Result<(String, String)> {
    let session = provider
        .submit_task(request)
        .await
        .map_err(|e| anyhow!("provider submit failed: {e}"))?;

    // Poll until completion (the provider handles timeout internally).
    let mut status = provider
        .poll_status(&session.session_id)
        .await
        .map_err(|e| anyhow!("provider poll failed: {e}"))?;

    let poll_interval = std::time::Duration::from_secs(2);
    let max_polls = 300; // 10 min at 2s intervals
    let mut polls = 0;

    while matches!(
        status,
        symbiotic_providers::types::TaskStatus::Queued
            | symbiotic_providers::types::TaskStatus::Running
    ) && polls < max_polls
    {
        tokio::time::sleep(poll_interval).await;
        status = provider
            .poll_status(&session.session_id)
            .await
            .map_err(|e| anyhow!("provider poll failed: {e}"))?;
        polls += 1;
    }

    let result = provider
        .get_result(&session.session_id)
        .await
        .map_err(|e| anyhow!("provider get_result failed: {e}"))?;

    let status_str = match result.status {
        symbiotic_providers::types::TaskStatus::Completed => "completed",
        symbiotic_providers::types::TaskStatus::Failed => "failed",
        symbiotic_providers::types::TaskStatus::TimedOut => "failed",
        symbiotic_providers::types::TaskStatus::Cancelled => "failed",
        _ => "failed",
    };

    // Log token usage if available.
    if let Some(tokens_in) = result.total_input_tokens {
        info!(
            "agent {} tokens: input={} output={} cost=${:.4}",
            session.session_id,
            tokens_in,
            result.total_output_tokens.unwrap_or(0),
            result.cost_usd.unwrap_or(0.0)
        );
    }

    Ok((result.output, status_str.to_string()))
}

/// Executor for `goal.report` steps.
///
/// Aggregates outputs from all previous steps and produces a final summary
/// report. Persists the report artifact under the goal directory.
#[allow(dead_code)] // Wired by goal runner in WS3
pub(crate) struct GoalReportExecutor {
    /// Directory for per-goal artifacts.
    pub(crate) goals_dir: PathBuf,
}

impl StepExecutor for GoalReportExecutor {
    fn execute(
        &self,
        step: &symbiotic_workflows::WorkflowStep,
        ctx: &WorkflowContext,
    ) -> Result<StepResult> {
        info!(
            "goal.report step_id={} workflow_id={} run_id={}",
            step.id, ctx.workflow_id, ctx.run_id
        );

        let mut outputs = HashMap::new();

        // Collect all agent step results from the context.
        let mut report_lines = vec![
            format!("# Self-Improve Report"),
            format!(""),
            format!("**Run ID:** {}", ctx.run_id),
            format!("**Workflow:** {}", ctx.workflow_id),
            String::new(),
            "## Steps Completed".to_string(),
            String::new(),
        ];

        // Enumerate agent results from context (keyed by step_id_status pattern).
        let mut step_count = 0u32;
        for key in ctx.outputs.keys() {
            if key.ends_with("_status")
                && ctx.outputs.get(key).map(|v| v.as_str()) == Some("completed")
            {
                let step_name = key.trim_end_matches("_status");
                let role = ctx
                    .outputs
                    .get(&format!("{step_name}_role"))
                    .cloned()
                    .unwrap_or_default();
                let agent_id = ctx
                    .outputs
                    .get(&format!("{step_name}_agent_id"))
                    .cloned()
                    .unwrap_or_default();
                if !role.is_empty() {
                    report_lines.push(format!("- **{role}** (agent: {agent_id}): completed"));
                    step_count += 1;
                }
            }
        }

        if step_count == 0 {
            report_lines.push("- No agent steps recorded.".to_string());
        }

        report_lines.push(String::new());
        report_lines.push("## Artifacts".to_string());
        report_lines.push(String::new());
        for key in ctx.outputs.keys() {
            if key.ends_with("_artifact") {
                if let Some(path) = ctx.outputs.get(key) {
                    report_lines.push(format!("- `{key}`: {path}"));
                }
            }
        }

        let report_text = report_lines.join("\n");

        // Persist report.
        let goal_dir = self.goals_dir.join(&ctx.run_id);
        let _ = fs::create_dir_all(&goal_dir);
        let report_file = goal_dir.join("report.md");
        if let Err(e) = fs::write(&report_file, &report_text) {
            return Ok(StepResult {
                step_id: step.id.clone(),
                status: StepStatus::Failed,
                outputs,
                error: Some(format!("failed to write report: {e}")),
            });
        }

        outputs.insert(
            "report_artifact".to_string(),
            report_file.to_string_lossy().to_string(),
        );
        outputs.insert("report_status".to_string(), "completed".to_string());
        outputs.insert("agent_steps_completed".to_string(), step_count.to_string());

        Ok(StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            outputs,
            error: None,
        })
    }
}

pub(crate) fn summarize_items(items: &[IntakeItemResult]) -> IntakeSummary {
    let mut summary = IntakeSummary {
        total: items.len(),
        ..IntakeSummary::default()
    };
    for item in items {
        match item.status {
            IntakeStatus::Ingested => summary.ingested += 1,
            IntakeStatus::Duplicate => summary.duplicates += 1,
            IntakeStatus::Blocked => summary.blocked += 1,
            IntakeStatus::Invalid => summary.invalid += 1,
            IntakeStatus::SecureRouted => summary.secure_routed += 1,
            IntakeStatus::FetchFailed
            | IntakeStatus::ParseFailed
            | IntakeStatus::StoreFailed
            | IntakeStatus::QueueFailed
            | IntakeStatus::SensitivePendingApproval => summary.failed += 1,
        }
        if item.review_queued {
            summary.review_queued += 1;
        }
    }
    summary
}
