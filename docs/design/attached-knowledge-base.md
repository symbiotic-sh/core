# Attached Knowledge Base — Project-Scoped Indexed Docs Mirror

**Status**: Proposed Specification
**Related Tasks**: T129 (this doc), T126 (Repo Manifest — provides attachment point), T127 (Project Bootstrap — creates AKB on onboarding), T128 (Source Archeology — writes back into AKB-indexed source)
**Related Docs**: `docs/design/repo-manifest.md`, `docs/design/project-bootstrap-process.md`, `docs/design/source-archeology.md`, `docs/design/vault-as-truth.md`, `docs/design/vault-organization.md`, `docs/architecture/distillery.md`, `docs/architecture/knowledge-storage.md`, `docs/design/scaffolding-retirement.md`

## Problem

Every project Symbiotic manages accumulates documentation — design docs, architecture notes, ADRs, operational runbooks, READMEs, external reference material. Today this content lives in the project's git repo as plain markdown files. Agents working on the project cannot retrieve it efficiently: Grep is brittle, vector search does not exist for repo content, and tier-aware ranking is confined to the sovereign vault.

The straightforward "fix" — ingest project docs into the personal sovereign vault under `library/` — is wrong. It conflates three distinct storage classes:

- **Sovereign vault** (user personal, SmartOffice app, Flux app, future standalone Symbiotic) — authoritative truth for a domain, sandboxed, potentially on a separate machine, with its own sovereign-sync remote.
- **Project-attached source repo** (T126) — code content, credential-isolated, agent worktrees, internal bare mirror + external origin.
- **Project documentation** — prose that needs retrieval benefits but is not authoritative truth (regenerable context, not anchor data).

Project documentation is a third class. It should get tiered-memory + semantic retrieval parity with the sovereign vault without polluting any sovereign store.

## Non-Goals

- **Not a new truth layer.** An AKB is a read-optimized indexed mirror. Source of truth is the markdown in the repo; the index is regenerable runtime cache. Writes go through the repo's normal edit path (or via T128 Source Archeology for drift reconciliation against the attached source), not through the AKB index.
- **Not a replacement for sovereign vaults.** Sovereign vaults remain the only authoritative storage class. AKBs carry no identity, no truth, no credential-adjacent material.
- **Not federated retrieval across machines.** AKBs live on the same machine as the daemon that serves them. Cross-machine AKB queries are out of scope for MVP.
- **Not a new indexing pipeline.** Reuses the existing Distillery (`docs/architecture/distillery.md`) per-root. The `kb_root` parameter is already configurable.
- **Not the path for one-shot repo ingestion.** When the operator wants to capture a repo's content as ambient Archive entries ("remember this library's ideas"), the Intake pipeline handles it the same way it handles tweets, URLs, and files — entries land in `knowledge-base/ledger/**`, retrievable via `RecallScope::Sovereign`. Ingested entries can carry `related_projects: [...]` frontmatter so they show up as project-proximate in federated recall, without needing to be attached as a reference-library AKB. See `repo-manifest.md` §Repo Intent Taxonomy for the full flow decision matrix.

## Core Decision

**An Attached Knowledge Base is a daemon-managed indexed mirror of a project-attached repo's markdown content, scoped to that repo, regenerable from source, retrievable via federated Recall.**

Key properties:

1. **Anchored to a repo**. An AKB exists only as an attachment to a T126 repo manifest with `repo_role: docs_akb` (or `reference_library` for external reference material like Axum docs, Stripe API spec).
2. **Repo is source of truth**. Markdown files in the repo are edited as files, git-tracked, sovereign-sync'd per the repo's own manifest. The AKB index is purely derived.
3. **Index is runtime cache**. Lives under `data/akb/{repo_id}/` on the daemon machine. Rebuildable from repo content. Not git-tracked, not sovereign-sync'd (the source is, which is enough).
4. **Scope-bounded retrieval**. Federated Recall extension accepts `scope: {repo: "repo:symbiotic-docs"}` or `scope: {project: "symbiotic"}`. An agent working on project X gets priority access to X's AKBs; a general personal-vault query stays confined to the sovereign vault.
5. **No cross-contamination**. AKB content never leaks into the personal vault or into other sovereign vaults. Sensitivity floor inherited from the source repo's sensitivity.

## Attachment Mechanism

T126 gains a `repo_role` discriminator on the `RepoManifest` frontmatter:

```yaml
---
id: repo:symbiotic-docs
project_id: symbiotic
slug: symbiotic-docs
repo_role: docs_akb           # NEW — one of: source | docs_akb | reference_library
state: active
source:
  url: file:///home/k/p/symbiotic
  provider: local
  default_branch: main
  protected_branches: [main]
  pinned_head: null
# ... standard repo manifest fields ...
indexing:                      # NEW — only present when repo_role != source
  root_paths: ["docs/", "README.md", "AGENTS.md", "CLAUDE.md"]
  exclude_patterns: ["**/node_modules/**", "**/target/**"]
  distillery_config:
    enable_reweave: false      # AKB ingest is read-only; reweave would modify source
    enable_semantic_verify: true
    model: "qwen3.5"
  tier_policy:
    default_tier: library      # AKB content starts as Tier 1 library-equivalent
    auto_promote: false        # promotion to higher tier requires operator consent
  refresh:
    on_commit: true            # reindex when the repo's git HEAD advances
    interval_secs: 3600        # periodic rescan
    incremental: true          # only reindex changed files
---
```

Three roles are recognized; exactly one applies:

| `repo_role` | AKB ingest | Agent worktrees | External push | Example |
| :--- | :--- | :--- | :--- | :--- |
| `source` | no | yes | gated by `agent_scopes.push_external` | `repo:symbiotic-runtime` (the Rust code) |
| `docs_akb` | yes | no (read-only) | no (writes via T128 only) | `repo:symbiotic-docs` (this repo's docs after self-bootstrap) |
| `reference_library` | yes | no | no (external third-party) | `repo:axum-docs`, `repo:stripe-api-docs` |

`source` repos *can* have an AKB attached in addition — in that case the source repo manifest declares its own role as `source`, and a second repo manifest with `repo_role: docs_akb` points at the same URL with a narrower `root_paths` scope. This keeps the model simple: one repo manifest, one role, one responsibility.

## Storage Layout

**Source (authoritative)**: repo content, managed per T126. Edited as files, git-tracked.

**Derived (cache)**: daemon-owned directory tree under `data/akb/{repo_id}/`. Atomic rebuild uses a POSIX rename-into-place on `data/akb/{repo_id}.new/` → `data/akb/{repo_id}/`; a `.lock` file is held during the rebuild. In-flight `recall.query` callers see the old index until the rename completes, then the new index — no torn reads. Same pattern `sqlite-vec` already uses against the main vault.

```
data/akb/
  repo-symbiotic-docs/
    index/
      fts5.db                  # SQLite FTS5 full-text index
      vectors.db               # sqlite-vec embeddings
      somatic.db               # SomaticStore instance for this AKB
    entities/
      {claim-id}.md            # atomic-claim records distilled from repo prose
      {decision-id}.md         # decision records
    manifest.toml              # current index version, last-rebuilt, content hashes per file
    .lock                      # daemon-held while rebuild in progress
```

Rebuild is atomic: new index is staged to `data/akb/{repo_id}.new/`, swapped in place on success. Partial rebuild failures restore the old index. No half-indexed AKB is ever queryable.

## Tier Structure Within an AKB

The sovereign vault's four-tier model (`archive` / `library` / `ledger` / `identity+operations+threads`) is too rich for a docs repo. AKBs use a compressed two-tier model:

| Tier | Content | Source Path Pattern |
| :--- | :--- | :--- |
| **Raw** | canonical prose as authored in the repo | files matching `indexing.root_paths` |
| **Distilled** | atomic-claim records + decision records extracted by Distillery Reduce/Reflect | generated under `data/akb/{repo_id}/entities/` |

There is no `identity` tier (an AKB does not carry persona). There is no `threads` tier (conversations happen in the sovereign vault, not in an AKB — AKBs are referenced *from* threads). Promotion from Raw to Distilled happens on ingest; no further tier promotion exists within an AKB.

`AkbTier` (`Raw | Distilled`) is a disjoint type from the vault's tier taxonomy (`archive | library | ledger | identity | operations | threads`). No mapping between them exists or is needed — a recall hit is labeled with its source scope (Sovereign vs. Akb) and its tier within that scope; cross-scope ranking uses the rules in §Federated Recall, not a unified tier lattice.

## Indexing Pipeline

The existing Distillery runs per-root with `kb_root = data/akb/{repo_id}/` and `repo_content_root = <repo worktree>/`. AKB applies the full canonical pipeline (Reduce → Reflect → Reweave → Verify, per `distillery-pipeline.md`) with **Reweave DISABLED** and **Conflict-Detection SKIPPED**. The table below enumerates AKB behavior per canonical stage AND per sub-stage (Dedup, Classify, PII are sub-stages in the canonical spec, not top-level stages):

| Stage | AKB behavior |
| :--- | :--- |
| Dedup | content-hash per source file; skip unchanged files on incremental rebuild |
| Reduce | extract atomic claims from repo prose with `impact_score` |
| Classify | skipped — all AKB claims default to "knowledge" (no self/methodology split) |
| Reflect | build graph context over both the AKB's own entities *and* cross-reference against linked repos' AKBs |
| Verify | deterministic + optional semantic verify against source file |
| Conflict detection | skipped — AKB is derived, so conflicts with source are irrelevant |
| Reweave | **disabled** — AKB never modifies the source repo. Doc drift reconciliation goes through T128 (external-repo distillery) which writes back to the source via a verified patchset. |
| PII Post-Check | applied to distilled entity records before they're indexed |
| Archive | distilled entities written to `data/akb/{repo_id}/entities/` |

The "reweave is disabled" rule is load-bearing: an AKB that rewrote source prose would violate the source-of-truth invariant. Agents that want to *change* documentation do so via a T128 drift-reconciliation run that produces a reviewable patchset.

## Federated Recall

The Recall Gateway gains a `scope` parameter. (Implementation chunk 05 will cite the existing gateway dispatch trait where this enum is wired; the extension point is a single dispatch function that maps `RecallScope` to the correct index backend — sovereign vault vs. AKB per-repo.)

```rust
pub enum RecallScope {
    Sovereign { vault: VaultRef },           // single sovereign vault
    Akb { repo: RepoId },                    // single AKB
    Project { project_id: ProjectId },       // all AKBs attached to a project + optionally the sovereign vault for cross-cutting identity matches
    Federated { includes: Vec<RecallScope> },// explicit multi-source
}
```

Default scope resolution rules:

- Agent spawned inside a goal whose process-manifest declares `project_id: X` defaults to `Project { X }`.
- Agent spawned outside a project context defaults to `Sovereign { main }` (the personal vault).
- Operator-issued manual recalls can override with an explicit `scope:` argument.
- Cross-scope results are merged with source provenance in the returned snippets (`[source: repo:symbiotic-docs] …snippet…`).

### Ranking (MVP precedence)

Cross-scope merge ranking for MVP uses the precedence:

```
scope-proximity  >  tier  >  recency
```

**Scope-proximity computation.** Hits are bucketed into three proximity classes, not two. The sovereign vault is not automatically "distant" just because it isn't an AKB — vault entries can carry project association, and when they do they count as project-proximate:

- **project-proximate** (highest) — hits where either:
  - the hit's source is an AKB attached to the currently-scoped project, OR
  - the hit's source is the sovereign vault AND the vault entry carries `project_id: {current}` or lists the current project in a `related_projects: [...]` tag. Vault entries may legitimately describe a project (operator's personal notes on the project's architecture, decisions about it, threads scoped to it); those hits are scope-proximate to work in that project even though they physically live in the personal vault.
- **project-neutral** (middle) — hits from the sovereign vault with no project association, OR from AKBs that are not attached to the currently-scoped project.
- **project-distant** (lowest) — hits from reference libraries (third-party corpora like `repo:axum-docs`) unless the current project explicitly includes them in its `Federated { includes: [...] }` scope, in which case they promote to project-neutral.

Within a proximity class, tier breaks ties (sovereign `ledger` > sovereign `library` > AKB `distilled` > AKB `raw`). Within a tier, recency breaks ties.

**Tagging mechanism for vault entries.** Vault entries gain an optional frontmatter field: `related_projects: ["project:flux", "project:smartoffice"]`. Entries without this field are treated as project-neutral. No migration is required — the absence of the tag is semantically meaningful ("this entry is not project-specific"). Setting the tag is a normal vault edit the operator or an agent can make.

Learning-to-rank (weighted combinations rather than strict precedence) is post-MVP.

## Integration with Existing Components

### With `docs/design/repo-manifest.md` (T126)

T126 gains the `repo_role` discriminator and the `indexing` block shown above. When `repo_role != source`, the daemon triggers an AKB rebuild on attach and on every subsequent commit to the repo. The `AccessBroker` enforces that `docs_akb` repos cannot spawn agent worktrees and cannot be targets of `push_external`.

### With `docs/design/project-bootstrap-process.md` (T127)

Phase 3 (Vault Ingestion) is extended: if the onboarded project has any attached repos with `repo_role: docs_akb` or `reference_library`, the ingestion atomic-commit *also* triggers an initial AKB build. The handshake thread in Phase 4 announces the AKB as a retrieval surface: `"docs-kb repo:flux-docs online — ask questions about the repo's architecture in this thread."`

For option (a) from the scaffolding-retirement discussion, Symbiotic's self-bootstrap attaches `repo:symbiotic` with `repo_role: docs_akb` (narrowed to `root_paths: ["docs/", "README.md", "AGENTS.md", "CLAUDE.md"]`), making this very slice's design docs retrievable by agents working on Symbiotic itself.

### With `docs/design/source-archeology.md` (T128)

T128's output patchsets land as commits on the source repo's agent branch. When those commits touch files inside an AKB's `indexing.root_paths`, the AKB indexer is triggered to reindex the affected files. The loop closes: T128 updates source docs → AKB reindexes → agents retrieving from the AKB see the updated prose on the next recall.

### With `docs/design/vault-as-truth.md` and `docs/design/sovereign-sync.md`

AKB derived data (under `data/akb/...`) is **not** part of the sovereign vault and **not** sovereign-sync'd. The source repo is sync'd per its own T126 manifest (its own remote, its own policy). On a fresh machine, the daemon rebuilds the AKB from the cloned repo content — no index transfer needed. This is the same relationship `sqlite-vec` has to the main vault today.

### With `docs/design/archive-policy-scope-hierarchy.md`

An AKB inherits its sensitivity floor from the source repo's declared sensitivity. A `docs_akb` over an open-source project defaults to `shareable`; one over a private company wiki defaults to `restricted`. The AccessBroker applies this floor at recall time — a lower-trust agent cannot retrieve from a higher-sensitivity AKB.

### With `docs/design/operator-reasoning-distillation.md`

The canonical `operator-protocol.md` from the operator-reasoning-distillation pipeline lives in the sovereign vault under `operations/skills/operator-protocol/`. It is *not* an AKB — it's authoritative methodology, not derived context. AKBs are a read-side convenience; `operator-protocol.md` is write-side truth for how agents work.

## Sandbox Boundary

AKB retrieval happens inside the daemon, not inside agent sandboxes. The agent issues a `recall.query` JSON-RPC call through the existing gateway; the daemon performs FTS5 + vector queries and returns snippets. The sandbox never touches `data/akb/` directly. This preserves the Nuclear Orchestrator invariant (`sandbox-transition-plan.md` §2 Invariant 1).

AKB indexing (rebuild) runs inside a **Distillery Sandbox** (the existing air-lock per `sandbox-transition-plan.md` §3), not on the host directly. The sandbox gets read-only access to the repo worktree + write access to a staging directory. After verification, the daemon atomically moves the staged index into place. No binary execution, no host compromise path.

## Lifecycle

| Event | AKB action |
| :--- | :--- |
| Repo manifest with `repo_role: docs_akb` attached to a project | Clone-if-needed; run initial indexing; emit `akb_created` event |
| Commit lands on repo's default branch | Triggered by the `repo_mirror_pull_completed` lifecycle event from T126 (AKB indexer is a pure consumer of T126 events — no post-receive hook, no FS-watch, no polling at the AKB layer). Incremental reindex on changed files matching `root_paths`; emit `akb_reindexed` |
| Repo manifest `state: paused` | AKB remains queryable; no new reindexing |
| Repo manifest `state: detached` | AKB index archived to read-only; eventually garbage-collected per retention policy |
| Corruption detected (hash mismatch in `manifest.toml`) | Full rebuild from source; emit `akb_corruption_recovered` |

No AKB ever has a `detached-and-forgotten` state that leaves queryable data behind. Garbage collection is explicit and logged.

## MVP Scope

**In MVP (T129 must land alongside T126/T127/T128)**:

- `repo_role` discriminator + `indexing` block on `RepoManifest`
- Daemon-managed AKB rebuild on repo attach and on commit
- Two-tier storage (Raw + Distilled)
- Distillery per-root invocation with reweave disabled
- Federated Recall with four scope variants (Sovereign / Akb / Project / Federated)
- Default scope resolution by goal's `project_id`
- Sandbox boundary enforcement (daemon-side retrieval, Distillery-Sandbox indexing)
- Symbiotic's own docs as the first AKB instance (`repo:symbiotic` with `docs_akb` role) — exercised end-to-end by T127 self-bootstrap

**Post-MVP**:

- Advanced tier structure (beyond Raw + Distilled)
- Learning-to-rank retrieval
- Cross-machine AKB federation
- AKB-to-AKB cross-referencing in Reflect stage (referenced in pipeline spec above but behind a feature flag)
- Incremental rebuild optimizations (merkle-tree content tracking)
- Reference library auto-refresh from upstream documentation sites

## Security Notes

- AKBs inherit credential isolation from T126: a `docs_akb` repo's credential (if any) is held by the daemon, never crossed into sandboxes. Reindex operations run with `file.read` scope to the worktree, `file.write` scope only to the staging directory.
- No internet access during reindex. The Distillery Sandbox profile denies `network.egress` for AKB operations. LLM calls for Reduce/Reflect go through the Nucleus's LlmGateway per the existing proxy-enrichment pattern — local Ollama by default.
- Sensitivity floor is hard-enforced at recall time. An agent with trust ceiling `restricted` recalling against an AKB floored at `private` receives zero results (not filtered snippets — a clean deny).
- PII post-check on distilled entity records prevents credential-looking strings from leaking into the index. This reuses `RedactionEngine` from the existing Distillery.

## Open Questions

- **Reference library refresh cadence.** For an AKB mirroring an upstream docs site (e.g., Axum, Stripe), how is upstream update detected? Polling upstream HEAD is straightforward for git-backed sources; HTML-scraped sources need a different primitive. **Resolved: MVP is git-backed only.** HTML-scraped deferred post-MVP.
- **Embeddings provider**. Local Ollama embedding model (`nomic-embed-text` per `llm-audit-trail.md`) is the default. Does AKB allow per-repo override (e.g., a code-specific embedding model for a source-adjacent AKB)? Defer to post-MVP.
- **Cross-AKB linking**. When a `docs_akb` references a decision that's also in the sovereign vault's `operator-protocol.md`, how is the link surfaced in recall results? Probably a `related:` field on the returned snippet. Worth prototyping in MVP if it's a one-day add; otherwise defer.
- **Retrieval auditing**. Does every recall against an AKB land in T120's audit trail, or only LLM-mediated recalls? **Resolved: deferred** alongside the five composition questions (T120 audit-level default, retention alignment, `kind` tag, PE goal-type bucket, RedactionEngine tuning) in T128/T129 `01-design-review.md`. All audit-surface tuning lands in one dated follow-up task after T126 implementation.
- **Federated merge ranking precedence** (scope-proximity / tier / recency) — **Resolved: scope-proximity > tier > recency**, with scope-proximity computed over THREE buckets (project-proximate / project-neutral / project-distant) rather than two. Vault entries tagged with `related_projects: [...]` including the current project are elevated to project-proximate — the personal vault is not automatically distant just because it isn't an AKB. See §Federated Recall §Ranking for the full mechanism.
- **Lifecycle events route**: `akb_created` / `akb_reindexed` / `akb_corruption_recovered` extend T126's lifecycle event list rather than running as a sibling stream. Single Archive event stream keeps consumers simple. Applied in `docs/design/repo-manifest.md` §Lifecycle Events.
