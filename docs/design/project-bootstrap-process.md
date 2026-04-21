# Project Bootstrap Process — Attaching External Projects to Symbiotic

**Status**: Proposed Specification
**Related Tasks**: T127 (this doc), T126 (Repo Manifest), T128 (Source Archeology), T129 (Attached Knowledge Base)
**Related Docs**: `docs/design/project-goal-process-model.md`, `docs/design/repo-manifest.md`, `docs/design/source-archeology.md`, `docs/design/attached-knowledge-base.md`, `docs/design/agent-company-model.md`, `docs/design/orchestration-integration.md`, `docs/design/vault-as-truth.md`, `docs/design/archive-policy-scope-hierarchy.md`, `docs/design/scaffolding-retirement.md`

## Problem

`project-goal-process-model.md` defines the end-state shape of a Project node (manifest + goals + processes + nested Archive layout) but says nothing about how a project *comes to exist*. Today the only project is the implicit `project:inbox` fallback; there is no declarative operator-level command that takes an external identity like a git URL, a folder path, or a product name and produces a valid project node with seeded goals and repo manifests.

Without that "start point," Symbiotic's agentic harness cannot be pointed at an existing codebase, knowledge body, or operational concern. The operator cannot say *"I have `git:flux`, please progress it further"* and get the system to spin up discovery, normalize documentation, ingest the project into the vault, and then hand over to ordinary goal/process workflows.

This doc specifies the **Project Bootstrap Process**: a canonical Process whose sole output is a valid, lint-passing project node that subsequent goals/processes can operate against. It uses only primitives that already exist in the system — the Declarative Control Plane reconciler, the Agent Company model, the Internal Git Swarm, the Distillery, Archive policy scopes, and Vault-as-Truth — plus the new Repo Manifest (T126) and Source Archeology (T128).

## Non-Goals

- Not a general "import any tool" flow. This Process onboards things the operator already thinks of as *projects* (repos, product efforts, operational books of work), not one-shot data ingests (those go through existing Archive intake).
- Not a replacement for `zero-touch-managed-onboarding.md`. That doc covers onboarding the Symbiotic system itself onto a user's VPS; this doc covers onboarding external work into an already-running Symbiotic.
- Not a silent doc-normalizer. Changes that would mutate the source repo land as a reviewable PR against the source, never as auto-pushed commits.

## Core Decision

Project bootstrap is a **named Process under a meta-project** (`project:symbiotic` itself) that:

1. Accepts an `onboard` command carrying a minimal project descriptor.
2. Runs four explicit phases inside the existing orchestration pipeline, each producing canonical Archive artifacts that the Reconciler and Distillery verify.
3. Leaves the operator with a ready-state project node — manifest, seeded goals/processes, attached repo manifests, and an opened handoff thread — and a reviewable PR against the source repo (if documentation drift was found).

Bootstrap is not a runtime primitive of its own. It is a Process with a new generation mode, `one_shot_bootstrap`, that extends the existing `recurring_tasks | goal_template | review_only` modes in `project-goal-process-model.md`.

## Invocation

### CLI / Matrix command

```text
symbiotic project onboard \
  --source git@github.com:kakajansh/flux.git \
  --slug flux \
  --title "Flux" \
  --domains product,infra \
  --policy-scopes company:default \
  --autonomy semi
```

The same command is available as a Matrix command (`/project onboard ...`) and as an `AskUserQuestion`-gated flow from the app.

Required inputs:

- `--source` — one of a git URL, a local path, or a logical handle (`local:~/p/flux`, `handle:some-name` for non-repo projects). **Phase 1's worker composition is conditional on source type**: `handle:` sources skip the clone step and produce an empty `repos/` array in the draft project manifest, which in turn makes Phase 2 a trivial noop (no repos to reconcile). `local:` and git-URL sources run the full clone + observation flow.
- `--slug` — canonical project slug.
- `--title` — human title.

Optional inputs:

- `--domains` — comma-separated list; seeds `project.domains`.
- `--policy-scopes` — comma-separated scope IDs; seeds `project.policy_scopes`.
- `--autonomy` — `manual | semi | auto`; controls how many bootstrap phases require operator approval.
- `--no-mirror` — skip mirror setup (diagnostic use only).
- `--hook-on-attach` — override the default `process:repo.onboard` hook on the created repo manifest.
- `--role` — one of `source | docs_akb | reference_library` (default `source`). When not `source`, the bootstrap emits a `RepoManifest` with that `repo_role` (see T126) and the required `indexing` block. Phase 3 **enqueues** the AKB build post-commit as fire-and-forget; it does NOT block the atomic vault commit, and AKB build completion is NOT part of the ready-state invariants. Multi-repo bootstrap is supported by repeating `--source ... --role ...` pairs.

### Archive effect of invocation

The CLI does not run the Process directly. It writes a single intent record:

```text
knowledge-base/operations/projects/symbiotic/processes/project-onboard.md
knowledge-base/operations/projects/symbiotic/goals/onboard-{target-slug}/plan.md
```

**The intent-record writer is the operator-side CLI directly** — no agent sandbox is involved. This is a local-FS write under operator authority; agent sandboxes are not required for bootstrap intent because the operator invoked it. The Reconciler detects the new goal manifest (per `orchestration-integration.md` Phase 1-2) and triggers execution. Bootstrap is therefore observable, resumable, and auditable through the same lens as every other goal.

**Idempotency**: a second invocation with an existing `--slug` exits cleanly — Matrix ack + CLI exit code 0 + an empty-op event (`project.onboard.skipped`) with reason `already_onboarded`. Same shape as a clean `git pull --ff-only` on an already-up-to-date branch.

## Phases

Bootstrap runs four phases, each a task cluster under the `onboard-{slug}` goal. Each phase emits canonical Archive events and commits to the vault; no phase writes to the target source repo.

### Phase 1 — Discovery

**Inputs**: `--source`, project slug, optional policy scopes.

**Worker composition**: one `discovery` agent (`fast` tier, read-only) plus one `analyst` agent (`deep` tier, read/write to Archive only). Tier names are canonical per `docs/NAMING-CANON.md`.

**What it does**:

- Clones the target source into a fresh sysbox sandbox via the existing `VmManager` (T114). No credential with push scope is required at this stage — pull scope only.
- Reads the top-level repo shape: `README`, `LICENSE`, `CLAUDE.md`, `AGENTS.md`, `CONTEXT.md`, `tasks/TASKS.md`, `tasks/NEXT.md`, `docs/`, `package.json`/`Cargo.toml`/`pyproject.toml`/`go.mod`, CI configs, last 50 commits, open branches, stale-ness signals (last push per branch).
- Captures non-code signals: contributor list, licence, dependency footprint, CI system in use.
- Produces a structured `discovery-report.md` artifact under `goals/onboard-{slug}/artifacts/` with typed frontmatter. Canonical frontmatter schema:

  ```yaml
  ---
  source: "git@github.com:kakajansh/flux.git"   # mirrors invocation --source
  source_type: git | local | handle
  languages: ["rust", "typescript"]
  build_systems: ["cargo", "pnpm"]
  test_commands: ["cargo test", "pnpm test"]
  ci_system: "github_actions" | "gitlab_ci" | "none" | "other"
  docs_present: ["README.md", "docs/architecture/overview.md"]
  docs_missing: ["CLAUDE.md", "AGENTS.md"]
  tasks_next_present: true | false
  last_activity_utc: "2026-04-10T12:00:00Z"
  apparent_health: "healthy" | "stale" | "abandoned"
  ---
  ```

  Downstream phases (Phase 2 template selection in T128; Phase 3 manifest promotion) read this typed frontmatter directly — no prose-regex.

**Output artifact**: `discovery-report.md` plus a draft `project.md` and one or more draft `repos/{slug}.md` manifests (not yet committed to the canonical project path — they live under the goal artifacts folder until Phase 4).

**Approval gate**: depends on `--autonomy`.

- `manual`: hard gate; operator must approve before Phase 2.
- `semi`: gate only if discovery finds authentication-relevant files (secrets, private keys, unclassified dependencies), otherwise auto-proceed.
- `auto`: proceed unless a `severity: critical` signal is raised.

### Phase 2 — Source Archeology

Phase 2 runs T128 Source Archeology (`docs/design/source-archeology.md`) — a six-stage pipeline (Excavate → Date → Diagnose → {Reconcile | Scaffold | Handoff} → Triage → Verify) that deeply inspects each attached repo, classifies staleness, and produces findings with per-finding dispositions (resolve / defer / escalate) based on a decision tree considering importance, goal alignment, operator mood, and peer agent review.

**Phase 2 is role-conditional** (resolves T126 `repo_role`):

| `repo_role` | Phase 2 behavior |
|-------------|------------------|
| `source` | Run full Source Archeology via T128 `ArcheologyTarget`. Diagnose picks the branch; Scaffold output, if any, routes to `{slug}-docs` via T127 Auto-Provision. |
| `docs_akb` | Run Source Archeology in Reconcile-only mode — a docs repo's archeology is doc-reconciliation-only; it does not scaffold against itself. AKB reindex observes any merged change downstream per T129. |
| `reference_library` | **Skip Source Archeology entirely.** T128's role gate rejects `reference_library` targets; an invocation would be guaranteed-noop. |

For multi-repo bootstraps, Source Archeology runs once per attached repo of roles `source` or `docs_akb`, producing a separate findings set + patchset (or scaffold package) per repo.

**Output is not just a patchset**: depending on Diagnose's verdict, Phase 2 emits one of:
- A reconciliation patchset (for `salvageable` verdict — existing docs can be fixed in place).
- A scaffold package destined for `{slug}-docs` (for `stale_beyond_salvage` or `no_docs` — fresh docs need to be written; Scaffold routes them to a separate docs repo via T127 Auto-Provision).
- An operator handoff report with explicit questions (for `needs_operator_input` — ambiguity too high for automated output).

Triage applies its decision tree to each finding in Reconcile / Scaffold output and decides disposition per-finding. Findings with `resolve` disposition flow to Verify and onto the output; findings with `defer` are recorded in the archeology checkpoint as open issues for future runs; findings with `escalate` are packaged into an operator notification card.

**Inputs**: Phase 1 discovery artifacts, target source clone.

**Worker composition**: Source Archeology's own stage workers (excavator, dater, diagnostician, reconciler/scaffolder/reporter, triager, verifier) inside the Archeology Sandbox (a T116 Distillery-Sandbox-class environment differentiated by `CHECK_NAME=archeology`). Driven by T128 Source Archeology as spec'd in `docs/design/source-archeology.md`.

**What it does**:

- Runs the four-stage Distillery pipeline (`Reduce → Reflect → Reweave → Verify`) but with the *target source repo* as the mutation target instead of the vault, producing a patchset (not a direct write).
- Reduce: extract claims from existing `README`/`CLAUDE.md`/`CONTEXT.md`/`AGENTS.md`/`tasks/*.md`.
- Reflect: compare claimed state against observed repo (branches, recent commits, test suite presence, scripts actually referenced, documented commands that exist vs. don't).
- Reweave: produce surgical patches for drifted docs and drafts for missing docs (CLAUDE.md scaffold, CONTEXT.md scaffold, tasks/TASKS.md summary row, NAMING-CANON if conventions are detectable). Patches are *never* committed to the source; they become a branch + patchset.
- Verify: the VaultLinter-equivalent for the target repo runs against the produced patchset (or a provider-specific lint, e.g. markdownlint) before handing off.

**Output artifact**: a `drift-report.md` summarizing what was found, plus a patchset attached to the goal (either as a branch under the internal bare mirror, or as a pre-formatted PR body for the external provider).

**Approval gate**:

- `manual`: always gate before PR is opened.
- `semi`: always gate if drift touches security-relevant files; auto-open PR for doc-only drift.
- `auto`: auto-open PR; operator review happens on the provider side.

**Critical rule**: even in `auto` mode, the external push happens only if `agent_scopes.push_external` on the repo manifest is `true` *and* no entry in `requires_operator_approval_for` overlaps the operations this PR performs. When gated, the PR stays as a local branch on the internal bare mirror and the operator is pinged via Matrix with a `[Push to origin]` / `[Hold]` / `[Discard]` chip set.

### Phase 3 — Vault Ingestion

**Inputs**: Phase 1 draft manifests, Phase 2 reconciled doc corpus.

**Worker composition**: one `ingest` agent (`deep` tier) with `archive.write` and `file.write` scopes limited to `operations/projects/{slug}/**`.

**What it does**:

- Materializes the canonical project node at `operations/projects/{slug}/`:
  - `project.md` (promoted from Phase 1 draft with any corrections from Phase 2 drift review).
  - `repos/{repo}.md` for each attached source (T126 schema). For each attached repo whose `repo_role != source`, the manifest includes the `indexing` block per T126 and the daemon enqueues an initial AKB build (T129) to run asynchronously after the atomic commit lands.
  - `goals/{bootstrap-goal}/plan.md` left intact as the audit record of how the project got here.
  - `processes/` seeded with one default process per detected cadence signal in Phase 1 (e.g. if the repo has a weekly release tag pattern, seed `weekly-release.md`).
- Computes a `project_index.md` under the project folder listing attached repos, seeded processes, linked threads, and the bootstrap goal's final status.
- Runs the vault linter (`./scripts/lint-vault.sh`) against the newly authored files before commit. A lint failure rolls back Phase 3 and requests Phase 2 rework.
- Commits to the vault using the canonical commit-format convention from `CONTEXT.md` §Commit Message Format and the small-scope-edit pattern implicit in `vault-as-truth.md`. Exact commit message shape: `feat(memory): bootstrap project:{slug}` — reuses the canonical `feat` type with `memory` as scope; no new top-level commit type is introduced. Single commit, atomic set of new files.

Note: AKB builds for any `docs_akb` / `reference_library` repos are NOT part of the Phase 3 atomic commit. They run post-commit and their derivatives under `data/akb/{repo_id}/` are rebuildable from the repo, so a missing or corrupted AKB does not compromise the ready-state invariants.

**Output artifact**: the commit SHA of the vault mutation, plus a `ingest-report.md` summarizing what was materialized.

**Approval gate**: none. The vault is the daemon's own data surface; committing to it does not require a per-phase operator decision. The vault's own sync loop (`sovereign-sync.md`) handles the push to the vault remote.

### Phase 4 — Ready-State Handshake

**Inputs**: vault commit from Phase 3, goal context.

**Worker composition**: no agent. This phase is a deterministic daemon operation.

**What it does**:

- Emits `project.onboarded` event carrying `{project_id, vault_commit_sha, attached_repo_ids, seeded_process_ids, bootstrap_goal_id}`. **`project.onboarded` means the project node is set up and ready for agents to work: goals can be filed, processes can be dispatched, the project's Matrix thread is live.** It does NOT mean all attached AKBs are queryable yet. AKB readiness is a separate, per-AKB event stream (`akb_created` → `akb_ready`) and an operator querying an attached docs repo immediately after `project.onboarded` may see empty results until that AKB's `akb_ready` fires. This split is intentional: onboarding completion should not block on minutes-long index builds.
- Opens a `#thread-{slug}` in Matrix (or reuses one if the operator supplied one at invocation) and posts a handoff card: *"Project `{title}` is onboarded. Attached repos: ... Seeded processes: ... AKB status: {N} attached, {M} ready, {K} still building (check back for `akb_ready` notifications). Next: pick a goal or let me suggest one."* Each AKB's subsequent `akb_ready` event posts a follow-up ping in the same thread: *"`repo:flux-docs` index is ready — you can now query it."* Room ACL at creation is minimum-privilege: **operator + assigned agents only**; external members require explicit invitation through the existing Matrix command plumbing. (Implementation chunk 06 will cite the existing room-creation primitive.)
- Flips the bootstrap goal's `state` to `achieved`.
- Flips the target project's `state` to `active`.
- Writes the final `NEXT.md`-equivalent summary (`operations/projects/{slug}/goals/onboard-{slug}/events/{ts}-onboarded.md`).

From this point the project is indistinguishable from any other project that existed since system bootstrap. Subsequent goals, processes, threads, and archived knowledge all route through the normal Reconciler / GoalProcessManager / SwarmOrchestrator pipeline.

## Process Manifest

```yaml
---
id: "process:symbiotic:project-onboard"
project_id: "project:symbiotic"
slug: "project-onboard"
title: "Project Onboarding Pipeline"
state: active
thread_id: "thread-symbiotic-ops"
owner_hint: "operator"

generator:
  mode: one_shot_bootstrap     # NEW: extends recurring_tasks | goal_template | review_only
  produces_goal_prefix: "onboard-"

inputs:
  required: [source, slug, title]
  optional: [domains, policy_scopes, autonomy, no_mirror, hook_on_attach]

phases:
  - id: discovery
    agents: [discovery, analyst]
    produces: [discovery-report.md, draft-project.md, draft-repos/*.md]
    gate_on_autonomy: [manual]
    gate_on_signal: [auth_surface_detected, dependency_unclassified]
  - id: doc_reconciliation
    agents: [reviewer]
    distillery: external_repo
    produces: [drift-report.md, patchset/*]
    gate_on_autonomy: [manual]
    gate_on_signal: [security_file_touched, external_push_required]
  - id: vault_ingestion
    agents: [ingest]
    writes_to: ["operations/projects/{slug}/**"]
    lint: "./scripts/lint-vault.sh"
    gate_on_autonomy: []
  - id: ready_handshake
    deterministic: true
    emits: [project.onboarded]

task_template:
  task_kind: execution
  task_driver: agent
  policy:
    escalation:
      mode: notify_operator
      audience: operator
---
```

This manifest is committed once, under `project:symbiotic`. It is not re-created per onboarding; each onboarding creates a new *goal* whose runtime projects across the phases described above.

## Autonomy Levels

| Level | Phase 1 Gate | Phase 2 Gate | Phase 3 Gate | External PR |
|-------|--------------|--------------|--------------|-------------|
| `manual` | always | always | never (vault only) | always gated |
| `semi` | on signal | on signal | never | gated if `push_external: true` but requires_operator_approval_for hit |
| `auto` | on critical | never | never | open unless repo manifest forbids |

`semi` is the intended default for developer-operator use; `manual` for high-sensitivity projects (finance, identity, production infra); `auto` only for toy projects or dogfooding.

## Smart Onboarding Expansion

Bootstrap is not a strict four-phase pipeline that takes one `--source` and blindly processes it. It is a **smart onboarding process** that can interact with the operator during the flow, autonomously discover related resources, and auto-provision surfaces the project needs but doesn't yet have. Three capabilities extend the strict phase flow:

### Capability 1 — Interactive Resource Discovery

After Phase 1 (Discovery) produces the first `discovery-report.md`, bootstrap pauses and **asks the operator via Matrix** (or the current UX surface):

> *"Found `repo:flux` as the source. Any other resources I should attach? For example: a docs repo, API specs, reference materials, related services. Say 'search company github' and I'll look, or give me URLs/handles directly, or say 'skip'."*

Operator responses are interpreted by a lightweight resolver worker:

- **Direct source (URL / path / handle)** → treated as an additional `--source` with `--role` inferred from content (see Capability 2) unless operator specified a role.
- **Search directive** (e.g. `"search my github for flux-docs"`, `"search company github for flux"`) → dispatches a discovery worker with a `search.external_sources` capability token. The worker runs an authenticated search against the indicated provider (github user/org, gitlab group, local folder), returns candidates, and re-prompts the operator: *"Found candidates: flux-docs, flux-ops, flux-integration. Which to attach?"*
- **Skip** → proceeds to Phase 2 with the repos gathered so far.

This loop can repeat: after each added resource, operator gets one more "anything else?" turn until they skip. Hard cap at some reasonable number of resolver cycles (e.g. 5) to prevent runaway loops.

### Capability 2 — Role Inference

When an operator supplies `--source` without `--role`, bootstrap infers the role from content signals captured in Phase 1's discovery pass:

| Signal | Inferred `repo_role` |
|--------|---------------------|
| Repo has `src/`, `Cargo.toml`, `package.json`, or similar source-tree markers | `source` |
| Repo has only `docs/`, `README`, ADR folders, no build system | `docs_akb` |
| Repo is a third-party corpus (e.g. URL matches known upstream docs sites, or repo name contains `-docs`/`-reference`/`-manual` for a project the operator does not own) | `reference_library` |
| Ambiguous | ask operator |

Role inference happens inside the Discovery worker; the operator can override per resource in the Interactive Resource Discovery loop.

### Capability 3 — Auto-Provision Missing Docs Repo

If after Capability-1 discovery **no repo with `repo_role: docs_akb` is attached** to the project, bootstrap offers to create one:

> *"No docs repo found for `project:flux`. I can create `flux-docs` as a new repo on your internal git server (or on github if you give me a provider handle), scaffold it with a starter layout (`architecture/`, `decisions/`, `runbooks/`, `README.md`), attach it as `docs_akb`, and seed the first AKB build. Proceed?"*

On operator approval:

1. Create the new bare repo on the daemon's internal git server at `data/git-server/repos/{slug}-docs.git`.
2. Populate with scaffold content from `templates/external-repo-scaffolds/docs-starter/`.
3. Make an initial commit (`feat(docs): scaffold project-docs for {slug}`) signed per the daemon's commit-signing identity.
4. Generate and write the `repos/{slug}-docs.md` manifest with `repo_role: docs_akb`, default `indexing.root_paths`, and `source.provider: local` (since the repo is daemon-hosted until the operator points it at an external remote).
5. Enqueue the initial AKB build post Phase 3 like any other attached `docs_akb`.
6. If operator supplied an external provider handle (github/gitlab/gitea), also push the scaffold to that remote and set `source.url` accordingly.

**Why this is load-bearing**: every project Symbiotic manages should have a retrievable docs surface. Without auto-provision, onboarding a project that doesn't already have a docs repo leaves the agent without the retrieval parity T129 promises. Auto-provision makes `docs_akb` attachment a near-universal property of onboarded projects rather than an operator-discipline requirement.

### Spec Completeness Note

The three capabilities above need additional design surface before implementation lands:

- Exact prompt templates (Matrix card shapes, CLI prompt wording).
- Resolver-worker protocol (discovery, search-external-sources, candidate-selection).
- Role inference heuristic's signal list (full spec per language/framework).
- Auto-provision scaffold template contents (what goes in `docs-starter/`).
- Failure modes (operator abandons mid-resolver; provider search times out; auto-provision can't reach external provider).

These are covered by new chunks in this task's chunk list (see `tasks/127-project-bootstrap-process/README.md`). They are NOT silently absorbed into the strict four-phase flow; they ride on top of it with explicit design before implementation.

## Interaction with Existing Runtime

### With the Reconciler

Bootstrap generates a new goal manifest under the meta-project. The existing Reconciler detects it, issues a `StartGoal` action, and the GoalProcessManager creates the runtime goal exactly as it does today. The only new surface is the `one_shot_bootstrap` generator mode, which the GoalProcessManager must learn to interpret (produces a time-bounded goal rather than a recurring task emitter).

### With the Agent Company Model

Phase 1 and Phase 2 workers follow the standard Agent Company patterns from `agent-company-model.md`: ephemeral sysbox sandboxes, communication via internal bare git + Nucleus RPC, Matrix mirror for transparent autonomy. Phase 2 runs T128 Source Archeology, which is a **sibling pipeline** to the vault Distillery — it reuses T116's sandbox primitives but has its own six-stage pipeline (Excavate → Date → Diagnose → {Reconcile | Scaffold | Handoff} → Triage → Verify), its own target type (`ArcheologyTarget`, not a Distillery variant), and its own artifact taxonomy. The two pipelines coexist; neither is a variant of the other.

### With Archive Policy Scopes

The onboarding goal inherits `project:symbiotic`'s policy scopes plus any scopes supplied at invocation. Phase-gate escalations use `notify_operator` by default and escalate to `raise_alert` when a security signal is detected, per `archive-policy-scope-hierarchy.md`.

### With T116 Internal Git Swarm

Phase 1's clone, Phase 2's patchset production, and Phase 3's repo-manifest setup all happen inside T116's existing sandboxed environment. The durable internal bare mirror (from T126) is initialized at the end of Phase 3 as part of vault ingestion, *after* the repo manifest lands in the canonical path.

### With Vault-as-Truth

All writes in Phase 3 follow the surgical-edit + semantic-commit conventions from `vault-as-truth.md`. The vault linter is authoritative; a lint failure rolls back.

## Rollback and Resume

Bootstrap is resumable because each phase writes canonical goal events. If the host crashes between Phase 2 and Phase 3, the next Reconciler tick observes the goal at `phase: doc_reconciliation, state: completed` and advances into Phase 3 on its own.

Rollback per phase:

- Phase 1 failure: goal events are kept for forensics; retry is a fresh onboarding attempt (new goal).
- Phase 2 failure: patchset is discarded; operator can choose to re-run Phase 2 with a broader discovery report or to skip doc reconciliation and proceed to Phase 3 with acknowledged debt.
- Phase 3 lint failure: vault commit never lands; goal drops to `blocked` with the lint output surfaced via Matrix.
- Phase 4: cannot fail — it is a deterministic state transition.

Hard failure at any phase leaves the goal in `failed` or `blocked` state; no partial project node is materialized at `operations/projects/{slug}/`. A half-materialized state is impossible because Phase 3 is a single atomic commit.

## Ready-State Contract

After Phase 4, the following invariants hold:

- `operations/projects/{slug}/project.md` exists and passes the vault linter.
- Every `repo:*` listed in `project.md` has a corresponding `repos/{repo}.md` that passes schema validation per T126, including the `repo_role` field and, when non-`source`, a well-formed `indexing` block.
- The onboarding goal is `achieved`, not deleted — it remains as the audit trail.
- A `#thread-{slug}` exists and has the handoff card posted.
- No unreviewed patchset is pending on the source repo without explicit operator acknowledgement.
- `project.state == active`.
- For each attached `docs_akb`/`reference_library` repo, an AKB build job has been enqueued (not necessarily completed; AKB is eventually-consistent post-ready-state per T129).

Subsequent user interaction with `project:{slug}` behaves identically to interaction with any pre-existing project.

## Test Strategy

| Test | Type | Description |
|------|------|-------------|
| Discovery on a real cloned repo | Integration | Point bootstrap at a fixture repo, verify `discovery-report.md` lists the right languages, build systems, and docs. |
| Discovery with missing docs | Unit | Verify drift-report flags `CLAUDE.md missing` when the source has none. |
| Phase 2 skip when no drift | Integration | Fixture with already-correct docs produces empty patchset; Phase 2 completes with `noop`. |
| Phase 2 PR gated in `manual` | Integration | Verify no external push occurs until approval. |
| Vault lint failure rolls back Phase 3 | Unit | Inject an invalid `project.md` draft; verify the commit doesn't land. |
| Full happy path on `git:flux` fixture | Integration | End-to-end: clone → discovery → reconciliation (assume clean docs) → ingestion → handshake. Verify project node exists and lint-passes. |
| Resume after crash between Phase 2 and 3 | Integration | Simulate crash mid-flight; next Reconciler tick completes Phase 3. |
| Unsupported source (`handle:...` without repo) | Unit | Verify bootstrap produces a project node without `repos/*.md` entries. |
| Idempotency on re-onboard | Unit | Second onboarding of same slug returns `already onboarded` without mutation. |

## Open Questions

- Should Phase 1 include a dependency-vulnerability scan? **Resolved: no** — keep bootstrap fast; a separate `process:dependency-audit` can run post-onboard under the newly-created project.
- Should a non-repo project (pure knowledge-body) skip Phase 2 entirely, or should Phase 2 run a `drift` pass against vault-internal notes? **Resolved: skip** — drift for non-repo projects is handled by the ordinary Distillery-on-vault path.
- What happens if the operator points bootstrap at the symbiotic repo itself? **Resolved: reject** with `project.onboard.skipped` reason `already_onboarded` — `project:symbiotic` is the meta-project and is permanent.
