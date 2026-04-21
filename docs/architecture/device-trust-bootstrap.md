# Device Trust Bootstrap


**Status**: Implemented (MVP)
**Crate**: `submodules/runtime/crates/symbiotic-trust/`

## Overview

Device trust bootstrap defines how the Symbiotic app establishes a **verified device** for sensitive actions (credential approvals, session handle export, payment flows). A device must complete SAS emoji verification via Matrix before it gains access to trust-critical operations.

## Implementation

The `symbiotic-trust` crate provides device trust in `src/device.rs`:

| Component | Description |
|-----------|-------------|
| `DeviceTrustLevel` | Enum: `Unverified`, `Verified`, `Revoked` |
| `DeviceTrustRecord` | Per-device trust state with verification timestamps |
| `DeviceTrustError` | Error types for trust check failures |
| `TrustCache` | Local JSON cache at `~/.symbiotic/trust-cache.json` with 24h TTL |
| SAS verification | Matrix SAS emoji verification protocol integration |

## Trust Levels

| Level | Allowed Actions |
|-------|-----------------|
| **Unverified** | Read non-sensitive channels, view system status |
| **Verified** | Approve credentials, view private alerts, export session handles |
| **Revoked** | No actions (must re-verify) |

## Design Reference

Full design specification including SAS protocol sequence, first-device bootstrap flow, approval/revocation flows, and test strategy:

- `docs/design/device-trust-bootstrap.md`

## Related Architecture Docs

- `docs/architecture/trust-capabilities.md`
- `docs/architecture/credential-sandbox.md`
- `docs/architecture/matrix-channels.md`
