//! Prompt version management: create, activate, rollback, and diff.
//!
//! Operates on [`AgentRole`] structs in-place and on role files on disk.
//! The registry is the source of truth at runtime; disk persistence is
//! handled by the [`save_role`] function.

use std::fs;
use std::path::Path;

use crate::error::AgentConfigError;
use crate::types::{AgentRole, PromptVersion};

/// Adds a new prompt version to a role.
///
/// Returns an error if the version tag already exists.
pub fn add_version(
    role: &mut AgentRole,
    version: String,
    system_prompt: String,
    changelog: Option<String>,
) -> Result<(), AgentConfigError> {
    if version.trim().is_empty() {
        return Err(AgentConfigError::ValidationError(
            "version tag cannot be empty".to_string(),
        ));
    }

    if system_prompt.trim().is_empty() {
        return Err(AgentConfigError::ValidationError(
            "system prompt cannot be empty".to_string(),
        ));
    }

    if role.versions.iter().any(|v| v.version == version) {
        return Err(AgentConfigError::DuplicateVersion {
            role: role.name.clone(),
            version,
        });
    }

    role.versions.push(PromptVersion {
        version,
        system_prompt,
        changelog,
    });

    Ok(())
}

/// Activates a specific version for a role.
///
/// Returns an error if the version doesn't exist.
pub fn activate_version(role: &mut AgentRole, version: &str) -> Result<(), AgentConfigError> {
    if !role.versions.iter().any(|v| v.version == version) {
        return Err(AgentConfigError::VersionNotFound {
            role: role.name.clone(),
            version: version.to_string(),
        });
    }

    role.active_version = version.to_string();
    Ok(())
}

/// Rolls back to the previous version (the one before the currently active version).
///
/// If there's only one version, returns an error.
pub fn rollback(role: &mut AgentRole) -> Result<String, AgentConfigError> {
    if role.versions.len() < 2 {
        return Err(AgentConfigError::ValidationError(format!(
            "cannot rollback role '{}': only {} version(s) exist",
            role.name,
            role.versions.len()
        )));
    }

    let current_idx = role
        .versions
        .iter()
        .position(|v| v.version == role.active_version);

    let new_idx = match current_idx {
        Some(0) => role.versions.len() - 1, // wrap to last
        Some(idx) => idx - 1,
        None => role.versions.len() - 1, // fallback to last
    };

    let new_version = role.versions[new_idx].version.clone();
    role.active_version = new_version.clone();
    Ok(new_version)
}

/// Returns the version history of a role as a list of (version, changelog) pairs.
pub fn version_history(role: &AgentRole) -> Vec<(&str, Option<&str>, bool)> {
    role.versions
        .iter()
        .map(|v| {
            (
                v.version.as_str(),
                v.changelog.as_deref(),
                v.version == role.active_version,
            )
        })
        .collect()
}

/// Saves a role definition to a TOML file.
///
/// The file is written to `dir/{role.name}.toml`.
pub fn save_role(role: &AgentRole, dir: &Path) -> Result<(), AgentConfigError> {
    if !dir.is_dir() {
        return Err(AgentConfigError::IoError(format!(
            "not a directory: {}",
            dir.display()
        )));
    }

    let content =
        toml::to_string_pretty(role).map_err(|e| AgentConfigError::ParseError(e.to_string()))?;

    let file_path = dir.join(format!("{}.toml", role.name));
    fs::write(&file_path, content)
        .map_err(|e| AgentConfigError::IoError(format!("{}: {e}", file_path.display())))?;

    Ok(())
}

/// Loads a single role from a TOML file.
pub fn load_role(path: &Path) -> Result<AgentRole, AgentConfigError> {
    let content = fs::read_to_string(path)?;
    let role: AgentRole = toml::from_str(&content)?;
    Ok(role)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentRole;

    fn make_role(name: &str, versions: Vec<(&str, &str)>) -> AgentRole {
        let active = versions[0].0.to_string();
        AgentRole {
            name: name.to_string(),
            description: format!("{name} role"),
            active_version: active,
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
                    changelog: Some(format!("{v} changelog")),
                })
                .collect(),
        }
    }

    #[test]
    fn add_version_succeeds() {
        let mut role = make_role("test", vec![("v1", "Hello")]);
        add_version(
            &mut role,
            "v2".to_string(),
            "Updated prompt".to_string(),
            Some("Added improvements".to_string()),
        )
        .unwrap();

        assert_eq!(role.versions.len(), 2);
        assert_eq!(role.versions[1].version, "v2");
        assert_eq!(role.versions[1].system_prompt, "Updated prompt");
    }

    #[test]
    fn add_version_rejects_duplicate() {
        let mut role = make_role("test", vec![("v1", "Hello")]);
        let err =
            add_version(&mut role, "v1".to_string(), "Duplicate".to_string(), None).unwrap_err();
        assert!(matches!(err, AgentConfigError::DuplicateVersion { .. }));
    }

    #[test]
    fn add_version_rejects_empty_tag() {
        let mut role = make_role("test", vec![("v1", "Hello")]);
        let err = add_version(&mut role, "".to_string(), "Prompt".to_string(), None).unwrap_err();
        assert!(matches!(err, AgentConfigError::ValidationError(_)));
    }

    #[test]
    fn add_version_rejects_empty_prompt() {
        let mut role = make_role("test", vec![("v1", "Hello")]);
        let err = add_version(&mut role, "v2".to_string(), "   ".to_string(), None).unwrap_err();
        assert!(matches!(err, AgentConfigError::ValidationError(_)));
    }

    #[test]
    fn activate_version_succeeds() {
        let mut role = make_role("test", vec![("v1", "First"), ("v2", "Second")]);
        assert_eq!(role.active_version, "v1");

        activate_version(&mut role, "v2").unwrap();
        assert_eq!(role.active_version, "v2");
    }

    #[test]
    fn activate_nonexistent_version() {
        let mut role = make_role("test", vec![("v1", "Hello")]);
        let err = activate_version(&mut role, "v99").unwrap_err();
        assert!(matches!(err, AgentConfigError::VersionNotFound { .. }));
    }

    #[test]
    fn rollback_to_previous() {
        let mut role = make_role("test", vec![("v1", "First"), ("v2", "Second")]);
        activate_version(&mut role, "v2").unwrap();
        assert_eq!(role.active_version, "v2");

        let rolled = rollback(&mut role).unwrap();
        assert_eq!(rolled, "v1");
        assert_eq!(role.active_version, "v1");
    }

    #[test]
    fn rollback_wraps_from_first_to_last() {
        let mut role = make_role(
            "test",
            vec![("v1", "First"), ("v2", "Second"), ("v3", "Third")],
        );
        assert_eq!(role.active_version, "v1");

        let rolled = rollback(&mut role).unwrap();
        assert_eq!(rolled, "v3");
    }

    #[test]
    fn rollback_single_version_fails() {
        let mut role = make_role("test", vec![("v1", "Only")]);
        let err = rollback(&mut role).unwrap_err();
        assert!(matches!(err, AgentConfigError::ValidationError(_)));
    }

    #[test]
    fn version_history_lists_all() {
        let role = make_role("test", vec![("v1", "First"), ("v2", "Second")]);
        let history = version_history(&role);

        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, "v1");
        assert!(history[0].2); // active
        assert_eq!(history[1].0, "v2");
        assert!(!history[1].2); // not active
    }

    #[test]
    fn save_and_load_role() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let role = make_role("researcher", vec![("v1", "You are a researcher.")]);

        save_role(&role, dir.path()).unwrap();

        let loaded = load_role(&dir.path().join("researcher.toml")).unwrap();
        assert_eq!(loaded.name, "researcher");
        assert_eq!(loaded.versions.len(), 1);
        assert_eq!(loaded.versions[0].system_prompt, "You are a researcher.");
    }

    #[test]
    fn save_role_rejects_nonexistent_dir() {
        let role = make_role("test", vec![("v1", "Hello")]);
        let err = save_role(&role, Path::new("/nonexistent/dir")).unwrap_err();
        assert!(matches!(err, AgentConfigError::IoError(_)));
    }

    #[test]
    fn add_then_activate_then_rollback() {
        let mut role = make_role("test", vec![("v1", "Original")]);

        add_version(
            &mut role,
            "v2".to_string(),
            "Updated".to_string(),
            Some("Better instructions".to_string()),
        )
        .unwrap();
        activate_version(&mut role, "v2").unwrap();
        assert_eq!(role.active_version, "v2");

        let rolled = rollback(&mut role).unwrap();
        assert_eq!(rolled, "v1");
        assert_eq!(role.active_version, "v1");
    }

    #[test]
    fn save_preserves_all_fields() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let role = AgentRole {
            name: "full".to_string(),
            description: "Full test".to_string(),
            active_version: "v1".to_string(),
            preferred_provider: Some("anthropic".to_string()),
            required_capabilities: vec!["archive.read".to_string()],
            context_patterns: vec!["docs/*".to_string()],
            min_trust_level: Some("ArchiveWrite".to_string()),
            requires_private_data: true,
            max_iterations: Some(15),
            versions: vec![
                PromptVersion {
                    version: "v1".to_string(),
                    system_prompt: "You are a security analyst.".to_string(),
                    changelog: Some("Initial version".to_string()),
                },
                PromptVersion {
                    version: "v2".to_string(),
                    system_prompt: "You are an expert security analyst.".to_string(),
                    changelog: Some("Added expertise framing".to_string()),
                },
            ],
        };

        save_role(&role, dir.path()).unwrap();
        let loaded = load_role(&dir.path().join("full.toml")).unwrap();

        assert_eq!(loaded.name, "full");
        assert_eq!(loaded.preferred_provider.as_deref(), Some("anthropic"));
        assert_eq!(loaded.required_capabilities, vec!["archive.read"]);
        assert_eq!(loaded.context_patterns, vec!["docs/*"]);
        assert_eq!(loaded.min_trust_level.as_deref(), Some("ArchiveWrite"));
        assert!(loaded.requires_private_data);
        assert_eq!(loaded.max_iterations, Some(15));
        assert_eq!(loaded.versions.len(), 2);
    }
}
