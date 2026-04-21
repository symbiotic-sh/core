# Source Archeology — Deep Repo Inspection with Triageable Findings


**Status**: Implemented (T128 §02–§15 landed; see `docs/architecture/source-archeology.md` for the live pipeline surface). Renamed from "External Repo Distillery" 2026-04-17; tier-name sweep 2026-04-18.
**Related Tasks**: T128 (this doc), T127 (Project Bootstrap Process), T126 (Repo Manifest), T129 (Attached Knowledge Base), T116 (Internal Git Swarm)
**Related Docs**: `docs/design/distillery-pipeline-spec.md`, `docs/design/distillery-pipeline.md`, `docs/design/internal-git-swarm.md`, `docs/design/agent-company-model.md`, `docs/design/vault-as-truth.md`, `docs/design/repo-manifest.md`, `docs/design/project-bootstrap-process.md`, `docs/design/attached-knowledge-base.md`

## Model-Tier Convention

Worker compositions below describe models by **capability + speed tier**, not by provider or model name. Canonical tiers (project-wide, see `docs/NAMING-CANON.md`):

- **`fast`** — low-latency, low-cost classifier / tagger. Short context; many cheap calls preferred over one deep call.
- **`balanced`** — mid-latency reasoning. Multi-input decisions that don't need deep chain-of-thought.
- **`deep`** — highest-capability reasoning; slowest. Used for multi-step planning, nuanced classification, structured output under ambiguity.

The runtime backend maps each tier to whatever model family makes sense per deploy. Never name providers here (`Haiku-class`, `Sonnet-class`, `gpt-4o`) — that couples the design to a specific vendor and rots as model lineups shift.

## Why This Is Not "External Repo Distillery"

The prior draft of this spec was called "External Repo Distillery" and reused the vault Distillery's four stages (Reduce → Reflect → Reweave → Verify). That framing conflated two fundamentally different processes. The vault Distillery **composes atomic claims into canonical truth** — a synthesis process. What we need here is the opposite: a process that **digs into a repo, assesses whether its docs accurately describe reality, detects staleness, and produces triageable findings** — an investigation process. Same stage names, opposite intent. Confusing.

This spec renames the primitive to **Source Archeology**, drops the Distillery stage metaphor, and adds two primitives the prior draft lacked:

1. **Staleness as a first-class dimension** — before reconciling anything, we classify how old, how wired, and how aspirational each artifact is.
2. **Triage as a distinct stage** — findings don't auto-patch. They become flagged issues with metadata; a decision-tree stage decides per-finding disposition (resolve / defer / escalate) based on importance, goal alignment, operator mood, and peer agent review.

## Problem

When a project is onboarded (T127 Phase 2) or when a repo manifest signals drift (T126 `hooks.on_drift_detected`), the daemon needs to understand the external source repo deeply enough to act on it. "Act" has three possible forms:

- **Reconcile** existing docs (repo has docs; some are drifted). Produce a patchset.
- **Scaffold** new docs (repo has no docs, or docs are too stale/aspirational to salvage). Produce fresh scaffolds in a separate `{slug}-docs` repo via T127 auto-provision.
- **Hand off** to the operator when ambiguity is too high for an automated output.

A shallow "compare claims to reality and patch the drift" pipeline can only produce the first output. Most real repos need one of the other two. Source Archeology is the primitive that supports all three.

## Non-Goals

- Not a vault Distillery replacement. The vault Distillery continues to own inward-facing claim synthesis.
- Not a general-purpose code refactoring tool. Mutation targets remain documentation files by default.
- Not a CI pipeline. Source Archeology does not run the source repo's tests or build.
- Not a credential holder. Push scope is resolved per-call via T126 `agent_scopes.push_external` plus operator approval.
- Not an issue tracker for all repo problems. Source Archeology surfaces documentation-level findings; code-level refactor findings belong elsewhere.

## Core Decision

Source Archeology is a **sibling pipeline** to the vault Distillery — it reuses T116's sandbox primitives, per-API connector model, and typed bundle extraction, but has its own stage model, its own artifact taxonomy, and its own decision primitives. The two pipelines coexist; neither is a variant of the other.

The pipeline has six stages:

```
Excavate → Date → Diagnose → {Reconcile | Scaffold | Handoff} → Triage → Verify
```

Stages 1–3 run unconditionally. Stage 4 branches on Diagnose's classification. Stage 5 (Triage) applies a decision tree to findings. Stage 6 (Verify) runs over whatever survived Triage.

## Target Type

Source Archeology does NOT share a target enum with the vault Distillery. Each pipeline owns its own:

```rust
pub struct ArcheologyTarget {
    pub repo_id: String,              // e.g. "repo:flux"
    pub base_branch: String,          // usually repo.source.default_branch
    pub goal_id: String,              // the goal under which this run happens
    pub allowed_paths: Vec<PathPattern>,  // authored per-invocation by the caller
                                          // (e.g. T127 Phase 2), NOT on the RepoManifest
    pub mode: ArcheologyMode,
}

pub enum ArcheologyMode {
    Full,                // run all six stages
    DryRun,              // Excavate → Date → Diagnose → Triage only; no outputs written back
    CheckpointOnly,      // update archeology-checkpoint.json only; no findings
}
```

`PathPattern` is `.gitignore`-style globs (same grammar as T126 `indexing.exclude_patterns`).

## Pipeline Stages

### Stage 1 — Excavate (deep repo inspection)

**Worker composition**: one `excavator` agent (`fast` tier, read-only) inside the Archeology Sandbox. Runs in parallel with the Date stage's workers where possible.

**Inputs**: read-only clone of the source repo (mounted at `/workspace/repo/`).

**What it does**: reads the full source tree and produces typed observations:

- Directory structure and top-level files (`README*`, `LICENSE*`, `CLAUDE.md`, `AGENTS.md`, `CONTEXT.md`, `docs/**`, `tasks/**`, `NAMING-CANON.md`)
- Build system detection (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `Makefile`, `justfile`)
- CI configuration (`.github/workflows/**`, `.gitlab-ci.yml`, `.circleci/**`)
- Test command discovery (scripts referenced from docs vs. scripts actually present)
- Branch inventory and recent commit pattern (last 50 commits, active branches, stale branches)
- Contributor surface and licence
- Dependency footprint (lock files, declared but unused deps — light pass, not a full audit)
- Wired-vs-declared analysis (script referenced in README → does the script exist in the repo?)

**Output**: `excavation-report.json` — typed, flat list of observations keyed by `{category, path, evidence}`.

### Stage 2 — Date (staleness assessment)

**Worker composition**: one `dater` agent (`fast` tier) plus a deterministic analyzer.

**Inputs**: Excavation report, git history.

**What it does**: per-artifact staleness classification. For every doc file and every claim the docs make about code, compute:

- **Last-commit age** (days since file was last modified)
- **Last-human-touched age** (days since a non-bot, non-auto-refactor commit touched it)
- **Wired-ness** (is this file referenced by live code / CI / wiring?)
- **Aspirational signal** (is the doc describing behavior that exists? behavior that existed? behavior that is planned?)

Each artifact gets a classification:

| Classification | Meaning |
|---|---|
| `fresh` | Recently touched; claims match code |
| `drifting` | Claims partially mismatch code; fixable |
| `stale` | Claims describe an earlier version of the code |
| `aspirational` | Claims describe behavior that never existed or is planned |
| `dead` | File exists but nothing references it |

**Output**: `staleness-report.json` — one row per artifact with classification + evidence pointers.

### Stage 3 — Diagnose (branch classifier)

**Worker composition**: one `diagnostician` agent (`deep` tier).

**Inputs**: Excavation + Staleness reports.

**What it does**: classifies the whole repo into ONE of four branch verdicts:

| Verdict | Meaning | Next stage |
|---|---|---|
| `salvageable` | Docs exist and most are `fresh` or `drifting`. Reconciliation will improve them. | Reconcile |
| `stale_beyond_salvage` | Docs exist but most are `stale` or `aspirational`. Patching around this would create more confusion. | Scaffold |
| `no_docs` | Little or no documentation present. | Scaffold |
| `needs_operator_input` | Ambiguity too high (e.g. contradictory doc claims the code confirms both of, or missing signals the automated classifier can't resolve). | Handoff |

The classifier is conservative: it prefers `needs_operator_input` over wrong action. Thresholds for the three auto-branches are tunable per-project via `archeology_policy` on the repo manifest (future extension; MVP uses defaults).

**Output**: `diagnosis.json` with `verdict`, `confidence` score, and `reasoning` (brief structured rationale).

### Stage 4a — Reconcile (only if Diagnose = `salvageable`)

**Worker composition**: one `reconciler` agent (`deep` tier).

**Inputs**: Excavation + Staleness reports + existing doc corpus.

**What it does**: produces surgical patches for `drifting` artifacts, referencing archeological evidence in the patch hunks' context. Patches are scoped to `allowed_paths`. Patches are **findings**, not direct applies — each patch is attached to a `Finding` record that flows into Triage.

**Output**: per-finding patch hunks, attached to the finding records.

### Stage 4b — Scaffold (if Diagnose = `stale_beyond_salvage` OR `no_docs`)

**Worker composition**: one `scaffolder` agent (`deep` tier).

**Inputs**: Excavation report, Staleness report (used to identify what the old docs GOT RIGHT vs. what they got wrong), project context (from T127 invocation).

**What it does**: produces fresh documentation from archeological findings — NOT from existing stale docs. Scaffolds include:

- `README.md` — one-paragraph project summary + how-to-build + how-to-test, all grounded in Excavation evidence
- `CLAUDE.md` — agent-facing rules derived from detected conventions
- `AGENTS.md` — role/responsibility map if the repo has an agentic component
- `docs/architecture.md` — from detected entrypoints, module boundaries, data flow
- `docs/build.md`, `docs/test.md`, `docs/deploy.md` — as warranted by CI/Makefile/scripts found

**Scaffold routing**: all scaffold output is written to a **separate `{slug}-docs` repo**, NEVER to the source repo's working tree. This composes with T127 Smart Onboarding Capability 3 (Auto-Provision Missing Docs Repo):

1. If a `repo:{slug}-docs` manifest already exists with `repo_role: docs_akb`, Scaffold writes to that repo's worktree.
2. If no docs repo is attached, Scaffold emits a `scaffold_package` artifact and signals T127 auto-provision to create `{slug}-docs` + attach it, then populate with the package.
3. The source repo is left untouched by Scaffold. Any pending findings against the source repo (e.g. "README references a script that doesn't exist — should we patch the README to remove it, or leave it as archeological evidence in the new docs repo?") flow into Triage as source-repo findings with their own disposition.

**Output**: `scaffold_package` artifact — a set of new files destined for `{slug}-docs`, plus a findings list for source-repo-side issues that Triage will evaluate.

### Stage 4c — Handoff (if Diagnose = `needs_operator_input`)

**Worker composition**: `reporter` agent (`fast` tier).

**Inputs**: all prior stage outputs.

**What it does**: compiles a human-readable `archeology-report.md` + a set of explicit operator questions (e.g. *"README mentions `legacy-auth/`; the code still has references but tests don't exercise it. Update README to reflect current active paths? Preserve as historical note? Remove?"*). No automated output.

Handoff is the only branch where Triage is a no-op — operator answers are required before any action.

### Stage 5 — Triage (NEW — decision-tree over findings)

**Worker composition**: one `triager` agent (`deep` tier), optionally followed by a `reviewer` agent (peer-review).

**Inputs**: the set of `Finding` records produced by Reconcile or Scaffold.

**What it does**: for each finding, run a decision tree that produces one of three dispositions:

| Disposition | Meaning |
|---|---|
| `resolve` | Apply this finding's action (patch, new file, etc.) in the output of this run |
| `defer` | Flag as an open issue; do not apply; record in the archeology checkpoint so it's surfaced on future runs |
| `escalate` | Surface to the operator for explicit decision before any action |

The decision tree inputs:

1. **Importance** (`severity` tag on the finding)
   - `critical`: always `escalate` regardless of other inputs
   - `high` / `medium` / `low`: consulted as a weighted input
2. **Goal alignment** — does this finding touch files in scope for the currently-driving goal?
   - `in_scope`: weights toward `resolve`
   - `out_of_scope`: weights toward `defer`
3. **Operator mood** — MVP reads this from `ArcheologyTarget.mode` and the repo's `autonomy` level plus any recent operator signals in the goal's Matrix thread:
   - `auto`: weights toward `resolve`
   - `semi`: default — mix
   - `manual`: weights toward `escalate`
4. **Peer review** — if configured, a second agent re-reviews the triager's per-finding disposition. Disagreements escalate.

MVP decision tree is declarative and simple:

```
if finding.severity == "critical":
    escalate
elif operator.autonomy == "manual":
    escalate   # operator wants full control
elif finding.goal_alignment == "out_of_scope" and finding.severity < "high":
    defer
elif peer_review_configured and peer_review.disagrees:
    escalate
else:
    resolve
```

Post-MVP: the decision tree becomes an LLM-reasoning stage that can weigh the inputs richly, consult historical operator decisions from the Archive, and learn from which findings the operator accepted vs. reverted.

**Output**: `triage-decisions.json` — per-finding disposition + rationale. Findings with `resolve` disposition flow into Verify; findings with `defer` are recorded in the archeology checkpoint as open issues; findings with `escalate` are packaged into an operator notification card.

### Stage 6 — Verify

**Worker composition**: two verifiers (patch applicability + lint).

**Inputs**: findings with `resolve` disposition from Triage.

**What it does**:

- **Patch applicability check**: each patch is applied against a fresh checkout of `base_branch` in a throwaway worktree. Failures reject that finding.
- **Lint check**: `markdownlint` on post-patch tree. Failures reject findings whose files fail lint.
- **Scaffold lint**: new files go through the same lint pass.

On Verify rejection of a finding, the finding is downgraded to `defer` and surfaced to the operator via the archeology checkpoint's "Verify-rejected" section.

## Archeology Checkpoint Artifact

T128 writes a fresh checkpoint under the triggering goal's artifact folder each run:

```text
knowledge-base/operations/projects/{project}/goals/{goal}/artifacts/
  source-archeology/
    {timestamp}/
      archeology-checkpoint.json        # run-to-run memory + open-issue ledger
      excavation-report.json
      staleness-report.json
      diagnosis.json
      findings.json                     # all findings from Reconcile/Scaffold
      triage-decisions.json             # disposition per finding
      scaffold-package/                  # present only if Diagnose = stale_beyond_salvage | no_docs
        *.md
      patchset/                          # present only if Diagnose = salvageable
        *.patch
      archeology-report.md               # human-readable summary
      metadata.json
```

### Checkpoint schema (`archeology-checkpoint.json`)

```json
{
  "schema_version": 1,
  "repo_id": "repo:flux",
  "run_timestamp": "2026-04-17T12:00:00Z",
  "observed_head": "abc123...",
  "previous_observed_head": "def456...",
  "diagnosis_verdict": "salvageable",
  "diagnosis_confidence": 0.82,
  "staleness_by_file": {
    "README.md": {"classification": "drifting", "last_human_touch_age_days": 42},
    "docs/architecture.md": {"classification": "stale", "last_human_touch_age_days": 187},
    "CLAUDE.md": {"classification": "aspirational", "evidence": "references workflows/templates/ which does not exist"}
  },
  "discrepancy_files": [
    {
      "path": "README.md",
      "findings": ["contradicted", "missing_claim"],
      "status_on_next_run": "recheck"
    }
  ],
  "aspirational_claims": [
    {"file": "CLAUDE.md", "line_range": "45-52", "claim": "workflows are defined under workflows/templates/"}
  ],
  "open_issues": [
    {
      "finding_id": "F-2026-04-17-003",
      "disposition": "defer",
      "reason": "out_of_scope for current goal; severity=medium",
      "eligible_for_next_run": true
    }
  ],
  "files_analyzed": ["README.md", "CLAUDE.md", "docs/architecture.md"],
  "no_drift_files": ["LICENSE", "docs/contribute.md"],
  "findings_count": 7,
  "triage_summary": {"resolve": 3, "defer": 3, "escalate": 1}
}
```

### How the next run uses it

1. Look up the most recent `archeology-checkpoint.json` by walking `operations/projects/{project}/goals/**/artifacts/source-archeology/*/archeology-checkpoint.json` and picking the newest by `run_timestamp`.
2. If none exists, this is a first run — Excavate the full corpus.
3. If the repo's `pinned_head` equals checkpoint's `observed_head` AND the checkpoint's `open_issues` list is empty, emit a `noop` run record; skip all stages.
4. Otherwise, compute the union of:
   - files changed in `observed_head..current_head` (git diff)
   - files in `discrepancy_files` (recheck regardless of movement)
   - files in `open_issues[*].file_path` eligible for re-evaluation
   and run Excavate+Date on that union.
5. Run the remaining stages normally. Diagnose may re-classify the repo; deferred issues may upgrade to resolve if goal alignment has changed.
6. Write a new checkpoint.

## Scaffold Routing — Always to `{slug}-docs`

Scaffold stage output **always** routes to a separate `{slug}-docs` repo. Never to the source repo. This is load-bearing for three reasons:

1. **Source repo stays pristine.** Agents cannot inadvertently mutate code-repo content during onboarding. Only Reconcile targets the source, and only with Triage-approved findings.
2. **Docs repo is the canonical retrieval surface** (T129 AKB). If docs live in the source repo, they mix code-PR workflow with docs-update workflow. Keeping them separate makes each workflow cleaner.
3. **Bootstrap composability.** T127 Smart Onboarding Capability 3 (Auto-Provision Missing Docs Repo) already knows how to create `{slug}-docs`. Scaffold populates what Auto-Provision creates — one concern, two halves.

Findings that Archeology generates AGAINST the source repo (e.g. "README references a deleted script") flow into Triage as source-repo findings. Triage's decision tree can `resolve` them (produce a patch against the source, apply via the existing Reconcile → Verify path), `defer` (flag in the checkpoint as an open issue for future handling), or `escalate` (surface to operator). Triage disposition applies per-finding regardless of whether the finding originated in Reconcile or in Scaffold's source-side analysis.

## Per-API Connector Model (Sandbox ↔ Host)

Sandbox ↔ host communication uses a **per-API connector model**: each sandbox→host capability has its own scope-minimal primitive. Today's connectors:

- **Git push connector** — `X-Symbiotic-Push-Session` from `internal-git-swarm.md`, scoped to git push operations from sandbox to internal bare mirror.
- **Bundle extraction connector** — host-initiated Sysbox exec-visible file read; no sandbox-side token.
- **LLM gateway connector** — daemon-issued bearer token scoped to a single LLM session, validated by the LLM gateway's auth layer.

Future sandbox→host APIs (telemetry, structured progress events, triage-decision introspection, remote-debug) are spec'd as **separate connectors with their own auth primitives**, not as extensions of any existing token. Scope-minimal, failure-localized. Source Archeology requires no new connector beyond the three above.

## Typed Artifact Taxonomy

Source Archeology extends T116's typed bundle taxonomy with its own artifacts. `bundle_schema_version: 3` indicates a Source Archeology bundle.

| Artifact type | Source stage | Archive destination |
|---|---|---|
| `excavation_report` | Excavate | `goals/{goal}/artifacts/source-archeology/{ts}/excavation-report.json` |
| `staleness_report` | Date | `...staleness-report.json` |
| `diagnosis` | Diagnose | `...diagnosis.json` |
| `finding` | Reconcile or Scaffold | `...findings.json` |
| `triage_decision` | Triage | `...triage-decisions.json` |
| `scaffold_package` | Scaffold | `...scaffold-package/*` |
| `archeology_patch` | Reconcile (post-Triage resolve) | `...patchset/*.patch` |
| `archeology_report` | Handoff / final summary | `...archeology-report.md` |
| `archeology_metadata` | all | `...metadata.json` |

Routing policy: all nine types land under the triggering goal. None route to the vault ledger. Promoting a finding to a vault fact is a separate `vault.process` call.

## Integration with T127 Project Bootstrap

T127's Phase 2 is renamed to **"Source Archeology"** (from "Documentation Reconciliation"). The role-conditional behavior extends:

| `repo_role` | Phase 2 behavior |
|---|---|
| `source` | Run full Source Archeology. Diagnose picks branch. Scaffold routes to `{slug}-docs` via T127 Auto-Provision. |
| `docs_akb` | Run Source Archeology in Reconcile-only mode (a docs repo's archeology is doc-reconciliation-only; it does not scaffold against itself). |
| `reference_library` | Skip Source Archeology entirely. |

Scaffold → T127 Auto-Provision composition: when Scaffold has output to write and no `{slug}-docs` exists, Scaffold emits a `scaffold_package` artifact and a `need_docs_repo` signal. T127's Auto-Provision Missing Docs Repo capability reads the signal, creates `{slug}-docs`, attaches it as `docs_akb`, and populates with the package. The two are strictly decoupled — Source Archeology doesn't know about git-repo creation, and T127 doesn't know about scaffold content generation.

## Integration with T129 Attached Knowledge Base

- T129 AKB indexer observes changes to attached docs_akb repos via `repo_mirror_pull_completed`. When Source Archeology's Scaffold populates a newly-auto-provisioned `{slug}-docs`, the resulting initial commit triggers the AKB initial build normally.
- When Source Archeology's Reconcile patches land on a source repo's docs, and the source repo is ALSO attached as `docs_akb` (rare but allowed), the next mirror pull triggers an AKB reindex.
- The AKB is never a direct target of Source Archeology. AKB is derived runtime substrate; it doesn't get patched.

## Integration with T116 Internal Git Swarm

- Reuses T116's sandbox runtime (Archeology Sandbox = Distillery Sandbox class with different worker images).
- `CHECK_NAME=archeology` differentiates Source Archeology runs from vault Distillery runs in logs, metrics, and bundle-extraction plumbing.
- Branch protection hooks on the internal bare mirror apply: patchset branches produced by Reconcile must land under `agent/archeology/*`.
- Does not reuse the post-merge distillery dispatch path used by the vault Distillery. Source Archeology runs during project onboarding or on an `on_drift_detected` hook — there is no "post-merge" concept.

## Security Rules

- Sandbox has a read-only clone of the source repo; no credentials injected.
- Sandbox has no network egress. All observations come from the read-only clone.
- Reconcile's `file.write` scope is limited to `/workspace/patchset/`.
- Scaffold's `file.write` scope is limited to `/workspace/scaffold-package/`.
- `allowed_paths` enforced at two points (sandbox write-scope + verifier).
- A finding with `severity: security` (e.g. docs claim secrets-in-env-file are acceptable) always triggers `escalate` regardless of other Triage inputs.
- `repo_role` gate: Source Archeology MUST reject any `ArcheologyTarget` where `RepoManifest.repo_role == ReferenceLibrary`. `reference_library` repos are read-only to the whole harness.

## Failure Modes

| Failure | Handling |
|---|---|
| Clone fails (network / auth) | Phase fails cleanly; operator notified. No partial state. |
| Excavate fails on a parseable file | Observation recorded as `parse_error`; pipeline continues. |
| Date stage LLM timeout on aspirational-claim detection | Retry once; then mark affected artifacts as `drifting` conservatively. |
| Diagnose confidence below threshold | Force verdict to `needs_operator_input` regardless of the classifier's output. |
| Reconcile produces a patch outside `allowed_paths` | Verifier rejects; Triage re-evaluates; finding may be downgraded to `defer`. |
| Scaffold produces file outside `/workspace/scaffold-package/` | Sandbox denies write; scaffold finding rejected. |
| Triage decision tree produces no disposition | Hard error; treat as `escalate` by default. |
| Verify rejects a patch | Finding downgrades to `defer`; surfaced in checkpoint. |
| Markdownlint fails | Reject affected findings; max 2 retries before escalation. |
| External push fails (auth, rate limit) | Patchset remains on internal bare mirror; operator pinged. |

## Test Strategy

| Test | Type | Description |
|---|---|---|
| Excavate on clean fixture repo | Unit | Observations match expected structure. |
| Date classifies aspirational claim | Unit | CLAUDE.md claiming non-existent workflows is flagged `aspirational`. |
| Diagnose picks `salvageable` | Integration | Repo with current-but-drifted docs. |
| Diagnose picks `stale_beyond_salvage` | Integration | Repo where most docs describe prior architecture. |
| Diagnose picks `no_docs` | Integration | Repo with no docs at all. |
| Diagnose forces `needs_operator_input` on low confidence | Unit | Confidence below threshold overrides verdict. |
| Reconcile respects allowed_paths | Unit | Attempt to patch `src/` file; reconciler denied. |
| Scaffold routes to `{slug}-docs`, not source | Integration | Scaffold output never lands in source repo worktree. |
| Scaffold triggers T127 Auto-Provision when no docs repo | Integration | End-to-end: Diagnose=no_docs → Scaffold → Auto-Provision → `{slug}-docs` created → scaffold package populated. |
| Triage `critical` finding escalates | Unit | Regardless of other inputs. |
| Triage `manual` autonomy escalates | Unit | All findings escalate under `manual`. |
| Triage `auto` + `in_scope` + `medium` severity resolves | Unit | Decision tree happy path. |
| Triage peer-review disagreement escalates | Integration | Two agents, different dispositions → escalate. |
| Verify rejects non-applicable patch | Unit | Hand-craft patch against old SHA; reject. |
| Checkpoint open-issue carryover | Integration | Deferred finding re-evaluated on next run. |
| Full happy path on `git:flux` fixture | Integration | End-to-end, Diagnose=salvageable, Triage mostly resolves, some defer. |
| Autonomy gating (`manual` mode) | Integration | No source push without explicit approval. |
| Credential isolation | Security | Credential value never appears in any VM env, log, or artifact. |
| Typed bundle shape | Contract | Snapshot-test the bundle against schema v3. |

## Module Layout

```text
submodules/runtime/
  crates/symbiotic-agents/src/
    archeology_target.rs          # ArcheologyTarget type + ArcheologyMode enum
    source_archeology.rs          # pipeline impl (six stages)
    archeology_stages/
      excavate.rs
      date.rs
      diagnose.rs
      reconcile.rs
      scaffold.rs
      handoff.rs
      triage.rs
      verify.rs
    archeology_findings.rs        # Finding type + serialization
    archeology_decision_tree.rs   # Triage decision tree (MVP declarative)
    archeology_scaffolds.rs       # scaffold template lookup (shared with T127)
  services/symbiotic-daemon/src/
    archeology_dispatch.rs        # decides push-vs-hold post-Triage
    archeology_checkpoint.rs      # checkpoint read/write + open-issue carryover
templates/
  external-repo-scaffolds/        # (inherited from prior draft; template set unchanged)
    rust-workspace/
    sveltekit/
    monorepo-with-submodules/
    fallback/
    docs-starter/                 # used by Scaffold stage when auto-provisioning {slug}-docs
    ...
```

## Open Questions

- **Triage decision-tree evolution**: MVP is a declarative tree with four inputs. Post-MVP should the tree become an LLM-reasoning stage that can learn from historical operator decisions? Likely yes; track as a post-MVP task.
- **"Operator mood" richer signals**: MVP reads autonomy level + recent thread activity. Should we parse explicit `/mood aggressive` / `/mood minimal` commands? Operator decision.
- **Peer-review orchestration**: MVP runs at most one peer reviewer per Triage run. Post-MVP, a panel of reviewers with voting? Probably overkill; defer.
- **Deferred-issue expiry**: open_issues accumulated over many runs could grow unbounded. Need a policy for pruning stale defers. Proposed: `defer_ttl_days` on ArcheologyTarget, default 90. Confirm.
- **Scaffold template selection heuristic**: template picker reads discovery-report.md signals (from T127 Phase 1). For first-time runs, the discovery report may not exist yet. Fallback: run a light Excavate pass to seed selection. Confirm.
- **Aspirational-vs-stale distinction**: the Date stage's classifier has to distinguish "described past behavior" from "described future behavior." The boundary is fuzzy. MVP will err toward `stale` and surface uncertainty as `needs_operator_input`. Revisit once we have corpus data.

## What This Replaces

This doc supersedes `docs/design/external-repo-distillery.md` (moved to `source-archeology.md` via `git mv` on 2026-04-17). The prior draft's framing (Distillery-stage reuse, "distillery target variant") is no longer canonical. The per-API connector model, `allowed_paths` authored per-invocation, scaffold template inheritance, and the bundle extraction path are all preserved verbatim; the stage model, artifact taxonomy, and decision-tree Triage primitive are new.

Deferred composition questions (T120 audit-level default for archeology runs, retention alignment with PE graduation, `kind` tags, PE goal-type bucket, RedactionEngine tuning) remain deferred per prior operator resolution (b); they apply to Source Archeology runs identically to how they applied to the prior "External Repo Distillery" framing.
