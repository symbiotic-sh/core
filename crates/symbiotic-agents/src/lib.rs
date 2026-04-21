#![recursion_limit = "256"]

pub mod action_dispatch;
pub mod builtin_tools;
pub mod council;
pub mod execution_plan;
pub mod executor;
pub mod graduation;
pub mod llm;
pub mod llm_runtime;
pub mod monitoring;
pub mod pe_tools;
pub mod pipeline;
pub mod pool;
pub mod skill_synthesis;
pub mod source_archeology;
pub mod swarm;
pub mod tools;
pub mod workspace_tools;
pub mod worktree;

pub use action_dispatch::{
    ActionDispatcher, AgentOps, BatchDispatchSummary, DispatchAction, DispatchActionType,
    DispatchResult, GoalOps, IdentityOps, SkillOps,
};
pub use pool::{AgentPool, PoolError, PoolLlmType, SlotState, WorkerId, WorkerSlot};
pub use source_archeology::{
    ArcheologyError, ArcheologyMode, ArcheologyTarget, AspirationalClassifier, Autonomy,
    Classifiers, DeclarativeTriager, DiagnoseConfig, Diagnosis, DiagnosisClassifier,
    DiagnosisProjection, DiagnosisVerdict, ExcavationReport, Finding, FindingAction,
    FindingDisposition, FindingSeverity, FindingSourceStage, GitApplyCheckVerifier, GoalAlignment,
    HandoffConfig, HandoffInput, HandoffReport, LintVerifier, LlmAspirationalClassifier,
    LlmDiagnosisClassifier, LlmReconciler, LlmReporter, LlmScaffolder, LlmTriager,
    MarkdownlintVerifier, NoopLintVerifier, Observation, ObservationCategory, OperatorQuestion,
    OrchestratorInput, PatchVerifier, PathPattern, PipelineOutcome, PipelineRun, ProjectContext,
    RawDiagnosis, RawDisposition, ReconcileInput, ReconciledPatch, Reconciler, Reporter, Reviewer,
    ScaffoldConfig, ScaffoldFile, ScaffoldInput, ScaffoldOutput, Scaffolder,
    SourceArcheologyRunner, SourceSidePatch, StageConfigs, StalenessClass, StalenessReport,
    StalenessRow, TriageConfig, TriageContext, TriageDecision, Triager, VerifyConfig,
    VerifyOutcome,
};

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
pub use symbiotic_core::now_unix;
use symbiotic_trust::{AccessBroker, AccessRequest, AgentTrustLevel, CapabilityToken};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmType {
    Local { model: String },
    Cloud { provider: String, model: String },
    Hybrid { cloud: String, local: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentParent {
    Goal { slug: String },
    Stream { goal: String, stream: String },
    User,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureAgent {
    pub id: String,
    pub parent: AgentParent,
    pub llm_type: LlmType,
    pub trust_level: AgentTrustLevel,
    pub capability_tokens: Vec<String>,
    pub audit_trail: Vec<AuditEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub timestamp: u64,
    pub scope: String,
    pub status: String,
    pub token_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CapabilityRequest {
    pub scope: String,
    pub purpose: String,
}

#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub id: String,
    pub parent: AgentParent,
    pub requires_private_data: bool,
    pub capabilities: Vec<CapabilityRequest>,
    /// Optional agent role name (e.g., "researcher", "coder"). When set, the
    /// framework resolves the role configuration from the `RoleRegistry` to
    /// customize the system prompt and execution parameters.
    pub role: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FrameworkConfig {
    pub local_model: String,
    pub cloud_provider: String,
    pub cloud_model: String,
}

impl Default for FrameworkConfig {
    fn default() -> Self {
        Self {
            local_model: "qwen2.5:7b".to_string(),
            cloud_provider: "openrouter".to_string(),
            cloud_model: "anthropic/claude-sonnet".to_string(),
        }
    }
}

pub struct SecureAgentFramework {
    config: FrameworkConfig,
    broker: Mutex<AccessBroker>,
    agents: Mutex<HashMap<String, SecureAgent>>,
}

impl SecureAgentFramework {
    pub fn new(config: FrameworkConfig) -> Self {
        Self {
            config,
            broker: Mutex::new(AccessBroker::new()),
            agents: Mutex::new(HashMap::new()),
        }
    }

    pub fn spawn_agent(&self, task: TaskSpec, now: u64) -> Result<SecureAgent> {
        let llm_type = self.determine_llm_type(&task);
        let trust_level = max_trust_for_llm(&llm_type);
        let agent_id = format!("agent_{:x}", simple_hash(&format!("{}:{now}", task.id)));

        let mut token_ids = Vec::new();
        {
            let mut broker = self
                .broker
                .lock()
                .map_err(|_| anyhow!("broker lock poisoned"))?;
            for capability in &task.capabilities {
                let required = required_trust_for_scope(&capability.scope);
                if required > trust_level {
                    return Err(anyhow!(
                        "capability {} requires {:?}, exceeds agent trust {:?}",
                        capability.scope,
                        required,
                        trust_level
                    ));
                }

                let token_id = format!(
                    "tok_{:x}",
                    simple_hash(&format!("{}:{}:{}", agent_id, capability.scope, now))
                );
                // Derive goal scope from agent parent when scoped to a goal.
                let goal_scope = match &task.parent {
                    AgentParent::Goal { slug } => Some(slug.clone()),
                    AgentParent::Stream { goal, .. } => Some(goal.clone()),
                    AgentParent::User | AgentParent::System => None,
                };
                broker.issue_token(CapabilityToken {
                    token_id: token_id.clone(),
                    subject: agent_id.clone(),
                    trust_level,
                    scopes: [capability.scope.to_ascii_lowercase()]
                        .into_iter()
                        .collect(),
                    expires_at: now + 3600,
                    one_time: false,
                    consumed: false,
                    goal_scope,
                });
                token_ids.push(token_id);
            }
        }

        let agent = SecureAgent {
            id: agent_id.clone(),
            parent: task.parent,
            llm_type,
            trust_level,
            capability_tokens: token_ids,
            audit_trail: Vec::new(),
        };
        self.agents
            .lock()
            .map_err(|_| anyhow!("agents lock poisoned"))?
            .insert(agent_id, agent.clone());
        Ok(agent)
    }

    pub fn execute_scope(&self, agent_id: &str, scope: &str, now: u64) -> Result<()> {
        let mut agents = self
            .agents
            .lock()
            .map_err(|_| anyhow!("agents lock poisoned"))?;
        let agent = agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        let scope_normalized = scope.to_ascii_lowercase();
        // Derive goal scope from agent parent for request matching.
        let goal_scope = match &agent.parent {
            AgentParent::Goal { slug } => Some(slug.clone()),
            AgentParent::Stream { goal, .. } => Some(goal.clone()),
            AgentParent::User | AgentParent::System => None,
        };
        let mut token_id: Option<String> = None;
        for candidate in &agent.capability_tokens {
            let mut broker = self
                .broker
                .lock()
                .map_err(|_| anyhow!("broker lock poisoned"))?;
            if broker
                .evaluate(
                    candidate,
                    &AccessRequest {
                        subject: agent.id.clone(),
                        required_level: required_trust_for_scope(&scope_normalized),
                        scope: scope_normalized.clone(),
                        goal_scope: goal_scope.clone(),
                    },
                    now,
                )
                .is_ok()
            {
                token_id = Some(candidate.clone());
                break;
            }
        }

        if let Some(token_id) = token_id {
            agent.audit_trail.push(AuditEntry {
                timestamp: now,
                scope: scope_normalized,
                status: "allowed".to_string(),
                token_id: Some(token_id.clone()),
            });
            return Ok(());
        }

        agent.audit_trail.push(AuditEntry {
            timestamp: now,
            scope: scope_normalized,
            status: "denied".to_string(),
            token_id: None,
        });
        Err(anyhow!("scope not authorized"))
    }

    pub fn get_agent(&self, agent_id: &str) -> Result<Option<SecureAgent>> {
        Ok(self
            .agents
            .lock()
            .map_err(|_| anyhow!("agents lock poisoned"))?
            .get(agent_id)
            .cloned())
    }

    fn determine_llm_type(&self, task: &TaskSpec) -> LlmType {
        // Credential operations are handled by the Auth Script Engine
        // (infrastructure), not by agent LLMs. Only `requires_private_data`
        // (e.g. Distillery processing user content) forces a local model.
        if task.requires_private_data {
            return LlmType::Local {
                model: self.config.local_model.clone(),
            };
        }
        LlmType::Hybrid {
            cloud: format!("{}/{}", self.config.cloud_provider, self.config.cloud_model),
            local: self.config.local_model.clone(),
        }
    }
}

pub fn max_trust_for_llm(llm_type: &LlmType) -> AgentTrustLevel {
    match llm_type {
        LlmType::Local { .. } => AgentTrustLevel::ExternalAct,
        LlmType::Cloud { .. } => AgentTrustLevel::ArchiveWrite,
        LlmType::Hybrid { .. } => AgentTrustLevel::CredentialAccess,
    }
}

pub fn required_trust_for_scope(scope: &str) -> AgentTrustLevel {
    let scope = scope.to_ascii_lowercase();
    if scope == "vm.network.modify" || scope == "git.push:protected" || scope == "pr.merge" {
        AgentTrustLevel::ExternalAct
    } else if scope.contains("credential") || scope.contains("action.browser.login") {
        AgentTrustLevel::CredentialAccess
    } else if scope.contains("archive.write")
        || scope.starts_with("vm.")
        || scope == "git.push"
        || scope == "pr.create"
        || scope == "pr.review"
        || scope == "check.report"
    {
        AgentTrustLevel::ArchiveWrite
    } else {
        // git.read and unrecognized scopes default to ReadOnly
        AgentTrustLevel::ReadOnly
    }
}

fn simple_hash(input: &str) -> u64 {
    let mut acc = 1469598103934665603u64;
    for byte in input.bytes() {
        acc ^= byte as u64;
        acc = acc.wrapping_mul(1099511628211u64);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_agent_uses_hybrid_for_browser_login() {
        // action.browser.login no longer forces Local LLM — credential handling
        // is now done by the Auth Script Engine (infrastructure), not by the
        // agent's LLM. The scope requires CredentialAccess trust (agent is
        // requesting a login flow, not handling credentials directly).
        let framework = SecureAgentFramework::new(FrameworkConfig::default());
        let agent = framework
            .spawn_agent(
                TaskSpec {
                    id: "task-1".to_string(),
                    parent: AgentParent::Goal {
                        slug: "build".to_string(),
                    },
                    requires_private_data: false,
                    capabilities: vec![CapabilityRequest {
                        scope: "action.browser.login".to_string(),
                        purpose: "login flow".to_string(),
                    }],
                    role: None,
                },
                now_unix(),
            )
            .expect("spawn should work");

        assert!(matches!(agent.llm_type, LlmType::Hybrid { .. }));
        assert_eq!(agent.trust_level, AgentTrustLevel::CredentialAccess);
    }

    #[test]
    fn spawn_agent_uses_hybrid_for_non_sensitive_tasks() {
        let framework = SecureAgentFramework::new(FrameworkConfig::default());
        let agent = framework
            .spawn_agent(
                TaskSpec {
                    id: "task-2".to_string(),
                    parent: AgentParent::System,
                    requires_private_data: false,
                    capabilities: vec![CapabilityRequest {
                        scope: "archive.read".to_string(),
                        purpose: "read context".to_string(),
                    }],
                    role: None,
                },
                now_unix(),
            )
            .expect("spawn should work");

        assert!(matches!(agent.llm_type, LlmType::Hybrid { .. }));
    }

    #[test]
    fn execute_scope_records_audit_success_and_failure() {
        let framework = SecureAgentFramework::new(FrameworkConfig::default());
        let now = now_unix();
        let agent = framework
            .spawn_agent(
                TaskSpec {
                    id: "task-3".to_string(),
                    parent: AgentParent::System,
                    requires_private_data: false,
                    capabilities: vec![CapabilityRequest {
                        scope: "archive.read".to_string(),
                        purpose: "read".to_string(),
                    }],
                    role: None,
                },
                now,
            )
            .expect("spawn should work");

        framework
            .execute_scope(&agent.id, "archive.read", now + 1)
            .expect("scope should be allowed");
        let denied = framework.execute_scope(&agent.id, "credential.read", now + 2);
        assert!(denied.is_err());

        let updated = framework
            .get_agent(&agent.id)
            .expect("get_agent should not fail")
            .expect("agent exists");
        assert_eq!(updated.audit_trail.len(), 2);
        assert_eq!(updated.audit_trail[0].status, "allowed");
        assert_eq!(updated.audit_trail[1].status, "denied");
    }
}
