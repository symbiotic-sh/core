# Project, Goal, and Process Model

**Status**: Approved design target  
**Related Tasks**: T103, T108, T113, T115, T116  
**Related Docs**: `docs/design/goal-task-work-item-hierarchy.md`, `docs/design/goals-layer.md`, `control-plane/docs/design/declarative-control-plane.md`, `docs/design/thread-architecture.md`

## Goal

Define the canonical top-level model above tasks and work items so Symbiotic
can represent real operating reality cleanly:

- multiple companies / products / initiatives
- multiple outcomes inside each one
- reusable or recurring processes
- concrete planned work and execution beneath them

The system previously overloaded `goal` into too many meanings:

- project container
- desired outcome
- recurring process
- active runtime unit

That overload was tolerable while fixing `thread != project`, but it is no
longer good enough for the actual product.

## Core Decision

The correct end-state hierarchy is:

1. **Project**
   - top-level owned container
2. **Goal**
   - desired outcome inside a project
3. **Process**
   - reusable or recurring work structure inside a project
4. **Task**
   - planned slice under a goal or process
5. **Work Item**
   - atomic checked-out execution
6. **Artifact**
   - branch / PR / review / merge / CI / brief
7. **Thread**
   - messaging and observability surface only

## Definitions

### Project

Represents the thing being operated over time.

Examples:

- `Symbiotic`
- `Company A Growth`
- `Infra Ops`
- `Product B`

Responsibilities:

- namespace and grouping
- ownership
- aggregate priorities
- attached repos and major resources
- parent container for goals and processes

### Goal

Represents a desired outcome inside a project.

Examples:

- `Ship truthful observability`
- `Reduce incident rate`
- `Increase inbound qualified leads`
- `Land project-memory benchmark`

Responsibilities:

- outcome definition
- success condition
- urgency / approval policy
- parent for planned work toward that outcome

### Process

Represents reusable or recurring operating structure inside a project.

Examples:

- `Weekly architecture review`
- `Release loop`
- `Upgrade Symbiotic loop`
- `Sales follow-up loop`
- `Incident hygiene review`

Responsibilities:

- recurrence / cadence
- reusable operating structure
- optional generation of goals or recurring tasks
- operational continuity over time

Processes answer:

- what structured work should keep happening?

Goals answer:

- what outcome are we trying to achieve?

## Why Both Goal And Process Matter

Without `process`, recurring work gets flattened into one-off goals.

Without `goal`, desired outcomes get flattened into recurring machinery.

Example:

- project: `Symbiotic`
- goal: `Ship project-board truthfulness`
- process: `Weekly architecture review`

These are related but not interchangeable.

A process may:

- generate new goals
- generate recurring tasks under an existing goal
- trigger periodic reviews/check-ins

But a process should not replace the goal as the outcome container.

## Runtime Rule

Do not add more heavy runtime primitives than necessary.

The runtime ownership spine should stay:

- `project -> goal -> task -> work item -> artifact`

`process` should usually remain:

- Archive-native structure
- cadence / generation logic
- not a second execution primitive competing with `work item`

## Canonical Archive Layout

The end-state Archive layout should be:

```text
knowledge-base/
  operations/
    projects/
      {project}/
        project.md
        goals/
          {goal}/
            plan.md
            tasks/
            events/
        processes/
          {process}.md
```

This is not just one possible option. It is the best end-state because:

- ownership is explicit in the path
- project context does not need to be reconstructed indirectly
- goals and processes live beside each other under the same operated thing
- restore, navigation, and human inspection all become clearer

Current implementation does not match this yet, but this is the target shape we
should optimize toward.

## Canonical Manifests

### Project Manifest

Canonical path:

- `operations/projects/{project}/project.md`

Suggested frontmatter:

```yaml
---
id: "project:symbiotic"
slug: "symbiotic"
title: "Symbiotic"
state: active               # active | paused | archived
owner_hint: "operator"
priority: 1
thread_id: "thread-symbiotic"
policy_scopes:
  - "company:default"
repos:
  - "repo:symbiotic"
  - "repo:symbiotic-runtime"
  - "repo:symbiotic-app"
domains:
  - "product"
  - "runtime"
  - "design"
---
```

Responsibilities:

- top-level identity
- scope / namespace
- aggregate policy defaults
- attached repos/resources
- parent for goals and processes

### Goal Manifest

Canonical path:

- `operations/projects/{project}/goals/{goal}/plan.md`

Suggested frontmatter:

```yaml
---
id: "goal:symbiotic:truthful-observability"
project_id: "project:symbiotic"
slug: "truthful-observability"
title: "Ship truthful observability"
state: active                    # active | paused | achieved | abandoned
priority: 1
autonomy_level: semi             # manual | semi | auto
phase: implementation
owner_hint: "operator"
thread_id: "thread-symbiotic-observability"
policy_scopes:
  - "company:default"
success_criteria:
  - "Project Board reflects real daemon/runtime truth"
---
```

Responsibilities:

- define the outcome
- carry goal-specific policy and approval semantics
- parent planned tasks and lifecycle events

### Process Manifest

Canonical path:

- `operations/projects/{project}/processes/{process}.md`

Suggested frontmatter:

```yaml
---
id: "process:symbiotic:weekly-architecture-review"
project_id: "project:symbiotic"
slug: "weekly-architecture-review"
title: "Weekly architecture review"
state: active                   # active | paused | archived
thread_id: "thread-symbiotic-architecture"
owner_hint: "operator"
cadence:
  kind: weekly
  weekday: mon
  local_time: "09:00"
generator:
  mode: recurring_tasks         # recurring_tasks | goal_template | review_only
  target_goal_id: null
task_template:
  task_kind: review
  task_driver: declared
  title: "Review architecture drift"
  policy:
    escalation:
      mode: notify_operator
---
```

Responsibilities:

- define repeatable or recurring structure
- generate recurring tasks or goal instances
- provide stable operational continuity inside a project

## End-State Runtime Semantics

The runtime should not make `process` a competing execution primitive.

Best end-state:

- `project`
  - control-plane grouping and top-level ownership context
- `goal`
  - primary active outcome unit
- `process`
  - Archive-native generator / structure layer
- `task`
  - planning unit beneath goal or process
- `work item`
  - atomic execution projection

That means:

- goals produce active work toward outcomes
- processes generate or shape work over time
- work items remain the only atomic execution checkout unit

## Process Generation Modes

Processes should be able to do a small number of things well.

### 1. `recurring_tasks`

Used when the process should repeatedly create tasks under an existing goal or
project context.

Examples:

- weekly KPI review
- release checklist
- backlog grooming

### 2. `goal_template`

Used when each recurrence should instantiate a new goal.

Examples:

- monthly upgrade push
- quarterly planning cycle
- weekly prospecting campaign

### 3. `review_only`

Used when the process itself should only create reminders/checks and not new
execution by default.

Examples:

- architecture review
- incident postmortem checkpoint

These modes are enough for the end-state model. We do not need a huge process
DSL to start.

## Thread Rule

`thread` remains separate from all of the above.

A thread may attach to:

- project
- goal
- process
- task
- work item

But it owns none of them.

## Relationship To Existing Docs

When older docs say:

- `goal = project`
- `GoalProcessManager` as the top-level operating unit

read that as historical or implementation-centric wording, not final naming
canon.

The newer canonical interpretation is:

- some existing `goal`-centric runtime behavior will eventually be split across
  `project`, `goal`, and `process`

## Implementation Strategy

The best possible end-state is now decided:

- use nested `operations/projects/*`

Implementation can still happen in phases, but the target shape should no
longer be ambiguous.

Recommended catch-up order:

1. add `ProjectManifest` and `ProcessManifest` to the control-plane model
2. teach the parser to read `operations/projects/*`
3. add `project_id` to goal/task runtime structs
4. move planning/runtime docs and daemon code toward project-rooted paths
5. only then remove remaining goal-centric overload from implementation docs

Until then:

- do not introduce new design that further overloads `goal`
- do not add a parallel “company” runtime primitive
- prefer `project`, `goal`, and `process` explicitly in new docs
