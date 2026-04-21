//! Stage orchestration.
//!
//! Each stage implements [`ScanStage`], returning a [`StageOutcome`]. The
//! individual stages (`stage_a`, `stage_b`, `stage_c`) know how to assemble
//! their own verdict shapes; this module wires them together via small
//! composition helpers — [`run_stages_a_b`] for the deterministic ingest
//! pair (§03), and [`run_stages_a_b_c`] for the conditional LLM-lite
//! extension (§04).
//!
//! Stages D (capability-boundary) and E (annotation injection) run at
//! **context-assembly time** rather than ingest. Their orchestration
//! lives in [`crate::context_assembly`] (which provides the per-entry
//! cache + the `apply_context_stages` entry point); the stage primitives
//! themselves are exposed here so callers that want to run them
//! piecemeal (tests, the future Replay flagger) can do so.

pub mod stage_a;
pub mod stage_b;
pub mod stage_c;
pub mod stage_c_cache;
pub mod stage_c_prompt;
pub mod stage_d;
pub mod stage_e;

pub use stage_a::{run as run_stage_a, StageAConfig, StageAOutcome};
pub use stage_b::{run as run_stage_b, StageBConfig, StageBOutcome};
pub use stage_c::{
    run as run_stage_c, should_run as should_run_stage_c, StageCConfig, StageCOutcome,
    LOW_CONFIDENCE_FLAG_DOWNGRADE_MAX,
};
pub use stage_c_cache::{StageCCache, StageCCacheKey};
pub use stage_c_prompt::{
    parse_response as parse_stage_c_response, render_prompt as render_stage_c_prompt,
    ParsedResponse as StageCParsedResponse, StageCLabel,
};
pub use stage_d::{run as run_stage_d, StageDOutcome};
pub use stage_e::{wrap as run_stage_e, StageEConfig};

use crate::llm_gateway::LlmGateway;
use crate::types::{FirewallVerdict, ScanContext, TrustLevel, Verdict};
use crate::version::SECURITY_VERSION;
use time::OffsetDateTime;

/// Outcome of a single stage: either a terminal verdict (caller should stop
/// and return it), or a "continue" signal carrying a possibly-modified
/// payload + findings accumulated so far.
#[derive(Debug, Clone)]
pub enum StageOutcome {
    /// Stage produced a terminal verdict — e.g. Stage A quarantine, or a
    /// final pass verdict when no further stages apply.
    Terminal(FirewallVerdict),
    /// Stage cleared, may have rewritten the payload; pass the new payload
    /// + accumulated findings to the next stage.
    Continue {
        payload: String,
        verdict: FirewallVerdict,
    },
}

/// Trait implemented by each firewall scan stage.
pub trait ScanStage {
    type Config;
    /// Run the stage. `ctx` is the scan context (source, consuming scope,
    /// call site); `payload` is the content being evaluated.
    fn run(&self, ctx: &ScanContext, payload: &str, config: &Self::Config) -> StageOutcome;
}

/// Convenience: run Stage A then Stage B in sequence.
///
/// - If Stage A terminates (quarantine), returns that verdict.
/// - Otherwise, Stage B is applied to the cleaned payload; findings from
///   both stages accumulate on the returned verdict.
pub fn run_stages_a_b(
    ctx: &ScanContext,
    payload: &str,
    cfg_a: &StageAConfig,
    cfg_b: &StageBConfig,
) -> FirewallVerdict {
    match stage_a::run(ctx, payload, cfg_a) {
        StageAOutcome::Quarantined(v) => v,
        StageAOutcome::Passed {
            cleaned,
            findings: stage_a_findings,
        } => {
            let b_outcome = stage_b::run(ctx, &cleaned, cfg_b);
            let mut annotations = stage_a_findings;
            annotations.extend(b_outcome.findings);
            FirewallVerdict {
                verdict: b_outcome.verdict,
                verdict_version: SECURITY_VERSION.to_string(),
                scan_timestamp: OffsetDateTime::now_utc(),
                quarantine_class: b_outcome.quarantine_class,
                source_receipt_id: None,
                annotations,
            }
        }
    }
}

/// Convenience: run Stage A → Stage B → (conditional) Stage C in sequence.
///
/// Behaviour:
///
/// - If Stage A terminates (quarantine), that verdict is returned.
/// - Otherwise Stage B runs on the cleaned payload. If Stage B quarantines
///   outright, or if [`should_run_stage_c`] returns `false`, the combined
///   A+B verdict is returned and Stage C is not invoked.
/// - When Stage C is applicable, the provided `gateway` + `cache` are used
///   to evaluate the content; Stage C's composed verdict includes **Stage A
///   findings + Stage B findings + Stage C's annotation**.
///
/// Stage C's `suggested_trust` decision is surfaced on the returned
/// [`StagesAbcOutcome`] so callers can route the decrement to the trust
/// store without re-parsing the verdict annotations.
pub async fn run_stages_a_b_c(
    ctx: &ScanContext,
    payload: &str,
    trust: TrustLevel,
    cfg_a: &StageAConfig,
    cfg_b: &StageBConfig,
    cfg_c: &StageCConfig,
    gateway: &dyn LlmGateway,
    cache: &StageCCache,
) -> StagesAbcOutcome {
    // --- Stage A ---
    let (cleaned, stage_a_findings) = match stage_a::run(ctx, payload, cfg_a) {
        StageAOutcome::Quarantined(v) => {
            return StagesAbcOutcome {
                verdict: v,
                suggested_trust: None,
                ran_stage_c: false,
            };
        }
        StageAOutcome::Passed { cleaned, findings } => (cleaned, findings),
    };

    // --- Stage B ---
    let mut stage_b_outcome = stage_b::run(ctx, &cleaned, cfg_b);

    // If Stage C doesn't apply, compose a simple A+B verdict and return.
    if !should_run_stage_c(cfg_c, trust, &stage_b_outcome) {
        let mut annotations = stage_a_findings;
        annotations.extend(stage_b_outcome.findings);
        let verdict = FirewallVerdict {
            verdict: stage_b_outcome.verdict,
            verdict_version: SECURITY_VERSION.to_string(),
            scan_timestamp: OffsetDateTime::now_utc(),
            quarantine_class: stage_b_outcome.quarantine_class,
            source_receipt_id: None,
            annotations,
        };
        return StagesAbcOutcome {
            verdict,
            suggested_trust: None,
            ran_stage_c: false,
        };
    }

    // --- Stage C ---
    // Prepend Stage A's findings onto the Stage B outcome so Stage C's
    // composer picks them up (Stage C preserves whatever it receives on
    // `stage_b.findings`). This keeps the ingest-time finding list
    // complete: A + B + C annotations in source order.
    let mut findings_for_c = stage_a_findings;
    findings_for_c.append(&mut stage_b_outcome.findings);
    stage_b_outcome.findings = findings_for_c;

    let c_outcome =
        stage_c::run(ctx, &cleaned, trust, stage_b_outcome, cfg_c, gateway, cache).await;

    StagesAbcOutcome {
        verdict: c_outcome.verdict,
        suggested_trust: c_outcome.suggested_trust,
        ran_stage_c: true,
    }
}

/// Composed outcome of [`run_stages_a_b_c`].
///
/// The verdict is the final firewall verdict to persist; `suggested_trust`
/// is `Some(level)` only when Stage C returned `MALICIOUS`. Callers that
/// want Stage-C telemetry (did it run? cache hit vs. miss?) can inspect
/// `ran_stage_c`.
#[derive(Debug, Clone)]
pub struct StagesAbcOutcome {
    pub verdict: FirewallVerdict,
    pub suggested_trust: Option<TrustLevel>,
    pub ran_stage_c: bool,
}

/// Convenience constructor for a terminal "passed" verdict with no findings.
pub(crate) fn passed_verdict(scan_timestamp: OffsetDateTime) -> FirewallVerdict {
    FirewallVerdict {
        verdict: Verdict::Passed,
        verdict_version: SECURITY_VERSION.to_string(),
        scan_timestamp,
        quarantine_class: None,
        source_receipt_id: None,
        annotations: Vec::new(),
    }
}
