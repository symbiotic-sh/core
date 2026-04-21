//! Minimal LLM gateway surface consumed by Stage C (design §3.3).
//!
//! The firewall does **not** own an LLM runtime. Instead it declares the
//! narrow slice of behaviour it needs — a `fast`-tier single-shot invocation
//! with a stop-sequence contract — as a trait. The real T83 LLM Runtime
//! Manager (or any test double) implements it; the firewall stays free of a
//! runtime dependency and can be unit-tested in isolation.
//!
//! # Why a trait, not a concrete type
//!
//! Stage C is cheap + constrained by design. It only needs:
//!
//! - One `fast`-tier invocation per scan (see [`FastTierRequest`]).
//! - A textual response plus a flag indicating whether the upstream runtime
//!   refused the call (rate-limit, circuit-break, model unhealthy). On a
//!   refusal, Stage C fails conservatively to `SUSPICIOUS` (design §7).
//!
//! Production wiring lives in the daemon: the daemon binds the firewall's
//! [`LlmGateway`] to the real T83 gateway at startup. Tests here bind a
//! simple in-process mock so the firewall's behavioural tests don't need a
//! live model.

use async_trait::async_trait;

/// Minimum bytes of rationale returned by a `fast`-tier call. Stage C's
/// response parser accepts anything at least this long; shorter replies are
/// treated as "no rationale supplied" and are labelled with a placeholder.
pub const MIN_RATIONALE_BYTES: usize = 1;

/// Request shape for a `fast`-tier invocation used by Stage C.
///
/// The firewall wraps the operator-content into the canonical Stage C prompt
/// (see [`crate::stages::stage_c_prompt`]) before calling into the gateway.
/// The gateway receives an already-templated prompt; it does not see the
/// raw operator content outside of that template.
#[derive(Debug, Clone)]
pub struct FastTierRequest {
    /// The fully-rendered prompt including the content being evaluated.
    pub prompt: String,
    /// Optional upper bound on response tokens. Stage C's parser only needs
    /// a verdict token + one-sentence rationale; any gateway implementation
    /// that honours this cap keeps latency + cost bounded.
    pub max_response_tokens: Option<u32>,
}

/// Response from a `fast`-tier invocation.
///
/// The text field is passed to Stage C's strict parser
/// ([`crate::stages::stage_c_prompt::parse_response`]); any deviation from
/// the expected `SAFE | SUSPICIOUS | MALICIOUS` format is treated as
/// `SUSPICIOUS` (conservative fail-to-safe).
#[derive(Debug, Clone)]
pub struct FastTierResponse {
    /// Raw text output from the model.
    pub text: String,
    /// Observed latency in milliseconds. Surfaced into Stage C's tracing
    /// span for observability (design §6.5).
    pub latency_ms: u64,
}

/// Errors a `fast`-tier call can surface to Stage C.
///
/// Stage C's orchestrator maps each variant to a conservative verdict per
/// design §7 (`RateLimited` / `Unavailable` → `SUSPICIOUS`). We intentionally
/// keep this enum small and typed so the mapping is exhaustive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LlmGatewayError {
    /// The upstream rejected the call because the caller is rate-limited.
    /// Stage C maps this to a short-TTL `SUSPICIOUS` cache entry to avoid
    /// hammering the runtime.
    #[error("llm gateway rate limited: {0}")]
    RateLimited(String),
    /// Model or runtime is temporarily unavailable (health check failing,
    /// circuit breaker open, etc.). Same conservative mapping as rate-limit.
    #[error("llm gateway unavailable: {0}")]
    Unavailable(String),
    /// The gateway produced a response but it could not be parsed into a
    /// [`FastTierResponse`] (e.g. transport error). Treated the same as
    /// unavailable.
    #[error("llm gateway malformed response: {0}")]
    MalformedResponse(String),
    /// Any other error. Treated the same as unavailable.
    #[error("llm gateway error: {0}")]
    Other(String),
}

/// Minimal LLM surface consumed by Stage C.
///
/// # Implementation notes
///
/// - Production implementations forward to the T83 LLM Runtime Manager's
///   `fast`-tier routing. The firewall does not care which concrete model
///   resolves the tier; that is the runtime's job.
/// - Implementations MUST be `Send + Sync` so Stage C can be invoked from
///   the async scan pipeline.
/// - Implementations SHOULD cap response tokens when `max_response_tokens`
///   is set; Stage C always sets it.
#[async_trait]
pub trait LlmGateway: Send + Sync {
    /// Invoke a `fast`-tier model with the given prompt. Returns the raw
    /// textual response plus observed latency, or a typed error on failure.
    async fn invoke_fast_tier(
        &self,
        request: FastTierRequest,
    ) -> Result<FastTierResponse, LlmGatewayError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory mock that returns a pre-seeded response. Used by Stage C
    /// tests to drive each verdict branch.
    pub(crate) struct MockGateway {
        pub response: Result<FastTierResponse, LlmGatewayError>,
    }

    #[async_trait]
    impl LlmGateway for MockGateway {
        async fn invoke_fast_tier(
            &self,
            _request: FastTierRequest,
        ) -> Result<FastTierResponse, LlmGatewayError> {
            self.response.clone()
        }
    }

    #[tokio::test]
    async fn mock_gateway_returns_seeded_response() {
        let gw = MockGateway {
            response: Ok(FastTierResponse {
                text: "SAFE The content is benign.".into(),
                latency_ms: 42,
            }),
        };
        let got = gw
            .invoke_fast_tier(FastTierRequest {
                prompt: "ignored".into(),
                max_response_tokens: Some(64),
            })
            .await
            .expect("mock returns Ok");
        assert!(got.text.starts_with("SAFE"));
        assert_eq!(got.latency_ms, 42);
    }

    #[tokio::test]
    async fn mock_gateway_returns_seeded_error() {
        let gw = MockGateway {
            response: Err(LlmGatewayError::RateLimited("quota exceeded".into())),
        };
        let err = gw
            .invoke_fast_tier(FastTierRequest {
                prompt: "x".into(),
                max_response_tokens: None,
            })
            .await
            .expect_err("mock returns Err");
        assert!(matches!(err, LlmGatewayError::RateLimited(_)));
    }
}
