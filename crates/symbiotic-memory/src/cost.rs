//! Cost estimation for LLM-based memory extraction.
//!
//! Provides token estimation and cost projection utilities so operators can
//! verify that periodic fact extraction stays at "pennies" level when using
//! Haiku-tier (or equivalent) models.

use std::fmt;

/// Per-million-token pricing for a model.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    /// Cost per 1 million input tokens (USD).
    pub input_per_mtok: f64,
    /// Cost per 1 million output tokens (USD).
    pub output_per_mtok: f64,
}

/// Known model pricing entries.
///
/// Prices as of early 2026.  The list is intentionally small — only models
/// that are realistic extraction targets.
pub const PRICING: &[(&str, ModelPricing)] = &[
    (
        "haiku",
        ModelPricing {
            input_per_mtok: 0.25,
            output_per_mtok: 1.25,
        },
    ),
    (
        "sonnet",
        ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        },
    ),
    (
        "flash",
        ModelPricing {
            input_per_mtok: 0.075,
            output_per_mtok: 0.30,
        },
    ),
    (
        "gpt-4o-mini",
        ModelPricing {
            input_per_mtok: 0.15,
            output_per_mtok: 0.60,
        },
    ),
];

/// Look up pricing for a model by name (case-insensitive substring match).
///
/// Returns `None` if no entry matches.
pub fn lookup_pricing(model: &str) -> Option<ModelPricing> {
    let lower = model.to_lowercase();
    PRICING
        .iter()
        .find(|(name, _)| lower.contains(name))
        .map(|(_, p)| *p)
}

/// Approximate number of tokens for English text.
///
/// Uses the ~4 characters per token heuristic which holds reasonably well
/// for modern BPE tokenisers on English prose.
const CHARS_PER_TOKEN: usize = 4;

/// Estimate token count from a character length.
pub fn estimate_tokens(char_count: usize) -> u64 {
    char_count.div_ceil(CHARS_PER_TOKEN) as u64
}

/// A cost estimate for a single extraction call or a batch.
#[derive(Debug, Clone)]
pub struct CostEstimate {
    /// Estimated input tokens.
    pub input_tokens: u64,
    /// Estimated output tokens.
    pub output_tokens: u64,
    /// Model name used for pricing lookup.
    pub model: String,
    /// Estimated total cost in USD.
    pub estimated_cost_usd: f64,
}

impl fmt::Display for CostEstimate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "model={} in={} out={} cost=${:.6}",
            self.model, self.input_tokens, self.output_tokens, self.estimated_cost_usd
        )
    }
}

/// Average output tokens per extraction call.
///
/// A typical extraction response contains 3-5 facts at ~80 tokens each,
/// plus JSON overhead.  500 tokens is a conservative upper bound.
const AVG_OUTPUT_TOKENS_PER_EXTRACTION: u64 = 500;

/// System prompt overhead in tokens (the `EXTRACTION_SYSTEM_PROMPT`).
///
/// The prompt is ~600 characters ≈ 150 tokens.  Round up for safety.
const SYSTEM_PROMPT_TOKENS: u64 = 200;

/// Estimate the cost of a single extraction call.
///
/// `messages` is a slice of `(sender, body, timestamp)` tuples representing
/// the conversation to be extracted.  `model` is matched against the
/// [`PRICING`] table.
///
/// Returns `None` if the model is not in the pricing table.
pub fn estimate_extraction_cost(
    messages: &[(String, String, String)],
    model: &str,
) -> Option<CostEstimate> {
    let pricing = lookup_pricing(model)?;

    let body_chars: usize = messages.iter().map(|(_, body, _)| body.len()).sum();
    // Include sender names + timestamps as minor overhead.
    let meta_chars: usize = messages
        .iter()
        .map(|(sender, _, ts)| sender.len() + ts.len() + 2) // +2 for separators
        .sum();
    let content_tokens = estimate_tokens(body_chars + meta_chars);
    let input_tokens = content_tokens + SYSTEM_PROMPT_TOKENS;
    let output_tokens = AVG_OUTPUT_TOKENS_PER_EXTRACTION;

    let cost = (input_tokens as f64 * pricing.input_per_mtok
        + output_tokens as f64 * pricing.output_per_mtok)
        / 1_000_000.0;

    Some(CostEstimate {
        input_tokens,
        output_tokens,
        model: model.to_string(),
        estimated_cost_usd: cost,
    })
}

/// Estimate the cost of a batch of extraction calls.
///
/// `thread_count` threads, each with `avg_messages` messages of
/// `avg_message_chars` characters.
///
/// Returns `None` if the model is not in the pricing table.
pub fn estimate_thread_batch_cost(
    thread_count: usize,
    avg_messages: usize,
    model: &str,
) -> Option<CostEstimate> {
    let pricing = lookup_pricing(model)?;

    // Typical message length: ~100 chars (a short chat message).
    const AVG_MSG_CHARS: usize = 100;
    let chars_per_thread = avg_messages * AVG_MSG_CHARS;
    let tokens_per_thread = estimate_tokens(chars_per_thread) + SYSTEM_PROMPT_TOKENS;
    let total_input = tokens_per_thread * thread_count as u64;
    let total_output = AVG_OUTPUT_TOKENS_PER_EXTRACTION * thread_count as u64;

    let cost = (total_input as f64 * pricing.input_per_mtok
        + total_output as f64 * pricing.output_per_mtok)
        / 1_000_000.0;

    Some(CostEstimate {
        input_tokens: total_input,
        output_tokens: total_output,
        model: model.to_string(),
        estimated_cost_usd: cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2); // ceiling
        assert_eq!(estimate_tokens(400), 100);
        assert_eq!(estimate_tokens(2000), 500);
    }

    #[test]
    fn lookup_pricing_finds_known_models() {
        assert!(lookup_pricing("haiku").is_some());
        assert!(lookup_pricing("sonnet").is_some());
        assert!(lookup_pricing("flash").is_some());
        assert!(lookup_pricing("gpt-4o-mini").is_some());
    }

    #[test]
    fn lookup_pricing_case_insensitive() {
        assert!(lookup_pricing("Haiku").is_some());
        assert!(lookup_pricing("SONNET").is_some());
        assert!(lookup_pricing("claude-3-haiku-20240307").is_some());
    }

    #[test]
    fn lookup_pricing_unknown_returns_none() {
        assert!(lookup_pricing("unknown-model-xyz").is_none());
    }

    #[test]
    fn single_extraction_cost_haiku_small() {
        // 5 messages, ~100 chars each = ~500 chars ≈ 125 tokens + 200 system = 325 input
        let messages: Vec<(String, String, String)> = (0..5)
            .map(|i| {
                (
                    format!("user{i}"),
                    "A".repeat(100),
                    "2026-01-01T00:00:00Z".to_string(),
                )
            })
            .collect();

        let est = estimate_extraction_cost(&messages, "haiku").expect("pricing found");
        // Should be well under $0.01
        assert!(
            est.estimated_cost_usd < 0.01,
            "small extraction should cost < $0.01, got ${:.6}",
            est.estimated_cost_usd
        );
    }

    #[test]
    fn single_extraction_cost_haiku_medium() {
        // 20 messages, ~100 chars each = ~2000 chars ≈ 500 tokens + 200 system = 700 input
        let messages: Vec<(String, String, String)> = (0..20)
            .map(|i| {
                (
                    format!("user{i}"),
                    "B".repeat(100),
                    "2026-01-01T00:00:00Z".to_string(),
                )
            })
            .collect();

        let est = estimate_extraction_cost(&messages, "haiku").expect("pricing found");
        assert!(
            est.estimated_cost_usd < 0.01,
            "medium extraction should cost < $0.01, got ${:.6}",
            est.estimated_cost_usd
        );
    }

    #[test]
    fn single_extraction_cost_haiku_large() {
        // 50 messages, ~100 chars each = ~5000 chars ≈ 1250 tokens + 200 system = 1450 input
        let messages: Vec<(String, String, String)> = (0..50)
            .map(|i| {
                (
                    format!("user{i}"),
                    "C".repeat(100),
                    "2026-01-01T00:00:00Z".to_string(),
                )
            })
            .collect();

        let est = estimate_extraction_cost(&messages, "haiku").expect("pricing found");
        assert!(
            est.estimated_cost_usd < 0.01,
            "large extraction should cost < $0.01, got ${:.6}",
            est.estimated_cost_usd
        );
    }

    #[test]
    fn batch_cost_haiku_100_threads() {
        // 100 threads, 20 messages each — daily batch
        let est = estimate_thread_batch_cost(100, 20, "haiku").expect("pricing found");
        assert!(
            est.estimated_cost_usd < 1.00,
            "100 thread batch should cost < $1.00, got ${:.4}",
            est.estimated_cost_usd
        );
    }

    #[test]
    fn batch_cost_sonnet_is_much_higher() {
        let haiku = estimate_thread_batch_cost(100, 20, "haiku").expect("haiku");
        let sonnet = estimate_thread_batch_cost(100, 20, "sonnet").expect("sonnet");
        assert!(
            sonnet.estimated_cost_usd > haiku.estimated_cost_usd * 5.0,
            "sonnet should be significantly more expensive than haiku"
        );
    }

    #[test]
    fn unknown_model_returns_none() {
        let messages: Vec<(String, String, String)> = vec![(
            "user".to_string(),
            "hello".to_string(),
            "2026-01-01T00:00:00Z".to_string(),
        )];
        assert!(estimate_extraction_cost(&messages, "unknown-model").is_none());
        assert!(estimate_thread_batch_cost(10, 20, "unknown-model").is_none());
    }

    #[test]
    fn pricing_table_covers_expected_models() {
        let expected = ["haiku", "sonnet", "flash", "gpt-4o-mini"];
        for model in &expected {
            assert!(
                lookup_pricing(model).is_some(),
                "pricing table should include {model}"
            );
        }
    }

    #[test]
    fn cost_estimate_display() {
        let est = CostEstimate {
            input_tokens: 1000,
            output_tokens: 500,
            model: "haiku".to_string(),
            estimated_cost_usd: 0.000875,
        };
        let s = est.to_string();
        assert!(s.contains("haiku"));
        assert!(s.contains("1000"));
        assert!(s.contains("500"));
        assert!(s.contains("$"));
    }
}
