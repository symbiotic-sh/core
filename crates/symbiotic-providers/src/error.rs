//! Error types for provider operations.
//!
//! [`ProviderError`] covers all failure modes that can occur when interacting
//! with AI providers: network issues, auth failures, sensitivity violations,
//! budget enforcement, and agent task lifecycle errors.

use thiserror::Error;

use crate::{ProviderCapability, ProviderClass};

/// Error type for provider operations.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The provider is not reachable or not configured.
    #[error("provider unavailable: {0}")]
    Unavailable(String),

    /// The request was sent but the provider returned a failure.
    #[error("request failed: {0}")]
    RequestFailed(String),

    /// The provider returned data that could not be parsed.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Content sensitivity level is too high for the provider class.
    #[error(
        "sensitivity violation: {sensitivity:?} content cannot use {provider_class:?} provider"
    )]
    SensitivityViolation {
        /// The sensitivity level of the content.
        sensitivity: symbiotic_core::Sensitivity,
        /// The class of provider that was attempted.
        provider_class: ProviderClass,
    },

    /// The provider does not support the requested capability.
    #[error("capability not supported: {0:?}")]
    UnsupportedCapability(ProviderCapability),

    /// The usage budget has been exceeded.
    #[error("budget exceeded: {0}")]
    BudgetExceeded(String),

    /// The provider is rate-limiting requests.
    #[error("rate limited: retry after {retry_after_ms}ms")]
    RateLimited {
        /// How long to wait before retrying, in milliseconds.
        retry_after_ms: u64,
    },

    /// Authentication with the provider failed.
    #[error("auth failed: {0}")]
    AuthFailed(String),

    /// Provider configuration is invalid or missing.
    #[error("config error: {0}")]
    ConfigError(String),

    /// An agent task was cancelled before completion.
    #[error("agent task cancelled: {0}")]
    TaskCancelled(String),

    /// An agent task exceeded its timeout.
    #[error("agent task timed out after {timeout_seconds}s")]
    TaskTimeout {
        /// The timeout that was exceeded, in seconds.
        timeout_seconds: u64,
    },
}
