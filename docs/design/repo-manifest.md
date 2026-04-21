# Repo Manifest — Durable Project-Attached Repositories

**Status**: Proposed Specification
**Related Tasks**: T126 (this doc), T127 (Project Bootstrap Process), T128 (Source Archeology), T129 (Attached Knowledge Base), T116 (Internal Git Swarm)
**Related Docs**: `docs/design/project-goal-process-model.md`, `docs/design/agent-company-model.md`, `docs/design/internal-git-swarm.md`, `docs/design/credential-sandbox.md`, `docs/design/trust-capabilities.md`, `docs/design/attached-knowledge-base.md`

## Problem

`docs/design/project-goal-process-model.md` specifies that a project manifest carries a `repos: [...]` array (e.g. `repo:symbiotic`, `repo:symbiotic-runtime`, `repo:symbiotic-app`). It does not yet specify what a `repo:*` reference resolves to: where the manifest lives, what fields it carries, how the source URL is pinned, which credential the agents use, or how the durable Project-attached repo relates to the ephemeral per-goal bare repos already described in `agent-company-model.md` §3.

Without that contract, an operator cannot declaratively say *"attach `git@github.com:kakajansh/flux.git` to `project:flux` and let the harness operate on it"*. This doc fills that gap.

## Non-Goals

- This doc does not redesign the per-goal internal bare repositories. Those remain ephemeral, hosted by the Nucleus's passive git server, spun up per goal as described in `internal-git-swarm.md`.
- This doc does not introduce a second credential store. All credentials continue to flow through `credential-sandbox.md` and `trust-capabilities.md`.
- This doc does not mandate a specific hosting provider (GitHub, GitLab, Gitea, local bare). `source_url` is provider-agnostic.
- **This doc is NOT the path for one-shot repo ingestion.** When the operator's intent is "capture this repo's content as ambient Archive entries for ambient retrieval" (same shape as a tweet ingest or URL ingest — "remember this library's ideas"), the correct primitive is the existing **Intake** pipeline with a git URL as the source. Do NOT create a `RepoManifest` for one-shot content capture. See §Repo Intent Taxonomy below.

## Repo Intent Taxonomy — Three Separate Flows

Repos show up in Symbiotic in three distinct ways. Each has its own primitive, its own storage, and its own lifecycle. Conflating them leads to either over-heavy state (creating a durable mirror for content you only wanted to read once) or under-heavy state (trying to drive agent work against a repo that was never actually attached).

| Intent | Primitive | Storage | Lifecycle | Example |
|---|---|---|---|---|
| **Ingest** — capture repo content as ambient knowledge | **Intake** pipeline (existing; same shape as tweet/URL/file ingestion) | Archive entries synthesized via Distillery into `knowledge-base/ledger/**` | One-shot, content-only | "Ingest `github.com/tokio-rs/axum` README + docs/ as reference material in my personal vault." Lives as Archive entries. No mirror, no credential, no agent worktrees. |
| **Attach** — bind a repo to a project for agent work | **T126 Repo Manifest** + **T127 Project Bootstrap** | Durable internal bare mirror + credential binding + (if `docs_akb`) AKB index | Long-lived, project-scoped | "Attach `github.com/kakajansh/flux` to `project:flux` so Symbiotic can drive work on it." Creates `operations/projects/flux/repos/flux.md`. |
| **Analyze** — audit or reconcile an already-attached repo's docs | **T128 Source Archeology** | Archeology artifacts under the driving goal's `artifacts/source-archeology/{ts}/` | Periodic or on-trigger, per-run | "Check whether `repo:flux`'s docs are still accurate; reconcile or scaffold as needed." Only runs over a repo that is already attached via T126. |

**Disambiguation rules**:

1. If the operator says *"ingest this repo"* / *"remember this project"* / *"add as reference"* → Intake flow. Never `RepoManifest`.
2. If the operator says *"onboard this project"* / *"start driving X"* / *"attach this as a project repo"* → T127 Project Bootstrap (which creates `RepoManifest`s per attached source).
3. If the operator says *"check for doc drift on X"* / *"audit X's docs"* → T128 Source Archeology (requires X to already be attached).
4. If intent is ambiguous, bootstrap asks. Default fallback is Intake (the cheaper, one-shot path) — upgrading to Attach later is always possible; downgrading from Attach to Intake is expensive (have to tear down the mirror, credential, AKB).

**Where ingested repo content CAN support project work**: Archive entries produced by Ingest can carry `related_projects: [...]` frontmatter (see `attached-knowledge-base.md` §Federated Recall §Ranking) and thereby show up as project-proximate in Recall results. So "ingested Axum docs" can still be retrievable for an agent working on a project that uses Axum, without being an attached reference-library repo. The choice between Ingest-with-project-tag vs. Attach-as-reference-library is:

- **Ingest-with-project-tag**: lightweight, lives in personal vault, good for small or periodically-refreshed corpora.
- **Attach-as-reference-library**: durable AKB index with tier-aware retrieval, good for large corpora the operator wants mirrored locally with freshness guarantees.

## Core Decision

A **Repo Manifest** is a durable Archive-native record describing an external source repository that has been attached to a Project. It lives under the project's folder, is version-controlled with the rest of the Archive, and is the only place the daemon looks up source-URL / auth / mirror / checkout policy for that repo.

Canonical path:

```text
knowledge-base/
  operations/
    projects/
      {project}/
        project.md
        repos/
          {repo}.md
        goals/
        processes/
```

The project manifest's `repos: [repo:{slug}, ...]` array is resolved by slug against the sibling `repos/{slug}.md` file. No indirection, no central registry, no hidden daemon config.

## Canonical Frontmatter

Each repo manifest is a Markdown record with typed frontmatter and a short human-readable body.

```yaml
---
id: "repo:flux"
project_id: "project:flux"
slug: "flux"
title: "Flux (primary)"
state: active                    # active | paused | detached
repo_role: source                # source | docs_akb | reference_library (see T129)

source:
  url: "git@github.com:kakajansh/flux.git"
  provider: github               # github | gitlab | gitea | local | other
  default_branch: "main"
  protected_branches: ["main", "preview"]
  pinned_head: "777f1290abcd..."  # optional, used for verification

credential:
  id: "credential:github-flux-push"   # resolved via credential-sandbox
  scope: "push"                        # read | push | admin
  trust_floor: "CredentialAccess"

mirror:
  internal_bare_path: "data/git-server/repos/flux.git"  # Nucleus-owned
  direction: "bidirectional"           # pull_only | push_only | bidirectional
  sync_interval_secs: 300
  last_pulled_at: null
  last_pushed_at: null

checkout:
  worktree_root: "data/worktrees/flux/"
  agent_branch_prefix: "agent/"
  max_concurrent_worktrees: 4
  cleanup_on_goal_close: true

agent_scopes:
  read: ["archive.read", "file.read"]
  write: ["archive.read", "archive.write", "file.read", "file.write", "vm.exec"]
  push_external: false             # whether the post-merge push to source.url is allowed
  requires_operator_approval_for: ["push_external", "force_push", "delete_branch"]

hooks:
  on_attach: "process:repo.onboard"    # optional: run this Process on attach
  on_drift_detected: "process:repo.drift_review"
  on_detach: null

# indexing: present ONLY when repo_role != source. Drives the Attached Knowledge
# Base pipeline (T129). For repo_role: source this block is omitted; AKB never
# indexes a source repo's code tree — T128 Source Archeology is the only
# write path that touches source repos.
indexing:
  root_paths: ["docs/", "README.md", "AGENTS.md", "CLAUDE.md"]
  exclude_patterns: ["**/node_modules/**", "**/target/**", "**/.git/**"]
  distillery_config:
    enable_reweave: false              # AKB never rewrites its source; read-only index
    enable_semantic_verify: true
    model: "qwen3.5"
  tier_policy:
    default_tier: library              # raw | distilled (AKB two-tier model — see T129)
    auto_promote: false
  refresh:
    on_commit: true
    interval_secs: 3600
    incremental: true

metadata:
  attached_at: "2026-04-16T00:00:00Z"
  attached_by: "operator"
  notes: "Primary SvelteKit + Rust Axum monorepo."
---

# Flux

Primary product repository. Bidirectional mirror; external push requires operator
approval per `agent_scopes.requires_operator_approval_for`. CI lives in
`.github/workflows/`, so Distillery verification should tolerate the delay between
local merge and external CI completion.
```

### Field Semantics

- **`id`** — canonical identifier matching the `repos: [...]` array on `project.md`. Must be unique within the Archive; format `repo:{slug}`.

- **`project_id`** — owning project. A repo manifest always lives under exactly one project; sharing a source URL across projects requires a separate `repo:` record in each (intentional — carries its own credential scope and agent policy).

- **`state`** — lifecycle flag. `active` means agents may read/write per `agent_scopes`. `paused` freezes mirror and agent activity without detaching. `detached` means the record is kept for audit but no runtime surface will touch it.

- **`repo_role`** — discriminator that selects the runtime surface the repo participates in. See T129 (`attached-knowledge-base.md`) for the full semantics. Three values:
  - `source` — a code repo that agents may clone, worktree, and (gated by `agent_scopes.push_external`) push back to via T128's Source Archeology patchset flow. `indexing` block MUST be absent.
  - `docs_akb` — an Attached Knowledge Base repo (typically a project-docs repo). The daemon builds a derived index under `data/akb/{repo_id}/` with the Distillery's Reduce/Classify/Reflect/Verify stages (Reweave DISABLED). Agents may query it via `RecallScope::Akb` but cannot spawn worktrees against it and cannot be targets of `push_external`. `indexing` block MUST be present.
  - `reference_library` — a read-only reference corpus indexed the same way as `docs_akb`, but with a stricter write ceiling: even a T128 patchset targeting it is rejected by `AccessBroker`. `indexing` block MUST be present.
  The `AccessBroker` consults this field at token-issue time; agent scopes listed in `agent_scopes.write` on a `docs_akb` or `reference_library` repo are ignored for anything except AKB reindex operations.

- **`source.url`** — canonical external URL. The daemon never substitutes this. Supports `ssh://`, `https://`, `file://`, and `git@host:owner/repo.git` forms.

- **`source.provider`** — coarse provider tag used only for hook selection (e.g. drift-review flow posts a PR differently on GitLab vs. Gitea). Opaque `other` is always valid.

- **`source.protected_branches`** — branches the internal bare mirror refuses to accept direct agent pushes to. Mirrors the update-hook pattern from `agent-company-model.md` §3 but applies it at the durable-repo level, not only per ephemeral goal repo.

- **`source.pinned_head`** — optional SHA. When present, the Project Bootstrap Process (T127) records the head at onboarding time; later drift checks compare against it to detect upstream history rewrites.

- **`credential.id`** — reference into `credential-sandbox.md` storage. The daemon resolves this at mirror/push time; agents never see the raw credential. `scope` narrows what the credential may do (read-only mirror vs. push-capable).

- **`credential.trust_floor`** — minimum trust level for an agent to request a capability token against this repo's operations. Composes with the agent's LLM trust ceiling (see `trust-capabilities.md`).

- **`mirror.internal_bare_path`** — daemon-owned durable bare repo backing this source. This is different from the per-goal ephemeral bare repos: it survives across goals, is the long-lived mirror of the external source, and is the canonical git object store for Project-scoped operations.

- **`mirror.direction`** — `pull_only` (external is authoritative, local is read-only cache), `push_only` (local authors, external receives — uncommon), `bidirectional` (the common case, with conflict mediation via `sovereign-sync.md` semantics applied per-repo instead of per-vault).

- **`checkout.worktree_root`** — where agent worktrees are materialized. Worktrees under this root are per-goal, per-agent, and cleaned up per `cleanup_on_goal_close`. Aligns with the existing `CONTEXT.md` worktree-isolation rules: agent worktrees live in per-submodule directories, never at the parent Archive level.

- **`agent_scopes.read` / `.write`** — capability scope sets an agent may request for read-only or write operations inside this repo. These compose with the agent's `task_spec.capabilities` via `AccessBroker::issue_tokens`.

- **`agent_scopes.push_external`** — gates whether the Nucleus may perform a `git push` from `internal_bare_path` to `source.url`. Default `false` during onboarding; flipped to `true` only after operator approval lands.

- **`agent_scopes.requires_operator_approval_for`** — explicit list of **operation names** (not escalation-mode names) that, when attempted, trigger the escalation mode configured on the containing project's `policy.escalation.mode` (`notify_operator` or `raise_alert` per `archive-policy-scope-hierarchy.md`). Default escalation mode when a gated operation fires is `raise_alert`; the request blocks until the operator approves. Corresponds to the approval-gate pattern already live in `declared_task_policy`.

- **`hooks.on_attach`** — Process that runs when the repo first transitions into `state: active`. Typically `process:repo.onboard`, but operators may point it at project-specific bootstrap Processes. Resolved against `operations/projects/{project}/processes/*.md`.

- **`hooks.on_drift_detected`** — Process that runs when the Source Archeology (T128) detects doc/state drift between the source repo and the Archive's project node.

- **`hooks.on_detach`** — optional cleanup Process.

## Integration with Existing Components

### With `project-goal-process-model.md`

The `repos: [...]` field on `project.md` is now resolvable. The daemon's project parser, when reading `project.md`, fans out to `repos/*.md` and loads each as a `RepoManifest`. The project runtime state carries a `Vec<RepoManifest>` keyed by slug.

### With `agent-company-model.md`

The "internal bare repo mirrors to external remote" pattern described there becomes the default runtime behavior when a repo manifest has `mirror.direction: bidirectional` and `agent_scopes.push_external: true`. The per-goal ephemeral bare repos spin up as clones of the durable `internal_bare_path`, not of the external source — this isolates agents from external-provider flakiness and from rate limits.

### With `internal-git-swarm.md`

The internal git swarm server now hosts two tiers of repos:

1. **Durable project-attached bare repos** — one per repo manifest, lifetime = repo manifest lifetime.
2. **Ephemeral goal bare repos** — one per goal, lifetime = goal lifetime, cloned from (1).

Branch protection hooks apply to both tiers, but the durable tier gates against `source.protected_branches`; the ephemeral tier gates per-goal feature branches. Concretely, the daemon installs `post-receive` and `update` hooks on every `mirror.internal_bare_path` at registry-load time; these hooks are daemon-owned and are not user-editable artifacts. The ephemeral per-goal hooks layer on top of the durable hooks with narrower (feature-branch) scope.

### With `sovereign-sync.md`

Repo mirror loops are separate from the Archive sync loop, but they reuse the same conflict-mediation primitives. A mirror conflict produces the same Matrix-quick-reply mediation UX (`[Keep Mine]` / `[Keep Yours]` / `[Merge Both]`) with subject `repo:{slug}` instead of `vault`.

### With `credential-sandbox.md`

`credential.id` is an opaque handle. The daemon resolves it at mirror/push time inside the credential sandbox; the agent process never receives it. Pre-push verification uses the same `./scripts/pre-push-verify.sh` pattern when the credential is a local SSH key, or provider-specific OAuth flows when not.

## Type Signatures

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoManifest {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub state: RepoState,
    #[serde(default = "RepoRole::default_source")]
    pub repo_role: RepoRole,

    pub source: RepoSource,
    pub credential: RepoCredentialBinding,
    pub mirror: RepoMirrorPolicy,
    pub checkout: RepoCheckoutPolicy,
    pub agent_scopes: RepoAgentScopes,
    pub hooks: RepoHooks,

    /// Present iff `repo_role != Source`. Drives T129 AKB pipeline.
    #[serde(default)]
    pub indexing: Option<RepoIndexingPolicy>,

    pub metadata: RepoMetadata,

    #[serde(skip)]
    pub body_markdown: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoState { Active, Paused, Detached }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoRole { Source, DocsAkb, ReferenceLibrary }

impl RepoRole {
    pub fn default_source() -> Self { RepoRole::Source }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndexingPolicy {
    pub root_paths: Vec<String>,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    pub distillery_config: RepoDistilleryConfig,
    pub tier_policy: RepoTierPolicy,
    pub refresh: RepoRefreshPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDistilleryConfig {
    pub enable_reweave: bool,
    pub enable_semantic_verify: bool,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoTierPolicy {
    pub default_tier: AkbTier,
    pub auto_promote: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AkbTier { Raw, Distilled }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRefreshPolicy {
    pub on_commit: bool,
    pub interval_secs: u64,
    pub incremental: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSource {
    pub url: String,
    pub provider: RepoProvider,
    pub default_branch: String,
    #[serde(default)]
    pub protected_branches: Vec<String>,
    #[serde(default)]
    pub pinned_head: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoProvider { Github, Gitlab, Gitea, Local, Other }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoCredentialBinding {
    pub id: String,
    pub scope: CredentialScope,
    pub trust_floor: TrustLevel,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScope { Read, Push, Admin }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMirrorPolicy {
    pub internal_bare_path: PathBuf,
    pub direction: MirrorDirection,
    pub sync_interval_secs: u64,
    #[serde(default)]
    pub last_pulled_at: Option<String>,
    #[serde(default)]
    pub last_pushed_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MirrorDirection { PullOnly, PushOnly, Bidirectional }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoCheckoutPolicy {
    pub worktree_root: PathBuf,
    pub agent_branch_prefix: String,
    pub max_concurrent_worktrees: u16,
    pub cleanup_on_goal_close: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoAgentScopes {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub push_external: bool,
    #[serde(default)]
    pub requires_operator_approval_for: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoHooks {
    #[serde(default)]
    pub on_attach: Option<String>,
    #[serde(default)]
    pub on_drift_detected: Option<String>,
    #[serde(default)]
    pub on_detach: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMetadata {
    pub attached_at: String,
    pub attached_by: String,
    #[serde(default)]
    pub notes: String,
}
```

## Resolution Rules

When an agent or Process references `repo:{slug}` inside a project context:

1. Load `knowledge-base/operations/projects/{project}/repos/{slug}.md`.
2. If `state != active`, reject the request and emit a canonical `repo_unavailable` goal event.
3. Resolve the credential via `credential-sandbox.md` only when a mirror/push action actually needs it; agent worktree reads use `internal_bare_path` directly.
4. Compose the agent's requested capability scopes against `agent_scopes.read` or `agent_scopes.write` using `AccessBroker::issue_tokens`. Scopes not listed on the repo manifest are denied even if the agent's trust ceiling would otherwise permit them.
5. For external push (`git push {internal_bare_path} → source.url`), check `agent_scopes.push_external` *and* run the approval gate for any member of `requires_operator_approval_for`.
6. **Role gate.** If `repo_role != Source`, all worktree-materialization, branch-creation, and `push_external` operations are denied unconditionally regardless of `agent_scopes`. The only capability a non-`Source` repo grants at runtime is `akb.query` (via `RecallScope::Akb { repo }`, see T129). If `repo_role == ReferenceLibrary`, even T128 patchset production is rejected.
7. **Indexing fan-out.** On successful attach (and on each mirror pull) of a repo with `repo_role != Source`, the daemon enqueues an AKB (re)build job scoped to `indexing.root_paths`. Missing or malformed `indexing` block on a non-`Source` repo is a hard load-time error.

## Module Layout

```text
submodules/runtime/
  crates/symbiotic-control-plane/src/
    repo_manifest.rs          # RepoManifest type + parser
    manifest.rs               # extend with project→repo fan-out
  services/symbiotic-daemon/src/
    repo_registry.rs          # in-memory RepoManifest index, event-driven reload
                              # on `repo_attached` / `repo_state_changed` /
                              # `repo_detached` lifecycle events (not FS-watch)
    repo_mirror.rs            # mirror loop per active repo
    repo_capabilities.rs      # scope composition with AccessBroker
```

## Lifecycle Events

Repo manifest lifecycle emits canonical goal/project events so the Archive carries full history:

- `repo_attached` — first write; `attached_at` set
- `repo_state_changed` — `active ↔ paused ↔ detached`
- `repo_mirror_pull_completed` / `repo_mirror_push_completed`
- `repo_mirror_conflict_opened` / `repo_mirror_conflict_resolved`
- `repo_drift_detected` — emitted by T128 Source Archeology
- `repo_external_push_approval_requested` / `repo_external_push_approved` / `repo_external_push_denied`
- `akb_created` — emitted on first attach of a repo with `repo_role != Source`; attachment exists but the index has not yet been populated (AKB is NOT yet queryable)
- `akb_ready` — emitted when the initial index build completes and the AKB becomes queryable for the first time. This is the event agents and the operator's Matrix handoff wait on; `akb_created` alone is not sufficient to query.
- `akb_reindexed` — emitted on each subsequent rebuild (incremental or full) after `akb_ready` has fired at least once
- `akb_corruption_recovered` — emitted when a corruption-triggered full rebuild completes
- `repo_detached`

These follow the same append-only event shape already used for goal/task events under `knowledge-base/operations/projects/{project}/goals/{goal}/events/*.md`; repo events may live under `operations/projects/{project}/repos/events/*.md` to avoid forcing every repo action into a goal scope.

## Migration Strategy

Existing project manifests (there is only one today: the implicit `project:inbox` fallback) carry no `repos` array. Migration is additive:

1. Ship the parser and in-memory registry with zero active repo manifests — runtime behavior unchanged.
2. Operator adds the first repo manifest (e.g. `repo:flux`) via the Project Bootstrap Process (T127).
3. Mirror loop comes online for that one repo; everything else is untouched.
4. Over time, additional repos attach as projects are onboarded.

No fallback shims, no backward-compat branches — consistent with the `NEXT.md` rebuild rule.

## Security Notes

- The Nucleus never gives an agent process access to `credential.id` directly. Mirror and push operations run host-side inside the credential sandbox.
- `push_external: true` combined with an empty `requires_operator_approval_for` is a supported but loud configuration — the daemon emits a `warning` trace on load and surfaces a one-time operator confirmation before the first external push.
- `source.protected_branches` enforcement is duplicated at both the internal bare server's update hook *and* the pre-external-push verifier. Defense in depth.
- A detached repo (`state: detached`) retains its manifest for audit but its `internal_bare_path` should be archived to a read-only location to prevent accidental reuse of stale credentials.
- `repo_role: docs_akb` and `repo_role: reference_library` repos are indexed inside the Distillery-Sandbox (no internet, proxy-enriched) just like Archive Distillery runs. AKB derivatives under `data/akb/{repo_id}/` live inside the vault's sensitivity envelope per T129 and are never shipped over the network outside sovereign-sync.

## Open Questions

- Should `pinned_head` be auto-advanced on every successful mirror pull, or only at explicit operator checkpoints? **Resolved: auto-advance, NO shadow field on the manifest.** Drift-detection memory (prior observed HEAD + set of files where discrepancies were found) is owned by T128 as a `drift-checkpoint.json` artifact under each drift-check goal's `artifacts/` folder — see `source-archeology.md` §Drift Checkpoint Artifact. Separation of concerns: the manifest describes the repo attachment; T128 owns its own analysis memory. T126 `RepoSource` schema stays minimal.
- Do we need a `repo:*` reference to escape the project namespace (e.g. a shared tooling repo visible to multiple projects)? **Resolved: no.** Manifest is copied per project; cross-project reuse is revisited only if it becomes a concrete operator pain point.
- Role-gate enforcement point: `AccessBroker` at token-issue time is **primary**; attach-time is a **load-time schema check** that validates the `indexing` block shape on non-`Source` repos. Mirror-loop time is NOT an enforcement point — a bug in either primary path should surface at the other, not at the mirror loop.
