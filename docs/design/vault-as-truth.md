# Vault as Truth — Knowledge Storage Architecture

> **Task:** Consolidates and resolves conflicts across `memory-store.md`, `memory-system.md`, `memory-system-simplification.md`, `distillery.md`
> **Supersedes:** Storage sections of `memory-store.md` (SQLite-as-canonical model)
> **Aligns with:** `VISION.md` §35, `CONTEXT.md` §19, `prd-symbiotic-brain.md` §129
> **Related:** `docs/design/entity-artifact-layout.md`, `docs/design/entity-type-and-schema-kind.md`

> Historical note: T111 keeps the older "Vault as Truth" task name, but current naming canon reserves `Vault` for credentials. In the memory system, `knowledge-base/` is the Archive-authored source of truth. This document specifies canonical memory storage, not secret storage.

---

## Problem Statement

Three authoritative documents already declare Markdown as the source of truth:

- **VISION.md**: "Plain Markdown files for human ownership, overlaid with a local SQLite Graph Index"
- **CONTEXT.md**: "stored as plain Markdown in an Obsidian vault, indexed by a temporal/emotional Graph DB"
- **prd-symbiotic-brain.md**: "The Markdown files are the source of truth; the graph is a derived index that can be rebuilt."

But `memory-store.md` and the implementation treat SQLite as the canonical store for entities, facts, and relationships — with no reference to Markdown origin. The Reweave stage destructively overwrites Markdown files without versioning. Soft-deletes live only in SQLite.

This document resolves the divergence by specifying **how** Markdown is the truth, what SQLite indexes, and how history is preserved.

---

## The Three-Layer Model

```
┌──────────────────────────────────────────────────────────┐
│  ARCHIVE (Markdown) — CANONICAL MEMORY TRUTH             │
│                                                          │
knowledge-base/
├── archive/           ← raw intake content (receipts)
├── library/           ← external reference docs
├── ledger/            ← canonical structured records + colocated derived briefs
├── threads/           ← generated thread memory docs
├── identity/          ← identity, preferences, calibration
└── operations/        ← goals, skills, handoffs, reports
│                                                          │
│  Human-readable. Obsidian-native. User-owned forever.    │
└───────────┬──────────────────────┬───────────────────────┘
            │ index                │ commit
            ▼                      ▼
┌─────────────────────┐   ┌────────────────────────────────┐
│  SQLite (search)    │   │  Git (history)                 │
│                     │   │                                │
│  FTS5 keywords      │   │  Per-file diffs (delta comp.)  │
│  vec0 embeddings    │   │  Blame (who wrote each fact)   │
│  Graph edges        │   │  Time travel (any version)     │
│  Temporal decay     │   │  Semantic commit messages      │
│  Sensitivity filter │   │                                │
│                     │   │  + SQLite metadata cache        │
│  DERIVED.           │   │    for fast history queries    │
│  DELETABLE.         │   │                                │
│  REBUILDABLE.       │   │  APPEND-ONLY.                  │
└─────────────────────┘   └────────────────────────────────┘
```

### Layer Rules

1. **Archive (Markdown)** — the only place facts are created, modified, or archived. If a fact isn't in a `.md` file, it doesn't exist.
2. **SQLite** — a derived search index. Deletable. Rebuildable by re-scanning the vault. Stores zero unique data.
3. **Git** — history storage. Every agent write is committed with a semantic message. `git log`, `blame`, and `show` provide full version history per file.

### The Rebuild Guarantee

At any time, the system can:
```
rm data/memory.db && symbiotic reindex
```
This scans all vault `.md` files, parses frontmatter + facts + relationships, and rebuilds the full SQLite index (entities, memories, FTS5, vec0 embeddings, graph edges). Nothing is lost.

---

## Identity, Readability, and Backups

To ensure the Vault functions flawlessly for both humans (in Obsidian) and machines (Vector DB / LLMs), we enforce three strict architectural rules regarding identity, formatting, and data sovereignty.

### 1. Identifying Every Piece of Information Uniquely
We require absolute precision when linking facts, files, and conversations. We use existing, standard URI formats to achieve this without inventing proprietary syntax.
*   **Files (Semantic Paths)**: Every file is identified by its semantic path relative to the vault root: `skills/frontend/react.md`.
*   **Atomic Facts (Obsidian Block References)**: When the Distillery extracts a specific decision or fact, it appends a standard Obsidian block reference to the end of the line: `^decision-react-01`.
*   **Conversations (Matrix URIs)**: Causality is tracked using exact Matrix event URIs: `matrix://room/!room_id/event/$event_id`.
*   **The Link**: If an agent or a document needs to cite a decision, it uses standard Markdown/Obsidian linking: `[[skills/frontend/react#^decision-react-01]]`. This is fully readable by humans, native to Obsidian, and precisely parsable by the Vector DB.

### 2. Readability in Obsidian (The Golden Rule of Formatting)
Because we inject dense YAML metadata (`provenance`, `derived_from`, `entity_type`) into the frontmatter, there is a risk of making the Markdown files ugly for human readers. 
*   **The Golden Rule**: *Metadata stays in YAML. Prose stays in the body.*
*   **Obsidian Integration**: Obsidian natively parses YAML frontmatter and hides it behind a clean, collapsible "Properties" UI. 
*   **Agent Instructions**: Agents are strictly instructed to write the *body* of the Markdown file as fluid, human-readable prose (like a Wikipedia article). We never pollute the body text with JSON blobs or raw metadata. The result is a Vault that reads like a beautifully formatted wiki to the user, while hiding an intense vector/metadata engine underneath.

### 3. Backups of the Whole System (Sovereign Sync)
Because of the "Nuclear Orchestrator" and "Vault as Truth" pivots, backing up the entire Symbiotic system is shockingly simple. We do not need to backup 5 different databases.
*   **What is Ephemeral?**
    *   The `sqlite-vec` database (Derived index, rebuilt from Markdown).
    *   The Sysbox Agent Swarms (Disposable containers).
    *   The Internal Git Swarm Server (Deleted when a goal finishes).
*   **What is Persistent?**
    *   The Vault (`knowledge-base/` and `data/vault/vault.db`).
*   **The Backup Mechanism**: The Vault is a Git repository. A background task periodically commits and pushes this repository to a private remote (e.g., a private GitHub repo or home NAS). Because the `vault.db` (which contains your Matrix E2EE encryption keys) lives inside this synced folder, **your entire identity, chat history access, and memory are snapshotted atomically**. If your host VPS is destroyed, you simply `git clone` the Vault on a new machine, and the Nucleus cold-boots to the exact state of the last commit.

This applies to operational planning state too:

- canonical goals live under `knowledge-base/operations/goals/{goal}/plan.md`
- canonical planned tasks live under `knowledge-base/operations/goals/{goal}/tasks/*.md`
- append-only plan and lifecycle events live under `knowledge-base/operations/goals/{goal}/events/*.md`
  and must carry structured frontmatter for affected task IDs / status changes /
  lineage edges in addition to human-readable body text
- runtime work items, leases, and branch ownership remain projections that can
  be reconstructed from Vault truth plus live runtime recovery

Each canonical entity record lives under `knowledge-base/ledger/{type}/{slug}/{slug}.md`.
Generated read artifacts may live beside it using explicit suffixes such as
`{slug}.brief.md`. The canonical file is both human-readable in Obsidian and
machine-parseable by the indexer.

Generated artifacts such as Thread Memory Docs and `{slug}.brief.md` are query surfaces, not truth. They are regenerated from canonical records, excluded from canonical indexing, and never edited directly.

The folder path mirrors the runtime entity taxonomy; it does not replace it.
Entity type authority remains in the runtime memory types and the canonical
frontmatter `type` field. The path is a storage convention, not a second schema.

Symbiotic keeps a fixed core `EntityType` ontology for memory semantics.
Richer user-defined structure belongs in a higher `SchemaKind` layer, not in a
fully dynamic top-level type system. That means:

- `type:` remains the stable core ontology
- `schema:` may later specialize the record without changing its core storage folder
- the filesystem mirrors `EntityType`, not `SchemaKind`

This preserves a stable retrieval and graph model while still leaving room for
future user-defined structured records. See
`docs/design/entity-type-and-schema-kind.md`.

### Example: `knowledge-base/ledger/concepts/kubernetes/kubernetes.md`

```markdown
---
id: kubernetes
type: concept
schema: infrastructure_platform
aliases: [k8s, kube]
space: knowledge
sensitivity: shareable
created: 2026-01-15T00:00:00Z
updated: 2026-03-23T14:30:00Z
---

# Kubernetes

## Facts
- Preferred orchestration platform for production [source: chat-2026-01-15] [type: decision] [confidence: 0.9]
- Supports auto-scaling via HPA and VPA [source: article-abc123] [type: finding] [confidence: 0.95]
- Monthly cost: $2400 for current workload [source: chat-2026-03-23] [type: finding] [confidence: 0.85]

## Relationships
- replaced_by: [[nomad]] [since: 2026-03-23]
- used_with: [[docker]], [[helm]]
- part_of: [[infrastructure]]

## History
### Archived Facts
- ~~Monthly cost: $2400 for current workload~~ [source: chat-2026-03-23] [type: finding] [archived: 2026-03-24, reason: superseded by refreshed infrastructure estimate, commit: abc1234]

### Relationship Changes
- removed: used_with -> [[helm]] [changed: 2026-03-24, reason: chart tooling retired, commit: def5678]
- replaced: replaced_by -> [[nomad]] => [[kubernetes]] [changed: 2026-03-25, reason: migration reversed after review, commit: 9876fed]
```

### Format Specification

**Frontmatter (YAML):**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | string | yes | Unique entity identifier (slug) |
| `type` | enum | yes | `person`, `project`, `concept`, `tool`, `task`, `organization` |
| `aliases` | string[] | no | Alternative names for entity matching |
| `schema` | string | no | Optional higher-level specialization layered on top of the fixed core `type` |
| `space` | enum | yes | `knowledge`, `identity`, `operations` |
| `sensitivity` | enum | no | `shareable`, `restricted`, `private` (default: `private`) |
| `created` | ISO 8601 | yes | When this entity was first created |
| `updated` | ISO 8601 | yes | Last modification timestamp (auto-updated by agent) |

**Facts section (`## Facts`):**

Each active fact is a Markdown list item with inline metadata in square brackets under `## Facts`. When a fact is no longer true, it leaves current truth and becomes semantic history. Git still preserves the full file history, but the Markdown itself should carry enough history for normal inspection without opening `git log`.

```
- {fact text} [source: {source-id}] [type: {fact-type}] [confidence: {0.0-1.0}]
```

| Metadata | Required | Values |
|----------|----------|--------|
| `source` | yes | Archive entry ID, chat session ID, or `manual` |
| `type` | no | `decision`, `finding`, `preference`, `entity`, `episode`, `methodology` |
| `confidence` | no | 0.0 to 1.0 (default: 0.8) |

**History and Archival:**

The Vault represents current truth plus visible semantic history.
- **Current truth** lives in `## Facts` and `## Relationships`.
- **Semantic history** lives in a final `## History` section so the active record stays readable first.
- **Archived / superseded facts** should appear under `## History` → `### Archived Facts` using strikethrough plus archival metadata.
- **Relationship mutations** should appear under `## History` → `### Relationship Changes` as explicit `removed:` / `replaced:` records instead of vanishing silently.
- **Provenance** is dual: the Markdown carries semantic change summaries, while Git commit messages and diffs still capture exact causal and textual history (for example `memory(k8s): archive cost assumption`).
- **Reconstruction** may use both the Markdown file and Git history, but the file itself must already tell the truth about what is current and what has been superseded.

Archived fact format:

```markdown
- ~~Old fact~~ [source: chat-2026-03-23] [type: decision] [archived: 2026-03-23, reason: contradicted by cost analysis, commit: abc1234]
```

**Relationships section (`## Relationships`):**

Typed directed edges using Obsidian `[[wikilinks]]`:

```
- {relationship_type}: [[{target-entity}]] [since: {date}]
```

Multiple targets on one line are comma-separated: `- used_with: [[docker]], [[helm]]`

Relationship history format:

```markdown
## History
### Relationship Changes
- removed: used_with -> [[helm]] [changed: 2026-03-24, reason: chart tooling retired, commit: def5678]
- replaced: replaced_by -> [[nomad]] => [[kubernetes]] [changed: 2026-03-25, reason: migration reversed after review, commit: 9876fed]
```

### Fact IDs

Facts don't have explicit IDs in the Markdown. The indexer generates deterministic IDs from `{entity_id}:{sha256(fact_text)[:12]}`. This is content-addressable — if the fact text changes, it's a new fact (the old one should move into `## History` as an archived fact).

---

## The Indexer

The vault indexer scans Markdown files and populates the SQLite search index. It runs:
- **On daemon startup** — full rebuild
- **On filesystem change** — incremental update (hash comparison)
- **On demand** — `symbiotic reindex` CLI command

### Indexer Pipeline

```
For each canonical `.md` file in `knowledge-base/{ledger,identity,operations}/`:
  1. Parse YAML frontmatter → entity record
  2. Parse ## Facts → active memories (status: active)
  3. Parse ## Relationships → active graph edges
  4. Parse ## History → archived memories + relationship change metadata
  5. For each fact:
     a. Generate fact ID: {entity_id}:{hash(text)}
     b. Generate embedding via provider → upsert to vec0
     c. Index text → FTS5
  6. Compute file content hash → store in index metadata
```

Generated artifacts such as `*.brief.md` are excluded from canonical indexing.
They may be synced and rendered by product surfaces, but they are not re-ingested
as truth.

### Incremental Updates

The indexer maintains a metadata table:

```sql
CREATE TABLE vault_file_index (
    file_path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    last_indexed INTEGER NOT NULL
);
```

On filesystem change:
1. Compute current file hash
2. Compare with `vault_file_index.content_hash`
3. If changed: re-parse the file, update all related SQLite rows
4. If unchanged: skip

## Editing Philosophy: Surgical vs. Full

To ensure high data integrity and minimize Git merge conflicts across distributed devices, Symbiotic follows a **Surgical-First** editing model.

### 1. Surgical Edits (Default)
Most operations (Intake, Reweave, Chat Updates) use surgical edits where the host's `VaultWriter` modifies specific lines or sections.
- **Git Friendly**: Localized changes allow Git to auto-merge simultaneous edits from different devices/actors.
- **Immutable Structure**: The AI cannot "hallucinate" a new file format because it never sees the full file write-path; it only issues structured commands (Add/Archive/Link).
- **Clear Provenance**: `git blame` accurately identifies the specific session/author for every individual fact.

### 2. Full Rewrites (On-Demand)
Full document rewrites are permitted **only** in specific scenarios:
- **Refactoring**: Explicit user command to "refactor" or "cleanup" an entity.
- **Summarization/Simplification**: When a note grows too large or noisy.
- **Initial Creation**: Drafting a completely new entity.
- **High-Volume Mutation**: If more than a certain number of facts (e.g., 5+) are being changed at once, the system may upgrade to a "Refactor" pass to ensure prose fluidity.

### 3. Validation Bridge
Regardless of the editing mode, the **VaultLinter** validates the final content before it is committed to disk. If the AI-generated "Full Rewrite" violates the YAML schema or section hierarchy, the write is rejected and the Agent must retry.

The current strict canonical contract is:
- top-level section order is `## Facts` → `## Relationships` → `## History`
- archived facts live only under `## History` → `### Archived Facts`
- relationship removals/replacements live only under `## History` → `### Relationship Changes`
- generated artifacts such as `{slug}.brief.md` are not canonical lint targets

The repo-level operator entrypoint for this validation is:

```bash
./scripts/lint-vault.sh
```

which wraps the runtime `symbiotic-linter` binary against the current
canonical ledger records under `knowledge-base/ledger/`.

---

## Write Paths

### Chat → Facts (Primary Path)

```
User: "K8s is too expensive, we're switching to Nomad"
  ↓
Agent processes intent:
  1. Archive old fact in ledger/concepts/kubernetes/kubernetes.md
  2. Add new fact in ledger/concepts/kubernetes/kubernetes.md
  3. Add relationship: kubernetes → nomad (replaced_by)
  4. Create ledger/concepts/nomad/nomad.md if it doesn't exist
  ↓
Agent edits Markdown files directly
  ↓
Git commit: "memory(kubernetes): archived cost assumption, added Nomad migration
  [source: chat-2026-03-23]"
  ↓
Indexer: re-indexes changed files → SQLite updated
```

### Shared Link / Intake

```
URL shared → Intake fetches content
  ↓
Raw content saved to knowledge-base/archive/{date}-{slug}.md (immutable receipt)
  ↓
Distillery extracts claims → identifies relevant entities
  ↓
Agent updates entity .md files with new facts (with [source: article-{id}])
  ↓
Git commit: "intake(article-abc): 3 findings for [rust-async, tokio]"
  ↓
Indexer: re-indexes
```

### Reweave (New Evidence Updates Existing Knowledge)

```
New evidence contradicts existing fact
  ↓
Agent reads entity .md file (found via SQLite index)
  ↓
Agent edits the Markdown:
  - Removes old fact from ## Facts
  - Adds new fact to ## Facts with evidence link
  ↓
Git commit: "reweave(entity-name): updated N facts from article-xyz"
  ↓
Indexer: re-indexes (old fact → removed from SQLite, new fact → active)
```

**Critical**: Reweave no longer calls an LLM to "rewrite the entire note." Instead, it makes **surgical edits** — archiving specific facts and adding new ones. The note structure is preserved; only individual facts change.

### Entity Brief Process Action (`vault.process`)

```text
User highlights text from the entity-detail surface
  ↓
App sends `vault.process { entity_id, text, source? }`
  ↓
Daemon runs an entity-targeted Distillery pass
  1. Reduce selected text into candidate claims
  2. Constrained reflect against the target canonical entity
  3. Surgical reweave only on that entity's canonical `{slug}.md`
  ↓
If durable mutations were produced:
  - semantic git commit
  - re-index changed files
  - regenerate sibling `{slug}.brief.md`
  - emit `entity_profile.updated`
  ↓
App refreshes the generated brief cache by content hash
```

Rules:

- `vault.process` targets exactly one canonical entity record
- it must not route through `thread.distillery`
- it must not mutate generated `.brief.md` artifacts directly
- it returns mutation-oriented result counts, not only aggregate Distillery report totals
- zero-mutation processing is allowed and should resolve as a successful no-op rather than a failed write

### User Edit in Obsidian (Edge Case)

```
User modifies entity .md in Obsidian
  ↓
Filesystem watcher detects change (content hash differs from vault_file_index)
  ↓
Indexer re-parses the file → SQLite updated
  ↓
If new facts detected (not in SQLite): flag for Distillery quality check
If facts removed: mark as archived in SQLite
```

---

## Read Paths

### Agent Recall

```
User: "What do I know about our infrastructure?"
  ↓
SQLite: FTS5 + vec0 hybrid search for "infrastructure"
  ↓
Returns: entity IDs, fact text, scores, relationships
  ↓
Agent reads full .md files for deeper context if needed
  ↓
Agent composes answer from scored facts
```

### History Query

```
User: "What did I originally think about K8s?"
  ↓
SQLite metadata: SELECT FROM vault_history WHERE file LIKE '%kubernetes%'
  ↓
Git: show the pre-archival version of kubernetes.md
  ↓
Agent: "On Jan 15, you said K8s was the preferred platform.
  On Mar 23, you changed your mind due to costs."
```

### Obsidian Browsing

User opens `knowledge-base/ledger/kubernetes.md` in Obsidian:
- Sees current active facts
- `[[wikilinks]]` navigate the entity graph
- Obsidian git plugin shows file history (how things changed)

---

## History Storage

### Git as Version Control

The agent creates semantic commits for every vault mutation:

```
memory(kubernetes): archived cost assumption, added Nomad migration
  source: chat-2026-03-23
  facts_added: ["Monthly cost: $2400"]
  facts_archived: ["Scales well for our use case"]
```

Git provides:
- **Per-file history**: `git log -- knowledge-base/ledger/concepts/kubernetes/kubernetes.md`
- **Blame**: `git blame` shows when each line was written and by whom
- **Time travel**: `git show {commit}:{path}` reconstructs any past version
- **Diff**: `git diff` shows exactly what changed between versions

### History Metadata Cache

For fast history queries without shelling out to git, SQLite caches commit metadata:

```sql
CREATE TABLE vault_history (
    file_path TEXT NOT NULL,
    commit_hash TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    author TEXT NOT NULL,        -- 'agent' or 'user'
    summary TEXT NOT NULL,       -- semantic commit message
    source TEXT,                 -- "chat-2026-03-23" or "intake:article-abc"
    facts_added TEXT,            -- JSON array
    facts_archived TEXT,         -- JSON array
    PRIMARY KEY (file_path, commit_hash)
);
CREATE INDEX idx_vh_file ON vault_history(file_path);
CREATE INDEX idx_vh_time ON vault_history(timestamp);
```

Rebuilt from `git log --format` if ever lost.

### History Query Performance

| Query | Method | Speed |
|-------|--------|-------|
| Last update time for entity | SQLite: `MAX(timestamp) WHERE file = ?` | <1ms |
| What changed this week | SQLite: `WHERE timestamp > ?` | <1ms |
| Full file diff | Git: `git show {hash}:{path}` | ~5ms |
| Entity history (100 revisions) | SQLite list + git show | ~50ms |
| Who authored this fact | Git: `git blame {path}` | ~50ms |
| Reconstruct state at date X | Git: `git show {commit}:{path}` | ~5ms |

---

## What This Changes

### Documents Updated

| Document | Change |
|----------|--------|
| `archived/memory-store-sqlite-legacy.md` | Keep only as historical SQLite-schema reference with an explicit "derived index, not canonical" note |
| `memory-system.md` | Layer 2 (Neural Graph) is a derived index of Layer 3 (Vault) |
| `memory-system-simplification.md` | Soft-deletes are handled by visible archival in Markdown plus Git history |
| `distillery.md` | Reweave makes surgical edits, not full-note rewrites; git commits |

### Implementation Required

| Component | What | Priority |
|-----------|------|----------|
| Entity file format parser | Parse YAML frontmatter + fact sections from `.md` | P0 |
| Vault indexer | Scan `.md` files → populate SQLite (entities, FTS5, vec0, graph) | P0 |
| Rebuild command | `symbiotic reindex` — full rebuild from vault | P0 |
| Agent write path | Agent edits `.md` files directly (surgical fact add/archive) | P0 |
| Git commit integration | Agent creates semantic commits after vault mutations | P1 |
| Filesystem watcher | Detect user edits in Obsidian → trigger re-index | P1 |
| History metadata cache | Index git commits per file in SQLite | P2 |
| Reweave migration | Change from full-note LLM rewrite to surgical fact edits | P1 |

---

## Migration Path

The current system has entities/facts in SQLite. Migration:

1. **Export**: scan SQLite `entities` + `memories` tables → generate `.md` files in `knowledge-base/ledger/`
2. **Verify**: compare generated `.md` content with SQLite data
3. **Switch**: point the indexer at the vault, stop treating SQLite as canonical
4. **Delete**: old `data/memory.db` becomes the derived index

This can be done incrementally — new entities go to Markdown immediately, existing ones are migrated in a batch.

---

## Design Decisions

### Why not SQLite as truth?

SQLite is excellent for queries but poor for:
- **Human readability**: binary format, needs tooling to inspect
- **Portability**: tied to a specific schema version
- **User ownership**: user can't open it in Obsidian, can't read it in any editor
- **Version control**: no built-in history, no blame, no diff

Markdown is the opposite: universally readable, portable, git-diffable, Obsidian-native.

### Why not both as co-equal sources?

Split truth causes divergence. When SQLite says "active" but the Markdown says "archived," which wins? Every consumer must handle conflicts. One source of truth eliminates this class of bugs entirely.

### Why git for history instead of SQLite versioning?

Git provides delta compression (space-efficient), per-line blame, and universal tooling. SQLite versioning (storing full text per version) is space-wasteful and requires custom UI. Git history is browsable in any git client, including Obsidian's git plugin.

### Why not CRDT or real-time sync?

Symbiotic is single-user. The agent is the primary writer. CRDTs solve multi-writer conflict resolution, which we don't need. Git's simple commit model is sufficient.
