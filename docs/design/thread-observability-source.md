# Thread Observability Source

**Status**: Approved design target  
**Related Tasks**: T115, T113, T108  
**Related Docs**: `docs/design/project-board-observability-contract.md`, `docs/design/thread-architecture.md`, `docs/architecture/symbiotic-daemon.md`, `docs/architecture/symbiotic-app.md`

## Goal

Implement the truthful Tier 3 data source for thread execution visibility.

Phase 1 provides the daemon-owned `OperationsPillSummary` needed for the real thread view plus a narrow operations snapshot for the real `ProjectBoard`, including recent execution updates from the durable goal log. Phase 2 adds truthful artifact projection from persisted development ownership and a narrow chatter lane derived from daemon-owned bridge interaction logs. Phase 3 adds truthful raw runtime agent logs (`tool` / `result` / `blocked`) without pretending to expose hidden chain-of-thought. Phase 4 adds a separate daemon-owned current runtime status lane for per-agent role / sandbox / model / iteration metadata. The current next-step refinement is to source board operations from goal task work items and recent updates from canonical Archive goal events rather than treating top-level workflow runs as the only observability unit.

## Problem

The app can already route thread-scoped state events by `thread_id`, but the daemon does not yet materialize a durable observability summary keyed by thread.

That leaves two bad options:

- app-side inference from raw events
- fake/mock observability in the real app

Both are rejected by the current UX and architecture direction.

## Decision

The daemon will own a persistent **thread observability summary store** under `data/threads/` and emit a typed Matrix **state event** whenever a thread-scoped execution event changes the summary.

Phase 1 scope:

- persist per-thread `OperationsPillSummary`
- persist per-thread `ThreadObservabilitySnapshot`
- emit `thread.observability.summary` state events
- emit `thread.observability.snapshot` state events
- wire the app to render `OperationsPill` only from that real summary
- wire the app to open a truthful `ProjectBoard` from the real snapshot only
- route `goal.step.*` noise away from the visible thread timeline once the board has a truthful home for those updates

Phase 2 non-goals:

- full agent-to-agent peer chatter transport
- hidden chain-of-thought transport
- app-side reconstruction of operation cards
- fake active-agent counts

## Truth Boundary

Phase 1 summary should be built only from data the daemon already persists truthfully today:

- `GoalState`
- `ThreadRegistry`

The first `ProjectBoard` snapshot should use the same truth boundary. It may
list real active operations and persisted development artifacts without pretending the daemon already has:

- agent-to-thread ownership mapping
- agent-to-agent chatter transport
- raw reasoning logs

It may, however, surface:

- **recent execution updates** from the persisted goal run log
- **narrow runtime chatter** from a daemon-owned raw bridge interaction log
- **raw runtime tool/result/blocked logs** from a daemon-owned agent runtime log

because both are already daemon-owned durable truth.

It must not invent state from:

- inferred app-side event replay
- demo observability models
- guessed agent-thread counts

## Current Implementation Note

As of `2026-04-10`, all four phases described above are now implemented in the
daemon and real app:

- Phase 1: real `thread.observability.summary`
- Phase 2: truthful artifact and chatter projection
- Phase 3: truthful raw runtime `tool` / `result` / `blocked` logs
- Phase 4: daemon-owned `agent_statuses` from a dedicated runtime-status store

The remaining architecture gap is no longer the thread observability source
itself. It is richer action semantics for Archive-native declared work beyond
the first escalation slice. The observability source now already projects:

- task work items as the primary live operation cards when available
- Archive goal events as recent updates for task ownership / status / replan changes
- development artifacts beneath those tasks/goals
- declared-task escalation updates such as `task_escalated` and
  `task_replan_requested`
- declared-condition updates such as `task_condition_set`,
  `task_condition_satisfied`, and `task_dependencies_satisfied`

## Phase 2 Artifact And Chatter Refinement

Truthful artifact projection may now include persisted swarm branch ownership
through two explicit attachment paths:

- swarm development artifact work items persist under the management store
- artifact work may carry an explicit `thread_id`
- artifact work may also carry the current `goal_scope`
- current bridge tokens already use `goal_scope` as the active workflow/run scope
- `GoalState.last_run_id` remains the compatibility join key shared with thread observability

So the board may project branch/review/merge artifacts by:

1. reading management work items whose explicit `thread_id` matches the thread
2. falling back to the thread's matching `GoalState` rows
3. collecting their `last_run_id` values
4. reading management work items whose `initiative_id` matches those run scopes
5. rendering only persisted branch lifecycle states (`active`, `pending_review`, `merged`, `closed`, `failed`)

This is still narrower than the final goal/task hierarchy, but it is now
cleaner than the earlier scope-only join: thread attachment is explicit on
daemon-owned management records, and the visible artifact lane comes only from
development-artifact work items while run-scope matching remains a
compatibility fallback.

Truthful chatter may now include bridge-owned runtime interactions when there is an explicit join:

- bridge auth requests may already carry a real `thread_id`
- other bridge interactions carry `goal_scope`
- `GoalState.last_run_id` remains the active run-scope join key

So the board may project a narrow chatter lane by:

1. taking the thread's matching `GoalState` rows
2. collecting their `last_run_id` values
3. reading raw bridge interaction records that match either:
   - explicit `thread_id`
   - matching `goal_scope`
4. rendering only daemon-owned interaction classes:
   - pending question
   - proposed plan
   - pending auth request

This is intentionally not peer-to-peer agent chatter and not hidden chain-of-thought logging.

## Phase 1 Refinement

The higher-level observability contract currently includes `total_active_agents`, but the daemon does not yet persist a truthful thread-to-agent mapping.

So in Phase 1:

- `total_active_agents` is optional
- when available, derive it only from recent daemon-owned runtime log activity within the active goal/run scope window
- the app must not render a fake satellite count

This is an explicit honesty refinement, not a fallback hack.

## Runtime Types

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OperationHeadline {
    pub title: String,
    pub status: OperationHeadlineStatus,
    pub current_step: Option<String>,
    pub progress_percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OperationHeadlineStatus {
    Running,
    Waiting,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OperationsPillSummary {
    pub thread_id: String,
    pub active_operation_count: u32,
    pub primary_operation: Option<OperationHeadline>,
    pub waiting_for_user: bool,
    pub has_failure: bool,
    pub total_active_agents: Option<u32>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadOperationCard {
    pub operation_id: String,
    pub title: String,
    pub status: OperationHeadlineStatus,
    pub current_step: Option<String>,
    pub owner_label: Option<String>,
    pub progress_percent: Option<u8>,
    pub active_agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadObservabilitySnapshot {
    pub thread_id: String,
    pub operations: Vec<ThreadOperationCard>,
    pub updates: Vec<ThreadExecutionUpdate>,
    pub chatter: Vec<ThreadChatterEvent>,
    pub agent_statuses: Vec<ThreadAgentRuntimeStatus>,
    pub agent_logs: Vec<ThreadAgentLogEntry>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadExecutionUpdate {
    pub operation_id: String,
    pub label: String,
    pub detail: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadChatterEvent {
    pub event_id: String,
    pub operation_id: Option<String>,
    pub from_agent: String,
    pub to: String,
    pub kind: ThreadChatterKind,
    pub message: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadAgentLogEntry {
    pub event_id: String,
    pub operation_id: Option<String>,
    pub agent_id: String,
    pub entry_type: ThreadAgentLogEntryType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_params: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadAgentRuntimeStatus {
    pub agent_id: String,
    pub operation_id: Option<String>,
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub status: ThreadAgentRuntimeStatusKind,
    pub detail: Option<String>,
    pub current_iteration: Option<u32>,
    pub max_iterations: Option<u32>,
    pub active_tool_name: Option<String>,
    pub updated_at: i64,
}
```

## Persistence

Store summaries and snapshots as JSON at:

`{data_dir}/threads/observability.json`

Store raw bridge interactions separately at:

`{data_dir}/bridge/raw-events.jsonl`

Store raw runtime agent logs separately at:

`{data_dir}/bridge/agent-logs.jsonl`

Store distilled runner checkpoints separately at:

`{data_dir}/bridge/checkpoints.jsonl`

Store current runtime status separately at:

`{data_dir}/agents/runtime-status.json`

Reason:

- same family as `ThreadRegistry`
- easy atomic save/load
- durable enough for restart-safe app/daemon behavior
- avoids inventing another DB for a still-small surface

## Module Layout

New daemon module:

`submodules/runtime/services/symbiotic-daemon/src/thread_observability.rs`

Responsibilities:

- summary data types
- snapshot data types
- file-backed store
- summary derivation from `GoalState`
- snapshot derivation from `GoalState`
- recent update derivation from the goal run log
- chatter derivation from the bridge raw interaction log
- runtime status derivation from the daemon-owned current runtime status store
- raw runtime log derivation from the agent runtime log
- Matrix state envelope builder

## Summary Derivation Rules

Phase 1 derivation is intentionally narrow.

### Thread Selection

Use the thread-scoped event's `thread_id`.

### Backing Goal State

Find the latest `GoalState` rows whose explicit attached `thread_id` matches the thread when available. When that is missing, fall back to legacy rows whose `last_run_id == thread_id`.

This fallback exists only because some older goal-thread flows still mirror workflow `goal_id` into `thread_id`. It is a compatibility bridge, not the end-state model.

### Active Operation Count

Count rows with non-terminal lifecycle states:

- `running`
- `queued`
- `awaiting_input`
- `awaiting_approval`
- `awaiting_auth`
- `deliberating`
- `retry`

Do not count:

- `completed`
- `cancelled`
- `rejected`
- `dlq`
- `failed`

Failures are surfaced separately via `has_failure`.

### Primary Operation

Choose the most recently updated non-terminal row. If none exist, choose the most recently updated failed row when present.

Field mapping:

- `title`
  - thread title from `ThreadRegistry` when available
  - else prettified goal template
- `status`
  - `Running` for active lifecycle states
  - `Waiting` for `awaiting_*`
  - `Failed` for `failed` / `retry` / `dlq` / `rejected`
- `current_step`
  - `GoalState.pipeline_stage`
- `progress_percent`
  - `None` in Phase 1 unless a truthful persisted value exists

### Waiting For User

True when any matching row is in:

- `awaiting_input`
- `awaiting_approval`
- `awaiting_auth`

### Has Failure

True when any matching row is in:

- `failed`
- `retry`
- `dlq`
- `rejected`

### Total Active Agents

Do not infer from current `AgentState.parent`.

Emit:

- `Some(n)` only if a truthful thread-to-agent source exists
- otherwise `None`

## Matrix Event Contract

Emit a thread-scoped state event:

```json
{
  "msgtype": "sym.e",
  "body": "Thread observability updated",
  "sym": {
    "v": 2,
    "k": 3,
    "s": 1,
    "ts": 1710000000,
    "t": "goal-abc123",
    "a": "thread.observability.summary",
    "d": {
      "thread_id": "goal-abc123",
      "active_operation_count": 1,
      "waiting_for_user": false,
      "has_failure": false,
      "updated_at": 1710000000,
      "primary_operation": {
        "title": "Fix deploy pipeline",
        "status": "running",
        "current_step": "executing",
        "progress_percent": null
      }
    }
  }
}
```

When the summary becomes empty, still emit the event with:

- `active_operation_count = 0`
- `primary_operation = null`
- `waiting_for_user = false`
- `has_failure = false`

The app uses that to clear the pill.

Emit a second thread-scoped state event for the Project Board:

```json
{
  "msgtype": "sym.e",
  "body": "Thread observability snapshot updated",
  "sym": {
    "v": 2,
    "k": 3,
    "s": 1,
    "ts": 1710000000,
    "t": "goal-abc123",
    "a": "thread.observability.snapshot",
    "d": {
      "thread_id": "goal-abc123",
      "operations": [
        {
          "operation_id": "inquisition:goal-abc123:job-1",
          "title": "Inquisition Goal Abc123",
          "status": "waiting",
          "current_step": "awaiting_input",
          "progress_percent": null,
          "active_agents": []
        }
      ],
      "updated_at": 1710000000
    }
  }
}
```

When no operations remain, still emit the snapshot event with empty
`operations` and `updates` lists so the app can clear the real Project Board state.

## Integration Plan

### Daemon

1. Load `ThreadObservabilityStore` during daemon startup.
2. Whenever the daemon sends a thread-scoped execution event, attempt a summary refresh.
3. Persist the updated summary and snapshot.
4. Emit `thread.observability.summary` to the same thread room.
5. Emit `thread.observability.snapshot` to the same thread room.
6. Skip recursive refresh for either observability event.

### App

1. Add a typed `OperationsPillSummary` Dart model.
2. Route `thread.observability.summary` through a dedicated state callback.
3. Route `thread.observability.snapshot` through a dedicated state callback.
4. Store the summary and snapshot keyed by `thread_id`.
5. Render `OperationsPill` only when a real summary exists.
6. Open `ProjectBoard` only when a real snapshot exists.
7. Keep `goal.step.*` events out of the visible thread timeline once the board snapshot is available.
8. Do not route observability events into the visible thread timeline.

## Why This Is The Right First Slice

- fits the already-approved daemon-owned projection rule
- keeps the app simple
- removes fake Tier 3 from the real product
- creates a durable source that the real `ProjectBoard` and later HTTP surfaces can reuse
- moves low-level execution updates into Tier 3 instead of spamming the main thread
- avoids overcommitting to agent/chatter/artifact semantics before the runtime actually persists them

## Config Schema

No new user-facing config.

Phase 1 uses the existing daemon `data_dir` and stores:

- `data/threads/observability.json`

## Verification Plan

Runtime:

- summary store save/load
- snapshot store save/load
- summary derivation from `GoalState`
- snapshot derivation from `GoalState`
- empty-summary clear behavior
- empty-snapshot clear behavior
- `send_matrix_event` emits thread summary + snapshot only for thread-scoped execution events
- observability events do not recurse

App:

- `thread.observability.summary` updates thread summary state
- `thread.observability.snapshot` updates thread snapshot state
- `ChatView` renders no pill without summary
- `ChatView` renders pill when summary exists
- tapping the real pill opens the real `ProjectBoard` when snapshot exists
- clearing event removes the pill
