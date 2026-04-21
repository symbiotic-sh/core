//! Metrics layer for Symbiotic self-improvement.
//!
//! Captures system performance and outcomes so Symbiotic can propose
//! evidence-based improvements. Metrics inform self-improvement proposals
//! but do not trigger autonomous learning.

pub mod aggregator;
pub mod dashboard;
pub mod proposals;
pub mod store;
pub mod types;

pub use aggregator::Aggregator;
pub use dashboard::{format_proposals, format_summary};
pub use proposals::{ProposalConfig, ProposalEngine};
pub use store::MetricStore;
pub use types::*;

/// Errors from the metrics layer.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("not found: {0}")]
    NotFound(String),
}
