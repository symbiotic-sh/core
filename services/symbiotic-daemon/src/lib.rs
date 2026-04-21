use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use credential_gateway::{
    CredentialGateway, GatewayConfig as CredentialGatewayConfig, GoalScopedVault,
    StaticThreatChecker,
};
use symbiotic_agent_config::RoleRegistry;
use symbiotic_agents::SecureAgentFramework;
use symbiotic_archive::FileArchiveStore;
use symbiotic_context::chunking::ChunkConfig;
use symbiotic_context::embedding::{
    EmbedError, EmbedResult as ContextEmbedResult, OllamaProvider, OpenAiProvider,
};
use symbiotic_context::graph::BfsGraphRetriever;
use symbiotic_context::intake_embeddings::{
    EmbedRouter, EmbeddingProcessorConfig, IntakeEmbeddingProcessor,
};
use symbiotic_context::pending_embeddings::PendingChunkStore;
use symbiotic_context::somatic::{SomaticIndex, SqliteSomaticStore};
use symbiotic_context::vector_index::VectorIndex;
use symbiotic_context::RecallGateway;
use symbiotic_core::intake::{
    normalize_url, IntakeBatchResult, IntakeItemResult, IntakeKind, IntakeRequest, IntakeRoute,
    IntakeSource, IntakeStatus,
};
pub(crate) use symbiotic_core::{harden_dir_permissions, harden_file_permissions};
use symbiotic_domains::DomainQueueStore;
use symbiotic_intake::{IntakePipeline, IntakePolicy};
use symbiotic_matrix::events::MatrixEventEnvelope;
use symbiotic_matrix::intake::{IntakeExecutor, IntakeMessageHandler, IntakeReply};
use symbiotic_memory::vault_indexer::VaultIndexer;
use symbiotic_memory::vault_watcher::{self, VaultWatcherHandle};
use symbiotic_memory::vault_writer::VaultWriter;
use symbiotic_memory::{SqliteGraphStore, SqliteMemoryStore};
use symbiotic_providers::{
    AnthropicProvider, CapabilitySet, ClaudeCodeCompletionProvider, CodexCompletionProvider,
    GenericOpenAiCompatProvider, OllamaCompletionProvider, OpenAiCompletionProvider, ProviderAuth,
    ProviderCapability, ProviderRegistry, ProviderRouter, RegisteredProvider,
};
use symbiotic_queue::{
    now_unix, EnqueueRequest, FailOutcome, FileQueueStore, JobStatus, QueueBackend, QueueJob,
};
use symbiotic_review::{FileReviewStore, ReviewEngine, ReviewRequest};
use symbiotic_trust::AccessBroker;
use symbiotic_vault_store::keys::{Identity as AgeIdentity, Recipient as AgeRecipient};
use symbiotic_vault_store::{BlobCategory, BlobMetadata, BlobStore};
use tracing::warn;
use url::Url;

// --- Existing modules ---
pub mod auth;
pub mod auth_approval_policies;
pub mod auth_job_coordinator;
pub mod auth_orchestrator;
pub mod bootstrap;
pub mod bridge_interactions;
pub mod control_plane;
pub mod goal_state;
pub mod http_api;
pub mod push;
pub mod push_client;
pub mod push_dispatcher;
pub mod recall_probes;
pub mod routing;
pub mod secrets;
pub mod tunnel;
pub mod workers;
pub mod x_intake;

// --- New modules (refactored from lib.rs) ---
pub mod agent_runtime_status;
pub mod agents;
pub(crate) mod approval_gate;
pub mod archeology_bundle;
pub mod archeology_dispatch;
pub mod archeology_notify;
pub mod archeology_sandbox;
pub mod auth_jobs;
pub mod auto_promotion;
pub mod availability;
pub mod brain_bootstrap;
pub mod command_runner;
pub mod commands;
pub mod config;
pub mod credential_validator;
pub mod declared_task_policy;
pub mod dispatch_ops;
pub mod entity_profiles;
pub mod events;
pub mod firewall_sink;
pub mod goal_management;
pub mod goal_pipeline;
pub mod goals;
pub mod install;
pub mod llm_audit;
pub mod llm_gateway;
pub mod matrix_gate;
pub mod matrix_interaction;
pub mod matrix_poster;
pub mod memory_docs;
pub mod model_integrity;
pub mod periodic_synthesis;
pub mod policy_scopes;
pub(crate) mod proposals;
pub mod reconciler;
pub(crate) mod repo_capabilities;
pub(crate) mod repo_events;
pub(crate) mod repo_mirror;
pub(crate) mod repo_registry;
pub(crate) mod repo_scheduler;
/// Sub-Goal Dispatcher + ResearchOnly backend (T130 §05). Consumes
/// `goal.unblocked` events and routes them to the appropriate backend per
/// `docs/design/grouped-inquisition.md` §4.1.
pub mod subgoal;
pub mod swarm_server;
pub mod thread_distillery;
pub mod thread_manager;
pub mod thread_observability;
pub mod thread_registry;
pub mod tool_adapters;
pub mod topic_router;
pub mod ux_classifier;

// Re-export public types from modules
// Re-export public types from modules (used by main.rs and integration tests)
pub use events::{
    DaemonEvent, DaemonStatusSnapshot, EventType, RoomCreationRequest, RoutedMatrixEnvelope,
};
pub use goal_state::{AgentState, GoalState};
pub use push::PushDevice;
pub use routing::{RoomRole, RoomRoleMap};
// FeatureLine is defined below (after SymbioticDaemon struct) and re-exported here
// so main.rs can construct transport-level feature lines.

/// (room_id, envelope) item queued by background daemon tasks for matrix delivery.
/// Drained by the pump loop in `main.rs`, which owns `Box<dyn MatrixTransport>`.
/// See T126 §08.b for the decoupling rationale.
pub type MatrixOutboundMessage = (String, symbiotic_matrix::events::MatrixEventEnvelope);
/// Sender half of the matrix outbound channel. Held by `DaemonMatrixPoster`
/// (and future background tasks) to enqueue matrix sends without borrowing
/// the `Box<dyn MatrixTransport>` that lives in `main.rs`.
pub type MatrixOutboundSender = tokio::sync::mpsc::UnboundedSender<MatrixOutboundMessage>;
/// Receiver half of the matrix outbound channel. Returned from
/// `SymbioticDaemon::open` so `main.rs` can hand it to the pump loop.
pub type MatrixOutboundReceiver = tokio::sync::mpsc::UnboundedReceiver<MatrixOutboundMessage>;

/// Request to spawn a goal from a background daemon task. The pump loop in
/// `main.rs` drains the receiver on the main (`!Send`) daemon thread and
/// dispatches each request through `SymbioticDaemon::handle_conflict_goal`.
///
/// Currently the only producer is the repo scheduler (§08.c mirror
/// conflicts). Future producers (e.g. agent lifecycle events) can share this
/// channel without changing the pump loop.
#[derive(Debug, Clone)]
pub struct ConflictGoalRequest {
    /// Human-readable goal description (becomes title + body seed).
    pub description: String,
    /// Matrix room id that the goal should be attributed to (typically the
    /// project's goals room, resolved from `room_roles`).
    pub room_id: String,
    /// Sender identity for the goal, e.g. `"scheduler@symbiotic.sh"`.
    pub sender: String,
}

/// Sender half of the conflict-goal channel. Cloned into each per-repo
/// scheduler task.
pub type ConflictGoalSender = tokio::sync::mpsc::UnboundedSender<ConflictGoalRequest>;
/// Receiver half of the conflict-goal channel. Returned from
/// `SymbioticDaemon::open` so `main.rs` can drain it in the pump loop.
pub type ConflictGoalReceiver = tokio::sync::mpsc::UnboundedReceiver<ConflictGoalRequest>;

// Crate-internal re-exports so that `mod tests` (which uses `use super::*`)
// can reference payload types and codecs that now live in submodules.
#[cfg(test)]
use credential_gateway::CredentialRecord;
#[cfg(test)]
pub(crate) use goals::{decode_workflow_payload, encode_workflow_payload, WorkflowRunPayload};
#[cfg(test)]
use symbiotic_agents::AgentParent;
#[cfg(test)]
use symbiotic_context::ContextRequest;
#[cfg(test)]
use symbiotic_workflows::StepExecutor;
#[cfg(test)]
use symbiotic_workflows::WorkflowStatus;

// Internal re-exports for convenience
use auth::*;
use events::*;
use goal_state::*;
use push::*;
use routing::*;
use workers::*;
use x_intake::*;

/// Adapter wrapping a `symbiotic-context` `EmbeddingProvider` to satisfy
/// the `symbiotic-providers` `ModelProvider` + `EmbeddingProvider` traits.
///
/// This allows existing `OllamaProvider` / `OpenAiProvider` implementations
/// from `symbiotic-context` to be registered in the new `ProviderRegistry`.
struct ContextEmbeddingAdapter {
    inner: Arc<dyn symbiotic_context::embedding::EmbeddingProvider>,
    name: String,
    capabilities: CapabilitySet,
}

impl ContextEmbeddingAdapter {
    fn new(name: &str, inner: Arc<dyn symbiotic_context::embedding::EmbeddingProvider>) -> Self {
        Self {
            inner,
            name: name.to_string(),
            capabilities: CapabilitySet::new(vec![ProviderCapability::Embedding]),
        }
    }
}

impl symbiotic_providers::ModelProvider for ContextEmbeddingAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    fn provider_class(&self) -> symbiotic_providers::types::ProviderClass {
        match self.inner.provider_class() {
            symbiotic_context::embedding::ProviderClass::Local => {
                symbiotic_providers::types::ProviderClass::Local
            }
            symbiotic_context::embedding::ProviderClass::Cloud => {
                symbiotic_providers::types::ProviderClass::Cloud
            }
        }
    }
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }
    fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }
    fn pricing(&self) -> Option<&symbiotic_providers::types::PricingInfo> {
        None
    }
}

#[async_trait]
impl symbiotic_providers::EmbeddingProvider for ContextEmbeddingAdapter {
    async fn embed(
        &self,
        text: &str,
    ) -> Result<symbiotic_providers::EmbedResult, symbiotic_providers::ProviderError> {
        match self.inner.embed(text).await {
            Ok(result) => Ok(symbiotic_providers::EmbedResult {
                embedding: result.embedding,
                model_name: result.model_name,
                dimensions: result.dimensions,
            }),
            Err(symbiotic_context::embedding::EmbedError::Unavailable(msg)) => {
                Err(symbiotic_providers::ProviderError::Unavailable(msg))
            }
            Err(e) => Err(symbiotic_providers::ProviderError::RequestFailed(
                e.to_string(),
            )),
        }
    }
}

/// Adapter that bridges `ProviderRouter` (from `symbiotic-providers`) to the
/// `EmbedRouter` trait expected by `IntakeEmbeddingProcessor`.
///
/// Converts between the two crates' error and result types, preserving the
/// `Unavailable` distinction needed for retry classification.
struct ProviderRouterAdapter {
    router: Arc<ProviderRouter>,
}

#[async_trait]
impl EmbedRouter for ProviderRouterAdapter {
    async fn embed(
        &self,
        text: &str,
        sensitivity: symbiotic_core::Sensitivity,
    ) -> Result<ContextEmbedResult, EmbedError> {
        match self.router.embed(text, sensitivity).await {
            Ok(result) => Ok(ContextEmbedResult {
                embedding: result.embedding,
                model_name: result.model_name,
                dimensions: result.dimensions,
            }),
            Err(symbiotic_providers::ProviderError::Unavailable(msg)) => {
                Err(EmbedError::Unavailable(msg))
            }
            Err(symbiotic_providers::ProviderError::SensitivityViolation {
                sensitivity,
                provider_class,
            }) => Err(EmbedError::SensitivityViolation {
                sensitivity,
                provider_class: match provider_class {
                    symbiotic_providers::types::ProviderClass::Local => {
                        symbiotic_context::embedding::ProviderClass::Local
                    }
                    _ => symbiotic_context::embedding::ProviderClass::Cloud,
                },
            }),
            Err(e) => Err(EmbedError::RequestFailed(e.to_string())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub queue_file: PathBuf,
    pub archive_root: PathBuf,
    pub vault_root: PathBuf,
    pub review_root: PathBuf,
    pub domain_root: PathBuf,
    pub credential_vault_file: PathBuf,
    pub audit_log_file: PathBuf,
    pub capability_tokens_file: PathBuf,
    pub bookmarks_api_file: PathBuf,
    pub bookmarks_browser_file: PathBuf,
    pub x_thread_fallback_file: PathBuf,
    pub x_api_base_url: String,
    pub x_client_id: Option<String>,
    pub x_client_secret: Option<String>,
    pub vps_provision_endpoint: Option<String>,
    pub vps_provision_token: Option<String>,
    pub hcloud_token: Option<String>,
    pub vps_region: String,
    pub vps_size: String,
    pub vps_image: String,
    pub vps_ssh_public_key: Option<String>,
    pub goal_log_file: PathBuf,
    pub goal_state_file: PathBuf,
    pub agent_log_file: PathBuf,
    pub agent_state_file: PathBuf,
    pub push_registry_file: PathBuf,
    pub push_token_key_file: PathBuf,
    pub push_outbox_file: PathBuf,
    pub push_ack_file: PathBuf,
    pub push_telemetry_file: PathBuf,
    pub push_gateway_url: Option<String>,
    pub push_gateway_api_key: Option<String>,
    pub push_apns_gateway_url: Option<String>,
    pub push_apns_gateway_api_key: Option<String>,
    pub push_fcm_gateway_url: Option<String>,
    pub push_fcm_gateway_api_key: Option<String>,
    /// APNs real gateway configuration (optional). When all three fields are set,
    /// a real `ApnsGateway` from `symbiotic-push` is created.
    pub push_apns_team_id: Option<String>,
    pub push_apns_key_id: Option<String>,
    pub push_apns_private_key_pem: Option<String>,
    /// Whether to use the APNs sandbox environment (default: false).
    pub push_apns_sandbox: bool,
    /// FCM real gateway configuration (optional). When all three fields are set,
    /// a real `FcmGateway` from `symbiotic-push` is created.
    pub push_fcm_project_id: Option<String>,
    pub push_fcm_service_account_email: Option<String>,
    pub push_fcm_private_key_pem: Option<String>,
    pub fetch_mode: FetchMode,
    pub worker_id: String,
    pub lease_seconds: u64,
    pub retry_backoff_seconds: u64,
    pub max_matrix_message_bytes: usize,
    pub blocked_hosts: HashSet<String>,
    /// Room-role map for routing by room_id instead of alias patterns.
    pub room_roles: RoomRoleMap,
    /// Sender allowlist: Matrix user IDs permitted to issue commands.
    /// When empty, sender authorization is denied unless
    /// `allow_open_access` is explicitly enabled.
    pub allowed_senders: HashSet<String>,
    /// Explicit development override for command admission when
    /// `allowed_senders` is empty.
    pub allow_open_access: bool,
    /// Data directory for persistent stores (vector index, etc.).
    pub data_dir: PathBuf,
    /// Custom Ollama endpoint URL (default: http://localhost:11434/api/embeddings).
    pub ollama_url: Option<String>,
    /// Ollama chat model for completion requests (default: "qwen3.5").
    /// Only used when `ollama_url` is set.
    /// Env var: `SYMBIOTIC_OLLAMA_CHAT_MODEL`.
    pub ollama_chat_model: Option<String>,
    /// Manifest of known-good local model digests for advisory integrity checks.
    /// Env var: `SYMBIOTIC_MODEL_MANIFEST_FILE`.
    pub model_manifest_file: PathBuf,
    /// OpenAI API key. When set, enables cloud embeddings for shareable content.
    pub openai_api_key: Option<String>,
    /// Anthropic API key. When set, enables cloud completions via Claude models.
    /// Env var: `ANTHROPIC_API_KEY`.
    pub anthropic_api_key: Option<String>,
    /// Claude Code CLI binary path. **Local dev only** — Anthropic Consumer
    /// Terms prohibit subscription-based CLI usage on VPS/server daemons.
    /// For production, use `anthropic_api_key` instead.
    /// Env var: `SYMBIOTIC_CLAUDE_CODE_BINARY`.
    pub claude_code_binary: Option<String>,
    /// Model override for Claude Code CLI completions (e.g. "sonnet", "opus").
    /// Env var: `SYMBIOTIC_CLAUDE_CODE_MODEL`.
    pub claude_code_model: Option<String>,
    /// Codex CLI binary path. When set (or auto-detected on PATH), enables
    /// OpenAI subscription-based completions via `codex exec`. Apache 2.0
    /// licensed — explicitly permitted for daemon/server use.
    /// Env var: `SYMBIOTIC_CODEX_BINARY`.
    pub codex_binary: Option<String>,
    /// Model override for Codex CLI completions (e.g. "o3", "gpt-4.1").
    /// Env var: `SYMBIOTIC_CODEX_MODEL`.
    pub codex_model: Option<String>,
    /// Gemini API key. When set, enables Google Gemini completions via the
    /// OpenAI-compatible endpoint. Free tier: 1,000 req/day.
    /// Env var: `GEMINI_API_KEY`.
    pub gemini_api_key: Option<String>,
    /// Gemini model (default: "gemini-2.5-flash").
    /// Env var: `SYMBIOTIC_GEMINI_MODEL`.
    pub gemini_model: Option<String>,
    /// OpenRouter API key. When set, enables aggregated model access via OpenRouter.
    /// Env var: `OPENROUTER_API_KEY`.
    pub openrouter_api_key: Option<String>,
    /// OpenRouter model (default: "anthropic/claude-sonnet-4").
    /// Env var: `SYMBIOTIC_OPENROUTER_MODEL`.
    pub openrouter_model: Option<String>,
    /// Explicit default completion provider override. When set, bypasses the
    /// auto-detection priority chain and forces this provider as default.
    /// Values: "gemini", "anthropic", "openai", "openrouter", "ollama", "codex", "claude-code".
    /// Env var: `SYMBIOTIC_DEFAULT_PROVIDER`.
    pub default_provider: Option<String>,
    /// Path to the Unix domain socket for the LLM Gateway (agent-runner bridge).
    /// Env var: `SYMBIOTIC_LLM_GATEWAY_SOCKET`.
    pub llm_gateway_socket: Option<String>,
    /// When true, the LLM gateway socket is made world-accessible (`0o666`) for
    /// bind-mounted Sysbox compatibility. Default is false (`0o600`).
    /// Env var: `SYMBIOTIC_LLM_GATEWAY_WORLD_ACCESSIBLE`.
    pub llm_gateway_world_accessible: bool,
    /// Agent execution backend: `react` (default) uses the internal ReAct loop
    /// with ProviderRouter; `cli` dispatches to external CLI agents (Claude Code
    /// / Codex CLI). Env var: `SYMBIOTIC_AGENT_BACKEND`.
    pub agent_backend: AgentBackend,
    /// Test-only runner harness mode for exercising the runner library seam
    /// without shelling out to the external binary.
    #[cfg(test)]
    pub runner_harness_mode: crate::workers::RunnerHarnessMode,
    /// Directory containing agent role TOML files (default: config/agents).
    pub role_dir: PathBuf,
    /// Path to the secrets env file (default: config/.env.secrets).
    pub secrets_file: PathBuf,
    /// Path to the Archive root (filesystem path: `knowledge-base/`, Obsidian vault).
    /// When set, the control-plane reconciler is started as a background task.
    /// Canonical product name: Archive. Env var: `SYMBIOTIC_ARCHIVE_PATH`.
    pub archive_path: Option<PathBuf>,
    /// Root directory for the age-encrypted blob store (Tier 3 / Private data).
    /// Defaults to `data/blob-store`.
    pub blob_store_root: PathBuf,
    /// Path to the age identity (secret key) file used for blob store encryption/decryption.
    /// When set, Private content is routed to the encrypted blob store.
    pub blob_store_key_file: Option<PathBuf>,
    /// When `true` (default), Tier 3 / Private content stays on the phone
    /// and is NOT synced to the VPS via Matrix.  The daemon replaces private
    /// event content with a metadata-only placeholder before sending, and
    /// filters out incoming events tagged as `private` sensitivity.
    pub tier3_phone_only: bool,
    /// Batch size for concurrent embedding generation (default: 8).
    pub embedding_batch_size: usize,
    /// Interval in seconds between periodic retries of pending (failed)
    /// embeddings.  Set to 0 to disable periodic retry.  Default: 300 (5 min).
    pub embedding_retry_interval_secs: u64,
    /// Interval in seconds between periodic active recall probe runs.
    /// Set to 0 to disable scheduled recall probes. Default: 86400 (24h).
    pub recall_probe_interval_secs: u64,
    /// Top-K value used for scheduled active recall probe runs. Default: 10.
    pub recall_probe_top_k: usize,
    /// Maximum subjects per scheduled active recall probe run. Default: 200.
    pub recall_probe_max_subjects_per_run: usize,
    /// Maximum deterministic queries per subject during scheduled probe runs.
    /// Default: 3.
    pub recall_probe_max_queries_per_subject: usize,
    /// Explicit path to SOUL.md identity file.
    /// When set, this takes priority over the `archive_path`-derived path.
    /// Env var: `SYMBIOTIC_SOUL_FILE`.
    ///
    /// Resolution order:
    /// 1. `soul_file` (explicit path from env or config)
    /// 2. `{archive_path}/identity/SOUL.md`
    /// 3. `~/.symbiotic/SOUL.md` (home directory default)
    pub soul_file: Option<PathBuf>,
    /// Path to the push notification preferences TOML file.
    pub push_preferences_file: PathBuf,
    /// Maximum push notifications per device per hour. 0 disables rate limiting.
    pub push_rate_limit_per_hour: u32,
    /// Maximum age in days for stale device pruning. Devices with `last_seen`
    /// older than this are removed during periodic pruning. Default: 90.
    pub push_stale_device_days: u64,
    /// Enable the Process Engineer meta-agent. When enabled, PE spawns after
    /// goal agent completions to analyze execution efficiency and create
    /// methodology improvements. Default: true.
    pub enable_process_engineer: bool,
    /// Path to the Process Engineer graduation database.
    pub pe_graduation_db: PathBuf,
    /// Path to the SQLite somatic marker database.
    /// Stores persistent emotional/temporal markers for memory recall boosting.
    pub somatic_db: PathBuf,
    /// Directory containing auth scripts (Playwright / shell login automation).
    /// When set, the auth sandbox launcher validates scripts from this path at
    /// startup and uses them for one-shot auth worker execution.
    /// Env var: `SYMBIOTIC_AUTH_SCRIPTS_DIR`.
    pub auth_scripts_dir: Option<PathBuf>,
    /// Explicit path to the auth sandbox worker binary. When unset, the daemon
    /// tries a sibling `credential-gateway` binary and then falls back to
    /// resolving `credential-gateway` on PATH.
    /// Env var: `SYMBIOTIC_AUTH_SANDBOX_BIN`.
    pub auth_sandbox_bin: Option<PathBuf>,
    /// Path to the Node.js binary (prepended to PATH for TypeScript script execution).
    /// Defaults to None (uses system `node`/`npx`).
    /// Env var: `SYMBIOTIC_NODE_BIN`.
    pub node_bin: Option<PathBuf>,
    /// Approval TTL for auth jobs before they expire.
    /// Env var: `SYMBIOTIC_AUTH_APPROVAL_TTL_SECS`.
    pub auth_approval_ttl_secs: u64,
    /// Follow-up input TTL for auth jobs awaiting MFA / captcha / passkey input.
    /// Env var: `SYMBIOTIC_AUTH_INPUT_TTL_SECS`.
    pub auth_input_ttl_secs: u64,
    /// Matrix homeserver URL for dynamic room creation (e.g. thread rooms).
    pub matrix_homeserver: Option<String>,
    /// Matrix server name (e.g. "symbiotic.local") for room alias resolution.
    pub matrix_server_name: Option<String>,
    /// Matrix access token for API calls. Obtained during bootstrap.
    pub matrix_access_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FetchMode {
    #[default]
    Auto,
    Curl,
    Stub,
}

/// Agent execution backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentBackend {
    /// Internal ReAct loop using ProviderRouter + built-in tools.
    #[default]
    React,
    /// External CLI agents (Claude Code CLI / Codex CLI via env vars).
    Cli,
    /// Decoupled agent-runner process (Phase 1 Nuclear Split).
    Runner,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            queue_file: PathBuf::from("data/queue/jobs.state"),
            archive_root: PathBuf::from("data/archive"),
            vault_root: PathBuf::from("data/vault"),
            review_root: PathBuf::from("data/review"),
            domain_root: PathBuf::from("domains"),
            credential_vault_file: PathBuf::from("data/vault/credentials.tsv"),
            audit_log_file: PathBuf::from("data/audit/context.log"),
            capability_tokens_file: PathBuf::from("data/audit/capability-tokens.json"),
            bookmarks_api_file: PathBuf::from("data/intake/bookmarks-api.txt"),
            bookmarks_browser_file: PathBuf::from("data/intake/bookmarks-browser.txt"),
            x_thread_fallback_file: PathBuf::from("data/intake/twitter-threads.txt"),
            x_api_base_url: "https://api.twitter.com/2".to_string(),
            x_client_id: None,
            x_client_secret: None,
            vps_provision_endpoint: None,
            vps_provision_token: None,
            hcloud_token: None,
            vps_region: "us-east-1".to_string(),
            vps_size: "shared-cpu-2x".to_string(),
            vps_image: "ubuntu-24-04".to_string(),
            vps_ssh_public_key: None,
            goal_log_file: PathBuf::from("data/goals/runs.log"),
            goal_state_file: PathBuf::from("data/goals/state.tsv"),
            agent_log_file: PathBuf::from("data/agents/lifecycle.log"),
            agent_state_file: PathBuf::from("data/agents/state.tsv"),
            push_registry_file: PathBuf::from("data/push/tokens.tsv"),
            push_token_key_file: PathBuf::from("data/push/token.key"),
            push_outbox_file: PathBuf::from("data/push/outbox.ndjson"),
            push_ack_file: PathBuf::from("data/push/acks.ndjson"),
            push_telemetry_file: PathBuf::from("data/push/delivery.log"),
            push_gateway_url: None,
            push_gateway_api_key: None,
            push_apns_gateway_url: None,
            push_apns_gateway_api_key: None,
            push_fcm_gateway_url: None,
            push_fcm_gateway_api_key: None,
            push_apns_team_id: None,
            push_apns_key_id: None,
            push_apns_private_key_pem: None,
            push_apns_sandbox: false,
            push_fcm_project_id: None,
            push_fcm_service_account_email: None,
            push_fcm_private_key_pem: None,
            fetch_mode: FetchMode::Auto,
            worker_id: "worker-main".to_string(),
            lease_seconds: 60,
            retry_backoff_seconds: 30,
            max_matrix_message_bytes: 32 * 1024,
            blocked_hosts: HashSet::new(),
            room_roles: RoomRoleMap::default(),
            allowed_senders: HashSet::new(),
            allow_open_access: false,
            data_dir: PathBuf::from("data"),
            ollama_url: None,
            ollama_chat_model: None,
            model_manifest_file: PathBuf::from("config/model-manifest.toml"),
            openai_api_key: None,
            anthropic_api_key: None,
            claude_code_binary: None,
            claude_code_model: None,
            codex_binary: None,
            codex_model: None,
            gemini_api_key: None,
            gemini_model: None,
            openrouter_api_key: None,
            openrouter_model: None,
            default_provider: None,
            llm_gateway_socket: None,
            llm_gateway_world_accessible: false,
            agent_backend: AgentBackend::default(),
            #[cfg(test)]
            runner_harness_mode: crate::workers::RunnerHarnessMode::Process,
            role_dir: PathBuf::from("config/agents"),
            secrets_file: PathBuf::from("config/.env.secrets"),
            archive_path: None,
            blob_store_root: PathBuf::from("data/blob-store"),
            blob_store_key_file: None,
            tier3_phone_only: true,
            embedding_batch_size: 8,
            embedding_retry_interval_secs: 300,
            recall_probe_interval_secs: 24 * 3600,
            recall_probe_top_k: 10,
            recall_probe_max_subjects_per_run: 200,
            recall_probe_max_queries_per_subject: 3,
            soul_file: None,
            push_preferences_file: PathBuf::from("data/push/preferences.toml"),
            push_rate_limit_per_hour: 30,
            push_stale_device_days: 90,
            enable_process_engineer: true,
            pe_graduation_db: PathBuf::from("data/agents/pe-graduation.db"),
            somatic_db: PathBuf::from("data/somatic.db"),
            auth_scripts_dir: None,
            auth_sandbox_bin: None,
            node_bin: None,
            auth_approval_ttl_secs: 300,
            auth_input_ttl_secs: 300,
            matrix_homeserver: None,
            matrix_server_name: None,
            matrix_access_token: None,
        }
    }
}

pub struct SymbioticDaemon {
    pub(crate) config: DaemonConfig,
    pub(crate) queue: Arc<dyn QueueBackend>,
    pub(crate) broker: Arc<Mutex<AccessBroker>>,
    pub(crate) bridge_session_store: Arc<Mutex<crate::bridge_interactions::BridgeSessionStore>>,
    pub(crate) bridge_interaction_log_store:
        Arc<Mutex<crate::bridge_interactions::BridgeInteractionLogStore>>,
    pub(crate) agent_runtime_log_store:
        Arc<Mutex<crate::bridge_interactions::AgentRuntimeLogStore>>,
    /// Mutable current runtime status per bridge-run agent.
    pub(crate) agent_runtime_status_store:
        Arc<Mutex<crate::agent_runtime_status::AgentRuntimeStatusStore>>,
    pub(crate) bridge_checkpoint_store:
        Arc<Mutex<crate::bridge_interactions::BridgeCheckpointStore>>,
    pub(crate) auth_jobs: Arc<Mutex<crate::auth_jobs::AuthJobStore>>,
    pub(crate) auth_approval_policies:
        Arc<Mutex<crate::auth_approval_policies::AuthApprovalPolicyStore>>,
    pub(crate) agents: SecureAgentFramework,
    pub(crate) credential_gateway: Arc<CredentialGateway>,
    /// Direct vault access for API credentials (bypasses hostname normalization).
    pub(crate) credential_vault: Arc<GoalScopedVault>,
    /// HTTP credential validator for per-provider API key checks.
    pub(crate) credential_validator: Box<dyn credential_validator::ValidateCredential>,
    pub(crate) workflow_runner: symbiotic_workflows::WorkflowRunner,
    pub(crate) workflow_registry: symbiotic_workflows::WorkflowRegistry,
    pub(crate) bookmarks_client: Arc<dyn BookmarksSyncClient>,
    pub(crate) archive_store: Arc<FileArchiveStore>,
    #[allow(dead_code)]
    pub(crate) vault_store: Arc<FileArchiveStore>,
    pub(crate) review_store: Arc<FileReviewStore>,
    pub(crate) review_engine: ReviewEngine,
    pub(crate) intake_handler: IntakeMessageHandler,
    pub(crate) pipeline: IntakePipeline,
    pub(crate) review_queue_adapter: Arc<QueueReviewAdapter>,
    pub(crate) recall_gateway: Arc<RecallGateway>,
    pub(crate) embedding_processor: Option<IntakeEmbeddingProcessor>,
    // vector_index_path removed — sqlite-vec persists to its own DB file
    pub(crate) push_registry: Arc<PushRegistry>,
    pub(crate) push_provider: Arc<dyn PushProvider>,
    pub(crate) room_roles: RoomRoleMap,
    pub(crate) allowed_senders: HashSet<String>,
    pub(crate) allow_open_access: bool,
    pub(crate) role_registry: Arc<RoleRegistry>,
    /// Identity content loaded from SOUL.md (shared with the reconciler).
    /// Agents read this to inject identity context into their system prompt.
    pub(crate) identity_content: Arc<Mutex<Option<String>>>,
    /// SQLite-backed trust store for persisting capability tokens and audit logs.
    pub(crate) trust_store: Option<Arc<symbiotic_trust::persistence::TrustStore>>,
    /// Provider router for sensitivity-aware AI provider dispatch (completions, embeddings, etc.).
    pub(crate) provider_router: Arc<ProviderRouter>,
    /// Age-encrypted blob store for Tier 3 (Private) data.
    /// When `Some`, Private content is routed here instead of being stored as plain `.md`.
    pub(crate) encrypted_blob_store: Option<BlobStore>,
    /// Age recipient (public key) for encrypting blobs in the blob store.
    pub(crate) blob_recipient: Option<AgeRecipient>,
    /// Shared pending chunk store for retry of failed embeddings.
    pub(crate) pending_embedding_store: Arc<Mutex<PendingChunkStore>>,
    /// GoalProcessManager for validated goal lifecycle management.
    /// Provides state transitions with validation, metrics tracking,
    /// phase advancement, and JSON persistence for crash recovery.
    pub(crate) goal_process_manager: Arc<Mutex<symbiotic_control_plane::GoalProcessManager>>,
    /// Persisted management-layer work items, scope claims, and heartbeats.
    /// Loaded at startup and used for restart-safe lease expiry and future
    /// orchestration ownership checks.
    #[allow(dead_code)]
    pub(crate) management_store: Arc<Mutex<symbiotic_control_plane::ManagementStore>>,
    /// In-memory index of durable per-project repo manifests, loaded from
    /// Archive at startup. Event-driven reload is wired in §05 together with
    /// the mirror scheduler.
    pub(crate) repo_registry: crate::repo_registry::SharedRepoRegistry,
    /// In-memory state machine for repo operations requiring operator approval
    /// (push_external, etc.). Sync mutex — guards are never held across await.
    /// See §07b.i for the state machine, §07b.ii-b for the async wrapper that
    /// opens tickets, §07b.iii-b (this chunk) for the sync dispatcher that
    /// resolves them.
    pub(crate) approval_gate: std::sync::Arc<std::sync::Mutex<crate::approval_gate::ApprovalGate>>,
    /// User-configurable push notification preferences.
    pub(crate) push_preferences: Arc<Mutex<PushPreferences>>,
    /// Per-device rate limiter for push notifications.
    pub(crate) push_rate_limiter: Arc<Mutex<push_dispatcher::PushRateLimiter>>,
    /// Process Engineer graduation store for tracking per-goal-type efficiency.
    pub(crate) graduation_store: Option<Arc<symbiotic_agents::graduation::SqliteGraduationStore>>,
    /// Agent execution monitor for PE to read execution records.
    pub(crate) agent_monitor: Option<Arc<symbiotic_agents::monitoring::SqliteAgentMonitor>>,
    /// Somatic marker index backed by SQLite for persistent emotional/temporal tagging.
    /// Used by the distillery pipeline and recall gateway for somatic-temporal boosting.
    #[allow(dead_code)]
    pub(crate) somatic_index: Arc<Mutex<SomaticIndex>>,
    /// One-shot auth sandbox launcher for browser/protocol authentication.
    /// `None` when no scripts directory is configured (or in test mode).
    pub(crate) auth_engine: Option<credential_gateway::auth_engine::AuthSandboxLauncher>,
    /// Skill registry for auto-loading agent capabilities.
    pub(crate) skill_registry: Arc<Mutex<symbiotic_skills::registry::SkillRegistry>>,
    /// Thread manager for thread lifecycle (creation, archival, routing).
    /// Initialized at startup with persisted registry from `{data_dir}/threads/`.
    pub(crate) thread_manager: Mutex<Option<thread_manager::ThreadManager>>,
    /// Durable per-thread Tier 3 summary store used for truthful Operations Pill updates.
    pub(crate) thread_observability_store:
        Arc<Mutex<thread_observability::ThreadObservabilityStore>>,
    /// Vault indexer for re-indexing entity files when they change on disk.
    /// Shared with the background `VaultWatcher` task.
    #[allow(dead_code)]
    pub(crate) vault_indexer: Arc<tokio::sync::Mutex<VaultIndexer>>,
    /// Handle to stop the background vault watcher on shutdown.
    #[allow(dead_code)]
    pub(crate) vault_watcher_handle: Option<VaultWatcherHandle>,
    /// Writer for surgical Markdown edits (add facts, archive facts, add relationships).
    pub(crate) vault_writer: VaultWriter,
    /// Cooldown tracker for auto-promotion dismissals (24h default).
    pub(crate) promotion_cooldown_tracker: Mutex<auto_promotion::PromotionCooldownTracker>,
    /// Pending thread room creation requests from sync command handlers.
    /// Drained by the async pump loop.
    pub(crate) pending_room_creations: Mutex<Vec<events::RoomCreationRequest>>,
    /// Last completed declared-task policy evaluation timestamp. This is
    /// derived runtime state only and is safe to rebuild from Archive truth.
    pub(crate) declared_task_policy_last_run: Mutex<Option<u64>>,
    /// Shared memory store for entity dedup and other memory operations.
    /// Shares the same SQLite connection as the `VaultIndexer`.
    pub(crate) memory_store: Arc<SqliteMemoryStore>,
    /// Periodic synthesizer for cross-thread knowledge consolidation and entity dedup.
    /// `None` when no archive/knowledge-base path is configured.
    pub(crate) periodic_synthesizer: Option<periodic_synthesis::PeriodicSynthesizer>,
    /// In-memory store for pending self-improvement proposals awaiting user approval.
    pub(crate) proposal_store: proposals::ProposalStore,
    /// Sandbox manager for spawning isolated agent processes (Phase 2).
    pub(crate) sandbox_manager: Option<Arc<Mutex<symbiotic_vm::manager::VmManager>>>,
    /// Sender for outbound matrix events originating from background daemon
    /// tasks (scheduler, future workers). The paired receiver is handed to
    /// `main.rs` at construction and drained by a pump-loop task that owns
    /// the MatrixTransport. See docs/design/repo-manifest.md §07b.iii / §08.
    pub(crate) matrix_outbound_tx: MatrixOutboundSender,
    /// Sender for conflict-goal spawn requests originating from background
    /// daemon tasks (scheduler). The paired receiver is handed to `main.rs`
    /// at construction and drained by the pump loop which calls
    /// `handle_conflict_goal` on the main (`!Send`) daemon thread.
    /// See T126 §08.c.
    pub(crate) conflict_goal_tx: ConflictGoalSender,
    /// Per-daemon `QuestionResolver` tracking every in-flight grouped
    /// inquisition. Populated by the batch-Inquisitor emission path in
    /// `goals.rs` and drained by the `goal.answer` handler in `commands.rs`
    /// (T130 §04a — wiring hookup).
    ///
    /// The resolver owns no durable state: on restart the map starts empty
    /// and any active group must be re-registered by replaying the Matrix
    /// event history (see `question_resolver.rs` module docs).
    pub(crate) question_resolver:
        std::sync::Arc<std::sync::Mutex<crate::goal_pipeline::question_resolver::QuestionResolver>>,

    /// Optional Sub-Goal Dispatcher (T130 §05). Consumes `goal.unblocked`
    /// events and routes them to the appropriate backend
    /// (ResearchOnly is live; other variants stubbed).
    ///
    /// The dispatcher is optional so construction paths that don't need
    /// sub-goal spawning (test fixtures, early boot) can skip the heavy
    /// LLM / Recall dependencies. A daemon that wants sub-goal spawning
    /// calls [`SymbioticDaemon::set_subgoal_dispatcher`] after
    /// construction, or on the way up in `main.rs`.
    pub(crate) subgoal_dispatcher: std::sync::Mutex<Option<crate::subgoal::Dispatcher>>,

    /// Handle to the `agent.execute` workflow executor, retained so the CLI
    /// (`symbiotic agent run`) can drive a one-shot agent goal against the
    /// local Archive without going through the workflow engine or Matrix.
    pub(crate) agent_execute_executor: Arc<crate::workers::AgentExecuteExecutor>,
}

/// A single line in the daemon startup feature summary.
pub struct FeatureLine {
    pub category: &'static str,
    pub enabled: bool,
    pub detail: String,
}

impl std::fmt::Display for FeatureLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let icon = if self.enabled { "\u{2713}" } else { "\u{2717}" };
        write!(f, "  {:<20} {} {}", self.category, icon, self.detail)
    }
}

impl SymbioticDaemon {
    pub fn open(
        config: DaemonConfig,
    ) -> Result<(Self, MatrixOutboundReceiver, ConflictGoalReceiver)> {
        // Matrix outbound channel — background daemon tasks (scheduler, future
        // workers) send (room_id, envelope) items here; `main.rs` drains the
        // receiver in the pump loop that owns `Box<dyn MatrixTransport>`.
        // See T126 §08.b for the decoupling rationale.
        let (matrix_outbound_tx, matrix_outbound_rx) =
            tokio::sync::mpsc::unbounded_channel::<MatrixOutboundMessage>();
        // Conflict-goal channel — scheduler enqueues goal-spawn requests here
        // when mirror conflicts are detected; `main.rs` drains the receiver
        // in the pump loop and dispatches via `handle_conflict_goal` which
        // runs on the main (`!Send`) daemon thread. See T126 §08.c.
        let (conflict_goal_tx, conflict_goal_rx) =
            tokio::sync::mpsc::unbounded_channel::<ConflictGoalRequest>();
        let queue = Arc::new(FileQueueStore::open(&config.queue_file)?);
        let archive_store = Arc::new(FileArchiveStore::open(&config.archive_root)?);
        let repo_registry = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::repo_registry::RepoRegistry::load_from_archive(&config.archive_root)?,
        ));
        let approval_gate = std::sync::Arc::new(std::sync::Mutex::new(
            crate::approval_gate::ApprovalGate::new(),
        ));
        let vault_store = Arc::new(FileArchiveStore::open(&config.vault_root)?);
        let review_store = Arc::new(FileReviewStore::open(&config.review_root)?);
        let domain_store = Arc::new(DomainQueueStore::open(&config.domain_root)?);
        let review_engine = ReviewEngine::default();
        let mut broker = load_access_broker(&config.capability_tokens_file)?;
        let credential_vault = Arc::new(GoalScopedVault::open(&config.credential_vault_file)?);
        let credential_gateway = Arc::new(CredentialGateway::new_scoped(
            CredentialGatewayConfig {
                blocked_targets: config.blocked_hosts.clone(),
                ..CredentialGatewayConfig::default()
            },
            Arc::new(StaticThreatChecker::new(config.blocked_hosts.clone())),
            credential_vault.clone(),
        ));
        let queue_executor = Arc::new(QueueIntakeExecutor {
            queue: queue.clone(),
        });
        let bookmarks_client = Arc::new(HybridBookmarksSyncClient::new(
            config.bookmarks_api_file.clone(),
            config.bookmarks_browser_file.clone(),
            credential_vault.clone(),
            config.x_api_base_url.clone(),
        ));
        let intake_store = Arc::new(ArchiveIntakeStore {
            archive_store: archive_store.clone(),
            vault_store: vault_store.clone(),
        });
        let workflow_registry = symbiotic_workflows::WorkflowRegistry::with_mvp_templates();
        // Build the workflow runner with most executors now. The `agent.execute`
        // executor is added later (after provider_router + role_registry + identity
        // are initialized).
        let workflow_runner = symbiotic_workflows::WorkflowRunner::new()
            .with_executor("intake.normalize", Arc::new(IntakeNormalizeExecutor))
            .with_executor(
                "intake.dedupe",
                Arc::new(IntakeDedupeExecutor {
                    store: intake_store.clone(),
                }),
            )
            .with_executor(
                "ingest.fetch",
                Arc::new(IngestFetchExecutor {
                    queue: queue.clone(),
                }),
            )
            .with_executor("archive.store", Arc::new(ArchiveStoreExecutor))
            .with_executor(
                "archive.review.enqueue",
                Arc::new(ArchiveReviewEnqueueExecutor {
                    queue: queue.clone(),
                }),
            )
            .with_executor(
                "domain.queue.pull",
                Arc::new(DomainQueueExecutor {
                    store: domain_store.clone(),
                }),
            )
            .with_executor(
                "goal.plan",
                Arc::new(GoalPlanExecutor {
                    goals_dir: config
                        .goal_log_file
                        .parent()
                        .unwrap_or(Path::new("data/goals"))
                        .to_path_buf(),
                }),
            )
            .with_executor(
                "goal.report",
                Arc::new(GoalReportExecutor {
                    goals_dir: config
                        .goal_log_file
                        .parent()
                        .unwrap_or(Path::new("data/goals"))
                        .to_path_buf(),
                }),
            );

        let review_queue = Arc::new(QueueReviewAdapter {
            queue: queue.clone(),
            max_attempts: 3,
            current_run_id: Mutex::new(None),
        });

        let pipeline = IntakePipeline::new(
            Arc::new(DaemonFetcher::new(
                config.fetch_mode,
                credential_vault.clone(),
                config.x_api_base_url.clone(),
                config.x_thread_fallback_file.clone(),
            )),
            intake_store,
            review_queue.clone(),
            Arc::new(DaemonSensitivityClassifier),
            IntakePolicy {
                blocked_hosts: config.blocked_hosts.clone(),
            },
        );
        // T132 §05: wire the JSONL quarantine sink. Matrix alerts default to
        // a no-op channel here; the outbound dispatcher already surfaces
        // related events through other paths.
        // Locate the kb root from `archive_root` (parent dir).
        let kb_root_for_audit = config
            .archive_root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| config.archive_root.clone());
        let pipeline =
            match crate::firewall_sink::JsonlQuarantineSink::audit_only(&kb_root_for_audit) {
                Ok(sink) => pipeline.with_quarantine_sink(Arc::new(sink)),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "firewall_sink: failed to open quarantine log; using noop sink"
                    );
                    pipeline
                }
            };

        // --- Embedding infrastructure (ProviderRouter from symbiotic-providers) ---
        let vector_index_path = VectorIndex::default_path(&config.data_dir);
        let vector_index = VectorIndex::open(
            &vector_index_path,
            symbiotic_context::vector_index::DEFAULT_EMBEDDING_DIM,
        )
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to open vector index, using in-memory fallback");
            VectorIndex::open_in_memory(symbiotic_context::vector_index::DEFAULT_EMBEDDING_DIM)
                .expect("in-memory vector index must succeed")
        });
        let shared_vector_index = Arc::new(Mutex::new(vector_index));

        let local_provider: Arc<dyn symbiotic_context::embedding::EmbeddingProvider> =
            if let Some(ref url) = config.ollama_url {
                Arc::new(OllamaProvider::with_config(
                    url.clone(),
                    "nomic-embed-text".to_string(),
                ))
            } else {
                Arc::new(OllamaProvider::new())
            };

        let mut provider_registry = ProviderRegistry::new();

        // Register local (Ollama) embedding provider.
        let ollama_adapter = Arc::new(ContextEmbeddingAdapter::new("ollama", local_provider));
        provider_registry.register(RegisteredProvider {
            base: ollama_adapter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
            completion: None,
            embedding: Some(ollama_adapter as Arc<dyn symbiotic_providers::EmbeddingProvider>),
            image: None,
            video: None,
            agent: None,
        });

        // Register cloud (OpenAI) embedding provider if API key is configured.
        if let Some(ref api_key) = config.openai_api_key {
            let cloud_provider: Arc<dyn symbiotic_context::embedding::EmbeddingProvider> =
                Arc::new(OpenAiProvider::new(api_key.clone()));
            let openai_adapter = Arc::new(ContextEmbeddingAdapter::new("openai", cloud_provider));
            provider_registry.register(RegisteredProvider {
                base: openai_adapter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                completion: None,
                embedding: Some(openai_adapter as Arc<dyn symbiotic_providers::EmbeddingProvider>),
                image: None,
                video: None,
                agent: None,
            });
            // Prefer cloud for shareable content (ProviderRouter handles sensitivity).
            let _ = provider_registry.set_default(ProviderCapability::Embedding, "openai");
        } else {
            let _ = provider_registry.set_default(ProviderCapability::Embedding, "ollama");
        }

        // --- Completion providers (for agent ReAct loop) ---

        // Register Ollama completion provider (local) when URL is configured.
        if let Some(ref url) = config.ollama_url {
            let model = config
                .ollama_chat_model
                .clone()
                .unwrap_or_else(|| "qwen3.5".into());
            let ollama_completion = Arc::new(OllamaCompletionProvider::new(url.clone(), model));
            provider_registry.register(RegisteredProvider {
                base: ollama_completion.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                completion: Some(
                    ollama_completion as Arc<dyn symbiotic_providers::CompletionProvider>,
                ),
                embedding: None,
                image: None,
                video: None,
                agent: None,
            });
        }

        // Register Claude Code CLI completion provider (subscription-based).
        // Uses the user's existing Pro/Max plan — no API key needed.
        let claude_code_registered = if let Some(ref binary) = config.claude_code_binary {
            let provider =
                ClaudeCodeCompletionProvider::new(binary.clone(), config.claude_code_model.clone());
            if provider.is_available() {
                tracing::info!(
                    "claude-code CLI detected at '{}', registering as completion provider",
                    binary
                );
                let provider = Arc::new(provider);
                provider_registry.register(RegisteredProvider {
                    base: provider.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                    completion: Some(provider as Arc<dyn symbiotic_providers::CompletionProvider>),
                    embedding: None,
                    image: None,
                    video: None,
                    agent: None,
                });
                true
            } else {
                tracing::warn!(
                    "claude-code CLI configured at '{}' but not found on PATH",
                    binary
                );
                false
            }
        } else if config.agent_backend == AgentBackend::Cli {
            // Auto-detect: check if `claude` is on PATH (only for Cli backend —
            // CLI providers produce errors when used as ReAct completion fallbacks).
            let auto_provider = ClaudeCodeCompletionProvider::new(
                "claude".to_string(),
                config.claude_code_model.clone(),
            );
            if auto_provider.is_available() {
                tracing::info!(
                    "claude-code CLI auto-detected on PATH, registering as completion provider"
                );
                let provider = Arc::new(auto_provider);
                provider_registry.register(RegisteredProvider {
                    base: provider.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                    completion: Some(provider as Arc<dyn symbiotic_providers::CompletionProvider>),
                    embedding: None,
                    image: None,
                    video: None,
                    agent: None,
                });
                true
            } else {
                false
            }
        } else {
            false
        };

        // Register Anthropic completion provider (cloud) when API key is present.
        if let Some(ref key) = config.anthropic_api_key {
            let anthropic = Arc::new(AnthropicProvider::new(
                ProviderAuth::ApiKey(key.clone()),
                "claude-sonnet-4-20250514".to_string(),
            ));
            provider_registry.register(RegisteredProvider {
                base: anthropic.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                completion: Some(anthropic as Arc<dyn symbiotic_providers::CompletionProvider>),
                embedding: None,
                image: None,
                video: None,
                agent: None,
            });
        }

        // Register OpenAI completion provider (cloud) when API key is present.
        if let Some(ref key) = config.openai_api_key {
            let openai_completion = Arc::new(OpenAiCompletionProvider::new(
                ProviderAuth::ApiKey(key.clone()),
                "gpt-4o-mini".to_string(),
            ));
            provider_registry.register(RegisteredProvider {
                base: openai_completion.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                completion: Some(
                    openai_completion as Arc<dyn symbiotic_providers::CompletionProvider>,
                ),
                embedding: None,
                image: None,
                video: None,
                agent: None,
            });
        }

        // Register Codex CLI completion provider (OpenAI subscription, Apache 2.0).
        let codex_registered = if let Some(ref binary) = config.codex_binary {
            let provider = CodexCompletionProvider::new(binary.clone(), config.codex_model.clone());
            if provider.is_available() {
                tracing::info!(
                    "codex CLI detected at '{}', registering as completion provider",
                    binary
                );
                let provider = Arc::new(provider);
                provider_registry.register(RegisteredProvider {
                    base: provider.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                    completion: Some(provider as Arc<dyn symbiotic_providers::CompletionProvider>),
                    embedding: None,
                    image: None,
                    video: None,
                    agent: None,
                });
                true
            } else {
                tracing::warn!("codex CLI configured at '{}' but not found on PATH", binary);
                false
            }
        } else if config.agent_backend == AgentBackend::Cli {
            // Auto-detect: check if `codex` is on PATH (only for Cli backend —
            // CLI providers produce errors when used as ReAct completion fallbacks).
            let auto_provider =
                CodexCompletionProvider::new("codex".to_string(), config.codex_model.clone());
            if auto_provider.is_available() {
                tracing::info!(
                    "codex CLI auto-detected on PATH, registering as completion provider"
                );
                let provider = Arc::new(auto_provider);
                provider_registry.register(RegisteredProvider {
                    base: provider.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                    completion: Some(provider as Arc<dyn symbiotic_providers::CompletionProvider>),
                    embedding: None,
                    image: None,
                    video: None,
                    agent: None,
                });
                true
            } else {
                false
            }
        } else {
            false
        };

        // Register Gemini completion provider (Google API key) when key is present.
        let gemini_registered = if let Some(ref key) = config.gemini_api_key {
            let model = config
                .gemini_model
                .clone()
                .unwrap_or_else(|| "gemini-2.5-flash".to_string());
            tracing::info!("registering Gemini completion provider (model: {})", model);
            let gemini = Arc::new(GenericOpenAiCompatProvider::gemini(
                ProviderAuth::ApiKey(key.clone()),
                model,
            ));
            provider_registry.register(RegisteredProvider {
                base: gemini.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                completion: Some(gemini as Arc<dyn symbiotic_providers::CompletionProvider>),
                embedding: None,
                image: None,
                video: None,
                agent: None,
            });
            true
        } else {
            false
        };

        // Register OpenRouter completion provider when key is present.
        let openrouter_registered = if let Some(ref key) = config.openrouter_api_key {
            let model = config
                .openrouter_model
                .clone()
                .unwrap_or_else(|| "anthropic/claude-sonnet-4".to_string());
            tracing::info!(
                "registering OpenRouter completion provider (model: {})",
                model
            );
            let openrouter = Arc::new(GenericOpenAiCompatProvider::openrouter(
                ProviderAuth::ApiKey(key.clone()),
                model,
            ));
            provider_registry.register(RegisteredProvider {
                base: openrouter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
                completion: Some(openrouter as Arc<dyn symbiotic_providers::CompletionProvider>),
                embedding: None,
                image: None,
                video: None,
                agent: None,
            });
            true
        } else {
            false
        };

        // Set default completion provider.
        // If SYMBIOTIC_DEFAULT_PROVIDER is set, use it directly (bypass priority chain).
        // Otherwise, fall back to auto-detection priority:
        //   Codex CLI > Claude Code (local dev) > Anthropic API > Gemini API > OpenAI > Ollama.
        if let Some(ref forced) = config.default_provider {
            tracing::info!("forcing default completion provider: {}", forced);
            let _ = provider_registry.set_default(ProviderCapability::Completion, forced);
        } else if codex_registered {
            let _ = provider_registry.set_default(ProviderCapability::Completion, "codex");
        } else if claude_code_registered {
            let _ = provider_registry.set_default(ProviderCapability::Completion, "claude-code");
        } else if config.anthropic_api_key.is_some() {
            let _ = provider_registry.set_default(ProviderCapability::Completion, "anthropic");
        } else if gemini_registered {
            let _ = provider_registry.set_default(ProviderCapability::Completion, "gemini");
        } else if openrouter_registered {
            let _ = provider_registry.set_default(ProviderCapability::Completion, "openrouter");
        } else if config.openai_api_key.is_some() {
            let _ = provider_registry.set_default(ProviderCapability::Completion, "openai");
        } else if config.ollama_url.is_some() {
            let _ = provider_registry.set_default(ProviderCapability::Completion, "ollama");
        }

        let provider_router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            provider_registry,
        ))));
        let embed_router: Arc<dyn EmbedRouter> = Arc::new(ProviderRouterAdapter {
            router: Arc::clone(&provider_router),
        });

        // --- Pending embedding store ---
        let pending_store_path = PendingChunkStore::default_path(&config.data_dir);
        let pending_store = match PendingChunkStore::open(&pending_store_path) {
            Ok(store) => {
                if !store.is_empty() {
                    tracing::info!(
                        pending_count = store.len(),
                        "pending_embeddings: loaded existing pending chunks"
                    );
                }
                store
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "pending_embeddings: failed to load pending store, starting empty"
                );
                PendingChunkStore::new_in_memory()
            }
        };
        let shared_pending_store = Arc::new(Mutex::new(pending_store));

        let embed_config = EmbeddingProcessorConfig {
            batch_size: config.embedding_batch_size,
        };

        let embedding_processor =
            match symbiotic_context::chunking::Chunker::new(ChunkConfig::default()) {
                Ok(chunker) => Some(
                    IntakeEmbeddingProcessor::with_config(
                        chunker,
                        embed_router,
                        shared_vector_index.clone(),
                        embed_config,
                    )
                    .with_pending_store(shared_pending_store.clone()),
                ),
                Err(_) => None, // Degrade gracefully if tokenizer init fails
            };

        let push_registry = Arc::new(PushRegistry::open(
            &config.push_registry_file,
            &config.push_token_key_file,
        )?);
        init_push_telemetry_file(&config.push_telemetry_file)?;
        let push_provider = build_push_provider(&config, &push_registry)?;
        let push_preferences = PushPreferences::load(&config.push_preferences_file);
        let push_preferences = Arc::new(Mutex::new(push_preferences));
        let push_rate_limiter = Arc::new(Mutex::new(push_dispatcher::PushRateLimiter::new(
            config.push_rate_limit_per_hour,
            3600,
        )));
        let room_roles = config.room_roles.clone();
        let allowed_senders = config.allowed_senders.clone();
        let allow_open_access = config.allow_open_access;

        let mut role_registry = RoleRegistry::new();
        let _ = symbiotic_agent_config::defaults::register_defaults(&mut role_registry);
        if config.role_dir.is_dir() {
            let _ = role_registry.load_from_dir(&config.role_dir);
        }
        let role_registry = Arc::new(role_registry);

        // --- Skill Registry (auto-load agent capabilities from skills directory) ---
        let mut skill_registry = symbiotic_skills::registry::SkillRegistry::new();
        let default_archive_root = config.data_dir.join("../knowledge-base");
        let skill_dir_candidates = if let Some(ref archive_path) = config.archive_path {
            vec![archive_path.join("operations/skills")]
        } else {
            vec![default_archive_root.join("operations/skills")]
        };
        if let Some(skills_dir) = skill_dir_candidates.into_iter().find(|dir| dir.is_dir()) {
            match skill_registry.load_from_dir(&skills_dir) {
                Ok(loaded) => {
                    if !loaded.is_empty() {
                        tracing::info!(
                            count = loaded.len(),
                            skills = %loaded.join(", "),
                            path = %skills_dir.display(),
                            "skills: loaded from Archive skills directory"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "skills: failed to load from skills directory");
                }
            }
        }
        let skill_registry = Arc::new(Mutex::new(skill_registry));

        // --- TrustStore (SQLite-backed token persistence + audit logging) ---
        let trust_store_db_path = config.data_dir.join("trust/trust.db");
        let trust_store = match symbiotic_trust::persistence::TrustStore::open(&trust_store_db_path)
        {
            Ok(store) => {
                // Try to load broker state from TrustStore. If the SQLite DB has tokens
                // that are newer/more complete than the JSON file, prefer them.
                match store.load_broker() {
                    Ok(restored_broker) => {
                        let restored_count = restored_broker.tokens().len();
                        if restored_count > 0 {
                            tracing::info!(
                                token_count = restored_count,
                                "trust_store: restored AccessBroker from SQLite"
                            );
                            broker = restored_broker;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "trust_store: failed to load broker from SQLite, using JSON fallback"
                        );
                    }
                }
                // Save current broker state to TrustStore for consistency.
                if let Err(e) = store.save_broker_state(&broker) {
                    tracing::warn!(
                        error = %e,
                        "trust_store: failed to save initial broker state"
                    );
                }
                #[allow(clippy::arc_with_non_send_sync)]
                Some(Arc::new(store))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %trust_store_db_path.display(),
                    "trust_store: failed to open SQLite DB, audit logging disabled"
                );
                None
            }
        };

        // --- Encrypted blob store (Tier 3 / Private data) ---
        let (encrypted_blob_store, blob_recipient) = init_blob_store(
            &config.blob_store_root,
            config.blob_store_key_file.as_deref(),
        );

        // --- SOUL.md identity loading (eager, before reconciler) ---
        // Load identity content on startup so agents get identity context
        // immediately, even if the reconciler loop hasn't ticked yet.
        // The reconciler shares the same Arc and can update it later via
        // ReloadIdentity.
        //
        // Resolution order:
        // 1. `soul_file` (explicit path from SYMBIOTIC_SOUL_FILE or config)
        // 2. `{archive_path}/identity/SOUL.md`
        // 3. `~/.symbiotic/SOUL.md` (home directory fallback)
        let identity_content = Arc::new(Mutex::new(None));
        let soul_loaded = if let Some(ref explicit_path) = config.soul_file {
            // Priority 1: explicit soul_file path.
            crate::control_plane::load_identity_from_file(explicit_path)
        } else if let Some(ref kb_path) = config.archive_path {
            // Priority 2: archive-relative canonical path.
            crate::control_plane::load_identity_on_startup(kb_path)
        } else {
            // Priority 3: home directory default (~/.symbiotic/SOUL.md).
            crate::control_plane::load_identity_from_home()
        };
        if let Some((content, hash)) = soul_loaded {
            tracing::info!(
                hash = %hash,
                "daemon: SOUL.md identity loaded on startup"
            );
            if let Ok(mut ic) = identity_content.lock() {
                *ic = Some(content);
            }
        }

        // --- GoalProcessManager (validated lifecycle for reconciler goals) ---
        let gpm_store_path = config.data_dir.join("goals/processes");
        let mut goal_process_manager =
            symbiotic_control_plane::GoalProcessManager::new(gpm_store_path);
        if let Err(e) = goal_process_manager.load() {
            tracing::warn!(
                error = %e,
                "goal_process_manager: failed to load persisted goals, starting fresh"
            );
        } else {
            let count = goal_process_manager.all_goals().count();
            if count > 0 {
                tracing::info!(
                    goal_count = count,
                    "goal_process_manager: loaded persisted goal processes"
                );
            }
        }
        let goal_process_manager = Arc::new(Mutex::new(goal_process_manager));

        // --- ManagementStore (task/claim/lease control-plane state) ---
        let management_store_path = config.data_dir.join("control-plane");
        let mut management_store =
            symbiotic_control_plane::ManagementStore::new(management_store_path);
        if let Err(e) = management_store.load() {
            tracing::warn!(
                error = %e,
                "management_store: failed to load persisted work items and claims, starting fresh"
            );
        } else {
            let work_item_count = management_store.work_item_count();
            let claim_count = management_store.claim_count();
            if work_item_count > 0 || claim_count > 0 {
                tracing::info!(
                    work_item_count,
                    claim_count,
                    active_claim_count = management_store.active_claim_count(),
                    "management_store: loaded persisted management-layer state"
                );
            }
            match management_store.expire_stale_claims(symbiotic_queue::now_unix() as i64) {
                Ok(expired_claim_ids) if !expired_claim_ids.is_empty() => {
                    tracing::info!(
                        expired_claim_count = expired_claim_ids.len(),
                        "management_store: expired stale claims during startup recovery"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "management_store: failed to expire stale claims during startup recovery"
                    );
                }
            }
        }
        if let Some(ref archive_root) = config.archive_path {
            match crate::goal_management::rehydrate_management_from_archive(
                &mut management_store,
                archive_root,
                symbiotic_queue::now_unix() as i64,
            ) {
                Ok(hydrated_goal_count) if hydrated_goal_count > 0 => {
                    tracing::info!(
                        hydrated_goal_count,
                        "management_store: rehydrated goal/task hierarchy from archive"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        archive_root = %archive_root.display(),
                        "management_store: failed to rehydrate goal/task hierarchy from archive"
                    );
                }
            }
        }
        let management_store = Arc::new(Mutex::new(management_store));

        // --- ThreadManager (thread lifecycle: creation, archival, routing) ---
        let mut thread_registry = thread_registry::ThreadRegistry::new(&config.data_dir);
        if let Err(e) = thread_registry.load() {
            tracing::warn!(
                error = %e,
                "thread_registry: failed to load persisted threads, starting fresh"
            );
        } else {
            let count = thread_registry.list_active().len();
            if count > 0 {
                tracing::info!(
                    thread_count = count,
                    "thread_manager: loaded persisted thread registry"
                );
            }
        }
        let thread_manager = thread_manager::ThreadManager::new(thread_registry);
        let mut thread_observability_store =
            thread_observability::ThreadObservabilityStore::new(&config.data_dir);
        if let Err(e) = thread_observability_store.load() {
            tracing::warn!(
                error = %e,
                "thread_observability: failed to load persisted summaries, starting fresh"
            );
        }
        let thread_observability_store = Arc::new(Mutex::new(thread_observability_store));
        let mut bridge_interaction_log_store =
            crate::bridge_interactions::BridgeInteractionLogStore::new(&config.data_dir);
        if let Err(e) = bridge_interaction_log_store.load() {
            tracing::warn!(
                error = %e,
                "bridge_interactions: failed to load raw interaction log, starting fresh"
            );
        }
        let bridge_interaction_log_store = Arc::new(Mutex::new(bridge_interaction_log_store));
        let mut agent_runtime_log_store =
            crate::bridge_interactions::AgentRuntimeLogStore::new(&config.data_dir);
        if let Err(e) = agent_runtime_log_store.load() {
            warn!(
                error = %e,
                "failed to load persisted agent runtime log store; starting fresh"
            );
        }
        let agent_runtime_log_store = Arc::new(Mutex::new(agent_runtime_log_store));
        let mut agent_runtime_status_store =
            crate::agent_runtime_status::AgentRuntimeStatusStore::new(&config.data_dir);
        if let Err(e) = agent_runtime_status_store.load() {
            tracing::warn!(
                error = %e,
                "agent_runtime_status_store: failed to load persisted runtime status, starting fresh"
            );
        }
        let agent_runtime_status_store = Arc::new(Mutex::new(agent_runtime_status_store));
        let mut bridge_checkpoint_store =
            crate::bridge_interactions::BridgeCheckpointStore::new(&config.data_dir);
        if let Err(e) = bridge_checkpoint_store.load() {
            tracing::warn!(
                error = %e,
                "bridge_interactions: failed to load checkpoint store, starting fresh"
            );
        }
        let bridge_checkpoint_store = Arc::new(Mutex::new(bridge_checkpoint_store));

        // --- Process Engineer infrastructure ---
        let graduation_store = if config.enable_process_engineer {
            match symbiotic_agents::graduation::SqliteGraduationStore::open(
                &config.pe_graduation_db,
            ) {
                Ok(store) => Some(Arc::new(store)),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to open PE graduation store, PE disabled");
                    None
                }
            }
        } else {
            None
        };
        let pe_agent_monitor = if config.enable_process_engineer {
            let monitor_path = config.data_dir.join("agents/monitor.db");
            match symbiotic_agents::monitoring::SqliteAgentMonitor::open(
                &monitor_path,
                &symbiotic_agents::monitoring::MonitorConfig::default(),
            ) {
                Ok(monitor) => Some(Arc::new(monitor)),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to open agent monitor for PE");
                    None
                }
            }
        } else {
            None
        };

        // --- Somatic marker store (SQLite-backed, persistent emotional/temporal tagging) ---
        let somatic_index = {
            // Ensure parent directory exists for the somatic DB.
            if let Some(parent) = config.somatic_db.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match SqliteSomaticStore::open(&config.somatic_db) {
                Ok(store) => {
                    let store = Arc::new(store);
                    let mut index = SomaticIndex::with_store(store);
                    // Load persisted markers into the in-memory cache.
                    // Uses a blocking approach since open() is not async.
                    match tokio::runtime::Handle::try_current() {
                        Ok(handle) => {
                            let load_result = std::thread::scope(|s| {
                                s.spawn(|| handle.block_on(index.load_from_store()))
                                    .join()
                                    .expect("somatic store load thread should not panic")
                            });
                            match load_result {
                                Ok(count) => {
                                    if count > 0 {
                                        tracing::info!(
                                            marker_count = count,
                                            "somatic_store: loaded persisted markers"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "somatic_store: failed to load markers from store"
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            tracing::warn!(
                                "somatic_store: no tokio runtime available, skipping initial load"
                            );
                        }
                    }
                    Arc::new(Mutex::new(index))
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %config.somatic_db.display(),
                        "somatic_store: failed to open SQLite DB, using in-memory fallback"
                    );
                    Arc::new(Mutex::new(SomaticIndex::new()))
                }
            }
        };

        // --- Vault Indexer & Watcher (Vault-as-Truth) ---
        let vault_kb_root = config
            .archive_path
            .clone()
            .unwrap_or_else(|| config.data_dir.join("../knowledge-base"));
        let memory_db_path = config.data_dir.join("memory.db");
        let memory_store = Arc::new(match SqliteMemoryStore::open(&memory_db_path) {
            Ok(store) => store,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "vault_indexer: failed to open memory store, using in-memory fallback"
                );
                SqliteMemoryStore::open_in_memory().expect("in-memory memory store must succeed")
            }
        });
        let graph_store = match SqliteGraphStore::open(&memory_db_path) {
            Ok(store) => Some(store),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "recall_graph: failed to open SQLite graph store, disabling graph retrieval"
                );
                None
            }
        };
        let vault_indexer = VaultIndexer::new(&memory_store);
        // Initialize the vault_file_index table (sync bridge from open()).
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let init_result = std::thread::scope(|s| {
                    let idx = &vault_indexer;
                    s.spawn(move || handle.block_on(idx.initialize()))
                        .join()
                        .expect("vault indexer init thread should not panic")
                });
                match init_result {
                    Ok(()) => tracing::debug!("vault_indexer: index table initialized"),
                    Err(e) => {
                        tracing::warn!(error = %e, "vault_indexer: failed to initialize index table")
                    }
                }
            }
            Err(_) => {
                tracing::warn!("vault_indexer: no tokio runtime, skipping initialization");
            }
        }
        let vault_indexer = Arc::new(tokio::sync::Mutex::new(vault_indexer));

        let mut recall_gateway = RecallGateway::with_vector_index(
            Arc::new(ArchiveContextProvider {
                archive_store: archive_store.clone(),
                vault_store: vault_store.clone(),
            }),
            Arc::new(FileAuditSink::open(config.audit_log_file.clone())?),
            shared_vector_index,
        );
        if let Some(graph_store) = graph_store {
            recall_gateway.set_graph(Arc::new(BfsGraphRetriever::new(graph_store)));
        }
        let recall_gateway = Arc::new(recall_gateway);

        // Start the background vault watcher (polls for Obsidian edits).
        // Only start if a tokio runtime is available (tests run without one).
        let vault_watcher_handle =
            if vault_kb_root.is_dir() && tokio::runtime::Handle::try_current().is_ok() {
                let watcher_config = symbiotic_memory::vault_watcher::VaultWatcherConfig {
                    vault_root: vault_kb_root.clone(),
                    poll_interval_secs: 30,
                };
                let handle = vault_watcher::start_watcher(
                    Arc::clone(&vault_indexer),
                    watcher_config,
                    Some(Box::new(|stats| {
                        tracing::info!(
                            indexed = stats.files_indexed,
                            entities = stats.entities_upserted,
                            "vault_watcher: re-indexed files"
                        );
                    })),
                );
                tracing::info!(
                    root = %vault_kb_root.display(),
                    "vault_watcher: started (30s poll interval)"
                );
                Some(handle)
            } else if !vault_kb_root.is_dir() {
                tracing::info!(
                    root = %vault_kb_root.display(),
                    "vault_watcher: skipped (directory not found)"
                );
                None
            } else {
                tracing::info!("vault_watcher: skipped (no tokio runtime)");
                None
            };

        let vault_writer = VaultWriter::new(&vault_kb_root);

        // --- Periodic Synthesizer (entity dedup, cross-thread merge) ---
        let periodic_synthesizer = if vault_kb_root.is_dir() {
            Some(periodic_synthesis::PeriodicSynthesizer::new(&vault_kb_root))
        } else {
            None
        };

        // --- One-shot auth sandbox launcher (optional, behind auth_scripts_dir) ---
        let auth_engine = build_auth_engine(&config, credential_vault.clone());

        // --- Shared broker for both daemon and agent executor ---
        let shared_broker = Arc::new(Mutex::new(broker));

        let sandbox_manager = if matches!(config.agent_backend, AgentBackend::Runner) && {
            #[cfg(test)]
            {
                !matches!(
                    config.runner_harness_mode,
                    crate::workers::RunnerHarnessMode::InProcess
                )
            }
            #[cfg(not(test))]
            {
                true
            }
        } {
            let backend = Box::new(symbiotic_vm::backends::sysbox::SysboxBackend::new()?);
            let bridge = symbiotic_vm::file_bridge::FileBridge::new(
                &std::env::current_dir()?,
                &config.data_dir,
            );
            let audit_path = config.data_dir.join("runtime/vm-audit.jsonl");
            Some(Arc::new(Mutex::new(symbiotic_vm::manager::VmManager::new(
                backend,
                &audit_path,
                bridge,
            ))))
        } else {
            None
        };

        let bridge_session_store = Arc::new(Mutex::new(
            crate::bridge_interactions::BridgeSessionStore::default(),
        ));
        let auth_jobs = Arc::new(Mutex::new(crate::auth_jobs::AuthJobStore::open(
            config.data_dir.join("auth-jobs"),
        )?));
        let auth_approval_policies = Arc::new(Mutex::new(
            crate::auth_approval_policies::AuthApprovalPolicyStore::open(
                config.data_dir.join("auth-approval-policies"),
            )?,
        ));

        // --- Add agent.execute executor (deferred until provider_router, role_registry,
        // and identity_content are ready) ---
        let agent_execute_executor = Arc::new(AgentExecuteExecutor {
            goals_dir: config
                .goal_log_file
                .parent()
                .unwrap_or(Path::new("data/goals"))
                .to_path_buf(),
            repo_root: std::env::var("SYMBIOTIC_REPO_WORKSPACE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(".")),
            provider_router: Arc::clone(&provider_router),
            role_registry: Arc::clone(&role_registry),
            identity_content: Arc::clone(&identity_content),
            agent_backend: config.agent_backend,
            archive_store: archive_store.clone(),
            queue: queue.clone(),
            vector_index: recall_gateway.vector_index().cloned(),
            broker: Some(Arc::clone(&shared_broker)),
            bridge_session_store: Arc::clone(&bridge_session_store),
            llm_gateway_socket: config.llm_gateway_socket.clone(),
            sandbox_manager: sandbox_manager.clone(),
            dispatch_backend: std::sync::OnceLock::new(),
            #[cfg(test)]
            runner_harness_mode: config.runner_harness_mode,
        });
        // Wire the dispatch_agent backend now that the executor Arc exists. The
        // backend holds a Weak<AgentExecuteExecutor> back to the executor so a
        // running sub-agent can be spawned through the same `run_agent_goal`
        // entry point. `set(...)` returns Err only if already initialised, which
        // can't happen here — one-time init, result intentionally ignored.
        let dispatch_backend: Arc<dyn symbiotic_agents::builtin_tools::DispatchAgentBackend> =
            Arc::new(crate::tool_adapters::DaemonDispatchBackend::new(
                Arc::downgrade(&agent_execute_executor),
            ));
        let _ = agent_execute_executor
            .dispatch_backend
            .set(dispatch_backend);
        let workflow_runner = workflow_runner.with_executor(
            "agent.execute",
            agent_execute_executor.clone() as Arc<dyn symbiotic_workflows::StepExecutor>,
        );

        Ok((
            Self {
                config,
                queue,
                broker: shared_broker,
                bridge_session_store,
                bridge_interaction_log_store,
                agent_runtime_log_store,
                agent_runtime_status_store,
                bridge_checkpoint_store,
                auth_jobs,
                auth_approval_policies,
                agents: SecureAgentFramework::new(symbiotic_agents::FrameworkConfig::default()),
                credential_gateway,
                credential_vault: credential_vault.clone(),
                credential_validator: Box::new(credential_validator::CredentialValidator::new(
                    std::time::Duration::from_secs(10),
                )),
                workflow_runner,
                workflow_registry,
                bookmarks_client,
                archive_store,
                vault_store,
                review_store,
                review_engine,
                intake_handler: IntakeMessageHandler::new(queue_executor),
                pipeline,
                review_queue_adapter: review_queue,
                recall_gateway,
                embedding_processor,
                // vector_index_path removed — sqlite-vec handles persistence
                push_registry,
                push_provider,
                room_roles,
                allowed_senders,
                allow_open_access,
                role_registry,
                identity_content,
                trust_store,
                provider_router,
                encrypted_blob_store,
                blob_recipient,
                pending_embedding_store: shared_pending_store,
                goal_process_manager,
                management_store,
                repo_registry,
                approval_gate,
                push_preferences,
                push_rate_limiter,
                graduation_store,
                agent_monitor: pe_agent_monitor,
                somatic_index,
                auth_engine,
                skill_registry,
                thread_manager: Mutex::new(Some(thread_manager)),
                thread_observability_store,
                vault_indexer,
                vault_watcher_handle,
                vault_writer,
                promotion_cooldown_tracker: Mutex::new(
                    auto_promotion::PromotionCooldownTracker::new(),
                ),
                pending_room_creations: Mutex::new(Vec::new()),
                declared_task_policy_last_run: Mutex::new(None),
                memory_store,
                periodic_synthesizer,
                proposal_store: proposals::ProposalStore::new(),
                sandbox_manager,
                matrix_outbound_tx,
                conflict_goal_tx,
                question_resolver: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::goal_pipeline::question_resolver::QuestionResolver::new(),
                )),
                subgoal_dispatcher: std::sync::Mutex::new(None),
                agent_execute_executor,
            },
            matrix_outbound_rx,
            conflict_goal_rx,
        ))
    }

    /// Run a one-shot agent goal against this daemon's stores (Archive, Recall,
    /// role registry, capability broker). Bypasses the Matrix + workflow-engine
    /// transport so that the CLI (or other in-process callers) can drive the
    /// same `AgentExecuteExecutor` that Matrix-delivered workflow steps hit.
    ///
    /// The executor dispatches to `AgentBackend::React` in-process by default,
    /// or to `AgentBackend::Runner` (Sysbox-sandboxed subprocess + LLM Gateway
    /// bridge) when `SYMBIOTIC_AGENT_BACKEND=runner` is configured.
    ///
    /// On success returns the agent's final output. On failure returns the
    /// failure message wrapped in `anyhow::Error` so CLI callers can bubble it
    /// up with `?`.
    pub fn run_agent_goal(&self, role: &str, goal: &str) -> Result<String> {
        let result = self.agent_execute_executor.run_agent_goal(role, goal);
        if result.status == "completed" {
            Ok(result.output)
        } else {
            Err(anyhow::anyhow!(result.output))
        }
    }

    pub fn archive_store(&self) -> Arc<FileArchiveStore> {
        Arc::clone(&self.archive_store)
    }

    pub fn memory_store(&self) -> Arc<SqliteMemoryStore> {
        Arc::clone(&self.memory_store)
    }

    pub fn queue(&self) -> Arc<dyn QueueBackend> {
        Arc::clone(&self.queue)
    }

    pub fn recall_gateway(&self) -> Arc<RecallGateway> {
        Arc::clone(&self.recall_gateway)
    }

    /// Install a [`crate::subgoal::Dispatcher`] for T130 §05 sub-goal
    /// routing. Call this after construction (typically in `main.rs`) once
    /// the LLM / Recall collaborators are ready.
    pub fn set_subgoal_dispatcher(&self, dispatcher: crate::subgoal::Dispatcher) {
        if let Ok(mut slot) = self.subgoal_dispatcher.lock() {
            *slot = Some(dispatcher);
        }
    }

    /// Return a cloned handle to the installed Sub-Goal Dispatcher, if any.
    pub fn subgoal_dispatcher(&self) -> Option<crate::subgoal::Dispatcher> {
        self.subgoal_dispatcher.lock().ok().and_then(|s| s.clone())
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    pub fn sandbox_manager(&self) -> Option<Arc<Mutex<symbiotic_vm::manager::VmManager>>> {
        self.sandbox_manager.as_ref().map(Arc::clone)
    }

    /// Public accessor for the durable T126 repo registry handle. Used by
    /// `main.rs` to hand the registry to `SwarmServer::new` without touching
    /// the `pub(crate)` field directly.
    pub fn repo_registry(&self) -> crate::repo_registry::SharedRepoRegistry {
        self.repo_registry.clone()
    }

    /// Public accessor for the approval gate handle. Used by integration
    /// tests (T126 §09) to simulate operator `!approve` decisions without
    /// going through the full Matrix dispatcher.
    pub fn approval_gate_for_test(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::approval_gate::ApprovalGate>> {
        self.approval_gate.clone()
    }

    /// Spawn the per-repo mirror scheduler (T126 §08.c).
    ///
    /// One `tokio::spawn` per active repo in the registry. Each task drives
    /// `mirror_pull_once` + `mirror_push_with_approval` on the manifest's
    /// `mirror.sync_interval_secs` cadence and emits conflict goals via the
    /// `conflict_goal_tx` channel.
    ///
    /// `cancel_rx` is a `tokio::sync::watch` receiver; tasks shut down cleanly
    /// when the paired sender broadcasts `true`. The caller (`main.rs`) keeps
    /// the sender alive for the daemon's lifetime.
    ///
    /// Returns one `JoinHandle` per spawned task. Caller should `.await` all
    /// of them after cancellation for a clean drain.
    pub async fn spawn_repo_scheduler(
        &self,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let operator_room_id = self.config.room_roles.resolve(RoomRole::Alerts).to_string();
        let project_goals_room = self.config.room_roles.resolve(RoomRole::Goals).to_string();
        let archive_root = self.config.archive_root.clone();
        let approval_ttl_secs = self.config.auth_approval_ttl_secs;
        let deps = crate::repo_scheduler::SchedulerDeps {
            registry: self.repo_registry.clone(),
            approval_gate: self.approval_gate.clone(),
            credential_vault: self.credential_vault.clone(),
            push_provider: self.push_provider.clone(),
            matrix_outbound_tx: self.matrix_outbound_tx.clone(),
            conflict_goal_tx: self.conflict_goal_tx.clone(),
            operator_room_id,
            project_goals_room,
            archive_root,
            agent_id: "scheduler@symbiotic.sh".to_string(),
            approval_ttl_secs,
            approval_poll_interval_ms: 500,
        };
        crate::repo_scheduler::spawn_from_registry(deps, cancel_rx).await
    }

    /// Dispatch a conflict-goal request drained from `conflict_goal_rx`. Must
    /// be called on the main daemon thread since `SymbioticDaemon` is `!Send`.
    /// T126 §08.c.
    pub fn handle_conflict_goal(&self, req: &ConflictGoalRequest, now: u64) -> Result<()> {
        self.process_goal_through_pipeline(&req.description, &req.room_id, &req.sender, now)
            .map(|_| ())
    }

    /// Returns feature lines based on daemon-internal state.
    pub fn feature_summary(&self) -> Vec<FeatureLine> {
        vec![
            FeatureLine {
                category: "Push (APNs)",
                enabled: self.config.push_apns_team_id.is_some(),
                detail: if self.config.push_apns_team_id.is_some() {
                    "real APNs gateway".into()
                } else {
                    "no APNs credentials".into()
                },
            },
            FeatureLine {
                category: "Push (FCM)",
                enabled: self.config.push_fcm_project_id.is_some(),
                detail: if self.config.push_fcm_project_id.is_some() {
                    "real FCM gateway".into()
                } else {
                    "no FCM credentials".into()
                },
            },
            FeatureLine {
                category: "Push (Gateway)",
                enabled: self.config.push_gateway_url.is_some(),
                detail: self
                    .config
                    .push_gateway_url
                    .clone()
                    .unwrap_or_else(|| "disabled".into()),
            },
            FeatureLine {
                category: "Push (File)",
                enabled: true,
                detail: format!("{}", self.config.push_outbox_file.display()),
            },
            FeatureLine {
                category: "AI: Ollama",
                enabled: true,
                detail: self
                    .config
                    .ollama_url
                    .clone()
                    .unwrap_or_else(|| "localhost:11434 (default)".into()),
            },
            FeatureLine {
                category: "AI: OpenAI",
                enabled: self.config.openai_api_key.is_some(),
                detail: if self.config.openai_api_key.is_some() {
                    "configured".into()
                } else {
                    "no API key".into()
                },
            },
            FeatureLine {
                category: "AI: Anthropic",
                enabled: self.config.anthropic_api_key.is_some(),
                detail: if self.config.anthropic_api_key.is_some() {
                    "configured".into()
                } else {
                    "no API key".into()
                },
            },
            FeatureLine {
                category: "Agent Backend",
                enabled: true,
                detail: match self.config.agent_backend {
                    AgentBackend::React => "react (internal loop)".into(),
                    AgentBackend::Cli => "cli (external agents)".into(),
                    AgentBackend::Runner => "runner (decoupled binary)".into(),
                },
            },
            FeatureLine {
                category: "Skills",
                enabled: self
                    .skill_registry
                    .lock()
                    .map(|r| !r.list().is_empty())
                    .unwrap_or(false),
                detail: self
                    .skill_registry
                    .lock()
                    .map(|r| {
                        let count = r.list().len();
                        if count > 0 {
                            format!("loaded {} from operations/skills/", count)
                        } else {
                            "none loaded".into()
                        }
                    })
                    .unwrap_or_else(|_| "error".into()),
            },
            FeatureLine {
                category: "Embeddings",
                enabled: self.embedding_processor.is_some(),
                detail: if self.embedding_processor.is_some() {
                    "processor active".into()
                } else {
                    "disabled".into()
                },
            },
            FeatureLine {
                category: "Archive",
                enabled: self.config.archive_path.is_some(),
                detail: self
                    .config
                    .archive_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not configured".into()),
            },
            FeatureLine {
                category: "Vault",
                enabled: true,
                detail: format!("{}", self.config.credential_vault_file.display()),
            },
            FeatureLine {
                category: "Trust Store",
                enabled: self.trust_store.is_some(),
                detail: if self.trust_store.is_some() {
                    "SQLite-backed".into()
                } else {
                    "in-memory only".into()
                },
            },
            FeatureLine {
                category: "Blob Encrypt",
                enabled: self.encrypted_blob_store.is_some(),
                detail: if self.encrypted_blob_store.is_some() {
                    format!("{}", self.config.blob_store_root.display())
                } else {
                    "no age key configured".into()
                },
            },
            FeatureLine {
                category: "Auth Sandbox",
                enabled: self.auth_engine.is_some(),
                detail: if let Some(ref dir) = self.config.auth_scripts_dir {
                    format!("{}", dir.display())
                } else {
                    "no scripts dir configured".into()
                },
            },
        ]
    }

    pub fn handle_intake_message(&self, message: &str) -> Result<IntakeReply> {
        self.intake_handler
            .handle_text(message, IntakeSource::Matrix)
    }

    /// Return the configured room_id for `role`, falling back to the default
    /// alias string when no mapping is present.
    pub fn resolve_room(&self, role: RoomRole) -> &str {
        self.room_roles.resolve(role)
    }

    /// Look up the Matrix room_id for a thread via the ThreadRegistry.
    /// Returns `None` when the thread has no mapped room or the manager
    /// is unavailable.
    pub fn resolve_thread_room(&self, thread_id: &str) -> Option<String> {
        self.thread_manager.lock().ok().and_then(|tm| {
            tm.as_ref()
                .and_then(|mgr| mgr.room_for_thread(thread_id).map(|r| r.to_string()))
        })
    }

    pub fn enqueue_intake_urls(
        &self,
        urls: Vec<Url>,
        tags: Vec<String>,
        source: IntakeSource,
    ) -> Result<IntakeBatchResult> {
        let executor = QueueIntakeExecutor {
            queue: self.queue.clone(),
        };
        executor.execute(IntakeRequest {
            source,
            kind: IntakeKind::Url,
            urls,
            note: None,
            tags,
            file_path: None,
            title: None,
        })
    }

    pub fn submit_intake_request(&self, request: IntakeRequest) -> Result<IntakeBatchResult> {
        match request.kind {
            IntakeKind::Url => {
                let executor = QueueIntakeExecutor {
                    queue: self.queue.clone(),
                };
                executor.execute(request)
            }
            IntakeKind::Note | IntakeKind::LocalFile => self.pipeline.process(request),
        }
    }

    pub fn submit_intake_urls_raw(
        &self,
        raw_urls: Vec<String>,
        tags: Vec<String>,
        source: IntakeSource,
    ) -> Result<IntakeBatchResult> {
        let mut normalized_urls = Vec::new();
        let mut invalid_items = Vec::new();

        for raw in raw_urls {
            match normalize_url(&raw) {
                Ok(url) => normalized_urls.push(url),
                Err(_) => invalid_items.push(IntakeItemResult {
                    input: raw,
                    normalized_url: None,
                    status: IntakeStatus::Invalid,
                    route: IntakeRoute::Archive,
                    review_queued: false,
                    review_job_id: None,
                    idempotency_key: None,
                    record_id: None,
                    error: Some("invalid URL format".to_string()),
                }),
            }
        }

        let mut result = if normalized_urls.is_empty() {
            IntakeBatchResult {
                run_id: format!("run_{}", now_unix()),
                items: Vec::new(),
                summary: symbiotic_core::intake::IntakeSummary::default(),
            }
        } else {
            self.submit_intake_request(IntakeRequest {
                source,
                kind: IntakeKind::Url,
                urls: normalized_urls,
                note: None,
                tags,
                file_path: None,
                title: None,
            })?
        };

        if !invalid_items.is_empty() {
            result.items.extend(invalid_items);
            result.summary = summarize_items(&result.items);
        }

        Ok(result)
    }

    pub fn queue_auth_issue_request(&self, target: &str, scopes: Vec<String>) -> Result<String> {
        let payload = format!(
            "{}|{}",
            escape_field(&target.to_ascii_lowercase()),
            scopes
                .into_iter()
                .map(|scope| escape_field(&scope.to_ascii_lowercase()))
                .collect::<Vec<_>>()
                .join(",")
        );
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "auth.issue".to_string(),
            payload,
            idempotency_key: format!("auth:{}:{}", target.to_ascii_lowercase(), now_unix()),
            max_attempts: 3,
            next_run_at: now_unix(),
            force: false,
        })?;
        Ok(outcome.job_id)
    }

    pub fn queue_bookmarks_sync(&self, source: &str, limit: u32) -> Result<String> {
        let source = source.to_ascii_lowercase();
        let limit = limit.max(1);
        if limit > MAX_BOOKMARKS_SYNC_LIMIT {
            return Err(anyhow!(
                "bookmarks sync limit {limit} exceeds max {MAX_BOOKMARKS_SYNC_LIMIT}"
            ));
        }
        let payload = format!("{}|{}", escape_field(&source), limit);
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "bookmarks.sync".to_string(),
            payload,
            idempotency_key: format!("bookmarks:{}:{}:{}", source, limit, now_unix()),
            max_attempts: 3,
            next_run_at: now_unix(),
            force: false,
        })?;
        Ok(outcome.job_id)
    }

    /// Execute the next available job from the queue, returning the primary
    /// event and any step-level progress events (for workflow runs).
    pub fn run_once(&self, now: u64) -> Result<Option<(DaemonEvent, Vec<DaemonEvent>)>> {
        self.queue.reclaim_expired_leases(now)?;
        let _ = self.enqueue_pending_archive_replans(now)?;
        let Some(job) =
            self.queue
                .lease_next(&self.config.worker_id, now, self.config.lease_seconds, None)?
        else {
            return Ok(None);
        };

        // workflow.run returns (DaemonEvent, Vec<DaemonEvent>); other jobs
        // are lifted into the same shape with an empty step_events vec.
        let event_result: Result<(DaemonEvent, Vec<DaemonEvent>)> = match job.type_name.as_str() {
            "ingest.fetch" => self
                .execute_ingest_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "archive.review.enqueue" => self
                .execute_review_enqueue_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "archive.review" => self
                .execute_review_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "auth.issue" => self
                .execute_auth_issue_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "workflow.run" => self.execute_workflow_job(job.clone(), now),
            "bookmarks.sync" => self
                .execute_bookmarks_sync_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "install.provision" => self
                .execute_install_provision_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "install.bootstrap" => self
                .execute_install_bootstrap_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "install.verify" => self
                .execute_install_verify_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "install.run" => self
                .execute_install_run_job(job.clone(), now)
                .map(|e| (e, Vec::new())),
            "thread.distillery" => self.execute_thread_distillery_job(job.clone(), now),
            unknown => {
                let _outcome = self.queue.fail(
                    &job.job_id,
                    &self.config.worker_id,
                    now,
                    self.config.retry_backoff_seconds,
                    &format!("unknown job type {unknown}"),
                )?;
                Ok((
                    DaemonEvent {
                        event_type: EventType::JobUnknown,
                        status: "failed".to_string(),
                        job_id: Some(job.job_id.clone()),
                        detail: unknown.to_string(),
                        goal_room: None,
                        goal_template: None,
                        goal_run_id: None,
                        goal_id: None,
                        intake_run_id: None,
                        url: None,
                        title: None,
                        sensitivity: None,
                        quick_replies: None,
                        thread_id: None,
                    },
                    Vec::new(),
                ))
            }
        };

        let (event, step_events) = match event_result {
            Ok(pair) => pair,
            Err(err) => {
                let outcome = self.queue.fail(
                    &job.job_id,
                    &self.config.worker_id,
                    now,
                    self.config.retry_backoff_seconds,
                    &err.to_string(),
                )?;
                (
                    DaemonEvent {
                        event_type: job
                            .type_name
                            .parse::<EventType>()
                            .unwrap_or(EventType::JobUnknown),
                        status: if outcome == FailOutcome::MovedToDlq {
                            "dlq".to_string()
                        } else {
                            "retry".to_string()
                        },
                        job_id: Some(job.job_id.clone()),
                        detail: err.to_string(),
                        goal_room: None,
                        goal_template: None,
                        goal_run_id: None,
                        goal_id: None,
                        intake_run_id: None,
                        url: None,
                        title: None,
                        sensitivity: None,
                        quick_replies: None,
                        thread_id: None,
                    },
                    Vec::new(),
                )
            }
        };

        // Fire-and-forget push dispatch for the event. Errors are logged
        // internally and never fail the main event loop.
        push_dispatcher::dispatch_event_push_full(
            &event,
            &self.push_registry,
            &*self.push_provider,
            &self.config.push_telemetry_file,
            now,
            Some(&self.push_preferences),
            Some(&self.push_rate_limiter),
        );

        Ok(Some((event, step_events)))
    }

    /// Accessors for the push infrastructure, used by `fire_push_for_event`
    /// in async contexts (e.g., the Matrix transport pump).
    #[allow(dead_code)]
    pub(crate) fn push_registry(&self) -> &Arc<PushRegistry> {
        &self.push_registry
    }

    #[allow(dead_code)]
    pub(crate) fn push_provider_arc(&self) -> &Arc<dyn PushProvider> {
        &self.push_provider
    }

    pub fn queued_jobs_of_type(&self, type_name: &str) -> Result<Vec<QueueJob>> {
        Ok(self
            .queue
            .list_by_status(JobStatus::Queued)?
            .into_iter()
            .filter(|job| job.type_name == type_name)
            .collect())
    }

    pub fn done_jobs_of_type(&self, type_name: &str) -> Result<Vec<QueueJob>> {
        Ok(self
            .queue
            .list_by_status(JobStatus::Done)?
            .into_iter()
            .filter(|job| job.type_name == type_name)
            .collect())
    }

    pub fn status_snapshot(&self, now: u64) -> Result<DaemonStatusSnapshot> {
        Ok(DaemonStatusSnapshot {
            queued: self.queue.list_by_status(JobStatus::Queued)?.len(),
            running: self.queue.list_by_status(JobStatus::Running)?.len(),
            failed: self.queue.list_by_status(JobStatus::Failed)?.len(),
            done: self.queue.list_by_status(JobStatus::Done)?.len(),
            dlq: self.queue.list_by_status(JobStatus::Dlq)?.len(),
            timestamp: now,
        })
    }

    /// Build a `status.snapshot` envelope from current queue state.
    /// Used by both the reactive status handler and the periodic emitter.
    /// When `uptime_secs` is provided, it is included in the envelope details.
    pub fn build_snapshot_envelope(
        &self,
        now: u64,
        uptime_secs: Option<u64>,
    ) -> Result<MatrixEventEnvelope> {
        let status = self.status_snapshot(now)?;
        let mut envelope = MatrixEventEnvelope::state("snapshot", now, "Daemon status snapshot")
            .with_detail_field("queued", status.queued)
            .with_detail_field("running", status.running)
            .with_detail_field("failed", status.failed)
            .with_detail_field("done", status.done)
            .with_detail_field("dlq", status.dlq)
            .with_detail_field("ts", status.timestamp);
        if let Some(uptime) = uptime_secs {
            envelope = envelope.with_detail_field("uptime_secs", uptime);
        }
        Ok(envelope)
    }

    pub fn list_agent_states(&self) -> Result<Vec<AgentState>> {
        load_agent_states(&self.config.agent_state_file)
    }

    pub fn agent_state(&self, agent_id: &str) -> Result<Option<AgentState>> {
        let states = self.list_agent_states()?;
        Ok(states
            .into_iter()
            .find(|state| state.agent_id.eq_ignore_ascii_case(agent_id)))
    }

    fn execute_ingest_job(&self, job: QueueJob, now: u64) -> Result<DaemonEvent> {
        let payload = decode_ingest_payload(&job.payload)
            .with_context(|| format!("invalid ingest payload for job {}", job.job_id))?;
        let run_id = payload.run_id.clone();
        let normalized_url = normalize_url(&payload.url)?;

        // Set the run_id on the review adapter so downstream review enqueue
        // jobs carry the same correlation id.
        self.review_queue_adapter.set_run_id(&run_id);
        let result = self.pipeline.process(IntakeRequest {
            source: payload.source,
            kind: IntakeKind::Url,
            urls: vec![normalized_url],
            note: None,
            tags: payload.tags,
            file_path: None,
            title: None,
        });
        self.review_queue_adapter.clear_run_id();
        let result = result?;

        let first = result.items.first().cloned().unwrap_or(IntakeItemResult {
            input: String::new(),
            normalized_url: None,
            status: IntakeStatus::QueueFailed,
            route: IntakeRoute::Archive,
            review_queued: false,
            review_job_id: None,
            idempotency_key: None,
            record_id: None,
            error: Some("missing intake result item".to_string()),
        });

        if matches!(
            first.status,
            IntakeStatus::Ingested
                | IntakeStatus::Duplicate
                | IntakeStatus::SecureRouted
                | IntakeStatus::Blocked
        ) {
            // --- Embedding generation (best-effort, never fails the ingest) ---
            if first.status == IntakeStatus::Ingested {
                self.run_intake_embeddings(first.record_id.as_deref(), &run_id);
            }

            // --- Tier 3 routing: Private content → encrypted blob store ---
            if first.status == IntakeStatus::Ingested {
                if let Some(ref record_id) = first.record_id {
                    self.route_private_to_blob_store(record_id, &run_id);
                }
            }

            // Look up the ingested document's sensitivity so we can tag the
            // outgoing Matrix event.  The Tier 3 phone-only filter in
            // `send_matrix_event` uses this tag to redact Private content.
            let doc_sensitivity = first
                .record_id
                .as_deref()
                .and_then(|rid| self.archive_store.get(rid).ok().flatten())
                .map(|doc| match doc.sensitivity {
                    symbiotic_archive::ArchiveSensitivity::Shareable => "shareable",
                    symbiotic_archive::ArchiveSensitivity::Restricted => "restricted",
                    symbiotic_archive::ArchiveSensitivity::Private => "private",
                })
                .map(String::from);

            self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
            return Ok(DaemonEvent {
                event_type: EventType::IngestFetch,
                status: "completed".to_string(),
                job_id: Some(job.job_id),
                detail: format!("{:?}", first.status),
                goal_room: None,
                goal_template: None,
                goal_run_id: None,
                goal_id: None,
                intake_run_id: Some(run_id),
                url: Some(payload.url.clone()),
                title: None,
                sensitivity: doc_sensitivity,
                quick_replies: None,
                thread_id: None,
            });
        }

        let outcome = self.queue.fail(
            &job.job_id,
            &self.config.worker_id,
            now,
            self.config.retry_backoff_seconds,
            first.error.as_deref().unwrap_or("ingest failure"),
        )?;
        Ok(DaemonEvent {
            event_type: EventType::IngestFetch,
            status: if outcome == FailOutcome::MovedToDlq {
                "dlq".to_string()
            } else {
                "retry".to_string()
            },
            job_id: Some(job.job_id),
            detail: format!("{:?}", first.status),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: Some(run_id),
            url: Some(payload.url.clone()),
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        })
    }

    /// Run intake embeddings for a freshly ingested document.
    ///
    /// Best-effort: embedding failures are logged but never propagate to the
    /// caller. This keeps the ingest pipeline reliable even when the embedding
    /// provider (Ollama, OpenAI) is offline or misconfigured.
    pub(crate) fn run_intake_embeddings(&self, record_id: Option<&str>, run_id: &str) {
        let Some(ref processor) = self.embedding_processor else {
            return; // Embeddings not configured — skip silently.
        };
        let Some(record_id) = record_id else {
            tracing::debug!(
                run_id = run_id,
                "intake_embeddings: skipping — no record_id"
            );
            return;
        };

        // Load the document from the archive to get content and sensitivity.
        let document = match self.archive_store.get(record_id) {
            Ok(Some(doc)) => doc,
            Ok(None) => {
                tracing::warn!(
                    record_id = record_id,
                    run_id = run_id,
                    "intake_embeddings: record not found in archive"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    record_id = record_id,
                    run_id = run_id,
                    error = %e,
                    "intake_embeddings: failed to load document from archive"
                );
                return;
            }
        };

        let sensitivity = match document.sensitivity {
            symbiotic_archive::ArchiveSensitivity::Shareable => {
                symbiotic_core::Sensitivity::Shareable
            }
            symbiotic_archive::ArchiveSensitivity::Restricted => {
                symbiotic_core::Sensitivity::Restricted
            }
            symbiotic_archive::ArchiveSensitivity::Private => symbiotic_core::Sensitivity::Private,
        };

        // process_document is async — bridge from the sync ingest job context
        // by spawning a scoped thread that can safely call handle.block_on
        // without deadlocking the current tokio runtime thread.
        let outcome = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let content = &document.content;
                std::thread::scope(|s| {
                    s.spawn(move || {
                        handle.block_on(processor.process_document(record_id, content, sensitivity))
                    })
                    .join()
                    .expect("embedding thread should not panic")
                })
            }
            Err(_) => {
                tracing::warn!(
                    record_id = record_id,
                    run_id = run_id,
                    "intake_embeddings: no tokio runtime — skipping"
                );
                return;
            }
        };

        tracing::info!(
            record_id = record_id,
            run_id = run_id,
            chunks_total = outcome.chunks_total,
            chunks_embedded = outcome.chunks_embedded,
            chunks_pending = outcome.chunks_pending,
            chunks_failed = outcome.chunks_failed,
            "intake_embeddings: processed"
        );

        // sqlite-vec persists writes immediately — no explicit save needed.
    }

    /// Retries embedding for pending chunks that previously failed due to
    /// provider unavailability.
    ///
    /// Best-effort: errors are logged but never propagated.  Call this from
    /// a periodic tick in the daemon event loop.
    ///
    /// Returns the number of successfully embedded chunks, or 0 if no
    /// processor is configured or no pending chunks exist.
    pub fn retry_pending_embeddings(&self) -> usize {
        let Some(ref processor) = self.embedding_processor else {
            return 0;
        };

        // Check if there are any pending chunks before spawning a thread.
        {
            let store = match self.pending_embedding_store.lock() {
                Ok(s) => s,
                Err(_) => return 0,
            };
            if store.is_empty() {
                return 0;
            }
            tracing::info!(
                pending_count = store.len(),
                "retry_pending_embeddings: attempting retry"
            );
        }

        let embedded = match tokio::runtime::Handle::try_current() {
            Ok(handle) => std::thread::scope(|s| {
                s.spawn(move || handle.block_on(processor.retry_pending()))
                    .join()
                    .expect("retry thread should not panic")
            }),
            Err(_) => {
                tracing::warn!("retry_pending_embeddings: no tokio runtime — skipping");
                return 0;
            }
        };

        if embedded > 0 {
            tracing::info!(
                embedded_count = embedded,
                "retry_pending_embeddings: successfully embedded pending chunks"
            );

            // sqlite-vec persists writes immediately — no explicit save needed.
        }

        // Log any chunks that were discarded after max retries.
        if let Ok(store) = self.pending_embedding_store.lock() {
            let remaining = store.len();
            if remaining > 0 {
                tracing::info!(
                    remaining_pending = remaining,
                    "retry_pending_embeddings: chunks still pending"
                );
            }
        }

        embedded
    }

    /// Run periodic thread auto-archive: archives threads idle for >30 days.
    ///
    /// Called from the serve loop on a ~1 hour cadence. Returns the list of
    /// archive events that should be emitted to Matrix.
    pub fn run_periodic_auto_archive(&mut self, now: u64) -> Vec<DaemonEvent> {
        let mut tm_guard = match self.thread_manager.lock() {
            Ok(g) => g,
            Err(_) => {
                tracing::warn!("periodic_auto_archive: thread_manager lock poisoned");
                return Vec::new();
            }
        };
        let Some(ref mut tm) = *tm_guard else {
            return Vec::new();
        };

        let idle_threshold_secs = 30 * 24 * 3600; // 30 days
        let events = tm.auto_archive(now, idle_threshold_secs);

        if !events.is_empty() {
            tracing::info!(
                archived_count = events.len(),
                "periodic_auto_archive: archived idle threads"
            );
            // Persist the registry after archiving.
            if let Err(e) = tm.save() {
                tracing::warn!(
                    error = %e,
                    "periodic_auto_archive: failed to save thread registry"
                );
            }
        }

        events
    }

    /// Run periodic entity deduplication: merges duplicate entities in the memory store.
    ///
    /// Called from the serve loop on a ~24 hour cadence. Idempotent — safe to
    /// run daily even though weekly would suffice.
    pub fn run_periodic_entity_dedup(&self) {
        let Some(ref synthesizer) = self.periodic_synthesizer else {
            tracing::debug!("periodic_entity_dedup: no synthesizer configured, skipping");
            return;
        };

        let store = self.memory_store.as_ref();

        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => std::thread::scope(|s| {
                s.spawn(move || handle.block_on(synthesizer.run_entity_dedup(store)))
                    .join()
                    .expect("entity dedup thread should not panic")
            }),
            Err(_) => {
                tracing::warn!("periodic_entity_dedup: no tokio runtime — skipping");
                return;
            }
        };

        match result {
            Ok(reports) => {
                if !reports.is_empty() {
                    tracing::info!(
                        merge_count = reports.len(),
                        "periodic_entity_dedup: completed merges"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "periodic_entity_dedup: failed"
                );
            }
        }
    }

    /// Run periodic friction detection: scan threads for structural problems and
    /// return proposal events to emit.
    ///
    /// Called from the serve loop on a ~24 hour cadence alongside entity dedup.
    pub async fn run_periodic_friction_detection(&self) -> Vec<DaemonEvent> {
        let Some(ref synthesizer) = self.periodic_synthesizer else {
            tracing::debug!("periodic_friction_detection: no synthesizer configured, skipping");
            return Vec::new();
        };

        let graph_snapshot = match self.memory_store.graph_maintenance_snapshot().await {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "periodic_friction_detection: failed to build graph maintenance snapshot"
                );
                None
            }
        };

        let tm_guard = match self.thread_manager.lock() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::warn!("periodic_friction_detection: thread_manager lock poisoned");
                return Vec::new();
            }
        };

        let Some(ref thread_manager) = *tm_guard else {
            tracing::debug!("periodic_friction_detection: no thread_manager, skipping");
            return Vec::new();
        };

        let proposals = synthesizer.run_friction_detection(thread_manager, graph_snapshot.as_ref());

        proposals
            .into_iter()
            .map(|routed| {
                let thread_id = routed.thread_id;
                let proposal = routed.proposal;
                // Store proposal so it can be approved later.
                let proposal_id = proposals::generate_proposal_id();
                let pending = proposals::PendingProposal {
                    id: proposal_id.clone(),
                    source: proposals::ProposalSource::Friction,
                    description: proposal.description.clone(),
                    suggestion: proposal.suggestion.clone(),
                    thread_id: thread_id.clone(),
                };
                self.proposal_store.insert(pending);

                let body = format!(
                    "{}\n\nSuggestion: {}",
                    proposal.description, proposal.suggestion
                );
                let quick_replies = serde_json::to_string(&[
                    format!("proposal.approve {proposal_id}"),
                    format!("proposal.dismiss {proposal_id}"),
                ])
                .ok();
                DaemonEvent {
                    event_type: EventType::StructuralProposal,
                    status: "pending".to_string(),
                    job_id: Some(proposal_id),
                    detail: body,
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies,
                    thread_id,
                }
            })
            .collect()
    }

    fn execute_review_enqueue_job(&self, job: QueueJob, now: u64) -> Result<DaemonEvent> {
        let review = decode_review_payload_full(&job.payload)
            .with_context(|| format!("invalid review enqueue payload for job {}", job.job_id))?;
        let record_id = review.record_id;
        let run_id = review.run_id;
        // Propagate run_id into the archive.review job payload
        let review_payload = encode_review_payload(&record_id, run_id.as_deref());
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "archive.review".to_string(),
            payload: review_payload,
            idempotency_key: format!("review-run:{record_id}"),
            max_attempts: 3,
            next_run_at: now_unix(),
            force: false,
        });

        match outcome {
            Ok(_) => {
                self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
                Ok(DaemonEvent {
                    event_type: EventType::ArchiveReviewEnqueue,
                    status: "completed".to_string(),
                    job_id: Some(job.job_id),
                    detail: format!("review queued for {record_id}"),
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: run_id.clone(),
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
            Err(err) => {
                let outcome = self.queue.fail(
                    &job.job_id,
                    &self.config.worker_id,
                    now,
                    self.config.retry_backoff_seconds,
                    &err.to_string(),
                )?;
                Ok(DaemonEvent {
                    event_type: EventType::ArchiveReviewEnqueue,
                    status: if outcome == FailOutcome::MovedToDlq {
                        "dlq".to_string()
                    } else {
                        "retry".to_string()
                    },
                    job_id: Some(job.job_id),
                    detail: err.to_string(),
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: run_id,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
        }
    }

    fn execute_review_job(&self, job: QueueJob, now: u64) -> Result<DaemonEvent> {
        let review = decode_review_payload_full(&job.payload)
            .with_context(|| format!("invalid review payload for job {}", job.job_id))?;
        let record_id = review.record_id;
        let run_id = review.run_id;
        let Some(document) = self.archive_store.get(&record_id)? else {
            let outcome = self.queue.fail(
                &job.job_id,
                &self.config.worker_id,
                now,
                self.config.retry_backoff_seconds,
                "archive record missing",
            )?;
            return Ok(DaemonEvent {
                event_type: EventType::ArchiveReview,
                status: if outcome == FailOutcome::MovedToDlq {
                    "dlq".to_string()
                } else {
                    "retry".to_string()
                },
                job_id: Some(job.job_id),
                detail: format!("missing record {record_id}"),
                goal_room: None,
                goal_template: None,
                goal_run_id: None,
                goal_id: None,
                intake_run_id: run_id,
                url: None,
                title: None,
                sensitivity: None,
                quick_replies: None,
                thread_id: None,
            });
        };

        let doc_url = document.source_url.clone();
        let doc_title = extract_title_from_markdown(&document.content);
        let mut tags = document.tags.clone();
        tags.push("review/auto".to_string());
        let summary = self.review_engine.summarize(&document.content);
        let result = self.review_store.store(ReviewRequest {
            record_id: record_id.clone(),
            source_url: document.source_url.clone(),
            tags,
            tldr: summary,
        });

        match result {
            Ok(_) => {
                self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
                Ok(DaemonEvent {
                    event_type: EventType::ArchiveReview,
                    status: "completed".to_string(),
                    job_id: Some(job.job_id),
                    detail: format!("review stored for {record_id}"),
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: run_id.clone(),
                    url: doc_url.clone(),
                    title: doc_title.clone(),
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
            Err(err) => {
                let outcome = self.queue.fail(
                    &job.job_id,
                    &self.config.worker_id,
                    now,
                    self.config.retry_backoff_seconds,
                    &err.to_string(),
                )?;
                Ok(DaemonEvent {
                    event_type: EventType::ArchiveReview,
                    status: if outcome == FailOutcome::MovedToDlq {
                        "dlq".to_string()
                    } else {
                        "retry".to_string()
                    },
                    job_id: Some(job.job_id),
                    detail: err.to_string(),
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: run_id,
                    url: doc_url,
                    title: doc_title,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
        }
    }

    fn execute_auth_issue_job(&self, job: QueueJob, now: u64) -> Result<DaemonEvent> {
        let (target, scopes) = decode_auth_issue_payload(&job.payload)
            .with_context(|| format!("invalid auth payload for job {}", job.job_id))?;
        match self.issue_login_session_handle(&target, scopes, now) {
            Ok(handle_id) => {
                self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
                Ok(DaemonEvent {
                    event_type: EventType::AuthIssue,
                    status: "completed".to_string(),
                    job_id: Some(job.job_id),
                    detail: handle_id,
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
            Err(err) => {
                if is_terminal_auth_issue_error(&err) {
                    self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
                    return Ok(DaemonEvent {
                        event_type: EventType::AuthIssue,
                        status: "failed".to_string(),
                        job_id: Some(job.job_id),
                        detail: err.to_string(),
                        goal_room: None,
                        goal_template: None,
                        goal_run_id: None,
                        goal_id: None,
                        intake_run_id: None,
                        url: None,
                        title: None,
                        sensitivity: None,
                        quick_replies: None,
                        thread_id: None,
                    });
                }
                let outcome = self.queue.fail(
                    &job.job_id,
                    &self.config.worker_id,
                    now,
                    self.config.retry_backoff_seconds,
                    &err.to_string(),
                )?;
                Ok(DaemonEvent {
                    event_type: EventType::AuthIssue,
                    status: if outcome == FailOutcome::MovedToDlq {
                        "dlq".to_string()
                    } else {
                        "retry".to_string()
                    },
                    job_id: Some(job.job_id),
                    detail: err.to_string(),
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
        }
    }

    fn execute_bookmarks_sync_job(&self, job: QueueJob, now: u64) -> Result<DaemonEvent> {
        let (source, limit) = decode_bookmarks_payload(&job.payload)
            .with_context(|| format!("invalid bookmarks payload for job {}", job.job_id))?;
        let urls = match self.bookmarks_client.list_bookmark_urls(source, limit) {
            Ok(urls) => urls,
            Err(err) => {
                let outcome = self.queue.fail(
                    &job.job_id,
                    &self.config.worker_id,
                    now,
                    self.config.retry_backoff_seconds,
                    &err.to_string(),
                )?;
                return Ok(DaemonEvent {
                    event_type: EventType::BookmarksSync,
                    status: if outcome == FailOutcome::MovedToDlq {
                        "dlq".to_string()
                    } else {
                        "retry".to_string()
                    },
                    job_id: Some(job.job_id),
                    detail: err.to_string(),
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                });
            }
        };

        let queue_executor = QueueIntakeExecutor {
            queue: self.queue.clone(),
        };
        let result = queue_executor.execute(IntakeRequest {
            source: IntakeSource::Bookmarks,
            kind: IntakeKind::Url,
            urls,
            note: None,
            tags: vec![
                "source/bookmarks".to_string(),
                format!("source/bookmarks-{}", source.as_str()),
            ],
            file_path: None,
            title: None,
        })?;

        if result.summary.failed > 0 {
            let outcome = self.queue.fail(
                &job.job_id,
                &self.config.worker_id,
                now,
                self.config.retry_backoff_seconds,
                "failed to enqueue one or more bookmark urls",
            )?;
            return Ok(DaemonEvent {
                event_type: EventType::BookmarksSync,
                status: if outcome == FailOutcome::MovedToDlq {
                    "dlq".to_string()
                } else {
                    "retry".to_string()
                },
                job_id: Some(job.job_id),
                detail: format!(
                    "source={} total={} ingested={} duplicate={} failed={}",
                    source.as_str(),
                    result.summary.total,
                    result.summary.ingested,
                    result.summary.duplicates,
                    result.summary.failed
                ),
                goal_room: None,
                goal_template: None,
                goal_run_id: None,
                goal_id: None,
                intake_run_id: None,
                url: None,
                title: None,
                sensitivity: None,
                quick_replies: None,
                thread_id: None,
            });
        }

        self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
        Ok(DaemonEvent {
            event_type: EventType::BookmarksSync,
            status: "completed".to_string(),
            job_id: Some(job.job_id),
            detail: format!(
                "source={} total={} ingested={} duplicate={}",
                source.as_str(),
                result.summary.total,
                result.summary.ingested,
                result.summary.duplicates
            ),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        })
    }
}

// --- Payload types and codecs remaining in lib.rs ---

#[derive(Debug, Clone)]
pub(crate) struct IngestPayload {
    pub url: String,
    pub tags: Vec<String>,
    pub source: IntakeSource,
    /// Intake run_id for correlating pipeline events.
    pub run_id: String,
}

pub(crate) fn encode_ingest_payload(payload: &IngestPayload) -> String {
    let escaped_tags: Vec<String> = payload.tags.iter().map(|tag| escape_field(tag)).collect();
    format!(
        "{}|{}|{}|{}",
        escape_field(&payload.url),
        escaped_tags.join(","),
        source_to_string(&payload.source),
        escape_field(&payload.run_id)
    )
}

pub(crate) fn decode_ingest_payload(encoded: &str) -> Result<IngestPayload> {
    let parts: Vec<&str> = encoded.splitn(4, '|').collect();
    if parts.len() < 3 {
        return Err(anyhow!("ingest payload must have at least 3 parts"));
    }
    let tags = if parts[1].is_empty() {
        Vec::new()
    } else {
        parts[1].split(',').map(unescape_field).collect()
    };
    // 4th field is run_id (added for pipeline event correlation).
    // Fall back to a generated value for payloads encoded before this field existed.
    let run_id = if parts.len() >= 4 && !parts[3].is_empty() {
        unescape_field(parts[3])
    } else {
        format!("run_{}", now_unix())
    };
    Ok(IngestPayload {
        url: unescape_field(parts[0]),
        tags,
        source: parse_source(parts[2])?,
        run_id,
    })
}

fn decode_auth_issue_payload(encoded: &str) -> Result<(String, Vec<String>)> {
    let parts: Vec<&str> = encoded.splitn(2, '|').collect();
    if parts.len() != 2 {
        return Err(anyhow!("auth payload must have 2 parts"));
    }
    let target = unescape_field(parts[0]);
    let scopes = if parts[1].is_empty() {
        Vec::new()
    } else {
        parts[1]
            .split(',')
            .map(unescape_field)
            .map(|scope| scope.to_ascii_lowercase())
            .filter(|scope| !scope.is_empty())
            .collect()
    };
    if scopes.is_empty() {
        return Err(anyhow!("auth payload must include at least one scope"));
    }
    Ok((target, scopes))
}

/// Parsed review payload containing the record_id and optional run_id.
pub(crate) struct ReviewPayload {
    pub record_id: String,
    pub run_id: Option<String>,
}

pub(crate) fn decode_review_payload_full(encoded: &str) -> Result<ReviewPayload> {
    let payload = encoded.trim();
    // Split on '|' to separate key=value pairs
    let parts: Vec<&str> = payload.splitn(2, '|').collect();
    let record_part = parts[0];
    let value = record_part
        .strip_prefix("record_id=")
        .ok_or_else(|| anyhow!("review payload must start with record_id="))?;
    let record_id = unescape_field(value);
    if record_id.trim().is_empty() {
        return Err(anyhow!("review payload missing record_id"));
    }
    let run_id = if parts.len() > 1 {
        parts[1]
            .strip_prefix("run_id=")
            .map(unescape_field)
            .filter(|v| !v.trim().is_empty())
    } else {
        None
    };
    Ok(ReviewPayload { record_id, run_id })
}

#[cfg(test)]
fn decode_review_payload(encoded: &str) -> Result<String> {
    Ok(decode_review_payload_full(encoded)?.record_id)
}

pub(crate) fn encode_review_payload(record_id: &str, run_id: Option<&str>) -> String {
    match run_id {
        Some(rid) => format!(
            "record_id={}|run_id={}",
            escape_field(record_id),
            escape_field(rid)
        ),
        None => format!("record_id={}", escape_field(record_id)),
    }
}

fn decode_bookmarks_payload(encoded: &str) -> Result<(BookmarksSource, u32)> {
    let parts: Vec<&str> = encoded.splitn(2, '|').collect();
    if parts.len() != 2 {
        return Err(anyhow!("bookmarks payload must have 2 parts"));
    }
    let source = parse_bookmarks_source(&unescape_field(parts[0]))?;
    let limit = parts[1]
        .parse::<u32>()
        .map_err(|_| anyhow!("invalid bookmarks limit"))?;
    if limit == 0 {
        return Err(anyhow!("bookmarks limit must be > 0"));
    }
    Ok((source, limit))
}

fn parse_bookmarks_source(value: &str) -> Result<BookmarksSource> {
    match value.to_ascii_lowercase().as_str() {
        "api" => Ok(BookmarksSource::Api),
        "browser" => Ok(BookmarksSource::Browser),
        other => Err(anyhow!("unsupported bookmarks source {other}")),
    }
}

pub(crate) fn source_to_string(source: &IntakeSource) -> &'static str {
    match source {
        IntakeSource::Cli => "cli",
        IntakeSource::Matrix => "matrix",
        IntakeSource::Share => "share",
        IntakeSource::Bookmarks => "bookmarks",
        IntakeSource::Notes => "notes",
        IntakeSource::Api => "api",
    }
}

pub(crate) fn parse_source(value: &str) -> Result<IntakeSource> {
    match value {
        "cli" => Ok(IntakeSource::Cli),
        "matrix" => Ok(IntakeSource::Matrix),
        "share" => Ok(IntakeSource::Share),
        "bookmarks" => Ok(IntakeSource::Bookmarks),
        "notes" => Ok(IntakeSource::Notes),
        "api" => Ok(IntakeSource::Api),
        other => Err(anyhow!("unknown intake source: {other}")),
    }
}

pub(crate) fn load_access_broker(path: &Path) -> Result<AccessBroker> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create capability dir {}", parent.display()))?;
        harden_dir_permissions(parent, 0o700)?;
    }
    if !path.exists() {
        fs::write(path, "[]")
            .with_context(|| format!("failed to initialize capability store {}", path.display()))?;
    }
    harden_file_permissions(path, 0o600)?;
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read capability store {}", path.display()))?;
    let tokens = if raw.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str::<Vec<symbiotic_trust::CapabilityToken>>(&raw)
            .with_context(|| format!("failed to parse capability store {}", path.display()))?
    };
    Ok(AccessBroker::from_tokens(tokens))
}

pub(crate) fn persist_access_broker(path: &Path, broker: &AccessBroker) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create capability dir {}", parent.display()))?;
        harden_dir_permissions(parent, 0o700)?;
    }
    let payload = serde_json::to_string_pretty(&broker.tokens())?;
    fs::write(path, payload)
        .with_context(|| format!("failed to write capability store {}", path.display()))?;
    harden_file_permissions(path, 0o600)?;
    Ok(())
}

pub(crate) fn escape_field(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('|', "%7C")
        .replace(',', "%2C")
}

pub(crate) fn unescape_field(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let a = chars.next();
        let b = chars.next();
        match (a, b) {
            (Some('2'), Some('5')) => out.push('%'),
            (Some('7'), Some('C')) => out.push('|'),
            (Some('2'), Some('C')) => out.push(','),
            (Some(x), Some(y)) => {
                out.push('%');
                out.push(x);
                out.push(y);
            }
            _ => out.push('%'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Blob store (Tier 3) helpers
// ---------------------------------------------------------------------------

/// Build the one-shot auth sandbox launcher from daemon config.
///
/// When `auth_scripts_dir` is configured and points to a valid directory,
/// constructs a `ScriptRegistry` from the directory and creates an
/// `AuthSandboxLauncher`. Returns `None` when the scripts directory is not set
/// or fails to scan (graceful degradation).
fn build_auth_engine(
    config: &DaemonConfig,
    _vault: Arc<dyn credential_gateway::CredentialVault>,
) -> Option<credential_gateway::auth_engine::AuthSandboxLauncher> {
    let scripts_dir = config.auth_scripts_dir.as_ref()?;

    let registry = match credential_gateway::script_registry::ScriptRegistry::from_dir(scripts_dir)
    {
        Ok(reg) => {
            let domains = reg.domains();
            tracing::info!(
                scripts_dir = %scripts_dir.display(),
                domain_count = domains.len(),
                "auth_engine: loaded script registry"
            );
            reg
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                scripts_dir = %scripts_dir.display(),
                "auth_engine: failed to load script registry — auth engine disabled"
            );
            return None;
        }
    };

    let worker_bin_name = format!("credential-gateway{}", std::env::consts::EXE_SUFFIX);
    let worker_bin = config
        .auth_sandbox_bin
        .clone()
        .or_else(|| {
            std::env::current_exe().ok().and_then(|path| {
                let sibling = path.with_file_name(&worker_bin_name);
                if sibling.exists() {
                    return Some(sibling);
                }

                path.parent()
                    .and_then(|dir| dir.parent())
                    .map(|dir| dir.join(&worker_bin_name))
                    .filter(|candidate| candidate.exists())
            })
        })
        .unwrap_or_else(|| std::path::PathBuf::from(worker_bin_name));

    let engine_config = credential_gateway::auth_engine::AuthSandboxLauncherConfig {
        worker_bin: worker_bin.clone(),
        vault_root: config.credential_vault_file.clone(),
        scripts_dir: scripts_dir.clone(),
        timeout: std::time::Duration::from_secs(60),
        node_bin: config.node_bin.clone(),
        goal_scope: None,
    };

    let engine = credential_gateway::auth_engine::AuthSandboxLauncher::new(registry, engine_config);
    tracing::info!(
        scripts_dir = %scripts_dir.display(),
        worker_bin = %worker_bin.display(),
        node_bin = ?config.node_bin,
        "auth_sandbox: initialized"
    );
    Some(engine)
}

/// Initialise the age-encrypted blob store from config.
///
/// When `key_file` is `Some` and contains a valid age identity, both the
/// `BlobStore` and the corresponding `Recipient` (public key) are returned.
/// On any error the store is disabled gracefully (returns `(None, None)`).
fn init_blob_store(
    root: &Path,
    key_file: Option<&Path>,
) -> (Option<BlobStore>, Option<AgeRecipient>) {
    let Some(key_path) = key_file else {
        tracing::info!("blob_store: no key file configured — Tier 3 encryption disabled");
        return (None, None);
    };

    let key_data = match fs::read_to_string(key_path) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %key_path.display(),
                "blob_store: failed to read key file — Tier 3 encryption disabled"
            );
            return (None, None);
        }
    };

    let identity: AgeIdentity = match key_data.trim().parse() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %key_path.display(),
                "blob_store: invalid age identity in key file — Tier 3 encryption disabled"
            );
            return (None, None);
        }
    };

    let recipient = identity.to_public();

    match BlobStore::new(root.to_path_buf()) {
        Ok(store) => {
            tracing::info!(
                root = %root.display(),
                "blob_store: initialised age-encrypted Tier 3 store"
            );
            (Some(store), Some(recipient))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                root = %root.display(),
                "blob_store: failed to create store directory — Tier 3 encryption disabled"
            );
            (None, None)
        }
    }
}

/// Auto-detect a `BlobCategory` from content keywords.
///
/// Uses simple keyword matching against lowercase content.
/// Falls back to `Custom("general")` when no specific category matches.
pub(crate) fn detect_blob_category(content: &str) -> BlobCategory {
    let lower = content.to_ascii_lowercase();

    // Medical keywords
    if lower.contains("diagnosis")
        || lower.contains("prescription")
        || lower.contains("bloodwork")
        || lower.contains("medical record")
        || lower.contains("patient")
        || lower.contains("cholesterol")
        || lower.contains("hemoglobin")
        || lower.contains("radiology")
        || lower.contains("pathology")
        || lower.contains("icd-10")
        || lower.contains("hipaa")
    {
        return BlobCategory::Medical;
    }

    // Financial keywords
    if lower.contains("tax return")
        || lower.contains("bank statement")
        || lower.contains("invoice")
        || lower.contains("balance sheet")
        || lower.contains("account number")
        || lower.contains("routing number")
        || lower.contains("1099")
        || lower.contains("w-2")
        || lower.contains("irs")
        || lower.contains("portfolio")
        || lower.contains("brokerage")
    {
        return BlobCategory::Financial;
    }

    // Legal keywords
    if lower.contains("contract")
        || lower.contains("non-disclosure")
        || lower.contains("nda")
        || lower.contains("power of attorney")
        || lower.contains("last will")
        || lower.contains("subpoena")
        || lower.contains("affidavit")
        || lower.contains("plaintiff")
        || lower.contains("defendant")
    {
        return BlobCategory::Legal;
    }

    // Credential keywords
    if lower.contains("password")
        || lower.contains("api_key")
        || lower.contains("secret_key")
        || lower.contains("-----begin private key-----")
        || lower.contains("-----begin rsa private key-----")
        || lower.contains("access_token")
    {
        return BlobCategory::Credential;
    }

    BlobCategory::Custom("general".to_string())
}

impl SymbioticDaemon {
    /// Route a Private-sensitivity document to the age-encrypted blob store.
    ///
    /// After content is ingested and sensitivity is determined to be `Private`:
    /// 1. Auto-detects the `BlobCategory` from content keywords.
    /// 2. Stores the encrypted blob via `BlobStore::store()`.
    /// 3. Overwrites the archive entry with a metadata-only placeholder
    ///    (no plaintext content).
    /// 4. Logs the routing decision.
    ///
    /// Best-effort: errors are logged but never fail the ingest pipeline.
    pub(crate) fn route_private_to_blob_store(&self, record_id: &str, run_id: &str) {
        let Some(ref blob_store) = self.encrypted_blob_store else {
            return; // Blob store not configured — skip.
        };
        let Some(ref recipient) = self.blob_recipient else {
            return; // No encryption key — skip.
        };

        // Load document from archive.
        let document = match self.archive_store.get(record_id) {
            Ok(Some(doc)) => doc,
            Ok(None) => {
                tracing::warn!(
                    record_id = record_id,
                    run_id = run_id,
                    "blob_route: record not found in archive"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    record_id = record_id,
                    run_id = run_id,
                    error = %e,
                    "blob_route: failed to load document from archive"
                );
                return;
            }
        };

        // Only route Private content.
        if document.sensitivity != symbiotic_archive::ArchiveSensitivity::Private {
            return;
        }

        let category = detect_blob_category(&document.content);
        let content_bytes = document.content.as_bytes();
        let title = document.title.clone();

        let metadata = BlobMetadata {
            title: title.clone(),
            tags: document.tags.clone(),
            size_bytes: content_bytes.len() as u64,
            content_type: "text/markdown".to_string(),
        };

        // Store encrypted blob.
        match blob_store.store(
            record_id,
            category.clone(),
            metadata,
            content_bytes,
            &[recipient],
        ) {
            Ok(_blob) => {
                tracing::info!(
                    record_id = record_id,
                    run_id = run_id,
                    category = %category,
                    title = %title,
                    size_bytes = content_bytes.len(),
                    "blob_route: Private content stored in encrypted blob store"
                );
            }
            Err(e) => {
                tracing::warn!(
                    record_id = record_id,
                    run_id = run_id,
                    error = %e,
                    "blob_route: failed to store encrypted blob — content remains in archive"
                );
                return;
            }
        }

        // Replace archive content with metadata-only placeholder.
        let placeholder = format!(
            "---\nblob_id: {record_id}\nstatus: encrypted\ncategory: {category}\n\
             title: {title}\n---\n\n> This entry is Tier 3 (Private). \
             Content is stored in the age-encrypted blob store.\n"
        );

        let update_result = self.archive_store.update_content(
            record_id,
            &placeholder,
            Some(&format!("[encrypted] {title}")),
            &["tier3/encrypted".to_string()],
        );

        match update_result {
            Ok(true) => {
                tracing::info!(
                    record_id = record_id,
                    run_id = run_id,
                    "blob_route: archive entry replaced with metadata-only placeholder"
                );
            }
            Ok(false) => {
                tracing::warn!(
                    record_id = record_id,
                    run_id = run_id,
                    "blob_route: archive record not found for placeholder update"
                );
            }
            Err(e) => {
                tracing::warn!(
                    record_id = record_id,
                    run_id = run_id,
                    error = %e,
                    "blob_route: failed to replace archive entry — plaintext remains"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
