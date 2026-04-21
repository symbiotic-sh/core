# Scaffolding Retirement — What Is Bootstrap Substrate vs. Canonical Surface

**Status**: Brief / cross-cutting reference
**Related Docs**: `docs/design/project-bootstrap-process.md`, `docs/design/project-goal-process-model.md`, `docs/design/vault-as-truth.md`, `docs/design/sovereign-sync.md`, `docs/design/operator-reasoning-distillation.md`, `docs/design/llm-audit-trail.md`, `docs/design/process-engineer.md`, `docs/design/attached-knowledge-base.md`, `docs/architecture/goals-layer.md`

## Purpose

Symbiotic currently carries a layer of hand-maintained coordination artifacts — `tasks/NEXT.md`, `tasks/TASKS.md`, `tasks/meta.json`, `tasks/{id}/README.md` chunk folders, the `./scripts/git-commit.sh` / `git-push.sh` wrappers, and the stop-read-first ritual in `CLAUDE.md`. These are **bootstrap substrate**, not end-state surfaces. They exist because the runtime that would own this state is not yet driving day-to-day work.

This doc records which scaffolding artifacts retire onto which runtime surfaces, what triggers the retirement, and which authoring surfaces survive permanently. A future reader should not mistake the current coordination shape for the end-state shape.

## Retirement Heuristic

For any repo artifact, ask: *"would this still exist if the Symbiotic daemon had been driving work since day one?"*

- **No** → bootstrap substrate; it retires when the runtime surface is live.
- **Yes** → canonical authoring surface; it persists regardless of runtime maturity.

Design docs and architecture prose are canonical (runtime can read them, but long-form rationale is not well-represented by atomic-claim records). Hand-maintained indices, continuation-state markdown, and shell wrappers around git are substrate.

## Retirement Map

| Scaffolding Artifact | Runtime Replacement | Retirement Trigger |
| :--- | :--- | :--- |
| `tasks/meta.json` | Archive entities under `operations/projects/symbiotic/goals/{goal}/tasks/{task}.md` with frontmatter per `project-goal-process-model.md` | T127 Project Bootstrap Process runs against Symbiotic itself; goal manifests + task manifests take over |
| `tasks/TASKS.md` | Generated `*.brief.md` read surface produced by Distillery reweave from canonical entity records; regenerated, never edited | Distillery brief-generation is wired for `operations/projects/symbiotic/*` |
| `tasks/{id}/README.md` + chunk files | Goal manifest + process manifest + work items; chunk becomes a runtime work-item state, not a static file | Process manifest generator modes (including `one_shot_bootstrap` from T127) drive real daemon behavior instead of remaining parser-only |
| `tasks/NEXT.md` | Runtime goal state + checkpoint artifacts from `operator-reasoning-distillation.md` Phase 8 (`checkpoint.create`) + daemon-compiled context packet (Phase C) | Operator reasoning distillation Phase C/D lands — daemon composes context packets, runner consumes them before tool work |
| Stop-read-first list in `CLAUDE.md` | Daemon-compiled context packet; static rules promoted into `knowledge-base/operations/skills/operator-protocol/operator-protocol.md` | Same as NEXT.md retirement — Phase C/D of operator-reasoning-distillation |
| `./scripts/git-commit.sh` / `git-push.sh` wrappers | **Three-destination split** (not a single retirement target): (1) semantic-commit format + co-author trailer knowledge → Symbiotic **agent context / skill** (portable across every attached repo, not baked into any one repo's tooling); (2) pre-commit / pre-push / CI enforcement → each repo's **dev pipeline**, owned and maintained by a future **CI/CD agent** role attached per `repo_role: source` repo; (3) daemon-side orchestration around commits (goal-event emission, audit entries at commit time) → `VaultWriter` for cases where the daemon authors a commit on an agent's behalf (never runs in a pre-commit hook — hooks stay out of daemon writes). | The wrappers stop being load-bearing once (1) the commit skill ships with the agent layer, (2) a repo has its pre-commit / pre-push hooks installed and a CI/CD agent maintaining them, and (3) `VaultWriter` owns the daemon-authored commit path. Each destination retires the wrappers independently; full wrapper deletion is gated on all three. |
| Manual co-author trailer on commits | Daemon identity signing (sovereign-sync §Security: "All agent commits are GPG-signed by the Daemon's identity to ensure provenance") | Signed commits land as a first-class sovereign-sync feature |
| Hand-picked chunk sequencing in README | Reconciler-driven work-item state machine per `orchestration-integration.md` | Reconciler watches goal manifests and issues `StartGoal` / `StartChunk` actions instead of relying on an operator reading the README |
| `docs/architecture/*.md` and `docs/design/*.md` living only on-disk in the app repo | External project-docs repo attached as `repo_role: docs_akb` (T129). The prose stays authoritative *inside that repo*, but agents reach it through the AKB's tiered memory + retrieval surface via `RecallScope::Akb { repo }` instead of by walking files. | T129 AKB pipeline is live and `repo:symbiotic-docs` (or equivalent) is attached to `project:symbiotic` during T127's first self-bootstrap. At that point `submodules/` is no longer the only way a reader reaches architecture prose. |
| This doc's own existence as prose | (Stays — see below) | n/a |

## Recursion Note — T127 Is the Forcing Function

T127 (Project Bootstrap Process) is explicitly one-way for most of the above. Its Phase 4 Ready-State Handshake anchors the project on runtime state (project manifest + repo manifests + handshake thread + `achieved`-state onboarding goal) rather than asking the operator to hand-edit `tasks/`. The moment `symbiotic project onboard --source git@github.com:kakajansh/symbiotic.git` succeeds (plus a sibling `--source ... --role docs_akb` for the project-docs repo), Symbiotic becomes one of the projects it manages. At that point:

- Today's `tasks/126-repo-manifest/` folder's role collapses into `operations/projects/symbiotic/goals/project-bootstrap-harness/tasks/repo-manifest.md`.
- NEXT.md's role is absorbed into runtime goal state.
- The operator-reasoning-distillation pipeline starts mining the Claude/Codex session logs produced during this very slice and promoting rules into canonical operator protocol.
- Git wrappers remain only as a transitional safety net. Their retirement is three-way (see the row above): commit-format/trailer knowledge moves into the Symbiotic agent layer (context + skill), enforcement moves into per-repo hooks + CI owned by a future CI/CD agent role, and daemon-authored commits route through `VaultWriter`. T127 lands before any of those three are complete, so the wrappers still exist at end-of-slice; they do not retire as part of the current MVP.
- The on-disk `docs/architecture/**` + `docs/design/**` corpus migrates to a sibling `project-docs` repo attached as `repo_role: docs_akb`. The T129 AKB pipeline indexes it with tiered memory + retrieval so agents get the same search/retrieval parity they have today with the main vault, without the design prose living in the app repo itself.

## What Survives Permanently

These are canonical authoring surfaces. They are *read* by the runtime (indexed, distilled, referenced from Archive records) but *edited* in place and not synthesized from atomic claims.

- **Repo-root rules**: `CLAUDE.md`, `CONTEXT.md`, `AGENTS.md`, `GEMINI.md` — agent-model-specific entrypoints. They become inputs to the daemon-compiled context packet rather than hand-followed rituals, but the files themselves remain the authoring surface.
- **Naming canon**: `docs/NAMING-CANON.md` — terminology discipline, authored as prose.
- **Conventions**: `tasks/CONVENTIONS.md` — task/chunk format spec. The tasks/ folder as a storage location retires; the conventions document (once updated to describe runtime task manifests instead of filesystem layout) remains as the spec for what a well-formed task record looks like.
- **Design docs**: `docs/design/*.md` — architecture prose, decision records, invariants. Long-form rationale is not a good fit for the Archive entity schema (optimized for atomic-claim retrieval, not paragraphs of context). The Distillery reads these to populate brief surfaces; editing stays in the design files. **Note**: the *files themselves* remain the authoring surface, but their on-disk home retires from the app repo into a sibling `project-docs` repo that is attached as `repo_role: docs_akb` (T129). Agents reach the prose through AKB retrieval, not by walking `submodules/`.
- **Architecture docs**: `docs/architecture/*.md` — same reasoning as design docs, including the location retirement into an AKB-attached `project-docs` repo. `docs/architecture/distillery.md` explicitly claims source-of-truth for implemented behavior, per the migration note on `distillery-pipeline-spec.md`; that claim survives the location move unchanged.
- **Configuration**: `config/*.toml`, `justfile`, etc. — operator-authored deployment state.
- **Source trees**: `submodules/` — managed by T116 internal git swarm + T126 repo manifest once those land, but the source files themselves remain the authoring surface.

## Repo Intent Disambiguation

Repos interact with Symbiotic in three distinct ways and the primitives must NOT be conflated:

- **Ingest** (Intake pipeline, existing) — one-shot repo content capture into Archive entries, same shape as tweet/URL/file ingestion. No mirror, no credential, no attachment.
- **Attach** (T126 + T127) — durable project-scoped binding with internal bare mirror, credential, and optional AKB index.
- **Analyze** (T128 Source Archeology) — pipeline over an already-attached repo; not applicable to ingested content.

See `docs/design/repo-manifest.md` §Repo Intent Taxonomy for the full disambiguation matrix and default-to-Intake rule. This distinction matters for the retirement discussion because ingested repo content is a permanent surface (ambient knowledge lives in the sovereign vault) while attached-repo surfaces retire alongside the project that owns them.

## Why This Matters for Current Work

Three practical implications for the T126/T127/T128/T129 slice:

1. **Implementation chunks should not over-invest in the scaffolding surface.** If a chunk is tempted to add helper utilities for `tasks/meta.json` manipulation or `NEXT.md` rendering, that's a tell the chunk is optimizing a surface that's slated for retirement. Prefer landing the runtime surface directly.

2. **The composition question surfaced in T128's design review** (see `source-archeology.md` review — T120 / T122 / Process Engineer / Operator Reasoning Distillation integration) is easier to resolve if the retirement direction is kept in mind: trace capture and tuning already has a canonical destination (`knowledge-base/operations/skills/operator-protocol/`), and T128 runs should feed that destination rather than create a parallel hand-maintained trace index.

3. **T129 is in the MVP slice precisely so the docs-location retirement is not a future promise.** The moment Symbiotic self-attaches `repo:symbiotic-docs` (or equivalent) as `docs_akb`, architecture/design prose has a runtime retrieval surface. Without T129 in the slice, architecture docs remain on-disk only and agents have weaker retrieval against them than against the main vault — a structural imbalance that would leak into every downstream goal. Slicing T129 with T126/T127/T128 prevents that debt.

## Reading Order for "Is This Scaffolding?"

When reviewing any new artifact or proposed file:

1. Check this doc's retirement map. If the artifact matches a scaffolding row, it's substrate.
2. If it doesn't match, apply the retirement heuristic ("would this exist if the daemon had been running since day one?").
3. If uncertain, check whether the artifact's content could be represented as Archive entity records with atomic-claim frontmatter. If yes, it's likely substrate waiting for runtime ownership. If no (long-form prose, rationale, decision records), it's likely canonical.

## Open Questions

- **`tasks/CONVENTIONS.md` evolution path**: when the tasks/ folder retires, does `CONVENTIONS.md` move to `docs/design/task-manifest-conventions.md` or stay under `tasks/`? The content is a spec either way; the location question is about discoverability for operators.
- **Shell wrapper removal timing**: the wrappers can't retire on a single signal; retirement is three-way (agent commit skill lands + per-repo hooks + CI installed with a CI/CD agent maintaining them + `VaultWriter` owns daemon-authored commits). The concrete gate is all three destinations being load-bearing, not any one of them. A dated follow-up task ("CI/CD agent role + wrapper retirement") once T127 + PE integration lands captures the sequencing.
- **CLAUDE.md vs. daemon context packet overlap**: once the packet is live, CLAUDE.md could either (a) stay as the human-readable spec that the packet assembler reads, or (b) be generated from a canonical rules record under `knowledge-base/operations/skills/operator-protocol/`. (b) is more consistent but more work. Defer to operator review.
