# Extraction Cost Model

## Overview

The Living Memory System (T109) periodically extracts facts from thread
conversations using an LLM.  This document specifies the cost model, verifies
that Haiku-tier extraction stays at "pennies" level, and defines thresholds
and fallback strategies.

## Model Pricing Table

Prices per **million tokens** (as of early 2026):

| Model | Input $/MTok | Output $/MTok | Tier |
|-------|-------------|---------------|------|
| Claude 3 Haiku | $0.25 | $1.25 | Budget |
| Gemini Flash | $0.075 | $0.30 | Budget |
| GPT-4o-mini | $0.15 | $0.60 | Budget |
| Claude 3.5 Sonnet | $3.00 | $15.00 | Mid |

The default extraction model is determined by the provider router config
(`symbiotic-core::ProviderRouter`).  If no router is configured, the daemon
falls back to the default model.

## Token Estimation

- **Heuristic:** ~4 characters per token (standard BPE on English text).
- **System prompt:** ~600 chars = ~150 tokens (rounded up to 200 for safety).
- **Output:** ~500 tokens per extraction (3-5 facts, JSON overhead).

## Per-Extraction Cost

| Thread Size | Messages | Est. Input Tokens | Est. Output Tokens | Haiku Cost | Flash Cost |
|-------------|----------|-------------------|--------------------|------------|------------|
| Small | 5 (~500 chars) | ~325 | ~500 | $0.000706 | $0.000174 |
| Medium | 20 (~2000 chars) | ~700 | ~500 | $0.000800 | $0.000203 |
| Large | 50 (~5000 chars) | ~1450 | ~500 | $0.000988 | $0.000259 |

All three tiers are well under the **$0.01 per extraction** threshold.

## Extraction Frequency

- **Active thread cooldown:** 4 hours (`DEFAULT_EXTRACTION_COOLDOWN_SECS` in `thread_distillery.rs`).
- **Idle thread cooldown:** 24 hours (no new messages = lower priority).
- **Maximum extractions per thread per day:** 6 (one every 4 hours).

## Daily Batch Cost

| Threads/Day | Avg Messages | Haiku Daily | Flash Daily |
|-------------|-------------|-------------|-------------|
| 10 | 20 | ~$0.008 | ~$0.002 |
| 50 | 20 | ~$0.040 | ~$0.010 |
| 100 | 20 | ~$0.080 | ~$0.020 |

All scenarios are well under **$1.00/day** for Haiku.

## Monthly Cost Projection

Assuming extraction runs 6x/day per active thread (every 4 hours):

| Active Threads | Haiku Monthly | Flash Monthly |
|----------------|---------------|---------------|
| 10 | ~$1.44 | ~$0.36 |
| 25 | ~$3.60 | ~$0.90 |
| 50 | ~$7.20 | ~$1.80 |

For typical personal usage (10-50 active threads), monthly extraction costs
on Haiku are **$1-8/month** — firmly in the "pennies per day" range.

## Threshold & Fallback Strategy

| Threshold | Value | Action |
|-----------|-------|--------|
| Per-extraction | $0.01 | If exceeded, increase cooldown to 8h |
| Daily batch | $1.00 | If exceeded, batch extractions (process top-N by activity) |
| Monthly | $25.00 | If exceeded, switch to Flash tier or increase cooldown to 12h |

### Fallback escalation

1. **Increase cooldown** — 4h -> 8h -> 12h -> 24h
2. **Batch by priority** — Only extract from threads with new messages since last extraction
3. **Downgrade model** — Switch to Flash tier (3x cheaper than Haiku)
4. **Skip idle threads** — Only extract from threads with activity in last 24h

## Components

| File | Role |
|------|------|
| `crates/symbiotic-memory/src/cost.rs` | Token estimation and cost projection |
| `crates/symbiotic-memory/src/extraction.rs` | LLM extraction with optional token tracking |
| `crates/symbiotic-memory/src/types.rs` | `ExtractionResult` with `input_tokens`/`output_tokens` |
| `services/symbiotic-daemon/src/thread_distillery.rs` | Extraction trigger with 4h cooldown |
| `tests/cost_verification_test.rs` | Automated cost threshold assertions |

## Key Decisions

- **Heuristic over tokeniser:** We use 4 chars/token rather than pulling in a
  tokeniser crate.  This is intentionally conservative (overestimates by ~10%)
  and avoids a heavy dependency for a monitoring/estimation feature.
- **Optional token fields:** `ExtractionResult.input_tokens` and
  `output_tokens` are `Option<u64>` — populated when the LLM API returns
  usage metadata, otherwise `None`.  This keeps the extraction path
  backward-compatible.
- **Model resolved at runtime:** The extraction model comes from the provider
  router config, not a compile-time constant.  Cost estimation accepts any
  model string and matches it against the pricing table.
