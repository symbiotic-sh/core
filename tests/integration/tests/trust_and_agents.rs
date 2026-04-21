//! Integration test: Trust -> Agent -> Tool Execution
//!
//! Verifies capability token issuance, agent spawning, scope enforcement,
//! and the built-in tool capability checks across the trust and agent crates.

use std::sync::Arc;

use anyhow::Result;
use symbiotic_agents::builtin_tools::{CapabilityChecker, RecallBackend, RecallItem, RecallTool};
use symbiotic_agents::tools::Tool;
use symbiotic_agents::{
    AgentParent, CapabilityRequest, FrameworkConfig, SecureAgentFramework, TaskSpec,
};
use symbiotic_trust::{AccessBroker, AccessRequest, AgentTrustLevel, CapabilityToken};

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

struct MockRecall;

#[async_trait::async_trait]
impl RecallBackend for MockRecall {
    async fn query(&self, query: &str, max_items: usize) -> Result<Vec<RecallItem>> {
        Ok(vec![RecallItem {
            id: "r1".to_string(),
            title: format!("Result for: {query}"),
            snippet: "Context snippet".to_string(),
            score: 0.95,
        }]
        .into_iter()
        .take(max_items)
        .collect())
    }
}

/// Capability checker backed by the real SecureAgentFramework.
struct FrameworkChecker {
    framework: Arc<SecureAgentFramework>,
    now: u64,
}

impl CapabilityChecker for FrameworkChecker {
    fn check(&self, agent_id: &str, scope: &str) -> Result<()> {
        self.framework.execute_scope(agent_id, scope, self.now)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn capability_token_grants_and_denies_access() {
    let now = symbiotic_trust::now_unix();
    let mut broker = AccessBroker::new();

    // Issue a token with archive.read scope at ReadOnly trust
    broker.issue_token(CapabilityToken {
        token_id: "tok-1".to_string(),
        subject: "agent-test".to_string(),
        trust_level: AgentTrustLevel::ReadOnly,
        scopes: ["archive.read".to_string()].into_iter().collect(),
        expires_at: now + 3600,
        one_time: false,
        consumed: false,
        goal_scope: None,
    });

    // Allowed: matching scope and sufficient trust
    let decision = broker
        .evaluate(
            "tok-1",
            &AccessRequest {
                subject: "agent-test".to_string(),
                required_level: AgentTrustLevel::ReadOnly,
                scope: "archive.read".to_string(),
                goal_scope: None,
            },
            now,
        )
        .expect("should allow");
    assert!(decision.allowed);

    // Denied: scope not in token
    let err = broker
        .evaluate(
            "tok-1",
            &AccessRequest {
                subject: "agent-test".to_string(),
                required_level: AgentTrustLevel::ReadOnly,
                scope: "archive.write".to_string(),
                goal_scope: None,
            },
            now,
        )
        .expect_err("should deny");
    assert!(err.to_string().contains("scope not permitted"));
}

#[test]
fn spawn_agent_with_capabilities_and_execute_scope() {
    let framework = SecureAgentFramework::new(FrameworkConfig::default());
    let now = symbiotic_agents::now_unix();

    // Spawn an agent with archive.read capability
    let agent = framework
        .spawn_agent(
            TaskSpec {
                id: "task-integration".to_string(),
                parent: AgentParent::System,
                requires_private_data: false,
                capabilities: vec![CapabilityRequest {
                    scope: "archive.read".to_string(),
                    purpose: "read Archive".to_string(),
                }],
                role: None,
            },
            now,
        )
        .expect("spawn");

    assert!(!agent.capability_tokens.is_empty());

    // Execute allowed scope
    framework
        .execute_scope(&agent.id, "archive.read", now + 1)
        .expect("archive.read should be allowed");

    // Execute denied scope
    let denied = framework.execute_scope(&agent.id, "credential.read", now + 2);
    assert!(denied.is_err(), "credential.read should be denied");

    // Verify audit trail
    let updated = framework
        .get_agent(&agent.id)
        .expect("get_agent should not fail")
        .expect("agent exists");
    assert_eq!(updated.audit_trail.len(), 2);
    assert_eq!(updated.audit_trail[0].status, "allowed");
    assert_eq!(updated.audit_trail[1].status, "denied");
}

#[test]
fn browser_login_scope_uses_hybrid_llm_and_credential_access_trust() {
    let framework = SecureAgentFramework::new(FrameworkConfig::default());
    let now = symbiotic_agents::now_unix();

    // Browser login no longer forces a Local LLM. The agent requests a login
    // flow, but credential execution happens in the auth infrastructure.
    let agent = framework
        .spawn_agent(
            TaskSpec {
                id: "task-login".to_string(),
                parent: AgentParent::Goal {
                    slug: "automation".to_string(),
                },
                requires_private_data: false,
                capabilities: vec![CapabilityRequest {
                    scope: "action.browser.login".to_string(),
                    purpose: "login to service".to_string(),
                }],
                role: None,
            },
            now,
        )
        .expect("spawn");

    assert!(
        matches!(agent.llm_type, symbiotic_agents::LlmType::Hybrid { .. }),
        "browser login should use hybrid routing"
    );
    assert_eq!(agent.trust_level, AgentTrustLevel::CredentialAccess);
}

#[test]
fn agent_cannot_exceed_trust_level_for_scope() {
    let framework = SecureAgentFramework::new(FrameworkConfig::default());
    let now = symbiotic_agents::now_unix();

    // A non-sensitive task gets Hybrid LLM -> CredentialAccess trust
    // Trying to request ExternalAct scope (browser login) should fail
    // because Hybrid trust is CredentialAccess, below ExternalAct
    let result = framework.spawn_agent(
        TaskSpec {
            id: "task-escalation".to_string(),
            parent: AgentParent::System,
            requires_private_data: false,
            capabilities: vec![
                CapabilityRequest {
                    scope: "archive.read".to_string(),
                    purpose: "read".to_string(),
                },
                CapabilityRequest {
                    scope: "action.browser.login".to_string(),
                    purpose: "login".to_string(),
                },
            ],
            role: None,
        },
        now,
    );

    // This should succeed because browser.login now requires only
    // CredentialAccess trust and is compatible with Hybrid routing.
    assert!(
        result.is_ok(),
        "agent with browser.login scope should get hybrid LLM and sufficient trust"
    );
}

#[tokio::test]
async fn recall_tool_checks_capability_before_execution() {
    let framework = Arc::new(SecureAgentFramework::new(FrameworkConfig::default()));
    let now = symbiotic_agents::now_unix();

    // Spawn agent with archive.read
    let agent = framework
        .spawn_agent(
            TaskSpec {
                id: "task-tool-test".to_string(),
                parent: AgentParent::System,
                requires_private_data: false,
                capabilities: vec![CapabilityRequest {
                    scope: "archive.read".to_string(),
                    purpose: "recall query".to_string(),
                }],
                role: None,
            },
            now,
        )
        .expect("spawn");

    let checker = Arc::new(FrameworkChecker {
        framework: framework.clone(),
        now: now + 1,
    });

    let tool = RecallTool::new(agent.id.clone(), Arc::new(MockRecall), checker);

    // Should succeed: agent has archive.read
    let result = tool
        .execute(serde_json::json!({"query": "daemon architecture"}))
        .await
        .expect("execute");
    assert!(result.success);
    assert!(result.output.contains("Result for: daemon architecture"));
}

#[tokio::test]
async fn recall_tool_denied_without_read_capability() {
    let framework = Arc::new(SecureAgentFramework::new(FrameworkConfig::default()));
    let now = symbiotic_agents::now_unix();

    // Spawn agent with only archive.write (not archive.read)
    let agent = framework
        .spawn_agent(
            TaskSpec {
                id: "task-no-read".to_string(),
                parent: AgentParent::System,
                requires_private_data: false,
                capabilities: vec![CapabilityRequest {
                    scope: "archive.write".to_string(),
                    purpose: "write only".to_string(),
                }],
                role: None,
            },
            now,
        )
        .expect("spawn");

    let checker = Arc::new(FrameworkChecker {
        framework: framework.clone(),
        now: now + 1,
    });

    let tool = RecallTool::new(agent.id.clone(), Arc::new(MockRecall), checker);

    // Should fail: agent lacks archive.read
    let err = tool
        .execute(serde_json::json!({"query": "test"}))
        .await
        .expect_err("should deny");
    assert!(
        err.to_string().contains("not authorized"),
        "error should indicate authorization failure: {err}",
    );
}

#[test]
fn one_time_token_consumed_after_single_use() {
    let now = symbiotic_trust::now_unix();
    let mut broker = AccessBroker::new();

    broker.issue_token(CapabilityToken {
        token_id: "ot-1".to_string(),
        subject: "agent-ot".to_string(),
        trust_level: AgentTrustLevel::ArchiveWrite,
        scopes: ["archive.write".to_string()].into_iter().collect(),
        expires_at: now + 3600,
        one_time: true,
        consumed: false,
        goal_scope: None,
    });

    let request = AccessRequest {
        subject: "agent-ot".to_string(),
        required_level: AgentTrustLevel::ArchiveWrite,
        scope: "archive.write".to_string(),
        goal_scope: None,
    };

    // First use: succeeds
    let decision = broker.evaluate("ot-1", &request, now).expect("first use");
    assert!(decision.allowed);

    // Second use: fails (consumed)
    let err = broker
        .evaluate("ot-1", &request, now + 1)
        .expect_err("second use should fail");
    assert!(err.to_string().contains("already consumed"));
}

#[test]
fn expired_token_denied() {
    let now = symbiotic_trust::now_unix();
    let mut broker = AccessBroker::new();

    broker.issue_token(CapabilityToken {
        token_id: "exp-1".to_string(),
        subject: "agent-exp".to_string(),
        trust_level: AgentTrustLevel::ReadOnly,
        scopes: ["archive.read".to_string()].into_iter().collect(),
        expires_at: now + 10,
        one_time: false,
        consumed: false,
        goal_scope: None,
    });

    // Use after expiry
    let err = broker
        .evaluate(
            "exp-1",
            &AccessRequest {
                subject: "agent-exp".to_string(),
                required_level: AgentTrustLevel::ReadOnly,
                scope: "archive.read".to_string(),
                goal_scope: None,
            },
            now + 11,
        )
        .expect_err("should be expired");
    assert!(err.to_string().contains("expired"));
}
