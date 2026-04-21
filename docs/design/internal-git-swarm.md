# Internal Git Swarm & Zero-Trust Distillery


**Status**: Phases 1–3 Implemented (Distillery, Linter, Integration Tests complete — see chunk checkmarks below)
**Task**: [T116](../../tasks/TASKS.md)
**Related**: T112 (Evolution Engine — blocked by this), T113 (Nuclear Split), T114 (Sysbox VmManager)

---

## Overview

Enable agents running in Docker/Sysbox sandboxes to collaborate on code via Git, with PR-based review, branch protection, CI checks, and a post-merge distillery — without the daemon ever running git commands. The daemon is a pure Rust orchestrator; git CLI only runs inside containers.

This is Symbiotic's internal GitHub: a secure collaboration layer for agent swarms.

---

## Hard Constraints

1. **Daemon = pure Rust orchestrator.** NO git CLI on the host. Git only runs inside containers.
2. **Branch protection** must enforce per-agent, per-branch permissions (like GitHub branch rules).
3. **Vault is separate.** The Vault (knowledge-base, passwords) has its own sandboxed environment with its own git + AccessBroker. NOT part of the swarm. See [Vault Sandbox Decision](#vault-sandbox-decision).

---

## Architecture

```
┌──────────────────┐        ┌────────────────────────────────┐
│  Git Server      │        │  Daemon (Rust)                 │
│  Container       │        │                                │
│  (Alpine + git   │        │  ┌─────────────┐               │
│   + http-backend)│◄──mgmt─┤  │ SwarmManager│ create/destroy│
│                  │        │  └──────┬──────┘ repos & server│
│  pre-receive ────┼──auth──┤  ┌──────┴──────┐               │
│  hook (HTTP)     │        │  │  PRManager  │ PR lifecycle  │
│                  │        │  └──────┬──────┘               │
│  /repos/*.git    │        │  ┌──────┴──────┐               │
│  (bare repos)    │        │  │ MergeRules  │ branch protect│
└────────┬─────────┘        │  └──────┬──────┘               │
         │                  │  ┌──────┴──────┐               │
    git clone/push          │  │CIDispatcher │ spawn checkers│
         │                  │  └─────────────┘               │
┌────────┴─────────┐        │                                │
│  Worker Agent VM │        │  AccessBroker (symbiotic-trust)│
│  (Sysbox)        │        │  CapabilityToken scopes:       │
│                  │        │    git.read, git.push           │
│  tools:          │        │    pr.create, pr.review         │
│    git_clone     ├─bridge─┤    pr.merge, check.report      │
│    git_push      │        └────────────────────────────────┘
│    pr_create     │                    │
│    pr_comment    │              JSON-RPC bridge
│                  │                    │
└──────────────────┘        ┌───────────┴────────────┐
                            │  Reviewer Agent VM     │
                            │  (Sysbox)              │
                            │                        │
                            │  tools:                │
                            │    git_clone            │
                            │    pr_list_comments     │
                            │    pr_approve           │
                            │    pr_request_changes   │
                            └────────────────────────┘
```

### Design Decisions

**Git server = sidecar container (not in daemon)**
- Alpine-based container running `git-http-backend` via lighttpd `mod_cgi`
- Daemon manages it via bollard (create, exec, destroy)
- `pre-receive` hook in the container calls daemon HTTP API for authorization
- **Verified** (Session 97): CGI header propagation confirmed end-to-end through lighttpd

**PR system modeled after GitHub**
- `SwarmPR` is a first-class object managed by the daemon
- Agents interact with PRs via JSON-RPC tools (like GitHub's REST API)
- Reviewers leave comments, approve/request changes
- CI agents run tests and report status
- Merge rules gate landing (N approvals, all checks pass, branch patterns)

**Capability scopes for git operations**
- New scopes: `git.read`, `git.push`, `git.push:protected`, `pr.create`, `pr.review`, `pr.merge`, `check.report`
- Integrated with existing `AccessBroker` + `CapabilityToken` (arbitrary string scopes)
- Trust level mapping: `git.read` → ReadOnly, `git.push` → ArchiveWrite, `pr.merge` → ExternalAct

**Push identity propagation**
- Branch protection now uses a **daemon-issued short-lived push session**. The agent runner asks the daemon for a push session over the existing JSON-RPC bridge before `git push`.
- The runner sends that session through the git HTTP transport as `X-Symbiotic-Push-Session`.
- `git-http-backend` exposes CGI environment variables to hooks, so the server-side `pre-receive` hook can read `HTTP_X_SYMBIOTIC_PUSH_SESSION`, call `/api/git/authorize`, and let the daemon validate both the short-lived session and the underlying `CapabilityToken`. Git documents that `REMOTE_USER`, `REMOTE_ADDR`, and other CGI variables are available to hooks, and that all CGI environment variables are available to `git-receive-pack` hooks. [Source](https://git-scm.com/docs/git-http-backend.html)
- This avoids the incorrect shared-container-env assumption and binds a push to a specific daemon-issued authorization context.
- **Verified end-to-end** (Session 97): A live container push confirmed lighttpd/CGI preserves `X-Symbiotic-Push-Session` as `HTTP_X_SYMBIOTIC_PUSH_SESSION` in the `pre-receive` hook environment. See `submodules/runtime/docker/git-server/test-push-auth.sh` for the verification script.

---

## Vault Sandbox Decision

The Vault (knowledge-base, passwords, identity documents) is the most security-critical part of Symbiotic. It MUST live in a **separate sandboxed environment** — its own git server container with its own storage volume. Key reasons:

1. **Machine isolation**: The vault can run on a completely separate machine, air-gapped from the agent swarm. Even if the daemon host or agent containers are compromised, the vault is safe.
2. **Access boundary**: All reads/writes go through the Scribe/VaultManager API, which enforces AccessBroker rules. No direct filesystem access.
3. **Independent git history**: The vault's git repo is NOT a branch in the swarm — it's a fully separate repository with its own commit history, managed by the vault service.

```
Agent Swarm Git          Vault Git
(code collaboration)     (identity + knowledge)
──────────────────       ────────────────────
Daemon ──bollard──►      Daemon ──HTTP API──►
  Git Server Container     Vault Service Container
  /repos/task-*.git        /vault/knowledge-base.git
  Branch protection        Scribe gatekeeper
  PR-based merges          AccessBroker-gated writes
  Can be ephemeral         Persistent, backed up
```

The vault sandbox is NOT part of T116 — it's tracked as a future evolution. T116 implements the agent swarm git server. The vault keeps its current model (VaultWriter + vault_git.rs) until the vault service is extracted.

---

## Key Types

```rust
// === symbiotic-git-swarm/src/types.rs ===

pub type SwarmRepoId = String;
pub type PullRequestId = String;

/// A bare git repo managed by the swarm system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmRepo {
    pub id: SwarmRepoId,
    pub container_path: String,        // Path inside git server container
    pub created_at: u64,
    pub status: SwarmRepoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwarmRepoStatus { Active, Completed, Failed }

/// A pull request — modeled after GitHub PRs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmPR {
    pub id: PullRequestId,
    pub repo_id: SwarmRepoId,
    pub branch: String,                // e.g. "feature/agent-42"
    pub base: String,                  // e.g. "main"
    pub title: String,
    pub description: String,
    pub author_agent: String,
    pub status: PRStatus,
    pub reviews: Vec<Review>,
    pub checks: Vec<CheckRun>,
    pub merge_rules: MergeRuleSet,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PRStatus { Open, Approved, Merged, Closed }

/// A review from a reviewer agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub reviewer_agent: String,
    pub verdict: ReviewVerdict,
    pub comments: Vec<ReviewComment>,
    pub submitted_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewVerdict { Approved, ChangesRequested, Commented }

/// A comment on a specific file/line in a PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub file: String,
    pub line: Option<u32>,
    pub body: String,
}

/// A CI check run result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub agent_id: String,
    pub status: CheckStatus,
    pub output: Option<String>,
    pub completed_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckStatus { Pending, Running, Success, Failure }

/// Branch protection + merge requirements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRuleSet {
    pub required_approvals: u32,
    pub required_checks: Vec<String>,          // Check names that must pass
    pub dismiss_stale_reviews: bool,
    pub allowed_merge_agents: Vec<String>,     // Agent IDs or "*"
}

/// Per-branch push rules (enforced by pre-receive hook).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRule {
    pub pattern: String,                       // Glob: "main", "release/*"
    pub allowed_push_scopes: Vec<String>,      // Capability scopes required
    pub require_pr: bool,                      // Must go through PR to land
}
```

---

## PR Lifecycle (GitHub-style)

```
Worker Agent                    Daemon                      Reviewer Agent
─────────────                   ──────                      ──────────────
git push feature/X ──────►  (git server container)
pr.create(branch,base,title) ─► PRManager.create()
                                │ status = Open
                                │ spawn reviewer VM ──────► clone repo
                                │ spawn CI VM                check diff
                                                             pr.comment(file,line,body)
                                ◄─── pr.request_changes() ──┘
                                │ forward to worker
◄── review_feedback ────────────┘
amend + git push ───────────►
                                │ dismiss stale reviews
                                │ re-run CI
                                                             pr.approve()
                                ◄────────────────────────────┘
                                │ check merge rules:
                                │   ✓ required_approvals met
                                │   ✓ all checks passed
                                │ merge (exec into git container)
                                │ status = Merged
                                │ trigger distillery ──────► clone main
                                                             run linter
                                                             LLM fix errors
                                                             write /output/
                                ◄── extract output ──────────┘
                                │ write to knowledge-base via VaultWriter
                                │ cleanup containers + repo
```

---

## Module Layout

```
submodules/runtime/
  crates/
    symbiotic-git-swarm/                    # NEW CRATE
      src/
        lib.rs                              # Exports
        types.rs                            # SwarmRepo, SwarmPR, Review, CheckRun, MergeRule
        server.rs                           # Git server container lifecycle (bollard)
        pr.rs                               # PRManager: create, comment, approve, merge
        merge_rules.rs                      # Branch protection + merge rule engine
        coordinator.rs                      # GitSwarmCoordinator: full swarm lifecycle
        ci.rs                               # CIDispatcher: spawn check agents
        distillery.rs                       # Post-merge distillery sandbox

    symbiotic-agent-runner/src/
      tools/
        mod.rs                              # MODIFY: add git, pr modules
        git.rs                              # NEW: GitCloneTool, GitPushTool
        pr.rs                               # NEW: PRCreateTool, PRCommentTool, etc.

    symbiotic-agents/src/lib.rs             # MODIFY: add git scope → trust mapping
    symbiotic-memory/src/bin/
      symbiotic-linter.rs                   # NEW: standalone linter binary

  services/symbiotic-daemon/src/
    swarm_server.rs                         # NEW: RPC handlers for swarm/PR operations
    lib.rs                                  # MODIFY: register swarm RPC methods
```

---

## Capability Scopes (new)

| Scope | Trust Level | Used By |
|-------|------------|---------|
| `git.read` | ReadOnly | All agents — clone/fetch |
| `git.push` | ArchiveWrite | Worker agents — push to feature branches |
| `git.push:protected` | ExternalAct | Only merge operations — push to main/release |
| `pr.create` | ArchiveWrite | Worker agents |
| `pr.review` | ArchiveWrite | Reviewer agents |
| `pr.merge` | ExternalAct | Daemon/coordinator only |
| `check.report` | ArchiveWrite | CI agents |

---

## Implementation Phases

### Phase 1: Git Transport + Basic PRs (MVP)

1. **Types + Git Server Container** — `symbiotic-git-swarm` crate with types, bollard-managed git server container
2. **PR System** — `PRManager` with create/review/merge lifecycle, `MergeRuleSet` evaluation
3. **Agent Tools** — `GitCloneTool`, `GitPushTool`, `PRCreateTool`, `PRCommentTool`, `PRApproveTool`
4. **Daemon Wiring** — RPC handlers for `pr.*` methods, git server startup

### Phase 2: Branch Protection + CI/Review Agents

5. **Branch Protection** — `pre-receive` hook calling daemon API, `BranchRule` enforcement, `CapabilityToken` integration
6. **CI + Reviewer Dispatching** — `CIDispatcher` spawns check agents, reviewer agent spawning with PR tool access

Implementation status (updated Session 97):
- **Chunk 5 — ✅ Complete**: Branch rules, push-session transport, `/api/git/authorize`, hook installation, daemon startup wiring. **Verified E2E**: git-server Docker image builds, lighttpd/CGI propagates `X-Symbiotic-Push-Session` into `pre-receive` hook. Key discovery: CGI processes don't inherit container env vars — solved with `entrypoint.sh` generating lighttpd `env.conf` at startup.
- **Chunk 6 — ✅ Complete**: `pr.create` initializes required checks, daemon accepts `check.report`, auto-merge on green, agent-runner has `check_report` tool. VM dispatch pipeline fully wired: `rpc_pr_create()` → `prepare_dispatch_plan()` → `build_ci_job()`/`build_reviewer_job()` → `VmManager::create` + background exec. The gateway socket now defaults to owner-only permissions and exposes an explicit `SYMBIOTIC_LLM_GATEWAY_WORLD_ACCESSIBLE=1` escape hatch for Sysbox user-namespace compatibility when bind-mounted into VMs. Agent container image (`symbiotic-agent-v1`) Dockerfile created.

### Phase 3: Distillery + Linter

7. **Distillery Sandbox** — ✅ post-merge validation, linter execution, LLM error fixing
8. **Linter Binary** — ✅ standalone `symbiotic-linter` wrapping `vault_linter::lint_file()`
9. **Integration Tests** — ✅ 26 integration tests: PR lifecycle, merge rules, branch protection, edge cases

---

## Files to Modify

| File | Change |
|------|--------|
| `submodules/runtime/Cargo.toml` | Add `symbiotic-git-swarm` to workspace members |
| `submodules/runtime/crates/symbiotic-agents/src/lib.rs` | Add git/pr scope → trust level mapping (~line 270) |
| `submodules/runtime/crates/symbiotic-agent-runner/src/main.rs` | Register git + PR tools (~line 83) |
| `submodules/runtime/crates/symbiotic-agent-runner/src/tools/mod.rs` | Add `pub mod git; pub mod pr;` |
| `submodules/runtime/services/symbiotic-daemon/src/lib.rs` | Add `pub mod swarm_server;` |
| `submodules/runtime/services/symbiotic-daemon/src/main.rs` | Spawn git server container at startup |
| `submodules/runtime/crates/symbiotic-memory/Cargo.toml` | Add `[[bin]]` for symbiotic-linter |

---

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Git server container image maintenance | Use official Alpine + git, pin versions, document Dockerfile |
| pre-receive hook latency (HTTP call to daemon) | Keep auth check fast (in-memory token lookup), timeout 2s |
| Reviewer agent produces low-quality reviews | Configurable reviewer prompts, fallback to auto-approve for low-risk |
| Non-fast-forward merges | Phase 1 rejects; Phase 2 adds rebase-in-sandbox |
| Agent pushes to wrong repo | Repo ID validated in pre-receive hook + tool enforces correct remote |
| Disk exhaustion from large repos | Size cap on receive-pack (100MB), cleanup on swarm completion |

---

## Implementation Details (Session 97)

### Git Server Container (Chunk 5)

**Location**: `submodules/runtime/docker/git-server/`

| File | Purpose |
|------|---------|
| `Dockerfile` | Alpine + `git` + `git-daemon` (provides `git-http-backend`) + `lighttpd` + `curl` + `jq` |
| `lighttpd.conf` | `mod_cgi` routing: `/` → `git-http-backend`, includes `env.conf` for CGI environment |
| `entrypoint.sh` | Generates `/etc/lighttpd/env.conf` from container env vars at startup |
| `pre-receive` | Hook template: reads `$HTTP_X_SYMBIOTIC_PUSH_SESSION`, calls `$AUTH_CALLBACK_URL` |
| `test-push-auth.sh` | End-to-end verification script (6 checks) |

**Critical finding**: lighttpd CGI processes do **not** inherit Docker container environment variables. Per RFC 3875, the CGI server controls the environment. Solution: `entrypoint.sh` reads container env vars and writes them into a lighttpd config include (`setenv.add-environment`) that lighttpd injects into every CGI process.

**Header propagation chain** (verified):
```
git push (http.extraHeader=X-Symbiotic-Push-Session: <token>)
  → lighttpd mod_cgi (RFC 3875: HTTP_X_SYMBIOTIC_PUSH_SESSION)
    → git-http-backend (inherits CGI env)
      → pre-receive hook (reads $HTTP_X_SYMBIOTIC_PUSH_SESSION)
        → curl POST to daemon /api/git/authorize
```

### Daemon↔VM Socket Transport (Chunk 6)

**Decision**: Default the Unix socket to owner-only permissions (`0o600`) and allow an explicit `SYMBIOTIC_LLM_GATEWAY_WORLD_ACCESSIBLE=1` override only when Sysbox bind mounts truly require it.

**Rationale**: The bridge handshake + capability model remains the real authorization boundary, but the local-process runner path should not leave the socket world-accessible when there is no VM bind-mount in play. An explicit override keeps the Sysbox escape hatch available without baking a weaker filesystem default into every deployment.

**Implementation**:
- `llm_gateway.rs`: `std::fs::set_permissions(path, Permissions::from_mode(0o600))` by default after `UnixListener::bind()`, switching to `0o666` only when `SYMBIOTIC_LLM_GATEWAY_WORLD_ACCESSIBLE=1` is set
- `swarm_server.rs`: `swarm_mounts()` bind-mounts socket at same path inside VM + bind-mounts runner binary
- Env var `SYMBIOTIC_SOCKET={path}` passed to container; agent-runner reads via `--socket` CLI arg

**Agent Container Image** (`symbiotic-agent-v1`):
- Location: `submodules/runtime/docker/agent-runner/Dockerfile`
- Base: `debian:bookworm-slim`
- Runtime deps: `libsqlite3-0`, `libssl3`, `git`, `ca-certificates`, `curl`, `jq`
- Entry point: `/usr/local/bin/symbiotic-agent-runner` (binary bind-mounted by daemon, not baked in)

**VM dispatch pipeline** (already fully wired):
```
rpc_pr_create()
  → prepare_dispatch_plan(pr)
    → build_ci_job(check_name, pr, repo_url, mounts, socket)
    → build_reviewer_job(pr, repo_url, mounts, socket)
  → execute_dispatch_plan(plan)
    → launch_dispatch_job(job) per job
      → VmManager::create(request) + VmManager::start(vm_id)
      → tokio::spawn(exec_and_destroy_vm(...))
```

### Linter Binary (Chunk 8)

**Location**: `submodules/runtime/crates/symbiotic-memory/src/bin/symbiotic-linter.rs`

```
symbiotic-linter file <path>          # lint single file via lint_file()
symbiotic-linter dir <path>           # recurse directory, lint all .md files
symbiotic-linter stdin                # read from stdin, lint via lint_content()
  --format json                       # (default) JSON array of findings
  --format text                       # human-readable [ERROR]/[WARNING] output
```

- Exit codes: 0 = clean, 1 = lint errors found, 2 = usage/IO error
- No new dependencies (uses existing `serde`/`serde_json`)
- Finding fields: `file`, `level`, `message`
- 281 existing memory tests pass

### Distillery Sandbox (Chunk 7)

**Location**: `submodules/runtime/services/symbiotic-daemon/src/swarm_server.rs`

Post-merge pipeline triggered automatically after `try_auto_merge_if_ready()` succeeds:

```
try_auto_merge_if_ready(pr_id)
  → fast_forward_merge(repo_id, branch, base) → sha
  → mark_merged(pr_id)
  → dispatch_distillery(pr_id, sha)  ← NEW
    → build_distillery_job(...)
    → VmManager::create + start
    → tokio::spawn(exec_and_destroy_vm)  ← background, non-blocking
```

**Key design decisions**:
- **Best-effort**: Merge succeeds even if distillery dispatch fails. Errors are logged, not propagated.
- **Distillery agent scopes**: `git.read` + `git.push` + `pr.create` (can create lint-fix PRs)
- **No special runner mode**: Distillery runs through the normal `symbiotic-agent-runner` goal path with a dedicated system prompt and post-merge context, rather than a one-off `--distillery` execution branch.
- **Typed bundle handoff**: Distillery writes `/workspace/distillery-bundle.json` with a canonical `report` plus an optional list of typed markdown artifacts under `/workspace/output/...`. The daemon validates each relative artifact path, extracts only the listed markdown files, and maps each `kind` into a daemon-owned Archive subtree under `operations/swarm-distillery/`. The untrusted distillery never chooses host-side Archive destinations directly.
- **Linter discovery**: `find_linter_binary_path()` checks next to runner binary, then `target/release/`, then `target/debug/`. Optional — distillery runs without it but skips linting.
- **`distillery.status` RPC**: Returns VM state (Running/Stopped/etc.) for a given VM ID.

**Distillery VM environment**:
| Env Var | Value |
|---------|-------|
| `SYMBIOTIC_SOCKET` | Daemon bridge socket path |
| `GIT_SERVER_URL` | Git server HTTP URL |
| `SWARM_REPO_ID` | Repository being distilled |
| `PR_ID` | Merged PR identifier, exposed so `check_report` can register |
| `CHECK_NAME` | Fixed to `distillery`, enabling the distillery summary check |
| `DISTILLERY_MODE` | `post-merge` |
| `MERGED_PR_ID` | PR that triggered distillery |
| `MERGED_SHA` | Merge commit SHA |
| `SWARM_BASE_BRANCH` | Target branch (e.g., `main`) |

**Distillery agent behavior** (defined in `DISTILLERY_AGENT_SYSTEM_PROMPT`):
1. Clone repo from git server
2. Run `symbiotic-linter dir .` on workspace
3. If lint errors → use LLM to fix → create follow-up PR
4. Always write a typed JSON bundle manifest to `/workspace/distillery-bundle.json`
5. If extra notes are needed, write markdown files under `/workspace/output/` and list them in the manifest
6. The daemon extracts the manifest plus listed files via `vm.file.extract`, then writes the canonical Archive note and typed artifact notes on the trusted side

**Bundle contract**:

```json
{
  "version": 1,
  "report": {
    "summary_markdown": "string",
    "decisions": ["string"],
    "patterns": ["string"],
    "follow_up_pr_title": "string|null",
    "lint_status": "clean|fix_pr_opened|skipped|failed"
  },
  "artifacts": [
    {
      "kind": "methodology_note|decision_note|pattern_note",
      "title": "string",
      "relative_path": "patterns/example.md"
    }
  ]
}
```

- `relative_path` must be a relative `.md` path with only normal path components.
- The daemon owns the Archive routing policy:
  - `methodology_note` → `operations/swarm-distillery/artifacts/methodology/`
  - `decision_note` → `operations/swarm-distillery/artifacts/decisions/`
  - `pattern_note` → `operations/swarm-distillery/artifacts/patterns/`
- This keeps retrieval-oriented artifact richness without reopening arbitrary host path writes from the distillery VM.

### Integration Tests (Chunk 9)

**Location**: `submodules/runtime/crates/symbiotic-git-swarm/tests/integration.rs`

26 integration tests exercising the public API (PRManager, MergeRuleSet, BranchRule) — no Docker required:

| Category | Count | Coverage |
|----------|-------|----------|
| PR Lifecycle | 6 | Create → review → checks → merge, rejection flow, close without merge |
| Merge Rules | 3 | min_approvals thresholds, required_checks subset, empty rules |
| Branch Rules | 2 | Protected branch detection, wildcard patterns |
| Reviews | 2 | Multiple reviewers, review comments storage |
| Edge Cases | 6 | Double merge, review merged/closed PR, close merged PR, merge unapproved |
| Stale Reviews | 2 | Dismiss enabled/disabled behavior |
| Check Lifecycle | 1 | Check update replaces existing |
| Evaluation Detail | 1 | Individual check status in merge evaluation |
| Cross-Repo Isolation | 1 | PRs across repos don't interfere |
| Self-Review | 1 | Author cannot review own PR |
| Duplicate PR | 1 | Same branch rejected |

Total crate tests: 52 (26 unit + 26 integration). All deterministic, fast, in-memory.

---

## Future Transport Improvements

The current daemon↔VM transport uses **bind-mounted Unix sockets**. The secure default is owner-only (`0600`) and the daemon exposes an explicit permissive mode (`SYMBIOTIC_LLM_GATEWAY_WORLD_ACCESSIBLE=1`) for Sysbox/user-namespace cases that cannot connect otherwise. Access control is still enforced at the RPC layer by `AccessBroker` + `CapabilityToken`, not by filesystem permissions alone.

If Sysbox user-namespace remapping proves unreliable with socket bind-mounts, consider these alternatives:

### Option B: TCP Bridge in Daemon

Add a TCP listener to the daemon (e.g., `127.0.0.1:<dynamic-port>`) that proxies to the same JSON-RPC handler. VMs connect via Docker bridge network or `host.docker.internal`.

- **Pro**: Works regardless of Sysbox filesystem quirks; standard networking
- **Con**: Socket is network-reachable by any container on the bridge — requires per-session auth tokens or IP-based filtering to prevent cross-VM access
- **Con**: Port allocation complexity; potential port exhaustion with many VMs

### Option C: socat Relay (Unix → TCP → Unix)

Run a `socat` process per VM that bridges the host Unix socket to a TCP port, with a VM-side `socat` recreating a Unix socket from the TCP connection. Agent-runner code stays unchanged (still connects to a local Unix socket path).

- **Pro**: Transport-transparent — no changes to agent-runner
- **Pro**: Per-VM isolation (each gets its own relay)
- **Con**: Extra process per VM; another failure mode to monitor
- **Con**: Same network exposure as Option B on the TCP segment
