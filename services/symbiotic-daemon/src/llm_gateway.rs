//! JSON-RPC gateway for sandboxed agent runners.
//!
//! Provides a Unix domain socket server that handles LLM completions,
//! remote tool execution, and goal status updates from decoupled
//! agent processes.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use credential_gateway::auth_engine::AuthSandboxLauncher;
use credential_gateway::{
    AuthRequest, CredentialGateway, GoalScopedVault, SessionPolicy, SessionType,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

use symbiotic_agent_runner::{ExecutionCheckpointArtifact, ExecutionContextPacket};
use symbiotic_agents::builtin_tools::{
    ArchiveTool, AskUserTool, CapabilityChecker, GeneratePlanTool, QueueTool, RecallTool,
};
use symbiotic_archive::FileArchiveStore;
use symbiotic_context::RecallGateway;
use symbiotic_core::protocol::{ChatMessage, LlmClient, Tool, ToolResult};
use symbiotic_git_swarm::types::PushSessionRequest;
use symbiotic_memory::tool_memory::{self, ToolInvocation, ToolMemoryStore};
use symbiotic_providers::ProviderRouter;
use symbiotic_queue::QueueBackend;
use symbiotic_trust::{now_unix, AccessBroker, AccessRequest};

use crate::agent_runtime_status::{
    AgentRuntimeProfile, AgentRuntimeStatusKind, AgentRuntimeStatusStore,
};
use crate::agents::ProviderRouterLlmClient;
use crate::auth_approval_policies::AuthApprovalPolicyStore;
use crate::auth_jobs::{AuthJobConfig, AuthJobRequest, AuthJobStore};
use crate::auth_orchestrator::BridgeAuthOrchestrator;
use crate::bridge_interactions::{
    AgentRuntimeLogEntryType, AgentRuntimeLogRecord, AgentRuntimeLogStore, BridgeCheckpointStore,
    BridgeInteractionKind, BridgeInteractionLogStore, BridgeInteractionRecord, BridgeSessionStore,
    PersistedCheckpointArtifact,
};
use crate::llm_audit::{self, LlmAuditEntry, LlmAuditEntryKind, LlmAuditLog, LlmAuditQuery};
use crate::swarm_server::SwarmServer;
use crate::tool_adapters::{
    scope_to_trust_level, DaemonArchiveBackend, DaemonCapabilityChecker, DaemonQueueBackend,
    DaemonRecallBackend,
};

pub struct LlmGateway {
    provider_router: Arc<ProviderRouter>,
    archive_store: Arc<FileArchiveStore>,
    queue: Arc<dyn QueueBackend>,
    recall_gateway: Arc<RecallGateway>,
    swarm: Option<Arc<SwarmServer>>,
    broker: Arc<Mutex<AccessBroker>>,
    credential_gateway: Arc<CredentialGateway>,
    credential_vault: Arc<GoalScopedVault>,
    auth_engine: Option<AuthSandboxLauncher>,
    auth_jobs: Arc<Mutex<AuthJobStore>>,
    auth_approval_policies: Arc<Mutex<AuthApprovalPolicyStore>>,
    session_store: Arc<Mutex<BridgeSessionStore>>,
    interaction_log_store: Arc<Mutex<BridgeInteractionLogStore>>,
    agent_runtime_log_store: Arc<Mutex<AgentRuntimeLogStore>>,
    agent_runtime_status_store: Arc<Mutex<AgentRuntimeStatusStore>>,
    checkpoint_store: Arc<Mutex<BridgeCheckpointStore>>,
    audit_log: Arc<Mutex<LlmAuditLog>>,
    tool_memory: Arc<Mutex<ToolMemoryStore>>,
    credentials_room_id: String,
    auth_job_config: AuthJobConfig,
    socket_path: String,
    world_accessible: bool,
}

impl LlmGateway {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider_router: Arc<ProviderRouter>,
        archive_store: Arc<FileArchiveStore>,
        queue: Arc<dyn QueueBackend>,
        recall_gateway: Arc<RecallGateway>,
        swarm: Option<Arc<SwarmServer>>,
        broker: Arc<Mutex<AccessBroker>>,
        credential_gateway: Arc<CredentialGateway>,
        credential_vault: Arc<GoalScopedVault>,
        auth_engine: Option<AuthSandboxLauncher>,
        auth_jobs: Arc<Mutex<AuthJobStore>>,
        auth_approval_policies: Arc<Mutex<AuthApprovalPolicyStore>>,
        session_store: Arc<Mutex<BridgeSessionStore>>,
        interaction_log_store: Arc<Mutex<BridgeInteractionLogStore>>,
        agent_runtime_log_store: Arc<Mutex<AgentRuntimeLogStore>>,
        agent_runtime_status_store: Arc<Mutex<AgentRuntimeStatusStore>>,
        checkpoint_store: Arc<Mutex<BridgeCheckpointStore>>,
        audit_log: Arc<Mutex<LlmAuditLog>>,
        tool_memory: Arc<Mutex<ToolMemoryStore>>,
        credentials_room_id: &str,
        auth_job_config: AuthJobConfig,
        socket_path: &str,
        world_accessible: bool,
    ) -> Self {
        Self {
            provider_router,
            archive_store,
            queue,
            recall_gateway,
            swarm,
            broker,
            credential_gateway,
            credential_vault,
            auth_engine,
            auth_jobs,
            auth_approval_policies,
            session_store,
            interaction_log_store,
            agent_runtime_log_store,
            agent_runtime_status_store,
            checkpoint_store,
            audit_log,
            tool_memory,
            credentials_room_id: credentials_room_id.to_string(),
            auth_job_config,
            socket_path: socket_path.to_string(),
            world_accessible,
        }
    }

    pub async fn run(self) -> Result<()> {
        let path = Path::new(&self.socket_path);
        if path.exists() {
            tokio::fs::remove_file(path).await?;
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let listener = UnixListener::bind(path)?;

        let mode = socket_mode(self.world_accessible);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;

        info!(socket = %self.socket_path, mode = format!("{mode:o}"), "LLM Gateway listening");

        let state = Arc::new(GatewayState {
            provider_router: self.provider_router,
            archive_store: self.archive_store,
            queue: self.queue,
            recall_gateway: self.recall_gateway,
            swarm: self.swarm,
            broker: self.broker,
            credential_gateway: self.credential_gateway,
            credential_vault: self.credential_vault,
            auth_engine: self.auth_engine,
            auth_jobs: self.auth_jobs,
            auth_approval_policies: self.auth_approval_policies,
            session_store: self.session_store,
            interaction_log_store: self.interaction_log_store,
            agent_runtime_log_store: self.agent_runtime_log_store,
            agent_runtime_status_store: self.agent_runtime_status_store,
            checkpoint_store: self.checkpoint_store,
            audit_log: self.audit_log,
            tool_memory: self.tool_memory,
            credentials_room_id: self.credentials_room_id,
            auth_job_config: self.auth_job_config,
        });

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, state).await {
                            error!(error = %e, "LLM Gateway connection error");
                        }
                    });
                }
                Err(e) => {
                    error!(error = %e, "LLM Gateway accept error");
                }
            }
        }
    }
}

fn socket_mode(world_accessible: bool) -> u32 {
    if world_accessible {
        0o666
    } else {
        0o600
    }
}

struct GatewayState {
    provider_router: Arc<ProviderRouter>,
    archive_store: Arc<FileArchiveStore>,
    queue: Arc<dyn QueueBackend>,
    recall_gateway: Arc<RecallGateway>,
    swarm: Option<Arc<SwarmServer>>,
    broker: Arc<Mutex<AccessBroker>>,
    credential_gateway: Arc<CredentialGateway>,
    credential_vault: Arc<GoalScopedVault>,
    auth_engine: Option<AuthSandboxLauncher>,
    auth_jobs: Arc<Mutex<AuthJobStore>>,
    auth_approval_policies: Arc<Mutex<AuthApprovalPolicyStore>>,
    session_store: Arc<Mutex<BridgeSessionStore>>,
    interaction_log_store: Arc<Mutex<BridgeInteractionLogStore>>,
    agent_runtime_log_store: Arc<Mutex<AgentRuntimeLogStore>>,
    agent_runtime_status_store: Arc<Mutex<AgentRuntimeStatusStore>>,
    checkpoint_store: Arc<Mutex<BridgeCheckpointStore>>,
    audit_log: Arc<Mutex<LlmAuditLog>>,
    tool_memory: Arc<Mutex<ToolMemoryStore>>,
    credentials_room_id: String,
    auth_job_config: AuthJobConfig,
}

#[derive(Debug, Clone)]
struct AuthenticatedSession {
    agent_id: String,
    goal_scope: Option<String>,
    bridge_token_id: String,
    thread_id: Option<String>,
}

async fn handle_connection(stream: UnixStream, state: Arc<GatewayState>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut session: Option<AuthenticatedSession> = None;

    while let Some(line) = lines.next_line().await? {
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32700, "message": format!("Parse error: {}", e) },
                    "id": null
                });
                writer
                    .write_all(serde_json::to_string(&err_resp)?.as_bytes())
                    .await?;
                writer.write_all(b"\n").await?;
                continue;
            }
        };

        if let Some(response) = process_request(request, &state, &mut session).await? {
            writer
                .write_all(serde_json::to_string(&response)?.as_bytes())
                .await?;
            writer.write_all(b"\n").await?;
        }
    }
    Ok(())
}

async fn process_request(
    request: Value,
    state: &GatewayState,
    session: &mut Option<AuthenticatedSession>,
) -> Result<Option<Value>> {
    let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = request.get("id").cloned();
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let expects_response = id.is_some();

    if method == "bridge.handshake" {
        let result = handle_bridge_handshake(params, state, session).await;
        return Ok(jsonrpc_response(id, result, expects_response));
    }

    let Some(authenticated) = session.as_ref() else {
        return Ok(jsonrpc_response(
            id,
            Err(anyhow!("bridge.handshake required before {method}")),
            expects_response,
        ));
    };

    let result = if method.starts_with("pr.")
        || method.starts_with("swarm.")
        || method.starts_with("check.")
        || method.starts_with("distillery.")
    {
        handle_swarm_rpc(method, params, state, authenticated).await
    } else {
        match method {
            "llm.chat" => handle_llm_chat(params, state, authenticated).await,
            "llm.audit.query" => handle_llm_audit_query(params, state, authenticated).await,
            "llm.audit.summary" => handle_llm_audit_summary(params, state, authenticated).await,
            "context.packet.load" => handle_context_packet_load(params, state, authenticated).await,
            "checkpoint.create" => handle_checkpoint_create(params, state, authenticated).await,
            "tool.execute" => handle_tool_execute(params, state, authenticated).await,
            "tool.stats" => handle_tool_stats(params, state, authenticated).await,
            "tool.affinity" => handle_tool_affinity(params, state, authenticated).await,
            "credential.request" => handle_credential_request(params, state, authenticated).await,
            "credential.authenticate" => {
                handle_credential_authenticate(params, state, authenticated).await
            }
            "goal.event" => handle_goal_event(params, state, authenticated).await,
            _ => Err(anyhow!("Method not found: {}", method)),
        }
    };

    Ok(jsonrpc_response(id, result, expects_response))
}

fn jsonrpc_response(
    id: Option<Value>,
    result: Result<Value>,
    expects_response: bool,
) -> Option<Value> {
    if !expects_response {
        if let Err(e) = &result {
            warn!(error = %e, "LLM Gateway notification failed");
        }
        return None;
    }

    Some(match result {
        Ok(res) => serde_json::json!({
            "jsonrpc": "2.0",
            "result": res,
            "id": id
        }),
        Err(e) => {
            warn!(error = %e, "LLM Gateway request failed");
            serde_json::json!({
                "jsonrpc": "2.0",
                "error": { "code": -32603, "message": e.to_string() },
                "id": id
            })
        }
    })
}

async fn handle_bridge_handshake(
    params: Value,
    state: &GatewayState,
    session: &mut Option<AuthenticatedSession>,
) -> Result<Value> {
    if session.is_some() {
        return Err(anyhow!("bridge session already authenticated"));
    }

    let agent_id = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing agent_id"))?
        .to_string();
    let token_id = params
        .get("token_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing token_id"))?
        .to_string();
    let runtime_profile = params
        .get("runtime_profile")
        .cloned()
        .map(serde_json::from_value::<AgentRuntimeProfile>)
        .transpose()
        .map_err(|e| anyhow!("invalid runtime_profile: {e}"))?;

    let token = {
        let broker = state
            .broker
            .lock()
            .map_err(|_| anyhow!("access broker lock poisoned"))?;
        broker
            .get_token(&token_id)
            .cloned()
            .ok_or_else(|| anyhow!("bridge token not found"))?
    };

    if token.subject != agent_id {
        return Err(anyhow!(
            "bridge token subject mismatch: token belongs to '{}'",
            token.subject
        ));
    }

    let request = AccessRequest {
        subject: agent_id.clone(),
        required_level: scope_to_trust_level("bridge.connect"),
        scope: "bridge.connect".to_string(),
        goal_scope: token.goal_scope.clone(),
    };

    {
        let mut broker = state
            .broker
            .lock()
            .map_err(|_| anyhow!("access broker lock poisoned"))?;
        broker.evaluate(&token_id, &request, now_unix())?;
    }

    if let Some(profile) = runtime_profile.clone() {
        let created_at = now_unix();
        let mut runtime_status_store = state
            .agent_runtime_status_store
            .lock()
            .map_err(|_| anyhow!("agent runtime status store lock poisoned"))?;
        runtime_status_store.record_handshake(
            agent_id.clone(),
            token_id.clone(),
            token.goal_scope.clone(),
            profile,
            created_at,
        )?;
    }

    *session = Some(AuthenticatedSession {
        agent_id: agent_id.clone(),
        goal_scope: token.goal_scope.clone(),
        bridge_token_id: token_id,
        thread_id: runtime_profile.and_then(|profile| profile.thread_id),
    });

    Ok(serde_json::json!({
        "agent_id": agent_id,
        "goal_scope": token.goal_scope,
    }))
}

fn authorize_session_scope(
    state: &GatewayState,
    session: &AuthenticatedSession,
    scope: &str,
) -> Result<()> {
    let request = AccessRequest {
        subject: session.agent_id.clone(),
        required_level: scope_to_trust_level(scope),
        scope: scope.to_string(),
        goal_scope: session.goal_scope.clone(),
    };
    let mut broker = state
        .broker
        .lock()
        .map_err(|_| anyhow!("access broker lock poisoned"))?;
    broker.evaluate(&session.bridge_token_id, &request, now_unix())?;
    Ok(())
}

async fn handle_llm_chat(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    let messages: Vec<ChatMessage> = serde_json::from_value(
        params
            .get("messages")
            .cloned()
            .ok_or_else(|| anyhow!("missing messages"))?,
    )?;
    let json_mode = params
        .get("json_mode")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let agent_id = session.agent_id.clone();

    authorize_session_scope(state, session, "llm.chat")?;

    let client = ProviderRouterLlmClient::new(
        Arc::clone(&state.provider_router),
        symbiotic_core::Sensitivity::Private,
        agent_id.clone(),
    );

    // Build prompt text for hashing/audit
    let prompt_text = messages
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt_hash = llm_audit::sha256_hex(&prompt_text);

    let start = Instant::now();
    let result = client.chat(&messages, json_mode).await;
    let latency_ms = start.elapsed().as_millis() as u64;

    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (_content, success, error_msg, completion_size, completion_text) = match &result {
        Ok(content) => (
            content.clone(),
            true,
            None,
            content.len(),
            Some(content.clone()),
        ),
        Err(e) => (String::new(), false, Some(e.to_string()), 0, None),
    };

    // Record audit entry (non-blocking: lock failures are logged but don't break the gateway)
    if let Ok(mut log) = state.audit_log.lock() {
        log.record(LlmAuditEntry {
            timestamp: now_epoch,
            kind: LlmAuditEntryKind::Chat,
            agent_id,
            model: "routed".to_string(),
            prompt_hash,
            completion_size,
            token_count: None,
            latency_ms,
            success,
            error: error_msg.clone(),
            prompt_text: Some(prompt_text),
            completion_text,
            verification: None,
        });
    }

    match result {
        Ok(content) => Ok(serde_json::json!({ "content": content })),
        Err(e) => Err(e),
    }
}

async fn handle_context_packet_load(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    authorize_session_scope(state, session, "bridge.connect")?;

    let goal = params
        .get("goal")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    let task_context = params
        .get("context")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let packet = ExecutionContextPacket {
        protocol_version: "v1".to_string(),
        agent_id: session.agent_id.clone(),
        goal_scope: session.goal_scope.clone(),
        thread_id: session.thread_id.clone(),
        goal,
        task_context,
        context_sources: vec![
            "CONTEXT.md".to_string(),
            "docs/NAMING-CANON.md".to_string(),
            "tasks/TASKS.md".to_string(),
            "tasks/NEXT.md".to_string(),
            "knowledge-base/operations/skills/operator-protocol/operator-protocol.md".to_string(),
        ],
        canonical_truth: "Canonical truth lives in Archive records under knowledge-base/, especially ledger/{type}/{slug}/{slug}.md. Generated briefs, thread docs, and retrieval indexes are derived surfaces and must not be edited as truth.".to_string(),
        derived_surfaces: "Derived surfaces include generated {slug}.brief.md artifacts, thread memory docs, Recall Gateway context packs, and SQLite or Neural Graph indexes. They may be refreshed or queried, but canonical mutation must target the underlying Archive records.".to_string(),
        verification_order: vec![
            "source-of-truth mutation".to_string(),
            "daemon or orchestrator follow-through".to_string(),
            "consuming UI or runner surface".to_string(),
            "lint / analyze / format on the touched boundary".to_string(),
        ],
        checkpoint_rule: "After each coherent slice, sync repo truth and emit a checkpoint artifact summarizing what landed, what was verified, and the next open boundary.".to_string(),
    };

    let mut store = state
        .session_store
        .lock()
        .map_err(|_| anyhow!("bridge session store lock poisoned"))?;
    store.record_context_packet(&session.bridge_token_id, packet.clone());
    drop(store);

    let created_at = now_unix();
    let mut interaction_store = state
        .interaction_log_store
        .lock()
        .map_err(|_| anyhow!("bridge interaction log store lock poisoned"))?;
    interaction_store.append(BridgeInteractionRecord {
        event_id: format!("{}:{created_at}:context", session.bridge_token_id),
        token_id: session.bridge_token_id.clone(),
        agent_id: session.agent_id.clone(),
        goal_scope: session.goal_scope.clone(),
        thread_id: None,
        kind: BridgeInteractionKind::ContextPacketLoaded,
        summary: "Loaded operator protocol context packet".to_string(),
        detail: packet.goal.clone(),
        created_at,
        raw_payload: serde_json::to_value(&packet)?,
    })?;

    Ok(serde_json::to_value(packet)?)
}

async fn handle_checkpoint_create(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    authorize_session_scope(state, session, "bridge.connect")?;
    let artifact: ExecutionCheckpointArtifact = serde_json::from_value(params)?;

    let mut store = state
        .session_store
        .lock()
        .map_err(|_| anyhow!("bridge session store lock poisoned"))?;
    store.record_checkpoint_artifact(&session.bridge_token_id, artifact.clone());
    drop(store);

    let created_at = now_unix();
    let mut checkpoint_store = state
        .checkpoint_store
        .lock()
        .map_err(|_| anyhow!("bridge checkpoint store lock poisoned"))?;
    checkpoint_store.append(PersistedCheckpointArtifact {
        token_id: session.bridge_token_id.clone(),
        agent_id: session.agent_id.clone(),
        goal_scope: session.goal_scope.clone(),
        created_at,
        artifact: artifact.clone(),
    })?;

    Ok(serde_json::json!({
        "stored": true,
        "agent_id": artifact.agent_id,
        "iterations": artifact.iterations,
    }))
}

async fn handle_tool_execute(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing tool name"))?;
    let tool_params = params.get("params").cloned().unwrap_or(Value::Null);
    let agent_id = session.agent_id.as_str();

    let caps = Arc::new(DaemonCapabilityChecker::for_goal_scope(
        Arc::clone(&state.broker),
        session.goal_scope.clone(),
    ));

    let input_hash =
        tool_memory::sha256_hex(&serde_json::to_string(&tool_params).unwrap_or_default());
    let start = Instant::now();

    let result: std::result::Result<ToolResult, anyhow::Error> = match name {
        "recall" => {
            let backend = Arc::new(DaemonRecallBackend::with_vector_search(
                Arc::clone(&state.archive_store),
                state
                    .recall_gateway
                    .vector_index()
                    .cloned()
                    .ok_or_else(|| anyhow!("vector index unavailable"))?,
                Arc::clone(&state.provider_router),
            ));
            let tool = RecallTool::new(agent_id.to_string(), backend, caps);
            tool.execute(tool_params).await
        }
        "archive" => {
            let backend = Arc::new(DaemonArchiveBackend::new(Arc::clone(&state.archive_store)));
            let tool = ArchiveTool::new(agent_id.to_string(), backend, caps);
            tool.execute(tool_params).await
        }
        "queue" => {
            let backend = Arc::new(DaemonQueueBackend::new(Arc::clone(&state.queue)));
            let tool = QueueTool::new(agent_id.to_string(), backend, caps);
            tool.execute(tool_params).await
        }
        "ask_user" => {
            let (tool, pending_question) = AskUserTool::new();
            let result = tool.execute(tool_params).await;
            if let Some(question) = pending_question.lock().ok().and_then(|guard| guard.clone()) {
                let mut store = state
                    .session_store
                    .lock()
                    .map_err(|_| anyhow!("bridge session store lock poisoned"))?;
                store.record_pending_question(&session.bridge_token_id, question.clone());
                drop(store);

                let created_at = now_unix();
                let mut interaction_store = state
                    .interaction_log_store
                    .lock()
                    .map_err(|_| anyhow!("bridge interaction log store lock poisoned"))?;
                interaction_store.append(BridgeInteractionRecord {
                    event_id: format!("{}:{created_at}:question", session.bridge_token_id),
                    token_id: session.bridge_token_id.clone(),
                    agent_id: session.agent_id.clone(),
                    goal_scope: session.goal_scope.clone(),
                    thread_id: None,
                    kind: BridgeInteractionKind::PendingQuestion,
                    summary: "Needs user input".to_string(),
                    detail: question.text.clone(),
                    created_at,
                    raw_payload: serde_json::to_value(&question)?,
                })?;
            }
            result
        }
        "generate_plan" => {
            let (tool, pending_plan) = GeneratePlanTool::new();
            let result = tool.execute(tool_params).await;
            if let Some(plan) = pending_plan.lock().ok().and_then(|guard| guard.clone()) {
                let mut store = state
                    .session_store
                    .lock()
                    .map_err(|_| anyhow!("bridge session store lock poisoned"))?;
                store.record_pending_plan(&session.bridge_token_id, plan.clone());
                drop(store);

                let created_at = now_unix();
                let mut interaction_store = state
                    .interaction_log_store
                    .lock()
                    .map_err(|_| anyhow!("bridge interaction log store lock poisoned"))?;
                interaction_store.append(BridgeInteractionRecord {
                    event_id: format!("{}:{created_at}:plan", session.bridge_token_id),
                    token_id: session.bridge_token_id.clone(),
                    agent_id: session.agent_id.clone(),
                    goal_scope: session.goal_scope.clone(),
                    thread_id: None,
                    kind: BridgeInteractionKind::ProposedPlan,
                    summary: "Proposed execution plan".to_string(),
                    detail: plan.summary.clone(),
                    created_at,
                    raw_payload: serde_json::to_value(&plan)?,
                })?;
            }
            result
        }
        _ => Err(anyhow!("Tool not found in Nucleus: {}", name)),
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (success, error_msg, output_size) = match &result {
        Ok(ref tr) => {
            let size = serde_json::to_string(tr).map(|s| s.len()).unwrap_or(0);
            (true, None, size)
        }
        Err(e) => (false, Some(e.to_string()), 0),
    };

    // Record tool invocation (non-blocking)
    if let Ok(mut store) = state.tool_memory.lock() {
        store.record(ToolInvocation {
            tool_name: name.to_string(),
            agent_id: agent_id.to_string(),
            timestamp: now_epoch,
            duration_ms,
            success,
            error: error_msg,
            input_hash,
            output_size,
            agent_fingerprint: String::new(),
            chain_hash: String::new(),
        });
    }

    let tool_result = result?;
    Ok(serde_json::to_value(tool_result)?)
}

async fn handle_tool_stats(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    authorize_session_scope(state, session, "bridge.connect")?;

    let tool_name = params
        .get("tool_name")
        .or_else(|| params.get("name"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing tool_name"))?;
    let window_size = params
        .get("window_size")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize)
        .unwrap_or(50);

    let store = state
        .tool_memory
        .lock()
        .map_err(|_| anyhow!("tool memory lock poisoned"))?;
    let stats = store
        .stats(tool_name, window_size)
        .ok_or_else(|| anyhow!("no tool stats available for {}", tool_name))?;
    Ok(serde_json::to_value(stats)?)
}

async fn handle_tool_affinity(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    authorize_session_scope(state, session, "bridge.connect")?;

    let agent_id = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(session.agent_id.as_str());
    let window_size = params
        .get("window_size")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize)
        .unwrap_or(50);

    let store = state
        .tool_memory
        .lock()
        .map_err(|_| anyhow!("tool memory lock poisoned"))?;
    let entries = store.affinity_for_agent(agent_id, window_size);
    if entries.is_empty() {
        return Err(anyhow!("no tool affinity available for {}", agent_id));
    }

    Ok(serde_json::json!({
        "agent_id": agent_id,
        "entries": entries,
        "count": entries.len(),
    }))
}

async fn handle_llm_audit_query(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    authorize_session_scope(state, session, "bridge.connect")?;

    let query = LlmAuditQuery {
        kind: match params.get("kind").and_then(|v| v.as_str()) {
            Some("model_verification") => Some(LlmAuditEntryKind::ModelVerification),
            Some("chat") => Some(LlmAuditEntryKind::Chat),
            _ => None,
        },
        agent_id: params
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        model: params
            .get("model")
            .and_then(|v| v.as_str())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        success: params.get("success").and_then(|v| v.as_bool()),
        since: params.get("since").and_then(|v| v.as_u64()),
        until: params.get("until").and_then(|v| v.as_u64()),
        limit: params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|value| value as usize),
    };

    let log = state
        .audit_log
        .lock()
        .map_err(|_| anyhow!("audit log lock poisoned"))?;
    let entries = log.query(&query);
    Ok(serde_json::json!({
        "entries": entries,
        "count": entries.len(),
    }))
}

async fn handle_llm_audit_summary(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    authorize_session_scope(state, session, "bridge.connect")?;

    let since = params.get("since").and_then(|v| v.as_u64());
    let log = state
        .audit_log
        .lock()
        .map_err(|_| anyhow!("audit log lock poisoned"))?;
    Ok(serde_json::to_value(log.summary_since(since))?)
}

async fn handle_goal_event(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    let event_type = params
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("agent.step");
    let status = params
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("working");
    let detail = params.get("detail").and_then(|v| v.as_str()).unwrap_or("");
    let agent_id = session.agent_id.as_str();

    info!(agent_id, event_type, status, detail, "Agent Event");

    if event_type.eq_ignore_ascii_case("agent.log") {
        persist_agent_runtime_log(params, state, session)?;
    } else if event_type.eq_ignore_ascii_case("agent.status")
        || event_type.eq_ignore_ascii_case("agent.step")
    {
        persist_agent_runtime_status(params, state, session)?;
    }

    Ok(Value::Bool(true))
}

fn persist_agent_runtime_status(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<()> {
    let status = match params
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("running")
        .to_ascii_lowercase()
        .as_str()
    {
        "starting" => AgentRuntimeStatusKind::Starting,
        "waiting" => AgentRuntimeStatusKind::Waiting,
        "blocked" => AgentRuntimeStatusKind::Blocked,
        "completed" => AgentRuntimeStatusKind::Completed,
        "failed" => AgentRuntimeStatusKind::Failed,
        _ => AgentRuntimeStatusKind::Running,
    };
    let detail = params
        .get("detail")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let current_iteration = params
        .get("current_iteration")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32);
    let max_iterations = params
        .get("max_iterations")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32);
    let active_tool_name = params
        .get("active_tool_name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let thread_id = params
        .get("thread_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .or_else(|| session.thread_id.clone());
    let updated_at = now_unix();

    let mut store = state
        .agent_runtime_status_store
        .lock()
        .map_err(|_| anyhow!("agent runtime status store lock poisoned"))?;
    store.record_event(
        &session.agent_id,
        &session.bridge_token_id,
        session.goal_scope.clone(),
        thread_id,
        status,
        detail,
        current_iteration,
        max_iterations,
        active_tool_name,
        updated_at,
    )?;
    Ok(())
}

fn persist_agent_runtime_log(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<()> {
    let entry_type = match params
        .get("entry_type")
        .and_then(|value| value.as_str())
        .unwrap_or("result")
        .to_ascii_lowercase()
        .as_str()
    {
        "tool" => AgentRuntimeLogEntryType::Tool,
        "blocked" => AgentRuntimeLogEntryType::Blocked,
        _ => AgentRuntimeLogEntryType::Result,
    };
    let content = params
        .get("content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing content"))?
        .to_string();
    let tool_name = params
        .get("tool_name")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let tool_params = params.get("tool_params").map(|value| match value {
        Value::String(inner) => inner.clone(),
        other => other.to_string(),
    });
    let thread_id = params
        .get("thread_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .or_else(|| session.thread_id.clone());
    let created_at = now_unix();

    let mut store = state
        .agent_runtime_log_store
        .lock()
        .map_err(|_| anyhow!("agent runtime log store lock poisoned"))?;
    store.append(AgentRuntimeLogRecord {
        event_id: format!("{}:{created_at}:agent-log", session.bridge_token_id),
        token_id: session.bridge_token_id.clone(),
        agent_id: session.agent_id.clone(),
        goal_scope: session.goal_scope.clone(),
        thread_id,
        entry_type,
        content,
        tool_name,
        tool_params,
        created_at,
        raw_payload: params,
    })?;
    Ok(())
}

async fn handle_credential_request(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    let caps = DaemonCapabilityChecker::for_goal_scope(
        Arc::clone(&state.broker),
        session.goal_scope.clone(),
    );
    caps.check(&session.agent_id, "credential.read")?;

    let service = params
        .get("service")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing service"))?;
    let scopes: Vec<String> = params
        .get("scopes")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_else(|| vec!["session.use".to_string()]);
    let session_type = match params
        .get("session_type")
        .and_then(|v| v.as_str())
        .unwrap_or("api")
    {
        "browser" => SessionType::Browser,
        "api" => SessionType::Api,
        other => return Err(anyhow!("unsupported session_type: {other}")),
    };

    let handle = state.credential_gateway.issue_session_handle_scoped(
        session.goal_scope.as_deref(),
        AuthRequest {
            target: service.to_string(),
            scopes,
            session_type,
            policy: SessionPolicy {
                exportable: false,
                requires_reauth: false,
            },
        },
        now_unix(),
    )?;

    Ok(serde_json::to_value(handle)?)
}

async fn handle_credential_authenticate(
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    let caps = DaemonCapabilityChecker::for_goal_scope(
        Arc::clone(&state.broker),
        session.goal_scope.clone(),
    );
    caps.check(&session.agent_id, "action.browser.login")?;

    let target = params
        .get("target")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing target"))?
        .to_ascii_lowercase();
    let purpose = params
        .get("purpose")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing purpose"))?
        .to_string();
    let scopes: Vec<String> = params
        .get("scopes")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_else(|| vec!["web.login".to_string()]);
    let session_type = match params
        .get("session_type")
        .and_then(|value| value.as_str())
        .unwrap_or("browser")
    {
        "browser" => SessionType::Browser,
        "api" => SessionType::Api,
        other => return Err(anyhow!("unsupported session_type: {other}")),
    };
    let thread_id = params
        .get("thread_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let auth_profile = params
        .get("auth_profile")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let goal_template = params
        .get("goal_template")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let goal_room = params
        .get("goal_room")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let goal_id = params
        .get("goal_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    let now = now_unix();
    let coordinator = crate::auth_job_coordinator::AuthJobCoordinator::new(
        state.auth_jobs.as_ref(),
        state.auth_approval_policies.as_ref(),
        state.auth_engine.as_ref(),
        state.credential_gateway.as_ref(),
        state.credential_vault.as_ref(),
        state.auth_job_config,
    );
    let response = BridgeAuthOrchestrator::new(
        coordinator,
        state.session_store.as_ref(),
        state.interaction_log_store.as_ref(),
        &state.credentials_room_id,
    )
    .authenticate(
        &session.bridge_token_id,
        AuthJobRequest {
            target,
            scopes,
            session_type,
            purpose,
            room_id: state.credentials_room_id.clone(),
            thread_id,
            auth_profile,
            requested_by: session.agent_id.clone(),
            goal_scope: session.goal_scope.clone(),
            goal_room,
            goal_template,
            goal_id,
        },
        now,
    )
    .await?;

    Ok(serde_json::to_value(response)?)
}

async fn handle_swarm_rpc(
    method: &str,
    params: Value,
    state: &GatewayState,
    session: &AuthenticatedSession,
) -> Result<Value> {
    let swarm = state
        .swarm
        .as_ref()
        .ok_or_else(|| anyhow!("swarm server unavailable"))?;
    let mut params = params;
    if let Some(obj) = params.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::json!(session.agent_id.clone()),
        );
    }

    let agent_id = session.agent_id.clone();
    let input_hash = tool_memory::sha256_hex(&serde_json::to_string(&params).unwrap_or_default());
    let start = Instant::now();

    let result = if method == "swarm.issue_push_session" {
        let request: PushSessionRequest = serde_json::from_value(params)?;
        Ok(serde_json::to_value(
            swarm.issue_push_session(request).await?,
        )?)
    } else {
        swarm.handle_rpc(method, params).await
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (success, error_msg, output_size) = match &result {
        Ok(ref v) => {
            let size = serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
            (true, None, size)
        }
        Err(e) => (false, Some(e.to_string()), 0),
    };

    // Record swarm RPC as tool invocation (non-blocking)
    if let Ok(mut store) = state.tool_memory.lock() {
        store.record(ToolInvocation {
            tool_name: format!("rpc:{}", method),
            agent_id,
            timestamp: now_epoch,
            duration_ms,
            success,
            error: error_msg,
            input_hash,
            output_size,
            agent_fingerprint: String::new(),
            chain_hash: String::new(),
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::RwLock;
    use std::time::Duration;

    use credential_gateway::auth_engine::{AuthSandboxLauncher, AuthSandboxLauncherConfig};
    use credential_gateway::script_registry::ScriptRegistry;
    use credential_gateway::CredentialVault;
    use symbiotic_context::{AuditRecord, AuditSink, RecallGateway};
    use symbiotic_memory::tool_memory::ToolMemoryStore;
    use symbiotic_providers::{ProviderRegistry, ProviderRouter};
    use symbiotic_queue::{FileQueueStore, QueueBackend};
    use symbiotic_trust::{AgentTrustLevel, CapabilityToken};

    struct NoopAudit;

    impl AuditSink for NoopAudit {
        fn record(&self, _record: AuditRecord) -> Result<()> {
            Ok(())
        }
    }

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn socket_mode_defaults_to_owner_only_and_can_be_opted_out() {
        assert_eq!(socket_mode(false), 0o600);
        assert_eq!(socket_mode(true), 0o666);
    }

    fn test_state_with_vault() -> (Arc<GatewayState>, Arc<credential_gateway::GoalScopedVault>) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("llm_gateway_test_{unique}_{counter}"));
        let _ = std::fs::create_dir_all(&root);
        let archive_store =
            Arc::new(FileArchiveStore::open(root.join("archive")).expect("open archive store"));
        let queue: Arc<dyn QueueBackend> =
            Arc::new(FileQueueStore::open(root.join("queue.json")).expect("open queue store"));
        let recall_gateway = Arc::new(RecallGateway::new(
            Arc::new(crate::workers::ArchiveContextProvider {
                archive_store: archive_store.clone(),
                vault_store: archive_store.clone(),
            }),
            Arc::new(NoopAudit),
        ));
        let credential_vault = Arc::new(
            credential_gateway::GoalScopedVault::open(root.join("credential-vault"))
                .expect("open credential vault"),
        );
        let provider_router = Arc::new(ProviderRouter::new(Arc::new(RwLock::new(
            ProviderRegistry::new(),
        ))));

        let state = Arc::new(GatewayState {
            provider_router,
            archive_store,
            queue,
            recall_gateway,
            swarm: None,
            broker: Arc::new(Mutex::new(AccessBroker::new())),
            credential_gateway: Arc::new(CredentialGateway::new_scoped(
                credential_gateway::GatewayConfig::default(),
                Arc::new(credential_gateway::StaticThreatChecker::new(
                    std::collections::HashSet::new(),
                )),
                credential_vault.clone(),
            )),
            credential_vault: credential_vault.clone(),
            auth_engine: None,
            auth_jobs: Arc::new(Mutex::new(
                AuthJobStore::open(root.join("auth-jobs")).expect("open auth job store"),
            )),
            auth_approval_policies: Arc::new(Mutex::new(
                AuthApprovalPolicyStore::open(root.join("auth-approval-policies"))
                    .expect("open auth approval policy store"),
            )),
            session_store: Arc::new(Mutex::new(BridgeSessionStore::default())),
            interaction_log_store: Arc::new(Mutex::new(BridgeInteractionLogStore::new(&root))),
            agent_runtime_log_store: Arc::new(Mutex::new(AgentRuntimeLogStore::new(&root))),
            agent_runtime_status_store: Arc::new(Mutex::new(AgentRuntimeStatusStore::new(&root))),
            checkpoint_store: Arc::new(Mutex::new(BridgeCheckpointStore::new(&root))),
            audit_log: Arc::new(Mutex::new(LlmAuditLog::new(
                llm_audit::LlmAuditLevel::MetadataOnly,
                30,
            ))),
            tool_memory: Arc::new(Mutex::new(ToolMemoryStore::new())),
            credentials_room_id: "#credentials".to_string(),
            auth_job_config: AuthJobConfig::default(),
        });

        (state, credential_vault)
    }

    fn credential_gateway_bin_for_test() -> PathBuf {
        let binary_name = format!("credential-gateway{}", std::env::consts::EXE_SUFFIX);
        let current_exe = std::env::current_exe().expect("current exe");
        let sibling = current_exe.with_file_name(&binary_name);
        if sibling.exists() {
            return sibling;
        }

        current_exe
            .parent()
            .and_then(|dir| dir.parent())
            .map(|dir| dir.join(&binary_name))
            .filter(|candidate| candidate.exists())
            .unwrap_or_else(|| PathBuf::from(binary_name))
    }

    fn test_state_with_auth_sandbox(
    ) -> (Arc<GatewayState>, Arc<credential_gateway::GoalScopedVault>) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("llm_gateway_auth_test_{unique}_{counter}"));
        let _ = std::fs::create_dir_all(&root);
        let archive_store =
            Arc::new(FileArchiveStore::open(root.join("archive")).expect("open archive store"));
        let queue: Arc<dyn QueueBackend> =
            Arc::new(FileQueueStore::open(root.join("queue.json")).expect("open queue store"));
        let recall_gateway = Arc::new(RecallGateway::new(
            Arc::new(crate::workers::ArchiveContextProvider {
                archive_store: archive_store.clone(),
                vault_store: archive_store.clone(),
            }),
            Arc::new(NoopAudit),
        ));
        let credential_vault = Arc::new(
            credential_gateway::GoalScopedVault::open(root.join("credential-vault"))
                .expect("open credential vault"),
        );
        let provider_router = Arc::new(ProviderRouter::new(Arc::new(RwLock::new(
            ProviderRegistry::new(),
        ))));
        let scripts_dir = root.join("auth-scripts");
        std::fs::create_dir_all(&scripts_dir).expect("create auth scripts dir");
        let script_path = scripts_dir.join("github.com.sh");
        std::fs::write(
            &script_path,
            r#"#!/bin/sh
cat >/dev/null
echo '{"success": true, "session": "sandbox_session_token"}'
"#,
        )
        .expect("write auth script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let registry = ScriptRegistry::from_dir(&scripts_dir).expect("script registry");
        let auth_engine = AuthSandboxLauncher::new(
            registry,
            AuthSandboxLauncherConfig {
                worker_bin: credential_gateway_bin_for_test(),
                vault_root: root.join("credential-vault"),
                scripts_dir,
                timeout: Duration::from_secs(30),
                node_bin: None,
                goal_scope: None,
            },
        );

        let state = Arc::new(GatewayState {
            provider_router,
            archive_store,
            queue,
            recall_gateway,
            swarm: None,
            broker: Arc::new(Mutex::new(AccessBroker::new())),
            credential_gateway: Arc::new(CredentialGateway::new_scoped(
                credential_gateway::GatewayConfig::default(),
                Arc::new(credential_gateway::StaticThreatChecker::new(
                    std::collections::HashSet::new(),
                )),
                credential_vault.clone(),
            )),
            credential_vault: credential_vault.clone(),
            auth_engine: Some(auth_engine),
            auth_jobs: Arc::new(Mutex::new(
                AuthJobStore::open(root.join("auth-jobs")).expect("open auth job store"),
            )),
            auth_approval_policies: Arc::new(Mutex::new(
                AuthApprovalPolicyStore::open(root.join("auth-approval-policies"))
                    .expect("open auth approval policy store"),
            )),
            session_store: Arc::new(Mutex::new(BridgeSessionStore::default())),
            interaction_log_store: Arc::new(Mutex::new(BridgeInteractionLogStore::new(&root))),
            agent_runtime_log_store: Arc::new(Mutex::new(AgentRuntimeLogStore::new(&root))),
            agent_runtime_status_store: Arc::new(Mutex::new(AgentRuntimeStatusStore::new(&root))),
            checkpoint_store: Arc::new(Mutex::new(BridgeCheckpointStore::new(&root))),
            audit_log: Arc::new(Mutex::new(LlmAuditLog::new(
                llm_audit::LlmAuditLevel::MetadataOnly,
                30,
            ))),
            tool_memory: Arc::new(Mutex::new(ToolMemoryStore::new())),
            credentials_room_id: "#credentials".to_string(),
            auth_job_config: AuthJobConfig::default(),
        });

        (state, credential_vault)
    }

    fn test_state() -> Arc<GatewayState> {
        test_state_with_vault().0
    }

    async fn authenticated_session(
        state: &GatewayState,
        token_id: &str,
        agent_id: &str,
        scopes: &[&str],
    ) -> Option<AuthenticatedSession> {
        issue_token_with_scopes(state, token_id, agent_id, scopes.iter().copied(), None);
        let mut session = None;
        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {
                    "agent_id": agent_id,
                    "token_id": token_id,
                }
            }),
            state,
            &mut session,
        )
        .await
        .expect("handshake should succeed")
        .expect("handshake should respond");
        assert_eq!(response["result"]["agent_id"].as_str(), Some(agent_id));
        session
    }

    fn issue_token(
        state: &GatewayState,
        token_id: &str,
        agent_id: &str,
        scope: &str,
        goal_scope: Option<&str>,
    ) {
        issue_token_with_scopes(state, token_id, agent_id, [scope], goal_scope);
    }

    fn issue_token_with_scopes<I, S>(
        state: &GatewayState,
        token_id: &str,
        agent_id: &str,
        scopes: I,
        goal_scope: Option<&str>,
    ) where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let now = now_unix();
        let collected_scopes: std::collections::HashSet<String> = scopes
            .into_iter()
            .map(|scope| scope.as_ref().to_string())
            .collect();
        let trust_level = collected_scopes
            .iter()
            .map(|scope| crate::tool_adapters::scope_to_trust_level(scope))
            .max()
            .unwrap_or(AgentTrustLevel::ReadOnly);
        state
            .broker
            .lock()
            .expect("broker lock")
            .issue_token(CapabilityToken {
                token_id: token_id.to_string(),
                subject: agent_id.to_string(),
                trust_level,
                scopes: collected_scopes,
                expires_at: now + 3600,
                one_time: false,
                consumed: false,
                goal_scope: goal_scope.map(str::to_string),
            });
    }

    #[tokio::test]
    async fn process_request_requires_bridge_handshake() {
        let state = test_state();
        let mut session = None;
        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "llm.chat",
                "params": {"messages": [], "json_mode": false}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should produce response")
        .expect("request with id should respond");

        let message = response["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(message.contains("bridge.handshake required"));
    }

    #[tokio::test]
    async fn process_request_tool_stats_returns_windowed_metrics() {
        let state = test_state();
        {
            let mut store = state.tool_memory.lock().expect("tool memory");
            store.record(ToolInvocation {
                tool_name: "recall".to_string(),
                agent_id: "agent-1".to_string(),
                timestamp: 100,
                duration_ms: 10,
                success: true,
                error: None,
                input_hash: "a".to_string(),
                output_size: 10,
                agent_fingerprint: String::new(),
                chain_hash: String::new(),
            });
            store.record(ToolInvocation {
                tool_name: "recall".to_string(),
                agent_id: "agent-1".to_string(),
                timestamp: 200,
                duration_ms: 40,
                success: false,
                error: Some("timeout".to_string()),
                input_hash: "b".to_string(),
                output_size: 0,
                agent_fingerprint: String::new(),
                chain_hash: String::new(),
            });
            store.record(ToolInvocation {
                tool_name: "recall".to_string(),
                agent_id: "agent-1".to_string(),
                timestamp: 300,
                duration_ms: 20,
                success: true,
                error: None,
                input_hash: "c".to_string(),
                output_size: 12,
                agent_fingerprint: String::new(),
                chain_hash: String::new(),
            });
        }

        let mut session =
            authenticated_session(&state, "token-tool-stats", "agent-1", &["bridge.connect"]).await;

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tool.stats",
                "params": {
                    "tool_name": "recall",
                    "window_size": 2
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should produce response")
        .expect("tool.stats should respond");

        assert_eq!(response["result"]["tool_name"].as_str(), Some("recall"));
        assert_eq!(response["result"]["total_invocations"].as_u64(), Some(2));
        assert_eq!(response["result"]["p50_latency_ms"].as_u64(), Some(40));
        assert_eq!(
            response["result"]["last_failure"]["error"].as_str(),
            Some("timeout")
        );
    }

    #[tokio::test]
    async fn process_request_tool_affinity_returns_ranked_entries() {
        let state = test_state();
        {
            let mut store = state.tool_memory.lock().expect("tool memory");
            store.record(ToolInvocation {
                tool_name: "archive".to_string(),
                agent_id: "agent-a".to_string(),
                timestamp: 100,
                duration_ms: 10,
                success: true,
                error: None,
                input_hash: "a".to_string(),
                output_size: 10,
                agent_fingerprint: String::new(),
                chain_hash: String::new(),
            });
            store.record(ToolInvocation {
                tool_name: "recall".to_string(),
                agent_id: "agent-a".to_string(),
                timestamp: 200,
                duration_ms: 50,
                success: false,
                error: Some("timeout".to_string()),
                input_hash: "b".to_string(),
                output_size: 0,
                agent_fingerprint: String::new(),
                chain_hash: String::new(),
            });
            store.record(ToolInvocation {
                tool_name: "recall".to_string(),
                agent_id: "agent-a".to_string(),
                timestamp: 300,
                duration_ms: 20,
                success: true,
                error: None,
                input_hash: "c".to_string(),
                output_size: 12,
                agent_fingerprint: String::new(),
                chain_hash: String::new(),
            });
        }

        let mut session = authenticated_session(
            &state,
            "token-tool-affinity",
            "agent-a",
            &["bridge.connect"],
        )
        .await;

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tool.affinity",
                "params": {
                    "window_size": 50
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should produce response")
        .expect("tool.affinity should respond");

        assert_eq!(response["result"]["agent_id"].as_str(), Some("agent-a"));
        assert_eq!(response["result"]["count"].as_u64(), Some(2));
        assert_eq!(
            response["result"]["entries"][0]["tool_name"].as_str(),
            Some("archive")
        );
        assert_eq!(
            response["result"]["entries"][0]["success_rate"].as_f64(),
            Some(1.0)
        );
        assert!(response["result"]["entries"][0]["latest_chain_hash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty()));
    }

    #[tokio::test]
    async fn process_request_llm_audit_query_filters_entries() {
        let state = test_state();
        {
            let mut log = state.audit_log.lock().expect("audit log");
            log.record(LlmAuditEntry {
                timestamp: 100,
                kind: LlmAuditEntryKind::Chat,
                agent_id: "agent-a".to_string(),
                model: "routed".to_string(),
                prompt_hash: "hash-a".to_string(),
                completion_size: 10,
                token_count: None,
                latency_ms: 20,
                success: false,
                error: Some("timeout".to_string()),
                prompt_text: Some("prompt-a".to_string()),
                completion_text: None,
                verification: None,
            });
            log.record(LlmAuditEntry {
                timestamp: 200,
                kind: LlmAuditEntryKind::Chat,
                agent_id: "agent-b".to_string(),
                model: "routed".to_string(),
                prompt_hash: "hash-b".to_string(),
                completion_size: 20,
                token_count: None,
                latency_ms: 30,
                success: true,
                error: None,
                prompt_text: Some("prompt-b".to_string()),
                completion_text: Some("done".to_string()),
                verification: None,
            });
        }

        let mut session =
            authenticated_session(&state, "token-audit-query", "agent-a", &["bridge.connect"])
                .await;

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "llm.audit.query",
                "params": {
                    "agent_id": "agent-a",
                    "success": false,
                    "limit": 10
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should produce response")
        .expect("llm.audit.query should respond");

        assert_eq!(response["result"]["count"].as_u64(), Some(1));
        assert_eq!(
            response["result"]["entries"][0]["agent_id"].as_str(),
            Some("agent-a")
        );
        assert_eq!(
            response["result"]["entries"][0]["error"].as_str(),
            Some("timeout")
        );
    }

    #[tokio::test]
    async fn process_request_llm_audit_summary_returns_aggregate_metrics() {
        let state = test_state();
        {
            let mut log = state.audit_log.lock().expect("audit log");
            log.record(LlmAuditEntry {
                timestamp: 100,
                kind: LlmAuditEntryKind::Chat,
                agent_id: "agent-a".to_string(),
                model: "model-a".to_string(),
                prompt_hash: "hash-a".to_string(),
                completion_size: 10,
                token_count: None,
                latency_ms: 10,
                success: true,
                error: None,
                prompt_text: Some("prompt-a".to_string()),
                completion_text: Some("ok".to_string()),
                verification: None,
            });
            log.record(LlmAuditEntry {
                timestamp: 200,
                kind: LlmAuditEntryKind::Chat,
                agent_id: "agent-a".to_string(),
                model: "model-b".to_string(),
                prompt_hash: "hash-b".to_string(),
                completion_size: 20,
                token_count: None,
                latency_ms: 50,
                success: false,
                error: Some("timeout".to_string()),
                prompt_text: Some("prompt-b".to_string()),
                completion_text: None,
                verification: None,
            });
        }

        let mut session = authenticated_session(
            &state,
            "token-audit-summary",
            "agent-a",
            &["bridge.connect"],
        )
        .await;

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "llm.audit.summary",
                "params": {
                    "since": 50
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should produce response")
        .expect("llm.audit.summary should respond");

        assert_eq!(response["result"]["total_calls"].as_u64(), Some(2));
        assert_eq!(response["result"]["failure_count"].as_u64(), Some(1));
        assert_eq!(response["result"]["avg_latency_ms"].as_u64(), Some(30));
        assert_eq!(
            response["result"]["top_models"][0]["count"].as_u64(),
            Some(1)
        );
    }

    #[tokio::test]
    async fn bridge_handshake_rejects_unknown_token() {
        let state = test_state();
        let mut session = None;
        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "missing"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should produce response")
        .expect("request with id should respond");

        let message = response["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(message.contains("bridge token not found"));
        assert!(session.is_none(), "failed handshake must not authenticate");
    }

    #[tokio::test]
    async fn tool_execute_uses_authenticated_session_and_denies_missing_scope() {
        let state = test_state();
        issue_token(
            &state,
            "bridge-token",
            "agent-1",
            "bridge.connect",
            Some("wf-1"),
        );
        issue_token(&state, "llm-token", "agent-1", "llm.chat", Some("wf-1"));

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tool.execute",
                "params": {
                    "name": "archive",
                    "agent_id": "spoofed-agent",
                    "params": {
                        "title": "Test",
                        "content": "body"
                    }
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        let message = response["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(
            message.contains("no capability token found"),
            "unexpected message: {message}"
        );
    }

    #[tokio::test]
    async fn goal_event_notification_does_not_emit_response() {
        let state = test_state();
        issue_token(&state, "bridge-token", "agent-1", "bridge.connect", None);

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "goal.event",
                "params": {
                    "event_type": "agent.step",
                    "status": "working",
                    "detail": "tick"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("notification should succeed");

        assert!(
            response.is_none(),
            "notifications must not produce a response"
        );
    }

    #[tokio::test]
    async fn goal_event_agent_log_persists_runtime_log_record() {
        let state = test_state();
        issue_token(
            &state,
            "bridge-token",
            "agent-1",
            "bridge.connect",
            Some("wf-1"),
        );

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "goal.event",
                "params": {
                    "event_type": "agent.log",
                    "entry_type": "tool",
                    "content": "Calling tool: read_file",
                    "tool_name": "read_file",
                    "tool_params": {"path":"README.md"},
                    "thread_id": "thread-1"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("notification should succeed");
        assert!(
            response.is_none(),
            "notifications must not produce a response"
        );

        let store = state
            .agent_runtime_log_store
            .lock()
            .expect("agent runtime log store lock");
        let records = store.recent_for_thread("thread-1", &["wf-1".to_string()], 8);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].agent_id, "agent-1");
        assert_eq!(records[0].tool_name.as_deref(), Some("read_file"));
        assert_eq!(records[0].entry_type, AgentRuntimeLogEntryType::Tool);
    }

    #[tokio::test]
    async fn bridge_handshake_runtime_profile_persists_initial_agent_status() {
        let state = test_state();
        issue_token(
            &state,
            "bridge-token",
            "agent-1",
            "bridge.connect",
            Some("wf-1"),
        );

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {
                    "agent_id": "agent-1",
                    "token_id": "bridge-token",
                    "runtime_profile": {
                        "role": "coder",
                        "sandbox_type": "vm_sandbox",
                        "model_label": "gpt-5.4",
                        "max_iterations": 15,
                        "thread_id": "thread-1"
                    }
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let store = state
            .agent_runtime_status_store
            .lock()
            .expect("agent runtime status store lock");
        let status = store.get("agent-1").expect("status should exist");
        assert_eq!(status.goal_scope.as_deref(), Some("wf-1"));
        assert_eq!(status.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(status.role.as_deref(), Some("coder"));
        assert_eq!(status.sandbox_type, "vm_sandbox");
        assert_eq!(status.model_label.as_deref(), Some("gpt-5.4"));
        assert_eq!(status.max_iterations, Some(15));
        assert_eq!(status.status, AgentRuntimeStatusKind::Starting);
    }

    #[tokio::test]
    async fn goal_event_agent_status_persists_runtime_status_record() {
        let state = test_state();
        issue_token(
            &state,
            "bridge-token",
            "agent-1",
            "bridge.connect",
            Some("wf-1"),
        );

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {
                    "agent_id": "agent-1",
                    "token_id": "bridge-token",
                    "runtime_profile": {
                        "role": "coder",
                        "sandbox_type": "vm_sandbox",
                        "thread_id": "thread-1"
                    }
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "goal.event",
                "params": {
                    "event_type": "agent.status",
                    "status": "running",
                    "detail": "Iteration 2/15",
                    "current_iteration": 2,
                    "max_iterations": 15,
                    "active_tool_name": "read_file"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("notification should succeed");
        assert!(
            response.is_none(),
            "notifications must not produce a response"
        );

        let store = state
            .agent_runtime_status_store
            .lock()
            .expect("agent runtime status store lock");
        let status = store.get("agent-1").expect("status should exist");
        assert_eq!(status.goal_scope.as_deref(), Some("wf-1"));
        assert_eq!(status.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(status.status, AgentRuntimeStatusKind::Running);
        assert_eq!(status.detail.as_deref(), Some("Iteration 2/15"));
        assert_eq!(status.current_iteration, Some(2));
        assert_eq!(status.max_iterations, Some(15));
        assert_eq!(status.active_tool_name.as_deref(), Some("read_file"));
    }

    #[tokio::test]
    async fn ask_user_tool_records_pending_question_in_session_store() {
        let state = test_state();
        issue_token(
            &state,
            "bridge-token",
            "agent-1",
            "bridge.connect",
            Some("wf-1"),
        );

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tool.execute",
                "params": {
                    "name": "ask_user",
                    "params": {
                        "question": "What budget?",
                        "quick_replies": ["Low", "High"]
                    }
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        assert!(
            response.get("error").is_none(),
            "ask_user should succeed: {response}"
        );

        let artifacts = state
            .session_store
            .lock()
            .expect("session store lock")
            .take_artifacts("bridge-token");
        let question = artifacts.pending_question.expect("pending question");
        assert_eq!(question.text, "What budget?");
        assert_eq!(
            question.quick_replies,
            Some(vec!["Low".to_string(), "High".to_string()])
        );
    }

    #[tokio::test]
    async fn context_packet_load_records_packet_in_session_store() {
        let state = test_state();
        let mut session =
            authenticated_session(&state, "bridge-token", "agent-1", &["bridge.connect"]).await;

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "context.packet.load",
                "params": {
                    "goal": "Ship the slice",
                    "context": "Current task context"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        assert!(
            response.get("error").is_none(),
            "context.packet.load should succeed: {response}"
        );
        assert_eq!(response["result"]["agent_id"].as_str(), Some("agent-1"));
        assert_eq!(response["result"]["goal"].as_str(), Some("Ship the slice"));

        let artifacts = state
            .session_store
            .lock()
            .expect("session store lock")
            .take_artifacts("bridge-token");
        let packet = artifacts.context_packet.expect("context packet");
        assert_eq!(packet.agent_id, "agent-1");
        assert_eq!(packet.goal_scope.as_deref(), None);
        assert!(packet
            .context_sources
            .iter()
            .any(|source| source == "CONTEXT.md"));
    }

    #[tokio::test]
    async fn checkpoint_create_records_checkpoint_artifact_in_session_store() {
        let state = test_state();
        let mut session =
            authenticated_session(&state, "bridge-token", "agent-1", &["bridge.connect"]).await;

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "checkpoint.create",
                "params": {
                    "protocol_version": "v1",
                    "agent_id": "agent-1",
                    "goal_scope": "wf-9",
                    "iterations": 4,
                    "context_packet_loaded": true,
                    "context_sources_read": ["CONTEXT.md", "tasks/NEXT.md"],
                    "checkpoint_summary": "Landed the coherent slice."
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        assert!(
            response.get("error").is_none(),
            "checkpoint.create should succeed: {response}"
        );
        assert_eq!(response["result"]["stored"].as_bool(), Some(true));

        let artifacts = state
            .session_store
            .lock()
            .expect("session store lock")
            .take_artifacts("bridge-token");
        let checkpoint = artifacts.checkpoint_artifact.expect("checkpoint artifact");
        assert_eq!(checkpoint.agent_id, "agent-1");
        assert_eq!(checkpoint.goal_scope.as_deref(), Some("wf-9"));
        assert_eq!(checkpoint.iterations, 4);
        assert!(checkpoint.context_packet_loaded);
    }

    #[tokio::test]
    async fn credential_request_returns_scoped_session_handle() {
        let (state, credential_vault) = test_state_with_vault();
        credential_vault
            .put_scoped(
                Some("wf-1"),
                credential_gateway::CredentialRecord {
                    service: "x.com".to_string(),
                    username: "alice".to_string(),
                    secret: "super-secret".to_string(),
                    totp_secret: None,
                },
            )
            .expect("seed scoped credential");
        issue_token_with_scopes(
            &state,
            "bridge-token",
            "agent-1",
            ["bridge.connect"],
            Some("wf-1"),
        );
        issue_token_with_scopes(
            &state,
            "cred-token",
            "agent-1",
            ["credential.read"],
            Some("wf-1"),
        );

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "credential.request",
                "params": {
                    "service": "https://x.com/i/bookmarks",
                    "scopes": ["web.login"],
                    "session_type": "browser"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        assert!(
            response.get("error").is_none(),
            "credential.request should succeed: {response}"
        );

        let handle = response
            .get("result")
            .cloned()
            .expect("result payload should be present");
        let handle_id = handle["handle_id"]
            .as_str()
            .expect("handle_id should be present");
        assert_eq!(handle["target"].as_str(), Some("x.com"));
        assert_eq!(handle["session_type"].as_str(), Some("Browser"));
        assert_eq!(handle["policy"]["exportable"].as_bool(), Some(false));
        assert!(
            handle.get("username").is_none(),
            "raw credentials must not leak"
        );
        assert!(
            handle.get("secret").is_none(),
            "raw credentials must not leak"
        );

        state
            .credential_gateway
            .validate_session_handle(handle_id, "x.com", "web.login", now_unix())
            .expect("issued handle should validate");
    }

    #[tokio::test]
    async fn credential_authenticate_records_pending_auth_request() {
        let state = test_state();
        issue_token_with_scopes(
            &state,
            "bridge-token",
            "agent-1",
            ["bridge.connect"],
            Some("wf-1"),
        );
        issue_token_with_scopes(
            &state,
            "auth-token",
            "agent-1",
            ["action.browser.login"],
            Some("wf-1"),
        );

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "credential.authenticate",
                "params": {
                    "target": "github.com",
                    "scopes": ["web.login"],
                    "session_type": "browser",
                    "purpose": "Open GitHub settings",
                    "thread_id": "thread-project-x"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        assert!(
            response.get("error").is_none(),
            "credential.authenticate should succeed: {response}"
        );

        let result = response
            .get("result")
            .cloned()
            .expect("result payload should be present");
        assert_eq!(result["state"].as_str(), Some("awaiting_approval"));
        assert_eq!(result["target"].as_str(), Some("github.com"));
        let request_id = result["request_id"]
            .as_str()
            .expect("request_id should be present");

        let auth_job = state
            .auth_jobs
            .lock()
            .expect("auth job store lock")
            .get(request_id)
            .expect("auth job should be persisted");
        assert_eq!(auth_job.target, "github.com");
        assert_eq!(auth_job.goal_scope.as_deref(), Some("wf-1"));
        assert_eq!(
            auth_job.status,
            crate::auth_jobs::AuthJobStatus::AwaitingApproval
        );

        let artifacts = state
            .session_store
            .lock()
            .expect("session store lock")
            .take_artifacts("bridge-token");
        let pending = artifacts
            .pending_auth_request
            .expect("pending auth request should be recorded");
        assert_eq!(pending.request_id, request_id);
        assert_eq!(pending.target, "github.com");
        assert_eq!(pending.room_id, "#credentials");
    }

    #[tokio::test]
    async fn credential_authenticate_auto_approves_with_matching_policy() {
        let (state, credential_vault) = test_state_with_auth_sandbox();
        credential_vault
            .put(credential_gateway::CredentialRecord {
                service: "github.com".to_string(),
                username: "user".to_string(),
                secret: "secret".to_string(),
                totp_secret: None,
            })
            .expect("seed login credential");
        let attestation = state
            .auth_engine
            .as_ref()
            .expect("auth engine")
            .resolve_attestation("github.com")
            .expect("resolve attestation");
        state
            .auth_approval_policies
            .lock()
            .expect("policy store lock")
            .create(
                crate::auth_approval_policies::AuthApprovalPolicyRequest {
                    target: "github.com".to_string(),
                    scopes: vec!["web.login".to_string()],
                    attestation,
                    created_from_request_id: "authreq_seed".to_string(),
                    created_by: "@user:test".to_string(),
                    purpose: "Open GitHub settings".to_string(),
                    ttl_secs: 3600,
                },
                now_unix(),
            )
            .expect("create remembered approval policy");
        issue_token_with_scopes(
            &state,
            "bridge-token",
            "agent-1",
            ["bridge.connect"],
            Some("wf-1"),
        );
        issue_token_with_scopes(
            &state,
            "auth-token",
            "agent-1",
            ["action.browser.login"],
            Some("wf-1"),
        );

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "credential.authenticate",
                "params": {
                    "target": "github.com",
                    "scopes": ["web.login"],
                    "session_type": "browser",
                    "purpose": "Open GitHub settings",
                    "thread_id": "thread-project-x"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        assert!(
            response.get("error").is_none(),
            "credential.authenticate should succeed: {response}"
        );
        assert_eq!(response["result"]["state"].as_str(), Some("completed"));
        assert_eq!(
            response["result"]["message"].as_str(),
            Some("Authentication completed via remembered approval policy")
        );
        let handle_id = response["result"]["handle"]["handle_id"]
            .as_str()
            .expect("handle id should be present");
        state
            .credential_gateway
            .validate_session_handle(handle_id, "github.com", "web.login", now_unix())
            .expect("issued handle should validate");

        let session_record = credential_vault
            .get_scoped(Some("wf-1"), "github.com")
            .expect("session lookup should work")
            .expect("goal-scoped session should be stored");
        assert_eq!(session_record.secret, "sandbox_session_token");
        assert!(
            state
                .session_store
                .lock()
                .expect("session store lock")
                .take_artifacts("bridge-token")
                .pending_auth_request
                .is_none(),
            "auto-approved auth should not leave a pending request artifact"
        );
    }

    #[tokio::test]
    async fn credential_request_does_not_fall_back_to_global_credentials_for_goal_scoped_session() {
        let (state, credential_vault) = test_state_with_vault();
        credential_vault
            .put_scoped(
                None,
                credential_gateway::CredentialRecord {
                    service: "x.com".to_string(),
                    username: "alice".to_string(),
                    secret: "super-secret".to_string(),
                    totp_secret: None,
                },
            )
            .expect("seed global credential");
        issue_token_with_scopes(
            &state,
            "bridge-token",
            "agent-1",
            ["bridge.connect"],
            Some("wf-1"),
        );
        issue_token_with_scopes(
            &state,
            "cred-token",
            "agent-1",
            ["credential.read"],
            Some("wf-1"),
        );

        let mut session = None;
        let handshake = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");
        assert!(handshake.is_some(), "handshake should respond");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "credential.request",
                "params": {
                    "service": "x.com",
                    "scopes": ["web.login"],
                    "session_type": "browser"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        let message = response["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(
            message.contains("missing credentials for target: x.com"),
            "unexpected message: {message}"
        );
    }

    #[tokio::test]
    async fn credential_request_requires_agent_capability_token_not_just_bridge_token() {
        let (state, credential_vault) = test_state_with_vault();
        credential_vault
            .put_scoped(
                Some("wf-1"),
                credential_gateway::CredentialRecord {
                    service: "x.com".to_string(),
                    username: "alice".to_string(),
                    secret: "super-secret".to_string(),
                    totp_secret: None,
                },
            )
            .expect("seed scoped credential");
        issue_token_with_scopes(
            &state,
            "bridge-token",
            "agent-1",
            ["bridge.connect"],
            Some("wf-1"),
        );

        let mut session = None;
        process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "bridge.handshake",
                "params": {"agent_id": "agent-1", "token_id": "bridge-token"}
            }),
            &state,
            &mut session,
        )
        .await
        .expect("handshake should succeed");

        let response = process_request(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "credential.request",
                "params": {
                    "service": "x.com",
                    "scopes": ["web.login"],
                    "session_type": "browser"
                }
            }),
            &state,
            &mut session,
        )
        .await
        .expect("request should respond")
        .expect("request with id should return response");

        let message = response["error"]["message"]
            .as_str()
            .expect("error message");
        assert!(
            message.contains("credential.read"),
            "unexpected message: {message}"
        );
    }
}
