//! Role registry: loads, stores, and resolves agent roles.
//!
//! The [`RoleRegistry`] is the central lookup for agent roles. It loads
//! TOML role definitions from a directory, validates them, and resolves
//! a role name into a [`ResolvedRole`] ready for the executor.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::error::AgentConfigError;
use crate::types::{AgentRole, ResolvedRole};

/// Central registry for agent role definitions.
///
/// Loads roles from TOML files and provides lookup by name. Each role
/// is validated on load (active version must exist, no duplicate versions).
pub struct RoleRegistry {
    roles: HashMap<String, AgentRole>,
}

impl RoleRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            roles: HashMap::new(),
        }
    }

    /// Registers a role. Returns an error if validation fails.
    ///
    /// If a role with the same name already exists, it is replaced.
    pub fn register(&mut self, role: AgentRole) -> Result<(), AgentConfigError> {
        validate_role(&role)?;
        self.roles.insert(role.name.clone(), role);
        Ok(())
    }

    /// Registers a role, returning an error if the name already exists.
    pub fn register_unique(&mut self, role: AgentRole) -> Result<(), AgentConfigError> {
        validate_role(&role)?;
        if self.roles.contains_key(&role.name) {
            return Err(AgentConfigError::DuplicateRole(role.name));
        }
        self.roles.insert(role.name.clone(), role);
        Ok(())
    }

    /// Looks up a role by name.
    pub fn get(&self, name: &str) -> Option<&AgentRole> {
        self.roles.get(name)
    }

    /// Resolves a role by name, returning the active prompt and configuration.
    ///
    /// This is the primary method used by the executor to get everything
    /// needed to run an agent with a specific role.
    pub fn resolve(&self, name: &str) -> Result<ResolvedRole, AgentConfigError> {
        let role = self
            .roles
            .get(name)
            .ok_or_else(|| AgentConfigError::RoleNotFound(name.to_string()))?;

        resolve_role(role)
    }

    /// Resolves a role with a specific version override.
    pub fn resolve_version(
        &self,
        name: &str,
        version: &str,
    ) -> Result<ResolvedRole, AgentConfigError> {
        let role = self
            .roles
            .get(name)
            .ok_or_else(|| AgentConfigError::RoleNotFound(name.to_string()))?;

        let prompt_version = role
            .versions
            .iter()
            .find(|v| v.version == version)
            .ok_or_else(|| AgentConfigError::VersionNotFound {
                role: name.to_string(),
                version: version.to_string(),
            })?;

        Ok(ResolvedRole {
            name: role.name.clone(),
            system_prompt: prompt_version.system_prompt.clone(),
            version: prompt_version.version.clone(),
            preferred_provider: role.preferred_provider.clone(),
            required_capabilities: role.required_capabilities.clone(),
            context_patterns: role.context_patterns.clone(),
            min_trust_level: role.min_trust_level.clone(),
            requires_private_data: role.requires_private_data,
            max_iterations: role.max_iterations,
        })
    }

    /// Returns the names of all registered roles.
    pub fn names(&self) -> Vec<&str> {
        self.roles.keys().map(|s| s.as_str()).collect()
    }

    /// Returns the number of registered roles.
    pub fn len(&self) -> usize {
        self.roles.len()
    }

    /// Returns true if no roles are registered.
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty()
    }

    /// Loads all `.toml` files from a directory as role definitions.
    ///
    /// Files are sorted alphabetically for deterministic load order.
    /// Returns the number of roles loaded.
    pub fn load_from_dir(&mut self, dir: &Path) -> Result<usize, AgentConfigError> {
        if !dir.is_dir() {
            return Err(AgentConfigError::IoError(format!(
                "not a directory: {}",
                dir.display()
            )));
        }

        let mut entries: Vec<_> = fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "toml")
                    .unwrap_or(false)
            })
            .collect();

        entries.sort_by_key(|e| e.file_name());

        let mut count = 0;
        for entry in entries {
            let content = fs::read_to_string(entry.path()).map_err(|e| {
                AgentConfigError::IoError(format!("{}: {e}", entry.path().display()))
            })?;
            let role: AgentRole = toml::from_str(&content).map_err(|e| {
                AgentConfigError::ParseError(format!("{}: {e}", entry.path().display()))
            })?;
            self.register(role)?;
            count += 1;
        }

        Ok(count)
    }
}

impl Default for RoleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Validates that a role definition is internally consistent.
fn validate_role(role: &AgentRole) -> Result<(), AgentConfigError> {
    if role.name.trim().is_empty() {
        return Err(AgentConfigError::ValidationError(
            "role name cannot be empty".to_string(),
        ));
    }

    if role.versions.is_empty() {
        return Err(AgentConfigError::NoVersions(role.name.clone()));
    }

    // Check for duplicate version tags.
    let mut seen = std::collections::HashSet::new();
    for v in &role.versions {
        if !seen.insert(&v.version) {
            return Err(AgentConfigError::DuplicateVersion {
                role: role.name.clone(),
                version: v.version.clone(),
            });
        }
    }

    // Active version must exist.
    if !role
        .versions
        .iter()
        .any(|v| v.version == role.active_version)
    {
        return Err(AgentConfigError::VersionNotFound {
            role: role.name.clone(),
            version: role.active_version.clone(),
        });
    }

    // System prompts must not be empty.
    for v in &role.versions {
        if v.system_prompt.trim().is_empty() {
            return Err(AgentConfigError::ValidationError(format!(
                "empty system prompt in role '{}' version '{}'",
                role.name, v.version
            )));
        }
    }

    Ok(())
}

/// Resolves the active prompt version from a role definition.
fn resolve_role(role: &AgentRole) -> Result<ResolvedRole, AgentConfigError> {
    let prompt_version = role
        .versions
        .iter()
        .find(|v| v.version == role.active_version)
        .ok_or_else(|| AgentConfigError::VersionNotFound {
            role: role.name.clone(),
            version: role.active_version.clone(),
        })?;

    Ok(ResolvedRole {
        name: role.name.clone(),
        system_prompt: prompt_version.system_prompt.clone(),
        version: prompt_version.version.clone(),
        preferred_provider: role.preferred_provider.clone(),
        required_capabilities: role.required_capabilities.clone(),
        context_patterns: role.context_patterns.clone(),
        min_trust_level: role.min_trust_level.clone(),
        requires_private_data: role.requires_private_data,
        max_iterations: role.max_iterations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PromptVersion;

    fn make_role(name: &str, active: &str, versions: Vec<(&str, &str)>) -> AgentRole {
        AgentRole {
            name: name.to_string(),
            description: format!("{name} role"),
            active_version: active.to_string(),
            preferred_provider: None,
            required_capabilities: vec![],
            context_patterns: vec![],
            min_trust_level: None,
            requires_private_data: false,
            max_iterations: None,
            versions: versions
                .into_iter()
                .map(|(v, p)| PromptVersion {
                    version: v.to_string(),
                    system_prompt: p.to_string(),
                    changelog: None,
                })
                .collect(),
        }
    }

    #[test]
    fn register_and_resolve() {
        let mut reg = RoleRegistry::new();
        reg.register(make_role(
            "researcher",
            "v1",
            vec![("v1", "You are a researcher.")],
        ))
        .unwrap();

        let resolved = reg.resolve("researcher").unwrap();
        assert_eq!(resolved.name, "researcher");
        assert_eq!(resolved.system_prompt, "You are a researcher.");
        assert_eq!(resolved.version, "v1");
    }

    #[test]
    fn resolve_nonexistent_role() {
        let reg = RoleRegistry::new();
        let err = reg.resolve("ghost").unwrap_err();
        assert!(matches!(err, AgentConfigError::RoleNotFound(_)));
    }

    #[test]
    fn resolve_specific_version() {
        let mut reg = RoleRegistry::new();
        reg.register(make_role(
            "coder",
            "v1",
            vec![
                ("v1", "You are a coder."),
                ("v2", "You are an expert coder. Write clean code."),
            ],
        ))
        .unwrap();

        let v1 = reg.resolve_version("coder", "v1").unwrap();
        assert_eq!(v1.system_prompt, "You are a coder.");

        let v2 = reg.resolve_version("coder", "v2").unwrap();
        assert_eq!(
            v2.system_prompt,
            "You are an expert coder. Write clean code."
        );
    }

    #[test]
    fn resolve_nonexistent_version() {
        let mut reg = RoleRegistry::new();
        reg.register(make_role("test", "v1", vec![("v1", "Hello")]))
            .unwrap();

        let err = reg.resolve_version("test", "v99").unwrap_err();
        assert!(matches!(err, AgentConfigError::VersionNotFound { .. }));
    }

    #[test]
    fn register_replaces_existing() {
        let mut reg = RoleRegistry::new();
        reg.register(make_role("a", "v1", vec![("v1", "First")]))
            .unwrap();
        reg.register(make_role("a", "v1", vec![("v1", "Second")]))
            .unwrap();

        assert_eq!(reg.len(), 1);
        let resolved = reg.resolve("a").unwrap();
        assert_eq!(resolved.system_prompt, "Second");
    }

    #[test]
    fn register_unique_rejects_duplicate() {
        let mut reg = RoleRegistry::new();
        reg.register_unique(make_role("a", "v1", vec![("v1", "First")]))
            .unwrap();

        let err = reg
            .register_unique(make_role("a", "v1", vec![("v1", "Second")]))
            .unwrap_err();
        assert!(matches!(err, AgentConfigError::DuplicateRole(_)));
    }

    #[test]
    fn validation_rejects_empty_name() {
        let mut reg = RoleRegistry::new();
        let err = reg
            .register(make_role("", "v1", vec![("v1", "Hello")]))
            .unwrap_err();
        assert!(matches!(err, AgentConfigError::ValidationError(_)));
    }

    #[test]
    fn validation_rejects_no_versions() {
        let mut reg = RoleRegistry::new();
        let err = reg.register(make_role("test", "v1", vec![])).unwrap_err();
        assert!(matches!(err, AgentConfigError::NoVersions(_)));
    }

    #[test]
    fn validation_rejects_missing_active_version() {
        let mut reg = RoleRegistry::new();
        let err = reg
            .register(make_role("test", "v99", vec![("v1", "Hello")]))
            .unwrap_err();
        assert!(matches!(err, AgentConfigError::VersionNotFound { .. }));
    }

    #[test]
    fn validation_rejects_duplicate_versions() {
        let mut reg = RoleRegistry::new();
        let err = reg
            .register(make_role(
                "test",
                "v1",
                vec![("v1", "First"), ("v1", "Duplicate")],
            ))
            .unwrap_err();
        assert!(matches!(err, AgentConfigError::DuplicateVersion { .. }));
    }

    #[test]
    fn validation_rejects_empty_prompt() {
        let mut reg = RoleRegistry::new();
        let err = reg
            .register(make_role("test", "v1", vec![("v1", "   ")]))
            .unwrap_err();
        assert!(matches!(err, AgentConfigError::ValidationError(_)));
    }

    #[test]
    fn names_and_len() {
        let mut reg = RoleRegistry::new();
        assert!(reg.is_empty());

        reg.register(make_role("a", "v1", vec![("v1", "Hello")]))
            .unwrap();
        reg.register(make_role("b", "v1", vec![("v1", "World")]))
            .unwrap();

        assert_eq!(reg.len(), 2);
        assert!(!reg.is_empty());
        let mut names = reg.names();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn get_returns_full_role() {
        let mut reg = RoleRegistry::new();
        reg.register(make_role(
            "coder",
            "v2",
            vec![("v1", "First"), ("v2", "Second")],
        ))
        .unwrap();

        let role = reg.get("coder").unwrap();
        assert_eq!(role.versions.len(), 2);
        assert_eq!(role.active_version, "v2");
    }

    #[test]
    fn load_from_dir_reads_toml_files() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // Write two role files.
        std::fs::write(
            dir.path().join("researcher.toml"),
            r#"
name = "researcher"
description = "Research agent"
active_version = "v1"

[[versions]]
version = "v1"
system_prompt = "You are a researcher."
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("coder.toml"),
            r#"
name = "coder"
description = "Coding agent"
active_version = "v1"
required_capabilities = ["archive.read", "archive.write"]

[[versions]]
version = "v1"
system_prompt = "You are a coder. Write clean, tested code."
"#,
        )
        .unwrap();

        // Write a non-TOML file (should be ignored).
        std::fs::write(dir.path().join("README.md"), "# Ignored").unwrap();

        let mut reg = RoleRegistry::new();
        let count = reg.load_from_dir(dir.path()).unwrap();

        assert_eq!(count, 2);
        assert_eq!(reg.len(), 2);
        assert!(reg.get("researcher").is_some());
        assert!(reg.get("coder").is_some());
        assert_eq!(
            reg.get("coder").unwrap().required_capabilities,
            vec!["archive.read", "archive.write"]
        );
    }

    #[test]
    fn load_from_dir_rejects_nonexistent_dir() {
        let mut reg = RoleRegistry::new();
        let err = reg
            .load_from_dir(Path::new("/nonexistent/path"))
            .unwrap_err();
        assert!(matches!(err, AgentConfigError::IoError(_)));
    }

    #[test]
    fn load_from_dir_rejects_invalid_toml() {
        let dir = tempfile::tempdir().expect("create temp dir");

        std::fs::write(dir.path().join("bad.toml"), "this is not valid toml = [[[").unwrap();

        let mut reg = RoleRegistry::new();
        let err = reg.load_from_dir(dir.path()).unwrap_err();
        assert!(matches!(err, AgentConfigError::ParseError(_)));
    }

    #[test]
    fn resolve_with_all_fields() {
        let role = AgentRole {
            name: "full".to_string(),
            description: "Full role".to_string(),
            active_version: "v1".to_string(),
            preferred_provider: Some("anthropic".to_string()),
            required_capabilities: vec!["archive.read".to_string(), "credential.read".to_string()],
            context_patterns: vec!["security/*".to_string()],
            min_trust_level: Some("CredentialAccess".to_string()),
            requires_private_data: true,
            max_iterations: Some(20),
            versions: vec![PromptVersion {
                version: "v1".to_string(),
                system_prompt: "You are a security analyst.".to_string(),
                changelog: Some("Initial".to_string()),
            }],
        };

        let mut reg = RoleRegistry::new();
        reg.register(role).unwrap();

        let resolved = reg.resolve("full").unwrap();
        assert_eq!(resolved.name, "full");
        assert_eq!(resolved.preferred_provider.as_deref(), Some("anthropic"));
        assert_eq!(resolved.required_capabilities.len(), 2);
        assert_eq!(resolved.context_patterns, vec!["security/*"]);
        assert_eq!(
            resolved.min_trust_level.as_deref(),
            Some("CredentialAccess")
        );
        assert!(resolved.requires_private_data);
        assert_eq!(resolved.max_iterations, Some(20));
    }

    #[test]
    fn default_creates_empty_registry() {
        let reg = RoleRegistry::default();
        assert!(reg.is_empty());
    }
}
