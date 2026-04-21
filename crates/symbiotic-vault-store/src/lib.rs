//! # symbiotic-vault-store
//!
//! Age-encrypted blob store for Tier 3 (vault-grade) sensitive data.
//!
//! This crate provides a simple encrypted blob store using the
//! [age](https://age-encryption.org/) encryption format. It is designed
//! for storing medical records, financial data, legal documents, and
//! other vault-grade sensitive content.
//!
//! ## Design principles
//!
//! - **Decrypted content never touches disk** -- plaintext is only held in memory.
//! - **Metadata is unencrypted** -- allows indexing/searching without decryption.
//! - **Atomic writes** -- index updates use write-to-temp-then-rename.
//! - **Key rotation** -- re-encrypt all blobs with a new key in one call.
//!
//! ## Storage layout
//!
//! ```text
//! {root}/
//! +-- index.json          # Unencrypted metadata index
//! +-- {id}.age            # age-encrypted content files
//! +-- ...
//! ```

mod error;
pub mod escrow;
mod store;
mod types;

pub use error::VaultStoreError;
pub use store::BlobStore;
pub use types::{BlobCategory, BlobMetadata, EncryptedBlob};

/// Re-export age key types for callers that need to generate or manage keys.
pub mod keys {
    /// Re-export `ExposeSecret` so callers can serialise an `Identity` to a
    /// string (for writing key files) without depending on `secrecy` directly.
    pub use age::secrecy::ExposeSecret;
    pub use age::x25519::{Identity, Recipient};
}
