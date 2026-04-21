//! Core types for agent role configuration and prompt versioning.

use serde::{Deserialize, Serialize};

/// A named agent role with associated prompt, provider preference, and capabilities.
///
/// Roles are loaded from TOML files and define how an agent should behave
/// when assigned a particular persona (researcher, coder, reviewer, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRole {
    /// Unique role identifier (e.g., "researcher", "coder", "reviewer").
    pub name: String,

    /// Human-readable description of what this role does.
    pub description: String,

    /// Active prompt version (e.g., "v1", "v2"). Must match a version in `versions`.
    pub active_version: String,

    /// Preferred provider name from the provider registry (e.g., "anthropic", "ollama").
    /// When `None`, the router picks the best available.
    #[serde(default)]
    pub preferred_provider: Option<String>,

    /// Required capability scopes (e.g., "archive.read", "credential.read").
    #[serde(default)]
    pub required_capabilities: Vec<String>,

    /// Archive tag patterns to include in agent context.
    /// Glob-style: e.g., ["architecture/*", "security/*"].
    #[serde(default)]
    pub context_patterns: Vec<String>,

    /// Minimum trust level name (e.g., "ReadOnly", "ArchiveWrite").
    #[serde(default)]
    pub min_trust_level: Option<String>,

    /// Whether this role handles private/sensitive data (forces local LLM).
    #[serde(default)]
    pub requires_private_data: bool,

    /// Maximum iterations for the ReAct loop (overrides framework default).
    #[serde(default)]
    pub max_iterations: Option<usize>,

    /// Prompt versions defined for this role.
    pub versions: Vec<PromptVersion>,
}

/// A single versioned prompt for an agent role.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptVersion {
    /// Version tag (e.g., "v1", "v2").
    pub version: String,

    /// The system prompt text. Either inline or loaded from a file.
    pub system_prompt: String,

    /// Optional changelog entry describing what changed from the prior version.
    #[serde(default)]
    pub changelog: Option<String>,
}

/// A resolved role ready for use by the executor.
///
/// Contains the active prompt text and all configuration needed to spawn
/// and run an agent with this role.
#[derive(Debug, Clone)]
pub struct ResolvedRole {
    /// Role name.
    pub name: String,
    /// Active system prompt text.
    pub system_prompt: String,
    /// Active version tag.
    pub version: String,
    /// Preferred provider name.
    pub preferred_provider: Option<String>,
    /// Required capability scopes.
    pub required_capabilities: Vec<String>,
    /// Context tag patterns.
    pub context_patterns: Vec<String>,
    /// Minimum trust level name.
    pub min_trust_level: Option<String>,
    /// Whether this role requires local-only LLM.
    pub requires_private_data: bool,
    /// Max ReAct iterations override.
    pub max_iterations: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_role_toml() -> &'static str {
        r#"
name = "researcher"
description = "Deep research agent for knowledge synthesis"
active_version = "v1"
preferred_provider = "anthropic"
required_capabilities = ["archive.read"]
context_patterns = ["research/*", "papers/*"]
min_trust_level = "ReadOnly"
requires_private_data = false
max_iterations = 15

[[versions]]
version = "v1"
system_prompt = "You are a research agent. Synthesize information from provided sources."
changelog = "Initial version"

[[versions]]
version = "v2"
system_prompt = "You are an expert research agent. Analyze and synthesize knowledge from multiple sources. Cite evidence for all claims."
changelog = "Added citation requirement and expertise framing"
"#
    }

    #[test]
    fn deserialize_role_from_toml() {
        let role: AgentRole = toml::from_str(sample_role_toml()).expect("parse role TOML");
        assert_eq!(role.name, "researcher");
        assert_eq!(
            role.description,
            "Deep research agent for knowledge synthesis"
        );
        assert_eq!(role.active_version, "v1");
        assert_eq!(role.preferred_provider.as_deref(), Some("anthropic"));
        assert_eq!(role.required_capabilities, vec!["archive.read"]);
        assert_eq!(role.context_patterns, vec!["research/*", "papers/*"]);
        assert_eq!(role.min_trust_level.as_deref(), Some("ReadOnly"));
        assert!(!role.requires_private_data);
        assert_eq!(role.max_iterations, Some(15));
        assert_eq!(role.versions.len(), 2);
    }

    #[test]
    fn serialize_role_roundtrip() {
        let role: AgentRole = toml::from_str(sample_role_toml()).expect("parse");
        let serialized = toml::to_string_pretty(&role).expect("serialize");
        let reparsed: AgentRole = toml::from_str(&serialized).expect("reparse");
        assert_eq!(reparsed.name, role.name);
        assert_eq!(reparsed.versions.len(), role.versions.len());
    }

    #[test]
    fn minimal_role_defaults() {
        let toml = r#"
name = "simple"
description = "Minimal role"
active_version = "v1"

[[versions]]
version = "v1"
system_prompt = "You are a helpful agent."
"#;
        let role: AgentRole = toml::from_str(toml).expect("parse minimal");
        assert_eq!(role.name, "simple");
        assert!(role.preferred_provider.is_none());
        assert!(role.required_capabilities.is_empty());
        assert!(role.context_patterns.is_empty());
        assert!(role.min_trust_level.is_none());
        assert!(!role.requires_private_data);
        assert!(role.max_iterations.is_none());
    }

    #[test]
    fn prompt_version_changelog_optional() {
        let toml = r#"
name = "test"
description = "Test"
active_version = "v1"

[[versions]]
version = "v1"
system_prompt = "Hello"
"#;
        let role: AgentRole = toml::from_str(toml).expect("parse");
        assert!(role.versions[0].changelog.is_none());
    }

    #[test]
    fn reject_unknown_fields() {
        let toml = r#"
name = "test"
description = "Test"
active_version = "v1"
unknown_field = "bad"

[[versions]]
version = "v1"
system_prompt = "Hello"
"#;
        let result = toml::from_str::<AgentRole>(toml);
        assert!(result.is_err());
    }
}
