//! Agent-related handlers and provider dispatch coordination.
//!
//! Contains agent spawning, scope execution, capability token
//! management, role resolution, and the [`ProviderRouterLlmClient`]
//! adapter that bridges `ProviderRouter` to the `LlmClient` trait
//! used by the agent executor.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use symbiotic_agent_config::RoleRegistry;
use symbiotic_agents::llm::{ChatMessage, LlmClient};
use symbiotic_agents::{AgentParent, CapabilityRequest, SecureAgent, TaskSpec};
use symbiotic_core::Sensitivity;
use symbiotic_providers::{CompletionRequest, ModelHint, ProviderRouter, Role as ProviderRole};
use symbiotic_trust::{AccessRequest, AgentTrustLevel};

use symbiotic_agents::graduation::{GraduationConfig, GraduationStore};

use crate::auth::{serialize_agent_parent, serialize_llm_type, serialize_trust_level};
use crate::goal_state::*;
use crate::{persist_access_broker, SymbioticDaemon};

/// Adapter that implements [`LlmClient`] (from `symbiotic-agents`) by routing
/// completion requests through a [`ProviderRouter`] (from `symbiotic-providers`).
///
/// This allows the agent executor's ReAct loop to use the full provider
/// infrastructure — sensitivity-aware routing, budget enforcement, health
/// checks, and retry logic — transparently.
///
/// # Type mapping
///
/// The agents crate uses its own `ChatMessage` with string roles, while the
/// providers crate uses a `Role` enum and its own `ChatMessage`. This adapter
/// converts between the two representations on every call.
pub struct ProviderRouterLlmClient {
    router: Arc<ProviderRouter>,
    sensitivity: Sensitivity,
    source: String,
    model_hint: ModelHint,
}

impl ProviderRouterLlmClient {
    /// Create a new adapter.
    ///
    /// - `router`: The shared provider router instance from the daemon.
    /// - `sensitivity`: The data sensitivity level for this agent's task.
    ///   Determines whether completions can use cloud or must stay local.
    /// - `source`: Attribution string for metering (e.g. "agent_execution").
    pub fn new(router: Arc<ProviderRouter>, sensitivity: Sensitivity, source: String) -> Self {
        Self {
            router,
            sensitivity,
            source,
            model_hint: ModelHint::Default,
        }
    }

    /// Create a new adapter with a model hint for cost/quality preference.
    ///
    /// Same as [`new`](Self::new) but allows specifying a [`ModelHint`] to
    /// influence model selection (e.g. `CheapFast` for extraction pipelines).
    pub fn with_hint(
        router: Arc<ProviderRouter>,
        sensitivity: Sensitivity,
        source: String,
        model_hint: ModelHint,
    ) -> Self {
        Self {
            router,
            sensitivity,
            source,
            model_hint,
        }
    }
}

/// Convert an agent `ChatMessage` (string role) to a provider `ChatMessage` (enum role).
fn to_provider_message(msg: &ChatMessage) -> symbiotic_providers::ChatMessage {
    let role = match msg.role.as_str() {
        "system" => ProviderRole::System,
        "assistant" => ProviderRole::Assistant,
        _ => ProviderRole::User, // default to user for unknown roles
    };
    symbiotic_providers::ChatMessage {
        role,
        content: msg.content.clone(),
    }
}

#[async_trait::async_trait]
impl LlmClient for ProviderRouterLlmClient {
    async fn chat(&self, messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
        let provider_messages: Vec<symbiotic_providers::ChatMessage> =
            messages.iter().map(to_provider_message).collect();

        let request = CompletionRequest {
            messages: provider_messages,
            max_tokens: None,
            temperature: None,
            stop: None,
            model_hint: self.model_hint,
        };

        let response = self
            .router
            .complete(&request, self.sensitivity, &self.source)
            .await
            .map_err(|e| anyhow!("provider router completion failed: {e}"))?;

        Ok(response.content)
    }
}

impl SymbioticDaemon {
    pub fn issue_capability_token(&self, token: symbiotic_trust::CapabilityToken) -> Result<()> {
        let mut broker = self
            .broker
            .lock()
            .map_err(|_| anyhow!("broker lock poisoned"))?;

        // Persist to TrustStore (SQLite) when available.
        if let Some(ref store) = self.trust_store {
            if let Err(e) = broker.issue_token_persisted(token.clone(), store) {
                tracing::warn!(error = %e, "trust_store: failed to persist issued token");
                // Fall through to JSON persistence as fallback.
                broker.issue_token(token);
            }
        } else {
            broker.issue_token(token);
        }

        persist_access_broker(&self.config.capability_tokens_file, &broker)?;
        Ok(())
    }

    pub fn spawn_task_agent(
        &self,
        task_id: &str,
        parent: AgentParent,
        requires_private_data: bool,
        scopes: Vec<String>,
        role: Option<String>,
        now: u64,
    ) -> Result<String> {
        let mut effective_private_data = requires_private_data;
        let mut effective_scopes = scopes;

        if let Some(ref role_name) = role {
            if let Ok(resolved) = self.role_registry.resolve(role_name) {
                if resolved.requires_private_data {
                    effective_private_data = true;
                }
                // Merge role's required capabilities with explicit scopes
                for cap in &resolved.required_capabilities {
                    if !effective_scopes.iter().any(|s| s == cap) {
                        effective_scopes.push(cap.clone());
                    }
                }
            }
        }

        let capabilities = effective_scopes
            .into_iter()
            .map(|scope| CapabilityRequest {
                scope,
                purpose: format!("task:{task_id}"),
            })
            .collect();
        let agent = self.agents.spawn_agent(
            TaskSpec {
                id: task_id.to_string(),
                parent,
                requires_private_data: effective_private_data,
                capabilities,
                role,
            },
            now,
        )?;
        append_agent_lifecycle_log(
            &self.config.agent_log_file,
            AgentLogEntry {
                ts: now,
                event: "agent.spawned",
                agent_id: &agent.id,
                status: "spawned",
                scope: None,
                detail: "",
            },
        )?;
        upsert_agent_state(
            &self.config.agent_state_file,
            AgentState {
                agent_id: agent.id.clone(),
                parent: serialize_agent_parent(&agent.parent),
                llm_type: serialize_llm_type(&agent.llm_type),
                trust_level: serialize_trust_level(agent.trust_level),
                last_status: "spawned".to_string(),
                last_scope: None,
                updated_at: now,
            },
        )?;
        Ok(agent.id)
    }

    /// Resolves a role name into an [`AgentExecConfig`] suitable for the executor.
    ///
    /// Returns `None` when the role is not set or not found in the registry.
    /// When SOUL.md identity content is available (loaded by the reconciler),
    /// it is injected into the config so agents carry the user's identity
    /// as a preamble in their system prompt.
    pub fn resolve_role_config(
        &self,
        role: Option<&str>,
    ) -> Option<symbiotic_agents::executor::AgentExecConfig> {
        let role_name = role?;
        let resolved = self.role_registry.resolve(role_name).ok()?;

        // Read identity_content from the shared reconciler handle.
        let identity = self
            .identity_content
            .lock()
            .ok()
            .and_then(|guard| guard.clone());

        Some(symbiotic_agents::executor::AgentExecConfig {
            system_prompt: Some(resolved.system_prompt),
            max_iterations: resolved.max_iterations,
            identity_context: identity,
            handoff_dir: None,
            agent_id: None,
            role: None,
            redact_output: true,
        })
    }

    /// Returns a reference to the daemon's shared `ProviderRouter`.
    ///
    /// Callers can clone the `Arc` to create a `ProviderRouterLlmClient` or
    /// use the router directly for embeddings, image generation, etc.
    pub fn provider_router(&self) -> &Arc<ProviderRouter> {
        &self.provider_router
    }

    /// Returns a clone of the daemon's shared capability broker.
    pub fn capability_broker(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<symbiotic_trust::AccessBroker>> {
        std::sync::Arc::clone(&self.broker)
    }

    /// Returns the shared bridge session store used by the runner gateway and
    /// workflow executor to exchange pending question/plan artifacts.
    pub fn bridge_session_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::bridge_interactions::BridgeSessionStore>> {
        std::sync::Arc::clone(&self.bridge_session_store)
    }

    pub fn bridge_interaction_log_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::bridge_interactions::BridgeInteractionLogStore>>
    {
        std::sync::Arc::clone(&self.bridge_interaction_log_store)
    }

    pub fn bridge_checkpoint_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::bridge_interactions::BridgeCheckpointStore>> {
        std::sync::Arc::clone(&self.bridge_checkpoint_store)
    }

    pub fn agent_runtime_log_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::bridge_interactions::AgentRuntimeLogStore>> {
        std::sync::Arc::clone(&self.agent_runtime_log_store)
    }

    pub fn agent_runtime_status_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::agent_runtime_status::AgentRuntimeStatusStore>>
    {
        std::sync::Arc::clone(&self.agent_runtime_status_store)
    }

    /// Returns a clone of the daemon's shared management store.
    pub fn management_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<symbiotic_control_plane::ManagementStore>> {
        std::sync::Arc::clone(&self.management_store)
    }

    /// Returns the shared auth job store used by the bridge and credentials
    /// room commands to coordinate approval-driven auth workflows.
    pub fn auth_job_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::auth_jobs::AuthJobStore>> {
        std::sync::Arc::clone(&self.auth_jobs)
    }

    /// Returns the shared auth approval policy store used by the bridge and
    /// credentials room to enforce remembered approval rules.
    pub fn auth_approval_policy_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::auth_approval_policies::AuthApprovalPolicyStore>>
    {
        std::sync::Arc::clone(&self.auth_approval_policies)
    }

    /// Returns the shared credential gateway used for session-handle issuance
    /// and credential sandbox enforcement.
    pub fn credential_gateway(&self) -> std::sync::Arc<credential_gateway::CredentialGateway> {
        std::sync::Arc::clone(&self.credential_gateway)
    }

    /// Returns the scoped credential vault used by auth/job flows.
    pub fn credential_vault(&self) -> std::sync::Arc<credential_gateway::GoalScopedVault> {
        std::sync::Arc::clone(&self.credential_vault)
    }

    /// Returns the configured auth sandbox launcher when available.
    pub fn auth_engine(&self) -> Option<credential_gateway::auth_engine::AuthSandboxLauncher> {
        self.auth_engine.clone()
    }

    /// Create a [`ProviderRouterLlmClient`] that routes agent completions
    /// through the daemon's `ProviderRouter` with the given sensitivity level.
    ///
    /// The returned client implements `LlmClient` and can be passed directly
    /// to the agent executor. Sensitivity is typically derived from the agent's
    /// task context (e.g. whether it handles private data).
    pub fn make_llm_client(&self, sensitivity: Sensitivity) -> ProviderRouterLlmClient {
        ProviderRouterLlmClient::new(
            Arc::clone(&self.provider_router),
            sensitivity,
            "agent_execution".to_string(),
        )
    }

    /// Returns a clone of the shared identity_content Arc.
    ///
    /// This is used to pass the daemon's identity handle to the reconciler so
    /// they share the same underlying `Mutex<Option<String>>`. When the
    /// reconciler executes `ReloadIdentity`, it updates this handle, and
    /// `resolve_role_config()` immediately sees the new content.
    pub fn identity_content(&self) -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
        std::sync::Arc::clone(&self.identity_content)
    }

    /// Returns a reference to the TrustStore (if available).
    pub fn trust_store(&self) -> Option<&std::sync::Arc<symbiotic_trust::persistence::TrustStore>> {
        self.trust_store.as_ref()
    }

    /// Returns a reference to the role registry.
    pub fn role_registry(&self) -> &RoleRegistry {
        &self.role_registry
    }

    pub fn execute_agent_scope(&self, agent_id: &str, scope: &str, now: u64) -> Result<()> {
        let execution = self.agents.execute_scope(agent_id, scope, now);
        let status = if execution.is_ok() {
            "allowed"
        } else {
            "denied"
        };
        append_agent_lifecycle_log(
            &self.config.agent_log_file,
            AgentLogEntry {
                ts: now,
                event: "agent.scope",
                agent_id,
                status,
                scope: Some(scope),
                detail: "",
            },
        )?;
        if let Some(agent) = self.agents.get_agent(agent_id)? {
            upsert_agent_state(
                &self.config.agent_state_file,
                AgentState {
                    agent_id: agent.id,
                    parent: serialize_agent_parent(&agent.parent),
                    llm_type: serialize_llm_type(&agent.llm_type),
                    trust_level: serialize_trust_level(agent.trust_level),
                    last_status: status.to_string(),
                    last_scope: Some(scope.to_ascii_lowercase()),
                    updated_at: now,
                },
            )?;
        }
        execution
    }

    pub fn get_agent(&self, agent_id: &str) -> anyhow::Result<Option<SecureAgent>> {
        self.agents.get_agent(agent_id)
    }

    pub fn authorize_capability(
        &self,
        token_id: &str,
        subject: &str,
        required_level: AgentTrustLevel,
        scope: &str,
        now: u64,
    ) -> Result<()> {
        let mut broker = self
            .broker
            .lock()
            .map_err(|_| anyhow!("broker lock poisoned"))?;

        let request = AccessRequest {
            subject: subject.to_string(),
            required_level,
            scope: scope.to_string(),
            goal_scope: None,
        };

        // Use audited evaluation when TrustStore is available.
        let result = if let Some(ref store) = self.trust_store {
            broker
                .evaluate_audited(token_id, &request, now, store)
                .map(|_| ())
        } else {
            broker.evaluate(token_id, &request, now).map(|_| ())
        };

        persist_access_broker(&self.config.capability_tokens_file, &broker)?;
        result
    }

    pub fn store_login_credential(
        &self,
        service: &str,
        username: &str,
        secret: &str,
    ) -> Result<()> {
        self.credential_gateway
            .put_credential(credential_gateway::CredentialRecord {
                service: service.to_ascii_lowercase(),
                username: username.to_string(),
                secret: secret.to_string(),
                totp_secret: None,
            })
    }

    pub fn issue_login_session_handle(
        &self,
        target: &str,
        scopes: Vec<String>,
        now: u64,
    ) -> Result<String> {
        let handle = self.credential_gateway.issue_session_handle(
            credential_gateway::AuthRequest {
                target: target.to_ascii_lowercase(),
                scopes,
                session_type: credential_gateway::SessionType::Browser,
                policy: credential_gateway::SessionPolicy {
                    exportable: false,
                    requires_reauth: false,
                },
            },
            now,
        )?;
        Ok(handle.handle_id)
    }

    pub fn validate_login_session_handle(
        &self,
        handle_id: &str,
        target: &str,
        scope: &str,
        now: u64,
    ) -> Result<()> {
        self.credential_gateway
            .validate_session_handle(handle_id, target, scope, now)
    }

    pub fn get_context(
        &self,
        request: &symbiotic_context::ContextRequest,
    ) -> Result<symbiotic_context::ContextPack> {
        self.recall_gateway.get_context(request)
    }

    pub fn archive_records(&self) -> Result<Vec<symbiotic_archive::ArchiveDocument>> {
        self.archive_store.list()
    }

    pub fn archive_record_by_url(
        &self,
        url: &str,
    ) -> Result<Option<symbiotic_archive::ArchiveDocument>> {
        let records = self.archive_store.list()?;
        // T54: The intake pipeline canonicalizes Twitter URLs before storing
        // (e.g. x.com/user/status/ID -> x.com/i/status/ID). Try the canonical
        // form as well so callers don't need to know about this detail.
        let canonical = url::Url::parse(url)
            .ok()
            .map(|parsed| symbiotic_intake::twitter::canonicalize_twitter_url(&parsed).to_string())
            .filter(|c| c != url);
        Ok(records.into_iter().find(|record| {
            let stored = record.source_url.as_deref();
            stored == Some(url) || canonical.as_deref().is_some_and(|c| stored == Some(c))
        }))
    }

    pub fn vault_records(&self) -> Result<Vec<symbiotic_archive::ArchiveDocument>> {
        self.vault_store.list()
    }

    pub fn review_record(&self, record_id: &str) -> Result<Option<symbiotic_review::ReviewRecord>> {
        self.review_store.get(record_id)
    }

    /// Optionally spawn a Process Engineer agent after a goal agent completes.
    ///
    /// Skips if:
    /// - PE is disabled (`enable_process_engineer: false`)
    /// - The finished agent IS a PE (prefix check, prevents recursion)
    /// - The goal type has graduated
    /// - Graduation store or agent monitor is unavailable
    ///
    /// Failures are logged but never propagated — PE is non-critical.
    pub fn maybe_spawn_process_engineer(&self, finished_agent_id: &str, goal_type: &str, now: u64) {
        if !self.config.enable_process_engineer {
            return;
        }

        // Prevent recursion: PE agents have role "process-engineer".
        if finished_agent_id.starts_with("pe_") {
            return;
        }

        let graduation_store = match &self.graduation_store {
            Some(store) => store,
            None => return,
        };

        // Agent monitor is available for PE tool construction when the PE
        // executor is wired up. For now we just verify it exists.
        if self.agent_monitor.is_none() {
            return;
        }

        // Check graduation.
        let config = GraduationConfig::default();
        match graduation_store.is_graduated(goal_type, &config) {
            Ok(true) => {
                tracing::debug!(goal_type, "PE: goal type graduated, skipping");
                return;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, goal_type, "PE: graduation check failed, spawning anyway");
            }
        }

        // Spawn PE agent.
        let pe_task_id = format!("pe-{goal_type}-{now}");
        match self.spawn_task_agent(
            &pe_task_id,
            AgentParent::System,
            false,
            vec!["archive.read".to_string(), "archive.write".to_string()],
            Some("process-engineer".to_string()),
            now,
        ) {
            Ok(agent_id) => {
                tracing::info!(
                    agent_id,
                    goal_type,
                    finished_agent_id,
                    "PE: spawned process engineer for goal type"
                );

                // Create PE workspace with completed goal artifacts
                let pe_dir = self
                    .config
                    .goal_log_file
                    .parent()
                    .unwrap_or(std::path::Path::new("data/goals"))
                    .join(&pe_task_id);
                let _ = std::fs::create_dir_all(&pe_dir);

                // Write PE GOAL.md
                let pe_goal = format!(
                    "# Process Engineer Analysis\n\n\
                    ## Task\n\n\
                    Analyze the completed goal execution and extract improvements.\n\n\
                    ## Completed Goal\n\n\
                    - Goal Type: {goal_type}\n\
                    - Agent ID: {finished_agent_id}\n\
                    - Completed At: {now}\n\n\
                    ## Instructions\n\n\
                    1. Read the completed goal's artifacts in `data/goals/{finished_agent_id}/`\n\
                    2. Analyze: efficiency, tool usage, error patterns, skill gaps\n\
                    3. Write a PE report to `knowledge-base/operations/pe-reports/{pe_task_id}.md`\n\
                    4. If patterns warrant it, add learned rules to CLAUDE.md\n\
                    5. If a reusable skill should be created, note it in the report\n"
                );
                let _ = std::fs::write(pe_dir.join("GOAL.md"), &pe_goal);

                // Queue PE execution job (uses same workflow system as regular goals)
                let pe_template = format!("agent-execute:{pe_task_id}");
                if let Err(e) = self.queue_workflow_run(&pe_template) {
                    tracing::warn!(error = %e, "PE: failed to queue PE execution job");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, goal_type, "PE: failed to spawn process engineer");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use symbiotic_providers::{
        CapabilitySet, CompletionResponse, ModelProvider, ProviderCapability, ProviderClass,
        ProviderError, ProviderRegistry, RegisteredProvider,
    };

    // -- Mock completion provider for testing the adapter -----------------------

    struct MockCompletionProvider {
        name: String,
        class: ProviderClass,
        model: String,
        capabilities: CapabilitySet,
        response_content: String,
        call_count: AtomicU32,
    }

    impl MockCompletionProvider {
        fn new(name: &str, class: ProviderClass, response: &str) -> Self {
            Self {
                name: name.to_string(),
                class,
                model: format!("{name}-model"),
                capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
                response_content: response.to_string(),
                call_count: AtomicU32::new(0),
            }
        }
    }

    impl ModelProvider for MockCompletionProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn provider_class(&self) -> ProviderClass {
            self.class
        }
        fn model_name(&self) -> &str {
            &self.model
        }
        fn capabilities(&self) -> &CapabilitySet {
            &self.capabilities
        }
        fn pricing(&self) -> Option<&symbiotic_providers::PricingInfo> {
            None
        }
    }

    #[async_trait]
    impl symbiotic_providers::CompletionProvider for MockCompletionProvider {
        async fn complete(
            &self,
            _request: &CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                content: self.response_content.clone(),
                model: self.model.clone(),
                input_tokens: Some(10),
                output_tokens: Some(20),
                finish_reason: Some("stop".into()),
            })
        }
    }

    fn register_completion(registry: &mut ProviderRegistry, provider: Arc<MockCompletionProvider>) {
        let base: Arc<dyn ModelProvider> = provider.clone();
        let completion: Arc<dyn symbiotic_providers::CompletionProvider> = provider;
        registry.register(RegisteredProvider {
            base,
            completion: Some(completion),
            embedding: None,
            image: None,
            video: None,
            agent: None,
        });
    }

    // -- Tests ------------------------------------------------------------------

    #[tokio::test]
    async fn adapter_routes_shareable_completion() {
        let mut registry = ProviderRegistry::new();
        let provider = Arc::new(MockCompletionProvider::new(
            "test-cloud",
            ProviderClass::Cloud,
            "Hello from cloud!",
        ));
        register_completion(&mut registry, provider.clone());

        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));
        let client =
            ProviderRouterLlmClient::new(router, Sensitivity::Shareable, "test".to_string());

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "You are a test agent.".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "Say hello".to_string(),
            },
        ];

        let result = client
            .chat(&messages, true)
            .await
            .expect("chat should succeed");
        assert_eq!(result, "Hello from cloud!");
        assert_eq!(provider.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn adapter_routes_restricted_to_local_only() {
        let mut registry = ProviderRegistry::new();
        let cloud = Arc::new(MockCompletionProvider::new(
            "cloud-provider",
            ProviderClass::Cloud,
            "cloud response",
        ));
        let local = Arc::new(MockCompletionProvider::new(
            "local-provider",
            ProviderClass::Local,
            "local response",
        ));
        register_completion(&mut registry, cloud.clone());
        register_completion(&mut registry, local.clone());

        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));
        let client =
            ProviderRouterLlmClient::new(router, Sensitivity::Restricted, "test".to_string());

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "private question".to_string(),
        }];

        let result = client
            .chat(&messages, false)
            .await
            .expect("chat should succeed");
        assert_eq!(result, "local response");
        // Cloud should never have been called.
        assert_eq!(cloud.call_count.load(Ordering::SeqCst), 0);
        assert_eq!(local.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn adapter_routes_private_to_local_only() {
        let mut registry = ProviderRegistry::new();
        let cloud = Arc::new(MockCompletionProvider::new(
            "cloud-provider",
            ProviderClass::Cloud,
            "cloud response",
        ));
        let local = Arc::new(MockCompletionProvider::new(
            "local-provider",
            ProviderClass::Local,
            "local response",
        ));
        register_completion(&mut registry, cloud.clone());
        register_completion(&mut registry, local.clone());

        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));
        let client = ProviderRouterLlmClient::new(router, Sensitivity::Private, "test".to_string());

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "very private".to_string(),
        }];

        let result = client
            .chat(&messages, false)
            .await
            .expect("chat should succeed");
        assert_eq!(result, "local response");
        assert_eq!(cloud.call_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn adapter_fails_when_no_providers() {
        let registry = ProviderRegistry::new();
        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));
        let client =
            ProviderRouterLlmClient::new(router, Sensitivity::Shareable, "test".to_string());

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        }];

        let err = client
            .chat(&messages, true)
            .await
            .expect_err("should fail with no providers");
        assert!(
            err.to_string()
                .contains("provider router completion failed"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn adapter_maps_role_strings_correctly() {
        // Use a provider that echoes the first message role for verification.
        struct RoleEchoProvider {
            name: String,
            model: String,
            capabilities: CapabilitySet,
        }

        impl ModelProvider for RoleEchoProvider {
            fn name(&self) -> &str {
                &self.name
            }
            fn provider_class(&self) -> ProviderClass {
                ProviderClass::Cloud
            }
            fn model_name(&self) -> &str {
                &self.model
            }
            fn capabilities(&self) -> &CapabilitySet {
                &self.capabilities
            }
            fn pricing(&self) -> Option<&symbiotic_providers::PricingInfo> {
                None
            }
        }

        #[async_trait]
        impl symbiotic_providers::CompletionProvider for RoleEchoProvider {
            async fn complete(
                &self,
                request: &CompletionRequest,
            ) -> Result<CompletionResponse, ProviderError> {
                // Echo back the roles of all messages as a comma-separated string.
                let roles: Vec<&str> = request
                    .messages
                    .iter()
                    .map(|m| match m.role {
                        ProviderRole::System => "system",
                        ProviderRole::User => "user",
                        ProviderRole::Assistant => "assistant",
                    })
                    .collect();
                Ok(CompletionResponse {
                    content: roles.join(","),
                    model: self.model.clone(),
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: None,
                })
            }
        }

        let mut registry = ProviderRegistry::new();
        let provider = Arc::new(RoleEchoProvider {
            name: "echo".to_string(),
            model: "echo-model".to_string(),
            capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
        });
        let base: Arc<dyn ModelProvider> = provider.clone();
        let completion: Arc<dyn symbiotic_providers::CompletionProvider> = provider;
        registry.register(RegisteredProvider {
            base,
            completion: Some(completion),
            embedding: None,
            image: None,
            video: None,
            agent: None,
        });

        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));
        let client =
            ProviderRouterLlmClient::new(router, Sensitivity::Shareable, "test".to_string());

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "system prompt".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "user message".to_string(),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: "assistant response".to_string(),
            },
            ChatMessage {
                role: "unknown_role".to_string(),
                content: "should become user".to_string(),
            },
        ];

        let result = client.chat(&messages, false).await.expect("should succeed");
        assert_eq!(result, "system,user,assistant,user");
    }

    #[tokio::test]
    async fn adapter_source_attribution() {
        let mut registry = ProviderRegistry::new();
        let provider = Arc::new(MockCompletionProvider::new(
            "test-provider",
            ProviderClass::Cloud,
            "ok",
        ));
        register_completion(&mut registry, provider);

        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));
        let client = ProviderRouterLlmClient::new(
            router,
            Sensitivity::Shareable,
            "agent_execution".to_string(),
        );

        // Just verify it works with the correct source — metering integration
        // is tested in symbiotic-providers. This confirms the source string
        // is passed through without error.
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "test".to_string(),
        }];

        let result = client.chat(&messages, true).await;
        assert!(result.is_ok());
    }
}
