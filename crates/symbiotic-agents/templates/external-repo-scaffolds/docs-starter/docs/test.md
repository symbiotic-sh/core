# {{project_name}} Test Guide

## Running the test suite

*(project-specific test command — e.g. `cargo test --workspace`,
`npm test`, `pytest`, `go test ./...`)*

## Coverage

*(coverage tool + target threshold if any)*

## Tiers

- **Unit** — fastest; isolate a single function/module. Target: <1s per test.
- **Integration** — multi-module; may touch filesystem/tempdir; no network.
- **End-to-end** — exercise the deployed surface; may require a running server. Expected to be slower + flakier; keep quarantined from the Unit/Integration cadence.

## When Source Archeology's Verify stage fails

1. Check `.debug-session/archeology-report.md` for the rejected findings.
2. Apply the proposed patch manually against a fresh checkout; note why it didn't apply cleanly.
3. Re-run the pipeline with `ArcheologyMode::DryRun` to see the rejection without side effects.

<!-- Symbiotic Source Archeology scaffold — placeholder tokens:
     {{project_name}} -->
