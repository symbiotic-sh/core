# Repository Structure


## Overview

This document defines the **greenfield target layout** for Symbiotic. It separates runtime services, UI clients, shared crates, workflows, schemas, and policies to make implementation scalable and easy to evolve.

**Status (2026-02-05)**: In progress. Greenfield structure is active; legacy snapshot and new service/crate layout are implemented.

## Target Layout (Planned)

```mermaid
flowchart TB
    Root[symbiotic/]
    Root --> Apps[apps/]
    Root --> Services[services/]
    Root --> Scripts[scripts/]
    Root --> Legacy[legacy/]
    Root --> Crates[crates/]
    Root --> Workflows[workflows/]
    Root --> Schemas[schemas/]
    Root --> Policies[policies/]
    Root --> Docs[docs/]
    Root --> Data[data/]
    Root --> Tasks[tasks/]
    Root --> Domains[domains/]

    Apps --> Mobile[mobile/ (Flutter)]
    Apps --> Desktop[desktop/ (future)]

    Services --> Daemon[symbiotic-daemon/]
    Services --> Gateway[credential-gateway/]
    Services --> Installer[symbiotic-installer/]

    Scripts --> Bootstrap[bootstrap-vps.sh]

    Legacy --> OldAgents[agents/ (compat only)]
    Legacy --> OldScripts[scripts/legacy-* (compat only)]
    Legacy --> OldDocs[legacy-docs/ (snapshot)]

    Crates --> Core[symbiotic-core/]
    Crates --> Ingest[symbiotic-intake/]
    Crates --> Archive[symbiotic-archive/]
    Crates --> Review[symbiotic-review/]
    Crates --> DomainsCrate[symbiotic-domains/]
    Crates --> Matrix[symbiotic-matrix/]
    Crates --> Queue[symbiotic-queue/]
    Crates --> Context[symbiotic-context/]
    Crates --> Trust[symbiotic-trust/]
    Crates --> Agents[symbiotic-agents/]
    Crates --> Cli[symbiotic-cli/]
    Crates --> Memory[symbiotic-memory/]
    Crates --> Browser[symbiotic-browser/]
    Crates --> Vm[symbiotic-vm/]
    Crates --> Skills[symbiotic-skills/]
    Crates --> Metrics[symbiotic-metrics/]

    Workflows --> Runtime[runner/]
    Workflows --> Templates[templates/]

    Schemas --> Msg[matrix-events.json]
    Schemas --> Context[context-pack.json]
    Schemas --> Workflow[workflow.json]

    Policies --> Trust[trust-policy.json]
    Policies --> Redact[redaction-policy.json]
    Policies --> Allow[allowlists/]

    Data --> Archive[archive/]
    Data --> ReviewData[review/]
    Data --> Vault[vault/]
    Data --> QueueData[queue/]
    Data --> Push[push/]
    Data --> Profiles[browser-profile/]
    Data --> Install[install/]
    Data --> UserFlows[workflows/ (user overrides)]
    Data --> UserPolicies[policies/ (user overrides)]
```

## Mapping from Current Repo

| Current | Target | Notes |
| --- | --- | --- |
| `crates/*` | `crates/*` | Keep and expand (new crates now include `archive`, `queue`, `context`, `trust`, `agents`, `memory`, `browser`, `vm`, `metrics`, `skills`) |
| `agents/*` | `legacy/agents/*` (snapshot) | Curate new workflow templates separately |
| `docs/architecture/*` | same | Source of truth |
| `knowledge-base/*` | `data/archive/*` | Canonical runtime name is Archive (legacy knowledge base remains under `legacy/knowledge-base`) |
| `scripts/*` | `legacy/scripts/*` (snapshot) | Rebuild deterministic tooling under `services/*`/`workflows/runner` |
| Pre-migration runtime paths | `legacy/*` | Snapshot lane; not used as runtime dependency |

### Legacy Snapshot Strategy (Greenfield Rebuild)

During T74/T72 migration, keep old implementation surfaces under `legacy/` so new architecture can move independently:

1. Move old implementation roots into `legacy/` snapshot folders.
2. Build new runtime paths directly in target folders (no path shims/adapters).
3. Cherry-pick code from `legacy/` only when needed, with new tests/contracts.
4. Keep `legacy/` out of active runtime entry points.

### Customization + Gitignore

- **Shipped defaults** live in repo (`workflows/templates/`, `policies/`).
- **User custom workflows** live in `data/workflows/` and are **gitignored**.
- **User policy overrides** live in `data/policies/` and are **gitignored**.

### Policy Source of Truth

- Human-readable policy specs live in `docs/architecture/*` (source of truth for design).
- Runtime policy artifacts live in `policies/*.json` (machine-consumable deployment files).
- During migration, `policies/` may be generated from approved architecture specs.

## Key Decisions

1. **Runtime workflows are first‑class**: separate from dev tasks in `tasks/`.
2. **Schemas + policies are explicit**: message formats and redaction rules are versioned.
3. **Services are isolated**: daemon/broker/gateway are separate deployable units.
4. **Data is grouped**: runtime data (Vault, Archive, Queue, Review, Push) live under `data/`.

## Related Docs

- `docs/architecture/system-map.md`
- `docs/architecture/runtime-workflows.md`
- `docs/architecture/context-delivery.md`
- `docs/architecture/symbiotic-daemon.md`
