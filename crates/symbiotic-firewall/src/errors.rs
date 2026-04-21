//! Error types for `symbiotic-firewall`.
//!
//! This chunk introduces only type definitions; the scan engine (future
//! chunks) will produce these errors. Keeping them centralized here avoids
//! a churn commit later.

use thiserror::Error;

/// Errors surfaced by the firewall crate.
#[derive(Debug, Error)]
pub enum FirewallError {
    /// A scan-input invariant was violated (e.g. empty payload where one was
    /// required, malformed [`crate::ScanContext`]).
    #[error("invalid scan input: {0}")]
    InvalidInput(String),

    /// Stage A structural sanitization rejected the payload.
    #[error("stage A structural violation: {0}")]
    StructuralViolation(String),

    /// The firewall version embedded in a stored verdict is not parseable
    /// or is newer than the running firewall (Replay should never rewind).
    #[error("unsupported firewall version: {0}")]
    UnsupportedVersion(String),

    /// The source receipt store could not satisfy a Rebuild lookup.
    #[error("source receipt unavailable: {0}")]
    SourceReceiptUnavailable(String),

    /// Stage D rejected an Archive entry at context-assembly time
    /// because the consuming agent's scope does not authorize a
    /// capability / credential / scope-elevation pattern observed in
    /// the entry's content. The caller should exclude the entry from
    /// the context pack, emit a `firewall.capability_smuggling.blocked`
    /// alert, and continue assembling the rest of the pack.
    #[error(
        "capability smuggling: entry {entry_id} blocked from agent {agent_id} context: {detail}"
    )]
    CapabilitySmuggling {
        entry_id: String,
        agent_id: String,
        detail: String,
    },

    /// Wrapped serialization error (verdict, context, etc.).
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Wrapped IO error (receipt store reads, quarantine log writes).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias for `Result<T, FirewallError>`.
pub type FirewallResult<T> = Result<T, FirewallError>;
