//! Error types for the vault store.

use thiserror::Error;

/// Errors that can occur during blob store operations.
#[derive(Debug, Error)]
pub enum VaultStoreError {
    /// I/O error reading or writing files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Encryption failed.
    #[error("encryption error: {0}")]
    Encrypt(String),

    /// Decryption failed (wrong key, corrupted file, etc.).
    #[error("decryption error: {0}")]
    Decrypt(String),

    /// The requested blob was not found in the index.
    #[error("blob not found: {0}")]
    NotFound(String),

    /// The blob file exists in the index but is missing on disk.
    #[error("blob file missing on disk: {0}")]
    FileMissing(String),

    /// No recipients provided for encryption.
    #[error("no recipients provided for encryption")]
    NoRecipients,
}
