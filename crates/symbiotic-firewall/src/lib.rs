//! Content Firewall — ingest-time scan engine.
//!
//! This crate is the scan boundary between untrusted ingested content and
//! the trusted Archive.
//!
//! # Chunks landed
//!
//! - **T132 §02** — frozen wire types ([`types`], [`errors`], [`version`]).
//! - **T132 §03** — Stage A (structural sanitization) + Stage B
//!   (prompt-injection heuristics) + scan cache ([`stages`], [`sanitize`],
//!   [`heuristics`], [`cache`]).
//! - **T132 §04** — Stage C (LLM-lite semantic review) + dedicated Stage C
//!   cache + minimal [`LlmGateway`][llm_gateway::LlmGateway] trait surface
//!   so the firewall can be wired to T83's runtime without depending on
//!   it directly.
//! - **T132 §06** — Stage D (capability-boundary check) + Stage E
//!   (annotation injection) at context-assembly time, plus the
//!   [`context_assembly::apply_context_stages`] orchestrator + per-`(entry,
//!   scope)` Stage D cache. Recall Gateway is the primary caller.
//!
//! # Layout
//!
//! - [`types`] — verdict / trust / source / scan-context shapes.
//! - [`errors`] — unified [`FirewallError`][errors::FirewallError].
//! - [`version`] — [`SECURITY_VERSION`][version::SECURITY_VERSION] const +
//!   semver helpers used by the Replay job.
//! - [`sanitize`] — HTML / Markdown structural sanitization (Stage A).
//! - [`heuristics`] — prompt-injection pattern rules (Stage B).
//! - [`llm_gateway`] — trait the firewall consumes for Stage C calls.
//! - [`stages`] — stage orchestration + per-stage entry points.
//! - [`cache`] — LRU cache of scan verdicts keyed by `(source, hash, version)`.
//! - [`capability_scope`] — Stage D pattern matchers + scope comparison.
//! - [`context_assembly`] — Stage D + E orchestrator used by the Recall
//!   Gateway / direct Archive readers.

pub mod cache;
pub mod capability_scope;
pub mod context_assembly;
pub mod errors;
pub mod heuristics;
pub mod llm_gateway;
pub mod sanitize;
pub mod stages;
pub mod types;
pub mod version;

pub use cache::{CacheKey, ScanCache, DEFAULT_CACHE_CAPACITY};
pub use capability_scope::{
    is_within_scope, scan as scan_capability_patterns, ScopeHit, ScopeHitKind,
};
pub use context_assembly::{
    apply_context_stages, AnnotatedContent, EntryForAssembly, StageDCache, STAGE_D_CACHE_TTL,
};
pub use errors::{FirewallError, FirewallResult};
pub use heuristics::{HeuristicHit, HitSeverity};
pub use llm_gateway::{FastTierRequest, FastTierResponse, LlmGateway, LlmGatewayError};
pub use stages::{
    parse_stage_c_response, render_stage_c_prompt, run_stage_a, run_stage_b, run_stage_c,
    run_stage_d, run_stage_e, run_stages_a_b, run_stages_a_b_c, should_run_stage_c, ScanStage,
    StageAConfig, StageAOutcome, StageBConfig, StageBOutcome, StageCCache, StageCCacheKey,
    StageCConfig, StageCLabel, StageCOutcome, StageCParsedResponse, StageDOutcome, StageEConfig,
    StageOutcome, StagesAbcOutcome, LOW_CONFIDENCE_FLAG_DOWNGRADE_MAX,
};
pub use types::{
    CallSite, CaptureCompleteness, ConsumingAgentScope, ContentSource, FindingKind,
    FirewallVerdict, QuarantineClass, ScanContext, SourceReceiptRef, Stage, StageFinding,
    TrustLevel, Verdict,
};
pub use version::{needs_replay, SecurityVersion, SECURITY_VERSION};
