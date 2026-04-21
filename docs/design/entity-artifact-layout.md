# Entity Artifact Layout

> **Task:** T111 Vault as Truth
> **Depends On:** `docs/design/vault-as-truth.md`, `docs/design/vault-organization.md`, `docs/design/entity-type-and-schema-kind.md`

## Problem

Symbiotic currently carries historical naming drift around entities:

- older memory design treated `knowledge-base/entities/` as the canonical entity graph
- newer Vault-as-Truth design made `ledger/` the canonical structured record layer
- generated entity brief docs later reused `knowledge-base/entities/...` as a product read surface

That leaves the repo with two competing meanings of `entities`:

1. canonical entity truth
2. generated entity read artifacts

This is understandable historically, but it is not a good end-state. It makes the truth boundary harder to read in the filesystem, in Obsidian, and in implementation code.

## Decision

Adopt a **per-entity artifact folder** under `ledger/`:

```text
knowledge-base/
  ledger/
    people/
      alice/
        alice.md
        alice.brief.md
    projects/
      symbiotic/
        symbiotic.md
        symbiotic.brief.md
```

Rules:

- `{slug}.md` is the **canonical truth record**
- `{slug}.brief.md` is the **generated distilled read surface**
- future sibling artifacts are allowed, but only with explicit suffixes

Within `{slug}.md`, current truth should appear first and semantic history should
stay in the same record under a final `## History` section. Symbiotic should not
create sibling history files for normal entity evolution; Git remains the exact
revision layer underneath.

This replaces the current conceptual split of:

```text
ledger/            canonical truth
entities/          generated entity briefs
```

with a clearer colocation model:

```text
ledger/{type}/{slug}/
  {slug}.md
  {slug}.brief.md
```

## Why This Layout

### 1. Clear truth boundary

The canonical record and its derived artifacts live together, but their roles are explicit in the filename.

### 2. Obsidian-friendly names

Role-only names like `canonical.md` / `profile.md` would create thousands of duplicate filenames in Obsidian. Using slug-prefixed names keeps files recognizable in basename-first UIs.

### 3. Better than top-level `entities/`

The current generated brief location overloads the historical meaning of `entities/`. Colocation under `ledger/` removes that ambiguity.

### 4. Extensible artifact model

The entity folder can later hold more specialized artifacts without losing clarity:

```text
alice.md
alice.brief.md
alice.timeline.md
alice.activity.md
```

The canonical truth file remains obvious.

## Type Authority

The filesystem layout does **not** define entity types by itself. It reflects a
runtime-owned core type taxonomy.

Current authority:

- runtime entity taxonomy lives in `submodules/runtime/crates/symbiotic-memory/src/types.rs`
- canonical Vault frontmatter uses the `type` field with those canonical values
- the parser normalizes frontmatter strings to the runtime type enum in
  `submodules/runtime/crates/symbiotic-memory/src/vault_parser.rs`

That means:

- folder names under `ledger/` are a projection of the entity type taxonomy
- file placement must not become a second competing source of truth
- the indexer/parser should continue to trust the canonical frontmatter `type`
  and runtime type mapping, not infer semantics only from paths

### Current canonical entity types

The current closed taxonomy is:

- `person`
- `project`
- `organization` (normalized storage/documentation form; current runtime enum
  variant is `Org`)
- `tool`
- `preference`
- `concept`
- `task`

Filesystem folders should use the pluralized, human-readable form of that
taxonomy:

```text
person        -> people/
project       -> projects/
organization  -> organizations/
tool          -> tools/
preference    -> preferences/
concept       -> concepts/
task          -> tasks/
```

### Design rule

If Symbiotic later grows richer typed document behavior, that higher layer still
becomes the authority and the filesystem layout mirrors it. The filesystem must
never become an independent typing system.

Custom structure should be layered via `SchemaKind`, not by turning the path
layout into a second ontology. See `docs/design/entity-type-and-schema-kind.md`.

### SmartOffice-Inspired Boundary

The relevant lesson from SmartOffice is structural, not literal:

- the runtime owns the generic typing framework
- storage paths mirror that framework
- specialized fields and validators belong in a higher schema layer

Symbiotic keeps a stronger closed core ontology than SmartOffice, but the same
boundary still applies:

- `EntityType` owns the core memory semantics
- `SchemaKind` will own optional specialization and validation
- `ledger/{type}/{slug}/...` mirrors that runtime-owned contract

## Artifact Taxonomy

### Required

- `{slug}.md`
  - canonical truth
  - directly editable
  - indexed as source of truth

### Allowed generated / auxiliary artifacts

- `{slug}.brief.md`
  - generated distilled summary/read surface
- future explicit suffixes only when justified:
  - `.timeline.md`
  - `.activity.md`
  - `.decisions.md`
  - `.refs.md`

### Forbidden patterns

- `canonical.md`
- `profile.md`
- role-only filenames without the entity slug
- generated files that are indistinguishable from canonical truth files

## Indexer and Watcher Rules

The filesystem layout only works if the implementation enforces a strict boundary:

- only `{slug}.md` is parsed as canonical truth by default
- `*.brief.md` and other suffixed artifacts are **not** canonical input to the Vault indexer
- generated artifacts may be watched for sync/export purposes, but not re-ingested as truth
- app edit paths must always target the canonical file, never the `.brief.md` artifact

## Product Surface Mapping

Under this model:

- the app’s current entity brief sheet should map to `{slug}.brief.md`
- direct edits (`vault.edit`) still mutate `{slug}.md`
- daemon follow-through regenerates `{slug}.brief.md`
- sync notifications should continue to refer to the generated artifact hash, not the canonical file hash

## Migration Direction

Current conceptual model:

```text
ledger/...                 canonical entity notes
entities/...               generated entity briefs
```

Target model:

```text
ledger/{type}/{slug}/{slug}.md
ledger/{type}/{slug}/{slug}.brief.md
```

Migration should include:

1. generator output path change
2. HTTP/API path resolution update
3. indexer canonical-file matching rule
4. watcher exclusions for suffixed generated artifacts
5. app sync path unchanged at the API level where possible
6. doc and task canon cleanup removing the overloaded top-level `entities/` meaning

## Historical Reasoning

This decision is not a fresh invention. It resolves an accumulated mismatch:

- early memory reviews and Task 49 used `entities/` to mean canonical entity graph
- Vault-organization and Vault-as-Truth later moved canonical structured records to `ledger/`
- generated entity briefs then reused `entities/`, creating the current ambiguity

This layout keeps the newer `ledger = canonical records` model while preserving a clean user-facing artifact surface.

## Non-Goals

- This does **not** change the rule that Git is the full revision history layer.
- This does **not** mean every document gets multiple sibling artifacts.
- This does **not** make `.brief.md` files editable truth.
- This does **not** introduce generic “history files” beside all notes; semantic
  history belongs in the canonical `{slug}.md` record under `## History`.

## Recommended Adoption Order

1. Approve this layout in T111.
2. Update the broader vault-organization and vault-as-truth docs to reference it.
3. Implement the path migration in runtime/app.
4. Remove the old top-level generated `entities/` location once the new path is live.
