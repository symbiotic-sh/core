# Agent Evolution — Declarative Roles, Tools, and Capabilities

Status: **design** (planned). The sections below describe where the system
should go, not where it is today. Shipped behaviour is covered in
[`docs/architecture/agent-orchestration.md`](../architecture/agent-orchestration.md).

## Context

Symbiotic's pitch is a "Declarative Cognitive Control Plane" — the user
declares thoughts, goals, and identity in Markdown; the runtime reconciles
reality to match them. The current agent layer is ~20% of that vision:

- Roles are declared (TOML files in `config/agents/`).
- Capabilities are declared per role (`required_capabilities`).
- Sub-agents can be dispatched by an orchestrator via the `dispatch_agent` tool.
- (Shipped in this iteration) The daemon **injects the live role roster** into
  any dispatcher's system prompt at resolve time, so new specialist roles are
  discovered automatically.

The missing 80% is the **write-back loop**: agents proposing changes to the
declarations, gated by trust. Without it, the system is merely configured, not
autonomous.

## Terminology

- **Role** — a named agent personality (`researcher`, `security-auditor`, …),
  declared in a TOML file with a system prompt, required capabilities, and a
  versioned prompt history.
- **Tool** — a callable action exposed to an agent's ReAct loop (`recall`,
  `archive`, `dispatch_agent`, `file_write`, …). Today, tools are compiled
  Rust in `symbiotic-agents/src/builtin_tools.rs`.
- **Capability** — a scoped grant (`archive.read`, `agent.dispatch`) that the
  `AccessBroker` checks before a tool executes. Declared in TOML, enforced in
  Rust via `scope_to_trust_level`.
- **Skill** — a higher-level composition (prompt fragment + tool loadout +
  checklist). The `symbiotic-skills` crate has a registry; skills are not yet
  fully wired to the execution path.

## Phase 1 — Parametric prompts (shipped in this iteration)

**Problem.** Orchestrator roles had to hard-code the list of available
specialists in their system prompt. Adding a new specialist required editing
the orchestrator TOML too. Fragile and error-prone.

**Fix.** At role resolve time (`AgentExecuteExecutor::resolve_role_config`),
if the resolved role declares `agent.dispatch`, the daemon appends an
auto-generated `## Available sub-agent roles` section derived from the role
registry. Source of truth is the filesystem; orchestrator TOMLs no longer list
specialists by name.

**Status.** Live.

## Phase 2a — Tool discovery via meta-tool (RAG-style)

**Problem.** Every registered tool's full JSONSchema lands in the system
prompt via `format_tools_for_prompt`. `generate_plan` alone is ~3 KB of
escalation, delivery-window, timezone, and SLA-policy schema that almost no
agent will ever use. Even after role-scoped filtering (shipped), adding new
tools still monotonically grows every agent's prompt.

**Proposed shape.** Invert the pattern: agents see a short ROSTER of tool
names + one-line descriptions by default. When an agent decides to use a
tool it doesn't yet have the schema for, it calls a meta-tool:

```
{"tool": "describe_tool", "params": {"name": "generate_plan"}}
→ observation: <full JSONSchema for generate_plan>
```

The agent then emits the actual tool call on the next turn. Schemas load
on-demand, exactly like retrieving a document via Recall — hence "RAG for
tools". Benefits:

- System prompts stay constant size as the tool registry grows.
- Unused tools cost zero context. `generate_plan` is invisible unless an
  agent asks for it.
- Tool parameter design can be richer without punishing every unrelated
  agent. Today we self-censor schemas to avoid the bloat.

**Cost.** One extra turn when a tool is used for the first time in a loop.
Cheap. Agents naturally cache the schema in their conversation history.

**Interaction with Phase 1 role-scoped filtering.** They're complementary:
the roster listed to an agent is still filtered by capability / role, but
the schemas come from the meta-tool instead of being inlined.

## Phase 2b — Dynamic tool discovery

**Goal.** An agent's system prompt should reflect the tools actually
registered for its execution, not a hard-coded list. Drop a new tool → every
compatible agent knows about it on the next invocation.

**Proposed shape.**

- Each `Tool` already carries `name()`, `description()`, and
  `parameters_schema()`. Collect these at `execute_react` time into a
  `## Available tools` block injected into the system prompt.
- Optional per-role tool filtering in TOML:
  ```toml
  tools_allowed = ["recall", "archive", "dispatch_agent"]
  # or
  tools_excluded = ["shell_exec"]
  ```
- For dispatchers, the injected specialist roster should include **which
  tools each specialist has**, so the orchestrator can pick appropriately.

**Open question.** Some tools (like `ask_user_group`) are only meaningful in
certain contexts (inquisitor flow). We may want a `context_tags` field on
tools so the injection filter is smarter than "every tool the daemon has".

## Phase 3 — Capability registry as data

**Problem.** Capability scope strings (`archive.read`, `action.browser.login`,
…) are coupled to a Rust `match` in `scope_to_trust_level` that maps them to
`AgentTrustLevel`. Inventing a new scope requires a recompile. That
contradicts the "declare in Markdown" vision.

**Proposed shape.** A `config/capabilities.toml` (or similar) that declares
all known capabilities as data:

```toml
[capabilities."archive.read"]
trust = "ReadOnly"
description = "Read entries from the Archive via Recall Gateway."
audit = "log_on_issue"

[capabilities."agent.dispatch"]
trust = "ExternalAct"
description = "Spawn a sub-agent with a specified role and goal."
audit = "log_on_issue"
```

The daemon loads this at startup; `scope_to_trust_level` becomes a hashmap
lookup. New scopes can be declared by the operator without a release.

**Risk.** An operator could declare a permissive trust level for a scope that
shouldn't have one. Mitigation: a schema-level allowlist of valid
`AgentTrustLevel` values per scope *namespace* (e.g., `credential.*` must map
to at least `CredentialAccess`).

## Phase 4 — Self-evolving roles

**Goal.** Agents propose improvements to their own role definitions based on
observed performance. A human (or a trusted supervisor agent) reviews and
applies.

**Ingredients already in the tree.**

- `process-engineer` role (defaults.rs) — described as analyzing past
  executions and creating methodology improvements.
- `graduation_store` on the daemon — tracks per-goal-type efficiency.
- `AgentMonitor` (SqliteAgentMonitor) — records every execution.

**Missing glue.**

1. A `role.propose` capability + tool: a `process-engineer` run produces a
   proposed `researcher.toml` v3 and writes it to a pending-proposals area
   (not `config/agents/` directly).
2. A review queue surfaces these to the user (or a `supervisor` role) via the
   existing review pipeline.
3. On approval, the proposal moves into `config/agents/` and the registry
   hot-reloads.

**Gates.**

- `required_capabilities` changes MUST require human approval, always. An
  agent cannot silently grant itself broader trust.
- Prompt-only changes can flow through an auto-approval path for roles that
  opt in (new version created; the old version is kept for rollback).

## Phase 5 — Tools as declared contracts

**Goal.** Agents can describe a tool they'd like to exist; the system either
provides it (built-in / plugin-provided) or routes the request to a human.

**Proposed shape.** Unify tool implementations behind a single declarative
schema:

```toml
[tool.calendar_create_event]
description = "Create a calendar event."
schema = { ... JSONSchema ... }
backend.kind = "http"
backend.url = "http://localhost:9000/tools/calendar"

[tool.shell_exec]
description = "Execute a shell command in the workspace."
schema = { ... }
backend.kind = "builtin"
backend.id = "symbiotic_agents::workspace_tools::ShellExecTool"
```

Every tool is addressable the same way. Backends can be:
- `builtin` — a Rust impl shipped with the daemon.
- `http` — a local or remote process that implements a fixed JSON-RPC shape.
- `wasm` — a sandboxed module.
- `subagent` — delegate to another role.

Agents then get a uniform `{name, schema, backend}` record. The daemon
resolves the backend at execute time.

## Phase 6 — Earned capabilities

**Goal.** The trust model becomes provisional and explicit. An agent's set of
capabilities expands based on durable, audited grants — not a static TOML
field.

**Proposed mechanics.**

1. Every tool call that fails capability check records a structured
   "capability request" event: `{role, capability, why, context}`.
2. A `supervisor` role (or the user) sees a rolling list of requested
   capabilities. Each can be:
   - Granted once (single-use token).
   - Granted for this goal-scope.
   - Granted durably (written into the role's `required_capabilities`).
3. Durable grants produce a proposal diff (Phase 4 pipeline).

**Why this is interesting.** Roles become evolved-by-use rather than
pre-declared. A `researcher` that repeatedly needs `web.fetch` will have it
proposed for durable grant; a `researcher` that never needs it never gets it.

## Sequencing

Recommended build order:

1. **Phase 2** (dynamic tool discovery) — small patch, high payoff; makes
   every agent prompt honest about its loadout.
2. **Phase 3** (capability registry as data) — unblocks Phase 6 by making
   scopes cheap to declare.
3. **Phase 4** (self-evolving roles) — requires the proposal + review UI.
4. **Phase 5** (tools as declared contracts) — substantial refactor; defer
   until there's a real use case for a non-builtin tool.
5. **Phase 6** (earned capabilities) — builds on Phases 3 and 4.

## Non-goals

- **Replacing the built-in tool set with a plugin system today.** The built-in
  set is small and stable; the plugin work belongs with Phase 5 when there's a
  clear external tool we want.
- **Fully autonomous self-modification.** Capability changes always need human
  (or supervisor-role) approval. The goal is earned autonomy, not unsupervised
  autonomy.
- **Markdown-only role declarations.** TOML is staying for structural fields
  (capabilities, versions); prompt bodies can be kept in Markdown if the
  ergonomics are better, but forcing everything into Markdown for its own sake
  doesn't add value.
