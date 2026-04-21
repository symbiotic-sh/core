# Declared Task Policy Time Windows

**Status**: Approved design target  
**Related Tasks**: T113, T115  
**Related Docs**: `docs/design/declared-task-policy-evaluator.md`, `control-plane/docs/design/declarative-control-plane.md`, `docs/architecture/goals-layer.md`

## Goal

Make declared-task escalation respect user time, not just raw elapsed seconds.

The evaluator already supports:

- built-in / operator / goal / task policy inheritance
- `after_secs`
- cooldown / `max_count`
- background `notify_operator` / `raise_alert` / `auto_replan`
- Archive-native blocker mutation and blocker clearing

The missing piece is **time semantics**:

- when a task becomes overdue
- when it is acceptable to notify a human
- how quiet hours and work windows affect repeated escalations

## Core Decision

Separate these two concepts:

1. `lateness_basis`
- determines when a blocked task becomes overdue

2. `delivery_window`
- determines when overdue work may be surfaced to humans

This avoids a common bad design where “don’t notify me at night” also silently
changes the meaning of task lateness.

## End-State Model

### Canonical Time Model

The system must distinguish between:

1. `instants`
- facts about when something actually happened
- stored canonically as UTC / Unix time
- examples:
  - `observed_at`
  - `created_at`
  - `last_status_change_at`
  - `cooldown_until`

2. `local timing policy`
- rules about when something should be considered late or delivered to humans
- stored canonically as timezone-aware policy, not pre-expanded Unix timestamps
- examples:
  - `timezone`
  - `delivery_window`
  - `quiet_hours`
  - `working_hours`
  - optional delivery-availability policy for operational audiences

3. `calendar-bound intentions`
- user or policy statements like "tomorrow at 09:00" or recurring availability
- stored as:
  - the resolved UTC instant when applicable
  - the original timezone-aware schedule/rule when recurrence or local semantics matter

This avoids a bad design where:

- all timing is flattened to local strings and becomes hard to reconcile
- or all timing is flattened to raw Unix time and loses local calendar meaning

The rule is:

- factual events use UTC/Unix
- scheduling policy uses timezone-aware local rules
- recurring calendar logic is derived from those local rules

### Lateness Basis

`lateness_basis` answers:

- should `after_secs` count raw elapsed time?
- or only time inside an active work window?

Canonical values:

- `wall_clock`
  - default
  - `after_secs` counts all elapsed time
- `delivery_window_elapsed`
  - `after_secs` counts only time inside the active delivery window

### Delivery Window

`delivery_window` answers:

- once a task is overdue, when may `notify_operator` or `raise_alert` actually
  surface?

Canonical values:

- `anytime`
- `outside_quiet_hours`
- `working_hours`
- `custom`

`auto_replan` remains special:

- canonical Archive events should still be emitted when overdue
- but thread/alert projection may wait for the delivery window
- replanning itself may still be enqueued immediately unless the policy says
  otherwise

## Archive-Native Policy Shape

This should layer exactly like the existing evaluator:

1. built-in defaults
2. operator defaults in `identity/preferences.md`
3. goal defaults in `operations/goals/{goal}/plan.md`
4. task overrides in task docs

Recommended operator-level shape:

```yaml
---
task_policy_defaults:
  evaluator:
    interval_secs: 30
    timezone: "Europe/Bratislava"
    delivery_window:
      mode: outside_quiet_hours
      quiet_hours:
        start_local: "22:00"
        end_local: "08:00"
    lateness_basis: wall_clock
---
```

This `timezone` anchors local schedule interpretation.

It does **not** replace canonical UTC event timestamps.

Goal-level override example:

```yaml
---
task_policy_defaults:
  evaluator:
    delivery_window:
      mode: working_hours
      working_hours:
        weekdays: [mon, tue, wed, thu, fri]
        start_local: "09:00"
        end_local: "18:00"
---
```

Task-level override example:

```yaml
---
policy:
  escalation:
    mode: raise_alert
    after_secs: 1800
    cooldown_secs: 3600
  timing:
    lateness_basis: delivery_window_elapsed
    delivery_window:
      mode: custom
      working_hours:
        weekdays: [sat, sun]
        start_local: "10:00"
        end_local: "16:00"
---
```

## Runtime Rules

### Storage Rule

Nucleus and Archive should store:

- runtime/event facts as UTC instants
- timing policy as explicit local-time rules plus timezone

That means a blocked task may have:

- `last_status_change_at: 1775900400`
- policy timezone: `Europe/Bratislava`
- delivery window: working hours `09:00-18:00`

The evaluator uses both:

- UTC instant for ordering and durability
- local policy for lateness and delivery math

### Overdue Rule

When a declared task is blocked:

- compute effective timing policy
- compute blocked duration according to `lateness_basis`
- if overdue:
  - append canonical Archive escalation facts
  - do not lose the fact just because a human-facing window is closed

### Projection Rule

When an overdue escalation exists:

- if current local time is inside the effective delivery window:
  - project to thread / alerts immediately
- otherwise:
  - append a canonical deferred/suppressed Archive fact
  - hold projection until the window opens

Recommended new Archive events:

- `task_escalation_deferred`
- `task_escalation_window_opened`

These are derived facts, not a second truth store.

## Why This Is Better

It preserves the right system properties:

- tasks can become overdue overnight
- humans are not spammed during quiet hours
- replay/restore from Archive remains possible
- operator defaults are configurable without changing task docs
- goal-specific delivery expectations remain possible

It also keeps the system ready for the minimal scheduling semantics Symbiotic
actually needs:

- quiet hours
- working windows
- recurring availability windows
- optional on-call delivery for technical workflows

Those are local-time policy problems layered on top of UTC event facts, not a
reason to stop storing factual timestamps canonically.

## Frontend Rendering Rules

Frontend must take the viewer's local timezone into account, but it must not
silently rewrite canonical policy semantics.

### Event Timestamps

- render factual timestamps in the viewer's local timezone by default
- preserve the canonical UTC/source instant internally
- show the source timezone/UTC value in detail surfaces when useful

### Policy Windows

- render policy windows using their canonical policy timezone
- example:
  - `Quiet hours: 22:00-08:00 Europe/Bratislava`
- if the viewer is in another timezone, the UI may additionally show a local
  conversion, but it must not hide the source policy timezone

### Cross-Timezone Work

If a task is anchored to another actor or team timezone:

- keep the canonical policy timezone visible
- optionally show the local viewer conversion secondarily

This prevents the UI from accidentally changing the meaning of:

- quiet hours
- working hours
- optional on-call delivery windows
- future calendar-derived policy when the operator enables it

### Calendar Direction

The eventual calendar layer should follow the same split:

- Archive stores canonical schedule/policy rules with timezone
- Nucleus resolves those rules into due/eligible delivery instants
- frontend renders resolved times locally while still exposing the canonical
  source timezone when it matters

## Deliberate Non-Goals For The First Timing Slice

Do not mix in yet:

- full calendar integrations
- workplace-admin scheduling semantics
- SLA math based on external vendor status pages
- mobile notification throttling policy

The first timing slice should only support:

- timezone
- quiet hours
- simple working hours
- lateness basis

## Next Implementation Slice

1. extend Archive preference / goal / task policy schema with timing fields
2. add timezone + delivery-window resolution to the evaluator
3. emit canonical deferred-window Archive events
4. only then project operator/alert events when the window opens
