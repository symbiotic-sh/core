# Architecture Arbitration Matrix (2026-04-04)

**Status**: Approved for alignment work
**Purpose**: Freeze the contested architectural decisions that are currently drifting across vision, design docs, implementation, and task state.
**Scope**: This document does not replace every affected doc immediately. It is the arbitration layer that downstream docs and tasks must converge to.

## Why This Exists

Symbiotic has accumulated multiple valid-but-conflicting descriptions of the system:

- `Archive` vs `Vault` semantics diverge across vision, naming, and memory docs.
- The old room model (`#control`, `#status`, `#intake`, `#goal-*`) coexists with the thread model (`#stream`, `#thread-*`).
- The deliberation-first workflow conflicts with older confidence-threshold auto-execution language.
- Memory ranking/decay has multiple overlapping theories active at once.
- The managed relay boundary in implementation is stronger than the boundary described in docs.

This matrix resolves those conflicts so future implementation can follow one model.

## Arbitration Table

| Topic | Adopted Decision | Rejected / Demoted Interpretations | Why |
|---|---|---|---|
| Knowledge layer naming | `Archive` is the persistent knowledge layer. `knowledge-base/` is the Archive's Markdown substrate. `Vault` remains the secrets / credential isolation boundary only. | Any model where the entire Markdown knowledge system is renamed to `Vault`. | The naming canon and security model are clearer when knowledge sync and secrets custody remain separate. |
| Source-of-truth model for memory | Markdown in `knowledge-base/` is the source of truth for user knowledge. SQLite/graph/vector stores are derived indexes. | Any model where the DB becomes canonical again. | This preserves inspectability, rebuildability, and Git history. |
| Archived fact representation | Superseded / archived facts remain first-class history. The canonical format must support visible archival metadata, whether via `## Archived` or an equivalent normalized section. | Any model that removes archived history from the Markdown representation entirely. | Decision history and fact provenance are core product semantics. |
| Desired-state manifest root | Desired state lives under `knowledge-base/operations/`, `knowledge-base/identity/`, and related Archive paths. Docs may use logical paths like `operations/goals/...`, but implementation paths must be Archive-root-relative. `self/` and `methodology/` are rejected historical names, not active aliases. | Ambiguous mixed usage where some docs imply repo-root `methodology/` and others imply `knowledge-base/methodology/`. | This removes watcher/bootstrap ambiguity and eliminates the overloaded `self` / `methodology` naming. |
| Interaction room model | The thread architecture is the target canon: `#stream` + `#thread-{slug}` + unchanged isolated/security rooms. Old rooms are migration compatibility only. | Treating the old room model as co-equal canon. | The app and daemon have already partially crossed this boundary. |
| Goal container model | A thread is the conversational/project container. Multiple goals may live within one thread. | Goal = room. | This matches the approved UX and current code direction. |
| Execution approval model | Deliberation-first is canonical for non-trivial work. Confidence thresholds may assist escalation, but they do not bypass the deliberation gate for goal execution. | Confidence-only auto-execution as a primary control mechanism. | This preserves the product’s “inquisition / blueprint / approval” contract. |
| Managed control-plane boundary | Managed mode may use a relay/tunnel as a product mode, but this is an explicit architecture choice, not an invisible implementation detail. Boundary docs must state that managed mode introduces an ongoing relay dependency. | Docs claiming app-to-runtime direct operation while implementation depends on relay proxying. | Trust and availability assumptions change materially here. |
| Memory ranking model | The canonical direction is: Markdown source of truth, read-time retrieval scoring, explicit superseding history, and one declared temporal model. FSRS may augment temporal decay only where runtime callsites actually use it. | Parallel canonical models for somatic weighting, simplified decay, and reinforced graph weights without an arbitration layer. | One retrieval system cannot honestly be “canonical” if multiple scoring theories compete without priority rules. |
| Instrumentation prerequisites for self-improvement | T112 depends on durable, queryable observability, not just in-memory per-process logs. | Treating volatile `Vec`-backed logs as sufficient long-term fitness evidence. | Evolution needs durable evidence and post-restart continuity. |
| Zero-trust agent execution | T113 is not complete until authenticated socket handshake and capability-bound request enforcement are real. | Treating world-writable sockets plus allow-all execution as “done.” | This is a security boundary, not a refactor detail. |

## Canonical Terms

### Archive

- Persistent knowledge layer.
- Human-readable Markdown under `knowledge-base/`.
- Synced / versioned knowledge substrate.
- Includes semantic, episodic, and procedural content.

### Vault

- Secret and credential isolation boundary.
- Not a synonym for the knowledge base.
- Not the primary label for Markdown memory.

### Nucleus

- Runtime orchestration service (`symbiotic-daemon`).

### Control Plane

This term is overloaded and must be split in writing:

- **Declarative control plane**: the runtime reconciliation/orchestration model inside the user-owned runtime.
- **Managed control-plane service**: the hosted service in `submodules/control-plane`.

Future docs should avoid using bare `control-plane` without one of those qualifiers.

## Required Follow-Through

These documents must be updated to align with this matrix:

- `docs/VISION.md`
- `docs/architecture/system-map.md`
- `docs/architecture/repo-structure.md`
- `control-plane/docs/architecture/runtime-control-plane-boundary.md`
- `docs/architecture/symbiotic-daemon.md`
- `control-plane/docs/design/declarative-control-plane.md`
- `docs/design/memory-system.md`
- `docs/design/memory-system-simplification.md`
- `docs/design/vault-as-truth.md`
- `docs/design/thread-architecture.md`
- `docs/design/deliberation-first-pipeline.md`

## Implementation Implications

### P0

1. Restore truthful task state before continuing major feature work.
2. Repair the app/control-plane install contract.
3. Finish the real T113 security boundary.

### P1

1. Decide whether managed relay dependency is a first-class product mode.
2. Collapse memory model duplication into one runtime-backed retrieval contract.
3. Make T120/T122 durable before using them as T112 evidence.

### P2

1. Remove stale duplicate docs.
2. Remove stale duplicate crates and legacy app boundary text.
3. Convert migration-era docs from implicit canon to explicitly historical.

## Non-Goals

- This document does not rewrite every architecture doc immediately.
- This document does not mark tasks complete.
- This document does not validate live provider behavior.

It exists to stop silent divergence and give the repo one explicit decision surface.
