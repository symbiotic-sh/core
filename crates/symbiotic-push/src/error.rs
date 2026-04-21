//! Error types for push notification delivery.

use thiserror::Error;

/// Errors that can occur during push notification operations.
#[derive(Debug, Error)]
pub enum PushError {
    /// The push token is invalid or malformed.
    #[error("invalid push token: {0}")]
    InvalidToken(String),

    /// The push token has expired and must be re-registered.
    #[error("push token has expired")]
    ExpiredToken,

    /// Failed to send the push notification to the provider.
    #[error("send failed: {0}")]
    SendFailed(String),

    /// The provider rate-limited the request.
    #[error("rate limited: retry after {retry_after_secs}s")]
    RateLimited {
        /// Seconds to wait before retrying.
        retry_after_secs: u64,
    },

    /// An error occurred in the token store.
    #[error("store error: {0}")]
    StoreError(String),

    /// The notification payload exceeds the provider's size limit.
    #[error("payload too large: {size} bytes exceeds {max} byte limit")]
    PayloadTooLarge {
        /// Actual payload size in bytes.
        size: usize,
        /// Maximum allowed size in bytes.
        max: usize,
    },

    /// The push provider is currently unavailable.
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
}

impl From<rusqlite::Error> for PushError {
    fn from(e: rusqlite::Error) -> Self {
        PushError::StoreError(e.to_string())
    }
}
