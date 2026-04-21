//! Core types for the Content Firewall.

pub mod scan_context;
pub mod source;
pub mod trust;
pub mod verdict;

pub use scan_context::{CallSite, ConsumingAgentScope, ScanContext};
pub use source::{CaptureCompleteness, ContentSource, SourceReceiptRef};
pub use trust::TrustLevel;
pub use verdict::{FindingKind, FirewallVerdict, QuarantineClass, Stage, StageFinding, Verdict};
