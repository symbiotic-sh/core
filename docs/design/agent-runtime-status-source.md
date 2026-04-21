# Agent Runtime Status Source

**Status**: Implemented (`2026-04-10`)  
**Related Tasks**: T113, T115, T108  
**Related Docs**: `docs/design/project-board-observability-contract.md`, `docs/design/thread-observability-source.md`, `docs/design/non-swarm-management-ownership.md`

## Goal

Define the canonical daemon-owned source for **live agent runtime metadata**:

- role
- sandbox type
- model route
- current lifecycle status
- current iteration / max iterations

This source is distinct from append-only raw runtime logs.

## Problem

The real Project Board and Agent Logs sheet now have truthful raw runtime
`tool` / `result` / `blocked` records, but the metadata surrounding those logs
is still incomplete.

Today the daemon does not persist a separate current-status lane for bridge-run
agents, which forces the app to fall back to placeholders or infer state from
raw log streams.

That is not acceptable for the end state.

## Core Decision

Symbiotic keeps **two distinct runtime observability stores**:

1. `data/bridge/agent-logs.jsonl`
   - append-only raw runtime facts
   - `tool` / `result` / `blocked`
   - debugging, audits, conflict resolution

2. `data/agents/runtime-status.json`
   - mutable current runtime state per agent
   - current profile and execution status
   - source of truth for live metadata in Tier 3 / Tier 4 shells

Raw logs are never the canonical source of current runtime status.

## Truth Boundary

The daemon may persist runtime status only from data it owns truthfully:

- bridge handshake profile sent by the runner
- daemon-owned `AgentState`
- explicit bridge `agent.status` events

The daemon must not infer runtime metadata from:

- log text parsing
- UI heuristics
- branch naming
- guessed model identity

## Runtime Profile

The runner should send a runtime profile during `bridge.handshake`.

```rust
pub struct AgentRuntimeProfile {
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub max_iterations: Option<u32>,
    pub thread_id: Option<String>,
}
```

Notes:

- `sandbox_type` must reflect the real execution mode (`vm_sandbox`,
  `local_process`, `in_process`, etc.).
- `model_label` is the truthful route/model label currently known to the daemon.
  If the daemon only knows the configured route class, it should store that
  rather than inventing a concrete provider/model.
- `thread_id` is optional; `goal_scope` remains the durable join key.

## Runtime Status Store

```rust
pub struct AgentRuntimeStatus {
    pub agent_id: String,
    pub token_id: String,
    pub goal_scope: Option<String>,
    pub thread_id: Option<String>,
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub status: AgentRuntimeStatusKind,
    pub detail: Option<String>,
    pub current_iteration: Option<u32>,
    pub max_iterations: Option<u32>,
    pub active_tool_name: Option<String>,
    pub updated_at: u64,
}

pub enum AgentRuntimeStatusKind {
    Starting,
    Running,
    Waiting,
    Blocked,
    Completed,
    Failed,
}
```

This store is mutable and restart-safe. Each agent has one current record.

## Event Model

The runner should emit explicit `goal.event` payloads with
`event_type = "agent.status"` whenever current runtime state changes.

Representative events:

- bridge connected / context loaded
- iteration advanced
- tool call started
- tool result completed
- blocked waiting on capability / auth / operator
- finished successfully
- failed

Example payload:

```json
{
  "event_type": "agent.status",
  "status": "running",
  "detail": "Iteration 3/15",
  "current_iteration": 3,
  "max_iterations": 15,
  "active_tool_name": null,
  "thread_id": "thread-build-runtime"
}
```

The runner may still emit existing `agent.step` events for compatibility, but
`agent.status` becomes the canonical current-state lane.

## Projection Contract

`thread.observability.snapshot` should project current status separately from
raw logs.

```rust
pub struct ThreadAgentRuntimeStatus {
    pub agent_id: String,
    pub operation_id: Option<String>,
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub status: AgentRuntimeStatusKind,
    pub detail: Option<String>,
    pub current_iteration: Option<u32>,
    pub max_iterations: Option<u32>,
    pub active_tool_name: Option<String>,
    pub updated_at: i64,
}
```

This lets the app render:

- real metadata bars in `AgentLogsSheet`
- truthful active-agent chips
- future richer operation cards

without conflating current status with raw debug logs.

## Join Rules

Thread projection may include a status record when either:

- the status has explicit `thread_id` matching the thread
- or the status `goal_scope` matches a `GoalState.last_run_id` currently
  attached to the thread

No weaker join is allowed.

## Persistence

Store current status at:

`{data_dir}/agents/runtime-status.json`

Reason:

- this is mutable current agent state, not raw bridge history
- it belongs with other daemon-owned agent state under `data/agents/`
- it stays restart-safe without inventing another DB

## App Policy

The app may show:

- role
- sandbox type
- model label
- iteration
- current runtime status

only when that data is present in the daemon snapshot.

Otherwise it must omit the field, not guess.

## Implementation Status

Implemented:

1. daemon `AgentRuntimeStatusStore`
2. runner handshake profile
3. explicit `agent.status` events
4. `thread.observability.snapshot.agent_statuses`
5. app consumption in `ProjectBoard` and `AgentLogsSheet`

## Remaining Gap

This slice solved truthful current runtime metadata.

It did **not** solve the broader goal/project ownership hierarchy for
development artifacts. That remains a separate management-layer problem.
