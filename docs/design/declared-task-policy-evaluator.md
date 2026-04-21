# Declared Task Policy Evaluator

**Status**: Approved design target  
**Related Tasks**: T113, T115, T111  
**Related Docs**: `control-plane/docs/design/declarative-control-plane.md`, `docs/design/thread-observability-source.md`, `docs/design/declared-task-policy-time-windows.md`, `docs/design/archive-policy-scope-hierarchy.md`, `docs/architecture/goals-layer.md`, `docs/architecture/symbiotic-daemon.md`

## Goal

Make Archive-native declared tasks first-class policy objects instead of one-shot
state transitions.

The end-state is:

- `Archive` is the only canonical durable truth
- `Nucleus` evaluates declared-task policy in the background
- policy evaluation appends canonical Archive events first
- Matrix/thread/UI effects are projections of those Archive facts
- operator defaults are configurable in the Archive, with strong built-in defaults

This extends the already-implemented first slice:

- explicit declared-task transitions via `goal.task.transition`
- explicit owner changes via `goal.task.assign`
- `on_enter_blocked` escalation
- Archive-native `task_replan_requested`
- daemon consumption of `task_replan_requested` into `task_replan_enqueued`

The missing layer is continuous background evaluation for:

- `after_secs`
- cooldown / repeat suppression
- repeated or stale replanning requests
- internal dependency wakeups
- explicit declared-condition wakeups

## Problem

Current declared-task policy is only partially runtime-aware:

- policy is stored canonically in Archive task docs
- entering `blocked` can escalate immediately
- `auto_replan` can request and enqueue replanning

But there is still no canonical evaluator for long-lived declared work:

- tasks with `after_secs` do not escalate later unless another command happens
- repeated escalation has no formal suppression rules beyond code-local checks
- `waiting` tasks cannot wake when dependencies change
- stale replan requests do not have a first-class lifecycle

That leaves the system too reactive to inbound commands instead of making
Nucleus a real Archive-driven reconciler.

## Decision

Add a background **Declared Task Policy Evaluator** to Nucleus.

It runs as part of the daemon's normal background loop and evaluates active
declared tasks from Archive truth.

It must:

1. load active goal/task snapshots from Archive
2. resolve effective task policy from layered defaults
3. derive policy state from task snapshots and task/goal events
4. append canonical Archive events for any policy consequence
5. project user-visible effects from those new Archive events

It must not:

- create a second canonical state store
- emit Matrix notices without an Archive fact first
- infer hidden state from app/UI behavior
- store durable planner truth outside the Archive

## Canonical Sources

Declared-task policy comes from four layers, highest precedence last:

1. built-in system defaults
2. operator defaults in `identity/preferences.md`
3. goal-level defaults in `operations/goals/{goal}/plan.md`
4. task-level `policy` in `operations/goals/{goal}/tasks/{task}.md`

The evaluator always resolves an **effective policy** from those layers before
making runtime decisions.

### 1. Built-In Defaults

These exist so the system behaves sensibly even when the operator does not
customize anything.

Recommended defaults:

| Task kind | Default mode | Default audience | Default severity | `on_enter_blocked` | `after_secs` | `max_count` | `cooldown_secs` |
|---|---|---|---|---:|---:|---:|---:|
| `waiting` | `notify_operator` | `operator` | `normal` | true | 21600 | 3 | 21600 |
| `approval` | `notify_operator` | `operator` | `normal` | true | 86400 | 3 | 86400 |
| `coordination` | `notify_operator` | `operator` | `normal` | true | 14400 | 3 | 14400 |
| `review` + `task_driver=declared` | `notify_operator` | `operator` | `normal` | true | 14400 | 3 | 14400 |

Built-in defaults intentionally do **not** auto-enable:

- `raise_alert`
- `auto_replan`

Those require explicit operator, goal, or task policy.

### 2. Operator Defaults

Operator defaults live in Archive under `identity/preferences.md`.

They tune default behavior without changing the canonical task schema itself.

Recommended shape:

```yaml
---
task_policy_defaults:
  evaluator:
    interval_secs: 30
    max_actions_per_tick: 32
  declared_task_defaults:
    waiting:
      mode: notify_operator
      audience: operator
      severity: normal
      on_enter_blocked: true
      after_secs: 21600
      max_count: 3
      cooldown_secs: 21600
    approval:
      mode: notify_operator
      audience: operator
      severity: normal
      on_enter_blocked: true
      after_secs: 86400
      max_count: 3
      cooldown_secs: 86400
    coordination:
      mode: notify_operator
      audience: operator
      severity: normal
      on_enter_blocked: true
      after_secs: 14400
      max_count: 3
      cooldown_secs: 14400
    review:
      mode: notify_operator
      audience: operator
      severity: normal
      on_enter_blocked: true
      after_secs: 14400
      max_count: 3
      cooldown_secs: 14400
---
```

This is user-configurable, but optional.

### 3. Goal-Level Defaults

Goal manifests may override operator defaults for that goal's task tree.

Recommended shape in `operations/goals/{goal}/plan.md`:

```yaml
---
task_policy_defaults:
  waiting:
    after_secs: 10800
  coordination:
    mode: auto_replan
    audience: oncall:infra
    severity: urgent
    after_secs: 7200
    max_count: 1
---
```

This is the right place for project-specific behavior like:

- "this project should auto-replan quickly"
- "approval tasks here should wait 48 hours before escalation"
- "coordination failures on this project should page alerts"

### 4. Task-Level Policy

Task docs remain the highest-precedence override:

```yaml
---
task_kind: waiting
task_driver: declared
declared_context:
  waiting_for: "operator budget confirmation"
policy:
  escalation:
    mode: notify_operator
    audience: operator
    severity: normal
    on_enter_blocked: true
    after_secs: 43200
    max_count: 2
    cooldown_secs: 21600
---
```

Task-level policy is for specific exceptions, not broad project defaults.

## Effective Policy Resolution

The evaluator resolves policy in this order:

```text
built-in defaults
-> operator defaults
-> goal defaults
-> task policy
```

Resolution rules:

- unset fields inherit from the lower-precedence layer
- explicitly set fields replace inherited values
- `task_kind` determines which default bucket applies
- `task_driver=declared` is required for escalation policy to have any effect
- illegal kind/driver combinations remain rejected during planning ingestion
- changing `mode` without explicitly setting `audience` / `severity` re-normalizes those fields to strong built-in defaults for that mode

### Audience And Severity

Escalation policy also carries:

- `audience`
  - who should receive the escalation
  - examples: `operator`, `team:infra`, `oncall:infra`, `exec`
- `severity`
  - how urgent the escalation is
  - values: `normal`, `high`, `urgent`, `critical`

Recommended defaults by mode:

- `notify_operator`
  - audience: `operator`
  - severity: `normal`
- `raise_alert`
  - audience: `operator`
  - severity: `high`
- `auto_replan`
  - audience: `operator`
  - severity: `normal`

## Runtime Model

### Snapshot Truth

Canonical snapshot state lives in task docs:

- `state`
- `execution_status`
- `last_status_change_at`
- `owner_hint`
- `declared_context`
- `policy`

### Event Truth

Canonical event truth lives in `operations/goals/{goal}/events/*.md`.

The evaluator must consume and emit structured goal events rather than inventing
parallel state.

## Policy State Machine

For declared tasks, the evaluator treats these task states as meaningful:

- `planned`
- `in_progress`
- `blocked`
- `done`
- `cancelled`

Escalation evaluation happens only when:

- `state == active`
- `task_driver == declared`
- `execution_status == blocked`
- effective escalation policy exists

When any of those is false, the task is skipped.

## Policy Triggers

### `on_enter_blocked`

Already implemented.

When a declared task explicitly transitions into `blocked`, Nucleus:

1. writes `task_status_changed`
2. writes `task_escalated`
3. optionally writes `task_replan_requested`
4. then projects user-facing side effects

### `after_secs`

Background evaluator trigger.

If a declared task has remained in `blocked` for at least `after_secs`, the
evaluator may escalate again if cooldown and repeat limits permit it.

Reference clock:

- start from `last_status_change_at` of the task snapshot
- if a later `task_escalated` event exists for the same trigger family, use
  that event to enforce cooldown

### Dependency Wakeups

Background evaluator trigger.

When a declared task depends on another task and all dependencies become
`done`, the evaluator may append a wakeup event instead of waiting for a manual
transition.

Recommended canonical event:

- `task_dependencies_satisfied`
- `task_condition_satisfied`

This does not force auto-start. It records the condition so the task can move
from passive blocking to a visible ready state.

### Repeated Or Stale Replans

The evaluator owns repeat suppression for replanning requests too.

Canonical states:

- `task_replan_requested`
- `task_replan_enqueued`
- `task_replan_skipped`

If a task remains blocked after a replan has already been requested/enqueued,
the evaluator should honor cooldown and `max_count` before issuing another
request.

## Archive Event Contract

Current canonical events stay in place:

- `task_status_changed`
- `task_escalated`
- `task_replan_requested`
- `task_replan_enqueued`
- `task_owner_changed`
- `plan_reconciled`

Add these canonical events for the evaluator:

- `task_escalation_suppressed`
  - cooldown or `max_count` prevented a new escalation
- `task_dependencies_satisfied`
  - all declared dependencies are now complete
- `task_replan_skipped`
  - replan was suppressed because an equivalent request is still active or the
    policy limit was reached

Recommended additional event metadata:

```rust
pub struct GoalEventManifest {
    pub goal_id: String,
    pub event_type: String,
    pub observed_at: i64,
    pub plan_version: u32,
    pub thread_id: Option<String>,
    pub task_id: Option<String>,
    pub previous_status: Option<String>,
    pub next_status: Option<String>,
    pub escalation_policy: Option<String>,
    pub escalation_trigger: Option<String>,
    pub escalation_audience: Option<String>,
    pub escalation_severity: Option<String>,
    pub escalation_count: Option<u32>,
    pub cooldown_until: Option<i64>,
    pub actor: Option<String>,
    pub note: Option<String>,
    // existing lineage fields omitted
}
```

The key design rule is:

- every user-visible policy effect must be explainable from Archive events

## Matrix / Thread Projection

The evaluator must not emit Matrix notices directly from hidden in-memory
decisions.

Instead:

1. evaluate task policy
2. append canonical Archive events
3. build projection actions from those new events
4. emit Matrix/thread/alert effects from those projection actions

This keeps the system restoreable and auditable.

### Projection Mapping

| Archive event | Projection |
|---|---|
| `task_escalated` + `notify_operator` | attached thread notice |
| `task_escalated` + `raise_alert` | attached thread notice + `#alerts` |
| `task_replan_requested` | thread update, optional board/update row |
| `task_replan_enqueued` | thread update, board/update row |
| `task_dependencies_satisfied` | thread update / board update only |
| `task_condition_satisfied` | thread update / board update only |
| `task_escalation_suppressed` | observability-only, no user-facing alert by default |
| `task_replan_skipped` | observability-only, no user-facing alert by default |

The thread board and header pill continue to consume the daemon-owned thread
observability projection, not raw Archive files directly.

Alert-grade projection is also allowed when:

- `audience != operator`
- `severity in {urgent, critical}`

This keeps the Archive policy expressive enough for on-call and operations use
cases without requiring a full organization router in the first slice.

## Module Layout

Recommended runtime layout:

```rust
// submodules/runtime/services/symbiotic-daemon/src/declared_task_policy.rs

pub struct DeclaredTaskPolicyEvaluatorConfig {
    pub interval_secs: u64,
    pub max_actions_per_tick: usize,
}

pub struct DeclaredTaskPolicyEvaluator {
    config: DeclaredTaskPolicyEvaluatorConfig,
}

pub struct PolicyEvaluationResult {
    pub archive_events_appended: usize,
    pub projection_actions: Vec<PolicyProjectionAction>,
}

pub enum PolicyProjectionAction {
    NotifyThread { thread_id: String, event: crate::events::DaemonEvent },
    RaiseAlert { event: crate::events::DaemonEvent },
    ReplanRequested { goal_id: String, task_id: String },
}

pub struct EffectiveDeclaredTaskPolicy {
    pub escalation: Option<EffectiveEscalationPolicy>,
}

pub struct EffectiveEscalationPolicy {
    pub mode: GoalTaskEscalationPolicy,
    pub on_enter_blocked: bool,
    pub after_secs: Option<u64>,
    pub max_count: Option<u32>,
    pub cooldown_secs: Option<u64>,
}

impl DeclaredTaskPolicyEvaluator {
    pub fn evaluate(
        &self,
        archive_root: &std::path::Path,
        now: u64,
    ) -> anyhow::Result<PolicyEvaluationResult>;
}
```

This evaluator should be called from the daemon background loop before normal
job leasing, alongside the existing replan scan.

## Integration Plan

1. Add `docs/design/declared-task-policy-evaluator.md`
2. Add `declared_task_policy.rs` in the daemon
3. Add operator default parsing from `identity/preferences.md`
4. Add goal-level default parsing from `plan.md`
5. Resolve effective policy for declared tasks
6. Implement `after_secs`
7. Implement cooldown / repeat suppression
8. Emit new canonical Archive events
9. Add daemon-side projection path for background policy events
10. Extend thread observability to read the new event types

## Initial Implementation Scope

The first implementation slice should be:

1. effective-policy resolution
2. `after_secs`
3. cooldown / `max_count`
4. `task_escalation_suppressed`
5. background projection of `notify_operator` / `raise_alert`

Do **not** mix this first slice with:

- SLA calendars
- wall-clock business hours
- automatic dependency graph repair
- speculative task auto-start

Keep the first evaluator deterministic and Archive-first.

## Why This Is The Right End State

This design preserves the system properties Symbiotic needs:

- full restore from Archive alone
- no hidden planner/control-plane truth
- user-configurable behavior with good defaults
- explicit, inspectable automation
- clean projection into thread observability without fake state

The result is a real declarative policy engine, not a collection of command
handler special cases.
