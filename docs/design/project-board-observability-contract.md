# Project Board Observability Contract

**Status**: Approved design target  
**Related Tasks**: T115, T108, T113  
**Related Docs**: `docs/design/ux-specification.md`, `docs/design/thread-architecture.md`, `docs/design/agent-collaboration-conflict-resolution.md`

## Goal

Define the data contract between daemon/runtime events and the app's Tier 3 / Tier 4 observability surfaces:

- `Operations Pill`
- `Project Board`
- `Agent Logs`

The UX shape already exists. What is missing is the event projection that makes these surfaces truthful.

## Current Problem

The real app currently has observability widgets, but their data is still demo-only. That creates false confidence:

- users see active execution that is not actually wired
- thread UI suggests visibility the runtime does not yet provide

Real app surfaces must not use fake execution data.

## Source UX Contract

`docs/design/ux-specification.md` already defines:

- Tier 3 = execution plane
- Tier 4 = raw reasoning/debug
- thread timeline should hide low-level swarm noise
- Project Board should expose active operations, internal chatter, and artifacts

This doc turns that into an implementation-facing event model.

## Core Decision

The app should not infer observability ad hoc from raw Matrix traffic.

Instead, the daemon should emit a typed **thread observability projection** that the app can render directly.

## Projection Model

### Thread Observability Snapshot

```rust
pub struct ThreadObservabilitySnapshot {
    pub thread_id: String,
    pub operations: Vec<OperationCard>,
    pub chatter: Vec<ChatterEvent>,
    pub agent_statuses: Vec<AgentRuntimeStatus>,
    pub agent_logs: Vec<AgentLogEntry>,
    pub artifacts: Vec<OperationArtifact>,
    pub updated_at: i64,
}
```

### Operations Pill Input

The pill should consume only the minimum status summary needed for Tier 3 indication.

```rust
pub struct OperationsPillSummary {
    pub thread_id: String,
    pub active_operation_count: u32,
    pub primary_operation: Option<OperationHeadline>,
    pub waiting_for_user: bool,
    pub has_failure: bool,
    pub total_active_agents: u32,
}
```

### Project Board Input

The Project Board should render structured execution state, not a replay of every raw event.

```rust
pub struct OperationCard {
    pub operation_id: String,
    pub goal_id: String,
    pub title: String,
    pub role: Option<String>,
    pub status: OperationStatus,
    pub progress: Option<ProgressSnapshot>,
    pub current_step: Option<String>,
    pub active_agents: Vec<String>,
}

pub struct ChatterEvent {
    pub operation_id: Option<String>,
    pub from_agent: String,
    pub to_agent: Option<String>,
    pub message: String,
    pub created_at: i64,
}

pub struct OperationArtifact {
    pub operation_id: String,
    pub kind: ArtifactKind,
    pub label: String,
    pub target: String,
}
```

### Agent Log Input

Tier 4 should stay raw and explicit, but it must not pretend to expose hidden
chain-of-thought. The truthful initial slice is runtime `tool`, `result`, and
`blocked` records only.

```rust
pub struct AgentRuntimeStatus {
    pub agent_id: String,
    pub operation_id: Option<String>,
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub status: AgentStatus,
    pub detail: Option<String>,
    pub current_iteration: Option<u32>,
    pub max_iterations: Option<u32>,
    pub active_tool_name: Option<String>,
    pub updated_at: i64,
}

pub struct AgentLogEntry {
    pub event_id: String,
    pub agent_id: String,
    pub operation_id: Option<String>,
    pub entry_type: AgentLogEntryType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_params: Option<String>,
    pub created_at: i64,
}
```

## Event Routing Rules

### Stay In Main Thread Timeline

These are strategic/user-relevant events and should remain visible in the thread:

- `goal.plan.proposed`
- `goal.question`
- `goal.result`
- user-visible approval/block events
- final failure/result summaries

### Feed Tier 3 Projection Only

These are execution-plane events and should update the observability projection without polluting the thread:

- worker started / worker stopped
- step advanced
- tool execution milestone
- `agent.status` runtime updates
- handoff emitted
- implementation/review phase transitions
- branch opened / PR opened / CI state changes

### Feed Tier 4 Raw Logs

These are deep-debug runtime events and should stay out of the main thread timeline:

- tool calls
- tool parameters
- tool outputs
- blocked/error results

If a model/runtime later emits an explicit public rationale field, that can join
Tier 4. Hidden chain-of-thought is not part of this contract.

## Projection Ownership

The daemon should own the projection build step.

Reasons:

- the daemon already sees execution truth across agents
- app-side inference would duplicate routing logic
- typed projection lets the app stay simple and honest

The app should:

- render the last known snapshot
- show no pill if there is no real summary
- show empty state in the Project Board if there is no live projection

## Real App Policy

Until the projection exists:

- do not render fake `OperationsPill` data in the real thread view
- keep demo/mock observability only in isolated test/demo surfaces

## Phase 1 Implementation

Phase 1 should stay narrow:

1. daemon emits `OperationsPillSummary`
2. app renders pill only when real summary exists
3. Project Board can initially show:
   - active operations
   - current step
   - active agents
4. truthful branch/review/merge artifacts may follow incrementally once they are projected from persisted management ownership
5. truthful runtime chatter may follow incrementally once it is projected from daemon-owned bridge interaction logs
6. truthful current runtime status should project separately once the daemon owns a dedicated per-agent status store
7. raw runtime tool/result/blocked logs follow as append-only Tier 4 facts

This gets truthful Tier 3 into the product without waiting for full Tier 4 depth.

## Current Implementation Note

As of `2026-04-10`, the daemon now does own a dedicated per-agent runtime
status store and projects `agent_statuses` into
`thread.observability.snapshot`. The real app now uses that lane for:

- live operation chip labels
- `AgentLogsSheet` metadata
- truthful active-agent counts when available

The remaining gap is not runtime metadata. It is the higher-level ownership
hierarchy for development artifacts under goal/project work.

## App Boundary

Primary app consumers:

- `submodules/app/lib/src/screens/chat_view.dart`
- `submodules/app/lib/src/widgets/operations_pill.dart`
- `submodules/app/lib/src/widgets/project_board.dart`
- `submodules/app/lib/src/widgets/agent_logs_sheet.dart`

## Next Step

Continue strengthening development artifact ownership under goals/projects.
The real thread now already uses live `OperationsPillSummary` and
`thread.observability.snapshot`; the next step is a cleaner end-state
goal/task/work-item hierarchy above the current explicit thread attachment plus
run-scope fallback.
