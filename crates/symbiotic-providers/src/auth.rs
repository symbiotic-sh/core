//! Provider authentication and credential resolution.
//!
//! Credentials are resolved at runtime via the [`CredentialResolver`] trait,
//! which allows pluggable backends (environment variables, vault, etc.).
//! The [`ProviderAuth`] type wraps different authentication mechanisms
//! while redacting secrets from debug output.

use std::fmt;

/// Authentication credential for a provider.
///
/// The `Debug` implementation redacts secret values to prevent
/// accidental exposure in logs or error messages.
#[derive(Clone)]
pub enum ProviderAuth {
    /// No authentication required (e.g. local Ollama).
    None,
    /// Bearer API key.
    ApiKey(String),
    /// OAuth2 access token.
    OAuthToken(String),
    /// Session-based token (e.g. cookie or refresh token).
    SessionToken(String),
}

impl fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "ProviderAuth::None"),
            Self::ApiKey(_) => write!(f, "ProviderAuth::ApiKey([REDACTED])"),
            Self::OAuthToken(_) => write!(f, "ProviderAuth::OAuthToken([REDACTED])"),
            Self::SessionToken(_) => write!(f, "ProviderAuth::SessionToken([REDACTED])"),
        }
    }
}

/// Resolves credentials for a named service.
///
/// Implementations look up API keys, tokens, or other auth material
/// from their backing store (environment, vault, config file, etc.).
pub trait CredentialResolver: Send + Sync {
    /// Resolve the credential for `service`, or `None` if not found.
    fn resolve(&self, service: &str) -> Option<ProviderAuth>;
}

/// Resolves credentials from environment variables.
///
/// For a service named `"openai"`, this looks up `OPENAI_API_KEY`.
/// Hyphens in service names are replaced with underscores and the
/// name is uppercased.
pub struct EnvVarResolver;

impl CredentialResolver for EnvVarResolver {
    fn resolve(&self, service: &str) -> Option<ProviderAuth> {
        let env_name = format!("{}_API_KEY", service.to_uppercase().replace('-', "_"));
        std::env::var(&env_name).ok().map(ProviderAuth::ApiKey)
    }
}
