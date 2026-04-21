# Local LLM Runtime

**Status**: Partially Implemented
**Crate**: `submodules/runtime/crates/symbiotic-agents/src/llm_runtime.rs`

## Overview

The local LLM runtime provides opt-in model inference for features like Distillery processing, on-device reasoning, and privacy-maximized workflows. It is **not** required for the base tier — credential operations are handled by the Auth Script Engine (deterministic Playwright scripts), not by an LLM.

When enabled (LLM tier), the runtime manages Ollama health checks, model availability, and inference routing.

## Implementation Status

| Component | Status |
|-----------|--------|
| LLM runtime abstraction | Implemented in `submodules/runtime/crates/symbiotic-agents/src/llm_runtime.rs` |
| Ollama Docker deployment | Specified in `docker-compose.vps.yml` (scaffold) |
| Health check contract | Designed, not yet wired |
| Preflight sequence | Designed, not yet wired |

## Design Reference

Full design specification including health check contract, preflight test checklist, failure behavior, and recovery flows:

- `docs/design/local-llm-runtime.md`
- `docs/design/compute-tiers.md` (tier definitions and resource requirements)

## Related Architecture Docs

- `docs/architecture/credential-sandbox.md`
- `docs/architecture/vps-deployment.md`
