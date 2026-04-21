# Metrics Layer

**Status**: Implemented (MVP)
**Crate**: `submodules/runtime/crates/symbiotic-metrics/`

## Overview

The metrics layer captures system performance and outcomes so Symbiotic can propose evidence-based improvements. Metrics inform self-improvement proposals but do **not** trigger autonomous learning.

## Implementation

The `symbiotic-metrics` crate provides:

| Module | Purpose |
|--------|---------|
| `types.rs` | `MetricEvent`, `ActionType`, `Outcome`, `EventDetails` structs |
| `store.rs` | SQLite-backed append-only event storage |
| `aggregator.rs` | Rolling-window metric computation (1h, 24h, 7d) |
| `proposals.rs` | Threshold-based improvement proposal engine |
| `dashboard.rs` | CLI output formatting for `symbiotic metrics` commands |

## Design Reference

Full design specification including event schemas, aggregation algorithms, proposal triggers, and CLI output formats:

- `docs/design/metrics-layer.md`
