use credential_gateway::GatewayError;
use symbiotic_agents::{AgentParent, LlmType};
use symbiotic_trust::AgentTrustLevel;

pub(crate) fn is_terminal_auth_issue_error(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<GatewayError>(),
        Some(
            GatewayError::BlockedTarget(_)
                | GatewayError::UnsafeTarget(_)
                | GatewayError::MissingCredentials(_)
        )
    )
}

pub(crate) fn serialize_agent_parent(parent: &AgentParent) -> String {
    match parent {
        AgentParent::Goal { slug } => format!("goal:{slug}"),
        AgentParent::Stream { goal, stream } => format!("stream:{goal}:{stream}"),
        AgentParent::User => "user".to_string(),
        AgentParent::System => "system".to_string(),
    }
}

pub(crate) fn serialize_llm_type(llm_type: &LlmType) -> String {
    match llm_type {
        LlmType::Local { model } => format!("local:{model}"),
        LlmType::Cloud { provider, model } => format!("cloud:{provider}:{model}"),
        LlmType::Hybrid { cloud, local } => format!("hybrid:{cloud}:{local}"),
    }
}

pub(crate) fn serialize_trust_level(level: AgentTrustLevel) -> String {
    match level {
        AgentTrustLevel::ReadOnly => "read_only".to_string(),
        AgentTrustLevel::ArchiveWrite => "archive_write".to_string(),
        AgentTrustLevel::CredentialAccess => "credential_access".to_string(),
        AgentTrustLevel::ExternalAct => "external_act".to_string(),
    }
}
