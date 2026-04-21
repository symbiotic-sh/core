# Archive Storage


## Overview

Archive storage is the durable content store used by ingestion, review queueing, and Recall Gateway retrieval.

**Status (2026-02-05)**: In progress. Persistent Archive/Vault stores and daemon wiring are implemented for MVP.

## Implemented Components

| Path | Purpose |
| --- | --- |
| `submodules/runtime/crates/symbiotic-archive/src/lib.rs` | Persistent archive/vault store contracts (`FileArchiveStore`) |
| `submodules/runtime/crates/symbiotic-review/src/lib.rs` | Review store + extractive Brief engine (`FileReviewStore`) |
| `submodules/runtime/services/symbiotic-daemon/src/lib.rs` | Archive + Vault adapters for intake and context retrieval |
| `data/archive/` | Archive record content/index files (created at runtime) |
| `data/vault/` | Vault record content/index files (created at runtime) |
| `data/review/` | Review summaries and index (created at runtime) |

## Storage Model

Each stored document includes:

- `record_id`
- `title`
- `content`
- `source_url` (optional)
- `tags`
- `sensitivity` (`shareable|restricted|private`)
- timestamps

Current persistence format:

- index metadata file (TSV-like format)
- content markdown/plain files per record

## Data Flow

```mermaid
flowchart LR
    Intake[Intake Pipeline] --> Store[FileArchiveStore]
    Store --> ArchiveData[data/archive]
    Intake --> VaultStore[FileArchiveStore (vault)]
    VaultStore --> VaultData[data/vault]
    ArchiveData --> Review[Archive review]
    Review --> ReviewStore[data/review]
    ArchiveData --> Recall[Recall Gateway]
    VaultData --> Recall
```

## Review Queue Contract

Archive ingestion is only considered complete when:

1. content is persisted in Archive
2. `archive.review.enqueue` is durably enqueued
3. `review_job_id` is returned

## Sensitivity Boundary

- Archive: shareable/restricted content
- Vault: high-sensitivity secure notes and credential-adjacent material

Routing policy is enforced by intake classifier and daemon adapters.

## Legacy Compatibility

Legacy markdown Archive remains under:

- `legacy/knowledge-base/`

Migration to current Archive runtime is tracked separately (post-MVP task lane).

## Planned Evolution

1. Replace file index with optimized backend after benchmark (if needed).
2. Add richer retrieval metadata for graph/vector coupling.
3. Add explicit migration tooling from legacy markdown corpus.
