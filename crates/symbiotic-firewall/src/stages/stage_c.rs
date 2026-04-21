//! Stage C — LLM-lite semantic review (design §3.3).
//!
//! Stage C is the only LLM-backed scan stage. It is triggered in two cases:
//!
//! 1. Stage B returned [`Verdict::Flagged`] with confidence ≥ the
//!    configured [`StageCConfig::trigger_threshold`] (default 0.30).
//! 2. The content source is [`TrustLevel::VeryLow`]. Very-Low sources
//!    (web fetches, browser-automation HTML) always get Stage C regardless
//!    of Stage B's verdict, because deterministic heuristics cannot be
//!    trusted against the entire web.
//!
//! If Stage B already quarantined (confidence ≥ quarantine threshold),
//! Stage C is **not** run — the quarantine verdict stands.
//!
//! # Verdict mapping
//!
//! | Stage C label   | Combined verdict                                                                                 |
//! |-----------------|---------------------------------------------------------------------------------------------------|
//! | `SAFE`          | Preserve Stage B verdict, but downgrade a low-confidence `Flagged` to `Passed` (see mapping).     |
//! | `SUSPICIOUS`    | `Verdict::Quarantined` + `QuarantineClass::SecurityRisk`.                                         |
//! | `MALICIOUS`     | `Verdict::Quarantined` + `QuarantineClass::SecurityRisk` + highest-severity finding + trust drop. |
//!
//! # Failure modes
//!
//! - Gateway `RateLimited` / `Unavailable` → `SUSPICIOUS` fallback with a
//!   5 min cache TTL (design §7).
//! - Malformed model response → `SUSPICIOUS` (see
//!   [`stage_c_prompt::parse_response`]).

use time::OffsetDateTime;

use crate::llm_gateway::{FastTierRequest, LlmGateway, LlmGatewayError};
use crate::stages::stage_c_cache::{StageCCache, StageCCacheKey};
use crate::stages::stage_c_prompt::{
    parse_response, render_prompt, ParsedResponse, StageCLabel, MALFORMED_RATIONALE,
};
use crate::stages::StageBOutcome;
use crate::types::{
    FindingKind, FirewallVerdict, QuarantineClass, ScanContext, Stage, StageFinding, TrustLevel,
    Verdict,
};
use crate::version::SECURITY_VERSION;

/// Downgrade boundary for SAFE verdicts. If Stage B's confidence is below
/// this value and Stage C returns SAFE, the combined verdict is downgraded
/// to `Passed`. Above it, Stage B's `Flagged` verdict is preserved with the
/// new Stage C annotation attached.
pub const LOW_CONFIDENCE_FLAG_DOWNGRADE_MAX: f32 = 0.50;

/// Configuration for Stage C.
#[derive(Debug, Clone)]
pub struct StageCConfig {
    /// Stage B confidence at or above which Stage C is triggered (when the
    /// trust level doesn't already force it). Design default: 0.30.
    pub trigger_threshold: f32,
    /// Cap on Stage C response tokens sent to the LLM gateway.
    pub max_response_tokens: u32,
}

impl Default for StageCConfig {
    fn default() -> Self {
        Self {
            trigger_threshold: 0.30,
            max_response_tokens: 128,
        }
    }
}

/// Is Stage C applicable given Stage B's outcome + the source trust level?
///
/// Returns `true` when either:
///
/// - `TrustLevel::VeryLow` forces Stage C (always).
/// - Stage B is in the `Flagged` band with confidence ≥ `trigger_threshold`.
///
/// Returns `false` when:
///
/// - Stage B already quarantined (no need to rescue-scan).
/// - Stage B passed cleanly and trust level doesn't force it.
pub fn should_run(cfg: &StageCConfig, trust: TrustLevel, stage_b: &StageBOutcome) -> bool {
    if stage_b.verdict == Verdict::Quarantined {
        return false;
    }
    if trust.force_stage_c() {
        return true;
    }
    stage_b.verdict == Verdict::Flagged && stage_b.confidence >= cfg.trigger_threshold
}

/// Final outcome of a Stage C run, composed with the prior Stage B outcome.
#[derive(Debug, Clone)]
pub struct StageCOutcome {
    /// Fully composed verdict to return from the scan pipeline.
    pub verdict: FirewallVerdict,
    /// Suggested updated trust level for the source. `None` unless Stage C
    /// returned `MALICIOUS`; callers route this suggestion to the trust
    /// store (firewall does not persist trust itself).
    pub suggested_trust: Option<TrustLevel>,
    /// The label Stage C returned. Exposed for callers that want to feed
    /// this into T120 audit telemetry without re-parsing `verdict`.
    pub label: StageCLabel,
}

/// Async entry point for Stage C. Uses the provided gateway to evaluate the
/// content, caches positive / negative / failure results per design §6.5,
/// and composes a final [`FirewallVerdict`] with accumulated findings.
///
/// `trust` is accepted for API symmetry with [`should_run`] and for
/// future per-trust-level behaviour (e.g. stricter cache TTL on VeryLow)
/// but is currently unused inside `run` — the applicability decision
/// happens upstream.
pub async fn run(
    ctx: &ScanContext,
    cleaned_payload: &str,
    _trust: TrustLevel,
    stage_b: StageBOutcome,
    cfg: &StageCConfig,
    gateway: &dyn LlmGateway,
    cache: &StageCCache,
) -> StageCOutcome {
    let source_kind = ctx.source.kind.as_str();
    let cache_key = StageCCacheKey::for_payload(source_kind, cleaned_payload);

    // Cache hit short-circuits the LLM call but still flows through the
    // composition step so the combined verdict reflects Stage B's findings.
    if let Some(cached) = cache.get(&cache_key) {
        let span = tracing::debug_span!(
            "firewall.stage_c",
            source = source_kind,
            content_hash = %cache_key.content_hash,
            label = cached.label.as_str(),
            cache = true,
        );
        let _guard = span.enter();
        tracing::debug!(rationale = %cached.rationale, "stage c cache hit");
        return compose_outcome(cached, stage_b);
    }

    let prompt = render_prompt(cleaned_payload);
    let request = FastTierRequest {
        prompt,
        max_response_tokens: Some(cfg.max_response_tokens),
    };

    let span = tracing::info_span!(
        "firewall.stage_c",
        source = source_kind,
        content_hash = %cache_key.content_hash,
        cache = false,
    );
    let _guard = span.enter();
    let response = gateway.invoke_fast_tier(request).await;

    let parsed = match response {
        Ok(resp) => {
            let parsed = parse_response(&resp.text);
            tracing::info!(
                label = parsed.label.as_str(),
                latency_ms = resp.latency_ms,
                rationale = %parsed.rationale,
                "stage c verdict",
            );
            cache.put(cache_key.clone(), parsed.clone());
            parsed
        }
        Err(err) => {
            let parsed = ParsedResponse {
                label: StageCLabel::Suspicious,
                rationale: gateway_fallback_rationale(&err),
            };
            tracing::warn!(
                error = %err,
                label = parsed.label.as_str(),
                "stage c gateway failure; fallback to SUSPICIOUS",
            );
            cache.put_failure_fallback(cache_key.clone(), parsed.clone());
            parsed
        }
    };

    compose_outcome(parsed, stage_b)
}

/// Convert a gateway error into a human-readable rationale for the
/// `SUSPICIOUS` fallback entry.
fn gateway_fallback_rationale(err: &LlmGatewayError) -> String {
    match err {
        LlmGatewayError::RateLimited(reason) => {
            format!("fast-tier gateway rate-limited: {reason}")
        }
        LlmGatewayError::Unavailable(reason) => {
            format!("fast-tier gateway unavailable: {reason}")
        }
        LlmGatewayError::MalformedResponse(reason) => {
            format!("fast-tier gateway malformed response: {reason}")
        }
        LlmGatewayError::Other(reason) => {
            format!("fast-tier gateway error: {reason}")
        }
    }
}

/// Compose Stage C's parsed response with Stage B's prior findings into the
/// final [`FirewallVerdict`].
fn compose_outcome(parsed: ParsedResponse, stage_b: StageBOutcome) -> StageCOutcome {
    let now = OffsetDateTime::now_utc();
    let stage_b_confidence = stage_b.confidence;
    let stage_b_verdict = stage_b.verdict;
    let mut annotations = stage_b.findings;
    let stage_b_qclass = stage_b.quarantine_class;

    match parsed.label {
        StageCLabel::Safe => {
            annotations.push(stage_c_finding(
                FindingKind::AnnotationNote,
                &parsed.rationale,
                0.0,
            ));
            let (verdict, qclass) =
                map_safe_outcome(stage_b_verdict, stage_b_confidence, stage_b_qclass);
            StageCOutcome {
                verdict: FirewallVerdict {
                    verdict,
                    verdict_version: SECURITY_VERSION.to_string(),
                    scan_timestamp: now,
                    quarantine_class: qclass,
                    source_receipt_id: None,
                    annotations,
                },
                suggested_trust: None,
                label: StageCLabel::Safe,
            }
        }
        StageCLabel::Suspicious => {
            let finding_confidence = if parsed.rationale == MALFORMED_RATIONALE {
                0.50
            } else {
                0.70
            };
            annotations.push(stage_c_finding(
                FindingKind::SemanticRisk,
                &parsed.rationale,
                finding_confidence,
            ));
            StageCOutcome {
                verdict: FirewallVerdict {
                    verdict: Verdict::Quarantined,
                    verdict_version: SECURITY_VERSION.to_string(),
                    scan_timestamp: now,
                    quarantine_class: Some(QuarantineClass::SecurityRisk),
                    source_receipt_id: None,
                    annotations,
                },
                suggested_trust: None,
                label: StageCLabel::Suspicious,
            }
        }
        StageCLabel::Malicious => {
            annotations.push(stage_c_finding(
                FindingKind::SemanticRisk,
                &parsed.rationale,
                1.0,
            ));
            StageCOutcome {
                verdict: FirewallVerdict {
                    verdict: Verdict::Quarantined,
                    verdict_version: SECURITY_VERSION.to_string(),
                    scan_timestamp: now,
                    quarantine_class: Some(QuarantineClass::SecurityRisk),
                    source_receipt_id: None,
                    annotations,
                },
                suggested_trust: Some(decrement_trust(stage_b_verdict)),
                label: StageCLabel::Malicious,
            }
        }
    }
}

fn stage_c_finding(kind: FindingKind, detail: &str, confidence: f32) -> StageFinding {
    StageFinding {
        stage: Stage::C,
        kind,
        detail: detail.to_string(),
        confidence,
    }
}

/// Given a SAFE Stage C label, decide whether to preserve Stage B's verdict
/// or downgrade a low-confidence `Flagged` to `Passed`.
fn map_safe_outcome(
    stage_b_verdict: Verdict,
    stage_b_confidence: f32,
    stage_b_qclass: Option<QuarantineClass>,
) -> (Verdict, Option<QuarantineClass>) {
    match stage_b_verdict {
        Verdict::Passed => (Verdict::Passed, None),
        Verdict::Flagged => {
            if stage_b_confidence <= LOW_CONFIDENCE_FLAG_DOWNGRADE_MAX {
                (Verdict::Passed, None)
            } else {
                (Verdict::Flagged, None)
            }
        }
        // Stage C should never run when Stage B is already quarantined (see
        // `should_run`); if it somehow did, we preserve the quarantine.
        Verdict::Quarantined => (Verdict::Quarantined, stage_b_qclass),
    }
}

/// Suggest a decremented trust level for a source after a `MALICIOUS`
/// verdict. This is a *suggestion* only — the firewall doesn't own the
/// trust-level store; callers (intake pipeline / trust store) decide
/// whether to apply the decrement. Stage B verdict is currently unused in
/// the mapping but kept in the signature so future extensions (e.g.
/// severity-aware decrements) can depend on it without a signature churn.
fn decrement_trust(_stage_b_verdict: Verdict) -> TrustLevel {
    // Single-step decrement; the trust store decides whether to floor at
    // VeryLow or clamp further (e.g. freeze source entirely).
    TrustLevel::VeryLow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_gateway::{FastTierResponse, LlmGatewayError};
    use crate::types::{CallSite, ConsumingAgentScope, ContentSource};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use time::OffsetDateTime;

    struct FakeGateway {
        response: Result<FastTierResponse, LlmGatewayError>,
    }

    #[async_trait]
    impl LlmGateway for FakeGateway {
        async fn invoke_fast_tier(
            &self,
            _req: FastTierRequest,
        ) -> Result<FastTierResponse, LlmGatewayError> {
            self.response.clone()
        }
    }

    fn ctx(kind: &str) -> ScanContext {
        ScanContext {
            source: ContentSource {
                kind: kind.into(),
                url: None,
                fetched_at: OffsetDateTime::now_utc(),
                claimed_content_type: Some("text/plain".into()),
                headers: BTreeMap::new(),
            },
            consuming_agent_scope: ConsumingAgentScope::minimal("agent-x"),
            call_site: CallSite::new("test.stage_c"),
        }
    }

    fn stage_b(verdict: Verdict, confidence: f32) -> StageBOutcome {
        StageBOutcome {
            verdict,
            quarantine_class: match verdict {
                Verdict::Quarantined => Some(QuarantineClass::SecurityRisk),
                _ => None,
            },
            findings: Vec::new(),
            confidence,
        }
    }

    #[test]
    fn should_run_when_stage_b_flagged_at_threshold() {
        let cfg = StageCConfig::default();
        let sb = stage_b(Verdict::Flagged, 0.30);
        assert!(should_run(&cfg, TrustLevel::Low, &sb));
    }

    #[test]
    fn should_not_run_when_stage_b_below_threshold() {
        let cfg = StageCConfig::default();
        let sb = stage_b(Verdict::Flagged, 0.10);
        assert!(!should_run(&cfg, TrustLevel::Low, &sb));
    }

    #[test]
    fn should_not_run_when_stage_b_quarantined() {
        let cfg = StageCConfig::default();
        let sb = stage_b(Verdict::Quarantined, 0.99);
        assert!(!should_run(&cfg, TrustLevel::VeryLow, &sb));
    }

    #[test]
    fn very_low_trust_forces_stage_c_even_on_pass() {
        let cfg = StageCConfig::default();
        let sb = stage_b(Verdict::Passed, 0.0);
        assert!(should_run(&cfg, TrustLevel::VeryLow, &sb));
    }

    #[test]
    fn low_trust_respects_stage_b_threshold() {
        let cfg = StageCConfig::default();
        let sb_pass = stage_b(Verdict::Passed, 0.0);
        assert!(!should_run(&cfg, TrustLevel::Low, &sb_pass));
    }

    #[tokio::test]
    async fn safe_label_downgrades_low_confidence_flag() {
        let gateway = FakeGateway {
            response: Ok(FastTierResponse {
                text: "SAFE Content is a benign article.".into(),
                latency_ms: 5,
            }),
        };
        let cache = StageCCache::default_capacity();
        let sb = stage_b(Verdict::Flagged, 0.35);
        let out = run(
            &ctx("web_fetch"),
            "benign content",
            TrustLevel::Low,
            sb,
            &StageCConfig::default(),
            &gateway,
            &cache,
        )
        .await;
        assert_eq!(out.label, StageCLabel::Safe);
        assert_eq!(out.verdict.verdict, Verdict::Passed);
        assert!(out.verdict.quarantine_class.is_none());
        assert!(out.suggested_trust.is_none());
    }

    #[tokio::test]
    async fn safe_label_preserves_high_confidence_flag() {
        let gateway = FakeGateway {
            response: Ok(FastTierResponse {
                text: "SAFE Looks fine on review.".into(),
                latency_ms: 5,
            }),
        };
        let cache = StageCCache::default_capacity();
        let sb = stage_b(Verdict::Flagged, 0.70);
        let out = run(
            &ctx("web_fetch"),
            "high-conf flagged content",
            TrustLevel::Low,
            sb,
            &StageCConfig::default(),
            &gateway,
            &cache,
        )
        .await;
        assert_eq!(out.label, StageCLabel::Safe);
        assert_eq!(out.verdict.verdict, Verdict::Flagged);
    }

    #[tokio::test]
    async fn suspicious_label_quarantines() {
        let gateway = FakeGateway {
            response: Ok(FastTierResponse {
                text: "SUSPICIOUS Possible role-reversal.".into(),
                latency_ms: 7,
            }),
        };
        let cache = StageCCache::default_capacity();
        let sb = stage_b(Verdict::Flagged, 0.40);
        let out = run(
            &ctx("web_fetch"),
            "content under review",
            TrustLevel::Low,
            sb,
            &StageCConfig::default(),
            &gateway,
            &cache,
        )
        .await;
        assert_eq!(out.label, StageCLabel::Suspicious);
        assert_eq!(out.verdict.verdict, Verdict::Quarantined);
        assert_eq!(
            out.verdict.quarantine_class,
            Some(QuarantineClass::SecurityRisk)
        );
        assert!(out.suggested_trust.is_none());
    }

    #[tokio::test]
    async fn malicious_label_quarantines_and_suggests_trust_drop() {
        let gateway = FakeGateway {
            response: Ok(FastTierResponse {
                text: "MALICIOUS Explicit jailbreak payload.".into(),
                latency_ms: 9,
            }),
        };
        let cache = StageCCache::default_capacity();
        let sb = stage_b(Verdict::Flagged, 0.60);
        let out = run(
            &ctx("web_fetch"),
            "jailbreak payload",
            TrustLevel::Low,
            sb,
            &StageCConfig::default(),
            &gateway,
            &cache,
        )
        .await;
        assert_eq!(out.label, StageCLabel::Malicious);
        assert_eq!(out.verdict.verdict, Verdict::Quarantined);
        assert_eq!(out.suggested_trust, Some(TrustLevel::VeryLow));
        // Highest-severity finding present.
        assert!(out
            .verdict
            .annotations
            .iter()
            .any(|f| f.stage == Stage::C && (f.confidence - 1.0).abs() < 1e-6));
    }

    #[tokio::test]
    async fn rate_limit_falls_back_to_suspicious() {
        let gateway = FakeGateway {
            response: Err(LlmGatewayError::RateLimited("quota".into())),
        };
        let cache = StageCCache::default_capacity();
        let sb = stage_b(Verdict::Flagged, 0.40);
        let out = run(
            &ctx("web_fetch"),
            "anything",
            TrustLevel::Low,
            sb,
            &StageCConfig::default(),
            &gateway,
            &cache,
        )
        .await;
        assert_eq!(out.label, StageCLabel::Suspicious);
        assert_eq!(out.verdict.verdict, Verdict::Quarantined);
        // Fallback rationale surfaced via the Stage C finding.
        assert!(out
            .verdict
            .annotations
            .iter()
            .any(|f| f.detail.contains("rate-limited")));
    }

    #[tokio::test]
    async fn malformed_response_is_suspicious() {
        let gateway = FakeGateway {
            response: Ok(FastTierResponse {
                text: "dunno".into(),
                latency_ms: 4,
            }),
        };
        let cache = StageCCache::default_capacity();
        let sb = stage_b(Verdict::Flagged, 0.40);
        let out = run(
            &ctx("web_fetch"),
            "x",
            TrustLevel::Low,
            sb,
            &StageCConfig::default(),
            &gateway,
            &cache,
        )
        .await;
        assert_eq!(out.label, StageCLabel::Suspicious);
    }

    #[tokio::test]
    async fn cache_hit_skips_gateway_call() {
        // Gateway returns MALICIOUS, cache returns SAFE — verify we picked up
        // the cache value.
        let gateway = FakeGateway {
            response: Ok(FastTierResponse {
                text: "MALICIOUS should not be seen".into(),
                latency_ms: 1,
            }),
        };
        let cache = StageCCache::default_capacity();
        let key = StageCCacheKey::for_payload("web_fetch", "cached-content");
        cache.put(
            key,
            ParsedResponse {
                label: StageCLabel::Safe,
                rationale: "cached-safe".into(),
            },
        );
        let sb = stage_b(Verdict::Flagged, 0.35);
        let out = run(
            &ctx("web_fetch"),
            "cached-content",
            TrustLevel::Low,
            sb,
            &StageCConfig::default(),
            &gateway,
            &cache,
        )
        .await;
        assert_eq!(out.label, StageCLabel::Safe);
    }
}
