# Testing

## Overview

The workspace test suite is split into two tiers so that pure unit tests
always run without requiring network access, while integration tests that
bind to local ports or make outbound connections are gated behind a feature
flag.

## Test Tiers

### Unit tests (hermetic, no network)

```bash
cargo test
```

Runs all `#[cfg(test)]` modules that do not require socket binding or
network connectivity. Safe to run in sandboxed / restricted environments.

### Integration tests (network required)

```bash
cargo test --features integration
```

Includes tests that start wiremock `MockServer` instances (which bind
ephemeral TCP ports) or make outbound HTTP connections. These tests are
gated behind `#[cfg(feature = "integration")]`.

To run integration tests for a single crate:

```bash
cargo test -p symbiotic-agents --features integration
cargo test -p credential-gateway --features integration
cargo test -p symbiotic-context --features integration
```

## Crates with Integration-Gated Tests

| Crate | What is gated | Feature |
|-------|--------------|---------|
| `symbiotic-agents` | LLM runtime wiremock tests (health check, model list, preflight, pull) | `integration` |
| `credential-gateway` | OAuth2 PKCE wiremock tests (code exchange, token refresh, full flow) | `integration` |
| `symbiotic-context` | Embedding service network-failure tests (connect to unreachable port) | `integration` |

## How It Works

Each affected crate defines an `integration` feature in its `Cargo.toml`:

```toml
[features]
integration = ["wiremock"]   # for crates using wiremock
integration = []             # for crates with outbound-only network tests
```

For wiremock-using crates, wiremock is an optional dependency that is only
compiled when the `integration` feature is active. The integration tests
live in a separate `mod integration_tests` block gated with
`#[cfg(all(test, feature = "integration"))]`.

## Adding New Integration Tests

When writing a test that binds to a port or makes network calls:

1. Place it in a `#[cfg(all(test, feature = "integration"))]` module
2. If the crate does not yet have the `integration` feature, add it to `Cargo.toml`
3. If using wiremock, add it as `wiremock = { version = "0.6", optional = true }` and
   include it in the feature: `integration = ["wiremock"]`
