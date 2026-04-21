//! Cost verification tests for LLM-based memory extraction.
//!
//! Ensures that Haiku-tier extraction stays at "pennies" level for
//! typical usage patterns.

use symbiotic_memory::cost::{
    estimate_extraction_cost, estimate_thread_batch_cost, lookup_pricing, PRICING,
};

// ---------------------------------------------------------------------------
// Helper: build a synthetic message list
// ---------------------------------------------------------------------------

fn make_messages(count: usize, avg_chars: usize) -> Vec<(String, String, String)> {
    (0..count)
        .map(|i| {
            (
                format!("user-{i}"),
                "x".repeat(avg_chars),
                "2026-01-15T12:00:00Z".to_string(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Single-extraction cost thresholds (< $0.01 per extraction for Haiku)
// ---------------------------------------------------------------------------

#[test]
fn haiku_cost_5_messages_under_one_cent() {
    // ~500 chars ≈ 125 content tokens + system prompt
    let msgs = make_messages(5, 100);
    let est = estimate_extraction_cost(&msgs, "haiku").expect("haiku pricing exists");
    assert!(
        est.estimated_cost_usd < 0.01,
        "5-message extraction on haiku should be < $0.01, got ${:.6}",
        est.estimated_cost_usd,
    );
}

#[test]
fn haiku_cost_20_messages_under_one_cent() {
    // ~2000 chars ≈ 500 content tokens + system prompt
    let msgs = make_messages(20, 100);
    let est = estimate_extraction_cost(&msgs, "haiku").expect("haiku pricing exists");
    assert!(
        est.estimated_cost_usd < 0.01,
        "20-message extraction on haiku should be < $0.01, got ${:.6}",
        est.estimated_cost_usd,
    );
}

#[test]
fn haiku_cost_50_messages_under_one_cent() {
    // ~5000 chars ≈ 1250 content tokens + system prompt
    let msgs = make_messages(50, 100);
    let est = estimate_extraction_cost(&msgs, "haiku").expect("haiku pricing exists");
    assert!(
        est.estimated_cost_usd < 0.01,
        "50-message extraction on haiku should be < $0.01, got ${:.6}",
        est.estimated_cost_usd,
    );
}

// ---------------------------------------------------------------------------
// Daily batch cost threshold (< $1.00 for 100 threads/day on Haiku)
// ---------------------------------------------------------------------------

#[test]
fn haiku_batch_100_threads_under_one_dollar() {
    let est = estimate_thread_batch_cost(100, 20, "haiku").expect("haiku pricing exists");
    assert!(
        est.estimated_cost_usd < 1.00,
        "100-thread daily batch on haiku should be < $1.00, got ${:.4}",
        est.estimated_cost_usd,
    );
}

#[test]
fn haiku_batch_50_threads_under_50_cents() {
    let est = estimate_thread_batch_cost(50, 20, "haiku").expect("haiku pricing exists");
    assert!(
        est.estimated_cost_usd < 0.50,
        "50-thread daily batch on haiku should be < $0.50, got ${:.4}",
        est.estimated_cost_usd,
    );
}

// ---------------------------------------------------------------------------
// Flash is even cheaper than Haiku
// ---------------------------------------------------------------------------

#[test]
fn flash_cheaper_than_haiku() {
    let haiku = estimate_thread_batch_cost(100, 20, "haiku").expect("haiku");
    let flash = estimate_thread_batch_cost(100, 20, "flash").expect("flash");
    assert!(
        flash.estimated_cost_usd < haiku.estimated_cost_usd,
        "flash (${:.6}) should be cheaper than haiku (${:.6})",
        flash.estimated_cost_usd,
        haiku.estimated_cost_usd,
    );
}

// ---------------------------------------------------------------------------
// Pricing table coverage
// ---------------------------------------------------------------------------

#[test]
fn pricing_table_has_all_expected_models() {
    let expected = ["haiku", "sonnet", "flash", "gpt-4o-mini"];
    for model in &expected {
        assert!(
            lookup_pricing(model).is_some(),
            "PRICING table must include '{model}'"
        );
    }
}

#[test]
fn pricing_table_values_are_positive() {
    for (name, p) in PRICING {
        assert!(
            p.input_per_mtok > 0.0,
            "{name}: input price must be positive"
        );
        assert!(
            p.output_per_mtok > 0.0,
            "{name}: output price must be positive"
        );
    }
}

#[test]
fn pricing_table_output_more_expensive_than_input() {
    for (name, p) in PRICING {
        assert!(
            p.output_per_mtok >= p.input_per_mtok,
            "{name}: output should cost >= input (in={}, out={})",
            p.input_per_mtok,
            p.output_per_mtok,
        );
    }
}

// ---------------------------------------------------------------------------
// Monthly cost projection for typical usage
// ---------------------------------------------------------------------------

#[test]
fn monthly_cost_10_active_threads_haiku() {
    // 10 threads, extracted 6x/day (every 4h), 30 days
    let daily = estimate_thread_batch_cost(10 * 6, 20, "haiku").expect("haiku");
    let monthly = daily.estimated_cost_usd * 30.0;
    assert!(
        monthly < 5.0,
        "10 active threads on haiku should cost < $5/month, got ${:.2}",
        monthly,
    );
}

#[test]
fn monthly_cost_50_active_threads_haiku() {
    // 50 threads, extracted 6x/day (every 4h), 30 days
    let daily = estimate_thread_batch_cost(50 * 6, 20, "haiku").expect("haiku");
    let monthly = daily.estimated_cost_usd * 30.0;
    assert!(
        monthly < 25.0,
        "50 active threads on haiku should cost < $25/month, got ${:.2}",
        monthly,
    );
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn unknown_model_returns_none_for_extraction() {
    let msgs = make_messages(5, 100);
    assert!(
        estimate_extraction_cost(&msgs, "nonexistent-llm-9000").is_none(),
        "unknown model should return None"
    );
}

#[test]
fn unknown_model_returns_none_for_batch() {
    assert!(
        estimate_thread_batch_cost(10, 20, "nonexistent-llm-9000").is_none(),
        "unknown model should return None"
    );
}

#[test]
fn zero_messages_still_has_system_prompt_cost() {
    let msgs: Vec<(String, String, String)> = vec![];
    let est = estimate_extraction_cost(&msgs, "haiku").expect("haiku pricing exists");
    // Even with 0 user messages, we still send the system prompt
    assert!(
        est.input_tokens > 0,
        "system prompt should contribute tokens"
    );
    assert!(
        est.estimated_cost_usd > 0.0,
        "cost should be > 0 for system prompt"
    );
}

#[test]
fn zero_threads_batch_is_free() {
    let est = estimate_thread_batch_cost(0, 20, "haiku").expect("haiku pricing exists");
    assert!(
        est.estimated_cost_usd == 0.0,
        "0 threads should cost $0, got ${:.6}",
        est.estimated_cost_usd,
    );
}
