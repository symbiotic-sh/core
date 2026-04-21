# EntityType and SchemaKind

> **Task:** T111 Vault as Truth
> **Depends On:** `docs/design/vault-as-truth.md`, `docs/design/entity-artifact-layout.md`

## Problem

Symbiotic needs two things at once:

1. a stable core memory ontology that the Distillery, Recall Gateway, graph
   maintenance, and UI can reliably understand
2. a way for users or future packages to define richer structured record shapes
   such as training plans, foods, protocols, or trackers without forking the
   entire memory model

If every installation can invent arbitrary top-level memory types, the core
memory system becomes weaker and less predictable. If everything is hardcoded,
the product becomes too rigid.

## Decision

Symbiotic uses a two-layer model:

- **EntityType** — fixed core ontology used by the memory engine
- **SchemaKind** — extensible structured specialization layered on top

This means the core engine keeps a closed set of cognitive categories, while
custom structure, validation, and specialized fields attach through schema kinds.

## 1. Core EntityType

`EntityType` is the built-in ontology that Symbiotic itself understands.

Current canonical types:

- `person`
- `project`
- `organization`
- `tool`
- `preference`
- `concept`
- `task`

These types are load-bearing because they shape:

- Distillery extraction prompts and normalization
- Recall Gateway ranking and balancing
- Neural Graph maintenance heuristics
- default UI labels and grouping
- default security and sensitivity posture

### Rule

`EntityType` remains a closed runtime-owned taxonomy unless an explicit
architecture decision says otherwise.

## 2. SchemaKind

`SchemaKind` is the extensibility layer.

A schema kind specializes a canonical entity record without replacing its core
memory semantics.

Examples:

- `type: concept` + `schema: food`
- `type: task` + `schema: training_program`
- `type: preference` + `schema: diet_preference`

This allows richer validation and product behavior without making the top-level
memory ontology fully dynamic.

## 3. Why Not a Fully Dynamic Top-Level Registry

A fully dynamic top-level type system would make core memory behavior less
coherent:

- retrieval scoring would become less predictable
- graph heuristics would lose shared meaning
- extraction prompts would drift per installation
- app surfaces would need endless type-specific branching
- cross-vault reasoning and portability would weaken

The fixed core ontology avoids that.

## 4. What SchemaKind Owns

Schema kinds may later define:

- required / optional structured fields
- allowed relationship types
- validation rules
- default brief generation hints
- specialized edit widgets
- specialized derived artifacts

Schema kinds should not change the meaning of the underlying core type.

## 5. Vault Representation

Canonical files remain normal Markdown entity records. A future schema kind can
be represented as metadata on the canonical file, for example:

```markdown
---
id: 5x5-strength
type: task
schema: training_program
space: operations
created: 2026-04-06T00:00:00Z
updated: 2026-04-06T00:00:00Z
---
```

The `type` field keeps the core ontology stable. The `schema` field adds
specialized structure.

## 6. Runtime Ownership Boundary

The SmartOffice precedent is useful in one narrow way: it cleanly separates
the generic entity framework from module- or schema-specific structure.

Symbiotic should keep the same separation of concerns while preserving its
stronger closed memory ontology:

- the runtime owns the core `EntityType` taxonomy
- canonical frontmatter mirrors that taxonomy with `type:`
- the filesystem mirrors runtime metadata
- schema-specific validation and richer fields belong in `SchemaKind`

This means the filesystem never becomes its own registry, and the UI does not
need to reverse-engineer types from paths or generated artifacts.

## 7. Filesystem Mapping

The filesystem layout follows the core type, not the schema kind:

```text
ledger/tasks/5x5-strength/5x5-strength.md
ledger/tasks/5x5-strength/5x5-strength.brief.md
```

not:

```text
ledger/training-programs/...
```

This keeps the storage hierarchy stable and understandable.

## 8. Runtime Authority

Current type authority lives in runtime memory code:

- `submodules/runtime/crates/symbiotic-memory/src/types.rs`
- `submodules/runtime/crates/symbiotic-memory/src/vault_parser.rs`

When schema kinds are introduced, they should be added as a higher layer on top
of that runtime authority. The filesystem must continue to mirror runtime type
metadata rather than inventing its own typing system.

### Recommended end-state seam

The long-term split should look like:

- `EntityType`
  - fixed core ontology
  - storage folder mapping
  - default memory semantics
- `SchemaKind`
  - optional specialization name, for example `food`, `training_program`
  - validation rules
  - allowed structured fields
  - specialized relationship rules
  - optional specialized brief-generation hints

Example:

```markdown
---
id: kimchi
type: concept
schema: food
space: knowledge
created: 2026-04-06T00:00:00Z
updated: 2026-04-06T00:00:00Z
---
```

The record still lives under the core type:

```text
ledger/concepts/kimchi/kimchi.md
ledger/concepts/kimchi/kimchi.brief.md
```

not:

```text
ledger/foods/...
```

## 9. Non-Goals

- This document does not introduce custom schemas in implementation yet.
- This document does not make `EntityType` dynamic.
- This document does not define the full schema registry API.

## 10. Recommended Adoption Order

1. Keep `EntityType` as the fixed core ontology.
2. Centralize type metadata in runtime implementation.
3. Finish the entity artifact path rewrite around that metadata.
4. Preserve optional `schema:` metadata in the parser/export path even before the full schema registry exists.
5. Add the actual `SchemaKind` registry only after the core path/layout migration is complete.
