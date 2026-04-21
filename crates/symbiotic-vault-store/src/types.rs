//! Core types for the vault store.

use serde::{Deserialize, Serialize};

/// Category of an encrypted blob, used for filtering and routing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlobCategory {
    /// Medical records (bloodwork, prescriptions, diagnoses).
    Medical,
    /// Financial data (tax returns, bank statements, invoices).
    Financial,
    /// Legal documents (contracts, wills, NDAs).
    Legal,
    /// Credentials and secrets not handled by symbiotic-trust Vault.
    Credential,
    /// User-defined category.
    Custom(String),
}

impl std::fmt::Display for BlobCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Medical => write!(f, "medical"),
            Self::Financial => write!(f, "financial"),
            Self::Legal => write!(f, "legal"),
            Self::Credential => write!(f, "credential"),
            Self::Custom(s) => write!(f, "custom:{s}"),
        }
    }
}

/// Unencrypted metadata stored alongside an encrypted blob.
///
/// Allows indexing and searching without decryption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobMetadata {
    /// Human-readable title.
    pub title: String,
    /// Tags for search and categorization.
    pub tags: Vec<String>,
    /// Size of the original (unencrypted) content in bytes.
    pub size_bytes: u64,
    /// MIME type of the content (e.g., "application/pdf", "text/markdown").
    pub content_type: String,
}

/// An encrypted blob entry in the index.
///
/// The actual encrypted content is stored in a separate `.age` file on disk.
/// This struct holds only the metadata and identification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedBlob {
    /// Unique identifier for this blob.
    pub id: String,
    /// Sensitivity category.
    pub category: BlobCategory,
    /// Unix timestamp (seconds) when this blob was stored.
    pub created_at: u64,
    /// Unencrypted metadata.
    pub metadata: BlobMetadata,
}
