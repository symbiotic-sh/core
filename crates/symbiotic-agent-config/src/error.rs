//! Error types for the agent configuration system.

use thiserror::Error;

/// Errors that can occur in the agent configuration system.
#[derive(Debug, Error)]
pub enum AgentConfigError {
    /// Role not found in registry.
    #[error("role not found: {0}")]
    RoleNotFound(String),

    /// Active version not found among role's versions.
    #[error("version '{version}' not found for role '{role}'")]
    VersionNotFound { role: String, version: String },

    /// No versions defined for a role.
    #[error("role '{0}' has no versions defined")]
    NoVersions(String),

    /// Duplicate role name during registration.
    #[error("duplicate role: {0}")]
    DuplicateRole(String),

    /// Duplicate version tag within a role.
    #[error("duplicate version '{version}' in role '{role}'")]
    DuplicateVersion { role: String, version: String },

    /// TOML parsing error.
    #[error("config parse error: {0}")]
    ParseError(String),

    /// File I/O error.
    #[error("file error: {0}")]
    IoError(String),

    /// Validation error.
    #[error("validation error: {0}")]
    ValidationError(String),
}

impl From<std::io::Error> for AgentConfigError {
    fn from(e: std::io::Error) -> Self {
        AgentConfigError::IoError(e.to_string())
    }
}

impl From<toml::de::Error> for AgentConfigError {
    fn from(e: toml::de::Error) -> Self {
        AgentConfigError::ParseError(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages() {
        let err = AgentConfigError::RoleNotFound("researcher".to_string());
        assert_eq!(err.to_string(), "role not found: researcher");

        let err = AgentConfigError::VersionNotFound {
            role: "coder".to_string(),
            version: "v99".to_string(),
        };
        assert!(err.to_string().contains("v99"));
        assert!(err.to_string().contains("coder"));

        let err = AgentConfigError::DuplicateRole("reviewer".to_string());
        assert!(err.to_string().contains("reviewer"));
    }

    #[test]
    fn io_error_converts() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let config_err: AgentConfigError = io_err.into();
        assert!(matches!(config_err, AgentConfigError::IoError(_)));
        assert!(config_err.to_string().contains("missing file"));
    }
}
