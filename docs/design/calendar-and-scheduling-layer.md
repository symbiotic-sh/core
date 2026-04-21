# Calendar And Scheduling Layer

**Status**: Approved design target  
**Related Tasks**: T109, T113, T115  
**Related Docs**: `docs/design/declared-task-policy-time-windows.md`, `docs/design/archive-policy-scope-hierarchy.md`, `docs/design/memory-system.md`, `docs/design/ux-specification.md`, `control-plane/docs/design/declarative-control-plane.md`

## Goal

Define the end-state scheduling model for Symbiotic without turning Symbiotic
into workplace admin software.

The system needs more than external calendar connectors. It needs a canonical
internal scheduling layer that can represent:

- imported provider events when the operator chooses to sync them
- Symbiotic-created reminders and recurring agent workflows
- approval windows
- due dates and waiting conditions
- operator delivery availability
- optional operational audiences such as `oncall:*` when Symbiotic is managing
  technical workflows

The right design is:

- `Archive` is the canonical scheduling store
- external providers are optional adapters, not the source of truth for the
  whole system
- `Nucleus` reconciles Archive truth into runtime actions and projections
- frontend renders time locally while preserving canonical source timezone

## Core Decision

Symbiotic needs an **internal canonical scheduling layer** plus **optional
external sync**.

It should not be:

- external-only
  - wrong because internal tasks, reminders, and orchestration windows would
    have no first-class home
- internal-only
  - too isolated from the operator's real schedule when external context is
    genuinely useful

So the end-state model is:

1. `Archive` stores canonical scheduling entities and timing policy
2. connectors optionally sync with external calendars when the operator wants
   that context
3. `Nucleus` derives actionable schedule projections
4. UI shows a merged schedule view without losing ownership/source semantics

This is a scheduling layer for agent workflows and self-management. It is not
intended to become a full HR, PTO, or workplace calendar product.

## Canonical Ownership Model

### Internal First-Class Entities

Calendar/scheduling truth in Archive should include at least:

- `CalendarSource`
- `CalendarEvent`
- `AvailabilityRule`
- `SchedulePolicy`
- `SyncCursor`

These are conceptually distinct even if the first implementation stores them in
Markdown records.

### Ownership Categories

Every calendar event should declare an ownership/source mode:

- `internal`
  - created by Symbiotic or the operator inside Symbiotic
  - reminders, recurring task runs, project checkpoints, approvals
- `external_mirror`
  - imported from an external provider
  - Google Calendar, Outlook, CalDAV
- `external_reference`
  - lightweight reference to a provider object not fully mirrored yet
- `derived`
  - generated projection from canonical work or policy
  - for example "task overdue follow-up window opened"

The system must never confuse:

- a provider event
- an internal task-derived reminder
- a synthetic projection for UI/ops purposes

## Archive Layout

Recommended initial layout:

```text
knowledge-base/
  operations/
    calendar/
      sources/
        {source-id}.md
      events/
        {event-id}.md
      availability/
        {rule-id}.md
      sync/
        {cursor-id}.md
```

This keeps the schedule layer:

- backupable with the rest of Archive
- inspectable by humans and agents
- reconstructible after restore

It also keeps it separate from:

- credentials in `Vault`
- volatile runner/lease state in `Nucleus`

## Data Model

### `CalendarSource`

Represents one attached provider or local source.

Suggested fields:

```yaml
id: "calendar-source:google-primary"
kind: "google"          # google | microsoft | caldav | ics | internal
owner: "@operator:test"
display_name: "Google Primary"
timezone: "Europe/Bratislava"
sync_mode: "read_write" # read_only | read_write | import_only
enabled: true
default_calendar: true
```

### `CalendarEvent`

Represents a concrete scheduled item.

Suggested fields:

```yaml
id: "calendar-event:abc123"
source_id: "calendar-source:google-primary"
ownership: "external_mirror"   # internal | external_mirror | external_reference | derived
provider_event_id: "google:evt_123"
thread_id: "thread-finance"
goal_id: "goal-portfolio"
task_id: "weekly-review"
title: "Weekly portfolio review"
status: "confirmed"            # tentative | confirmed | cancelled
transparency: "busy"           # busy | free
privacy: "restricted"          # public | restricted | private
timezone: "Europe/Bratislava"
starts_at: 1775900400
ends_at: 1775904000
all_day: false
recurrence_rule: null
location: null
attendees: []
last_synced_at: 1775800000
```

### `AvailabilityRule`

Represents durable delivery availability and scheduling policy.

Suggested fields:

```yaml
id: "availability:@operator:test:default"
subject: "@operator:test"      # later also operator, team:infra, oncall:infra
timezone: "Europe/Bratislava"
working_hours:
  weekdays: [mon, tue, wed, thu, fri]
  start_local: "09:00"
  end_local: "18:00"
quiet_hours:
  start_local: "22:00"
  end_local: "08:00"
availability_windows: []
```

### `SyncCursor`

Represents provider synchronization progress.

Suggested fields:

```yaml
id: "sync:google-primary"
source_id: "calendar-source:google-primary"
provider_cursor: "opaque-token"
last_success_at: 1775800000
last_attempt_at: 1775800000
last_full_sync_at: 1775700000
status: "ok"
```

## Time Model

This layer must follow the same rule already established in
`declared-task-policy-time-windows.md`:

- factual instants use UTC/Unix
- local schedule semantics use timezone-aware rules

So:

- event timestamps like `starts_at` / `ends_at` are canonical UTC instants
- recurrence rules and availability rules are evaluated in their declared
  timezone
- UI may render in viewer local time, but must preserve source timezone context

This is necessary for:

- DST correctness
- recurring schedules
- cross-timezone work
- delivery availability windows

## Internal Versus External Semantics

### Internal Events

Internal events should exist even if no provider is connected.

Examples:

- recurring goal run at 08:00
- weekly review checkpoint
- reminder to approve deployment
- focus block reserved for a goal
- optional unavailable window when the operator explicitly wants to suppress
  delivery

These are first-class Symbiotic records.

### External Events

External connectors should import provider events into canonical event records.

Examples:

- Google Calendar meeting
- Outlook appointment
- CalDAV-shared event

External events are not "special" at the UI layer, but they keep source and
ownership metadata so Symbiotic knows:

- whether it may edit them
- whether provider sync must write changes back
- whether the event was user-created outside Symbiotic

## Sync Strategy

### Direction

Each source declares one of:

- `read_only`
- `read_write`
- `import_only`

Default recommendations:

- Google / Microsoft personal calendars
  - start `read_only`
- dedicated Symbiotic-managed provider calendar
  - allow `read_write`
- ICS feeds
  - `import_only`

### Sync Loop

Nucleus should own connector sync and write canonical Archive updates first.

Provider sync loop:

1. fetch provider delta using `SyncCursor`
2. normalize provider objects into canonical `CalendarEvent` records
3. append/update Archive records
4. update `SyncCursor`
5. emit derived observability events if needed

### Conflict Policy

The first honest policy should be conservative:

- if external source is `read_only`, internal edits do not overwrite provider
- if source is `read_write`, edits require explicit ownership + provider write
  success before considered fully synced
- provider deletions become canonical event state changes, not silent removal

No hard deletes.

## Scheduling Semantics

The internal calendar layer should power:

- recurring goals
- reactive check windows
- task due dates
- approval deadlines
- deferred escalation windows
- daily/weekly brief generation
- future meeting prep / follow-up flows

This means scheduling is not only "calendar integration". It is part of the
orchestration substrate for agent workflows and personal operations.

The correct relationship is:

- calendar entities are canonical timing truth
- task policy may reference calendar/availability policy
- Nucleus reconciles both into actionable windows

## Frontend Contract

Frontend should render:

1. event times in viewer local timezone by default
2. canonical source timezone when relevant
3. source/ownership badges
   - internal
   - Google
   - Outlook
   - CalDAV
   - derived

For cross-timezone work, the UI should be able to show:

- canonical/source timezone
- viewer-local converted time

without hiding which one is authoritative.

## Connectors

Recommended provider order when external sync is needed:

1. `Google Calendar`
2. `Microsoft / Outlook Calendar`
3. `CalDAV`
4. `ICS import/export`

Why:

- Google and Microsoft cover most real users
- CalDAV provides open-protocol interoperability
- ICS gives lowest-friction import/export and backup exchange

The connector boundary should normalize into the same Archive model rather than
leaking provider schemas upward.

## Rust Implementation Direction

The first implementation should not try to solve every provider at once.

Recommended crate/module direction:

```text
submodules/runtime/
  crates/
    symbiotic-calendar/
      src/
        types.rs
        recurrence.rs
        availability.rs
        provider/
          google.rs
          microsoft.rs
          caldav.rs
          ics.rs
  services/
    symbiotic-daemon/
      src/
        calendar_sync.rs
        calendar_projection.rs
```

Key responsibilities:

- `symbiotic-calendar`
  - canonical types
  - recurrence evaluation
  - timezone-aware availability logic
  - provider normalization adapters
- `symbiotic-daemon`
  - connector orchestration
  - Archive persistence
  - thread/goal/task projection

## Rust Library Guidance

Use mature time primitives, not ad hoc date logic.

At minimum, the implementation should rely on:

- `chrono`
  - UTC/local timestamp handling
- `chrono-tz`
  - named timezone support
- `rrule`
  - recurrence rule expansion
- provider-specific clients later as needed

Do not build recurrence or timezone transitions manually.

## Phased Rollout

### Phase 1

- canonical Archive data model
- internal calendar events
- availability rules
- recurrence support for Symbiotic-created recurring work

### Phase 2

- merged schedule view
- minimal external read-only sync for operators who want calendar context

### Phase 3

- CalDAV
- ICS import/export
- read-write sync for explicitly managed calendars

### Phase 4

- optional technical delivery subjects such as `oncall:*`
- calendar-aware escalation timing and delivery policy

## Non-Goals For First Slice

Do not start with:

- full bi-directional sync for every provider
- organization directory and room routing
- PTO / holiday / workplace-admin semantics
- meeting transcription or meeting assistant logic
- provider-specific custom fields in the top-level canonical schema

## Decision Summary

The best end-state is:

- `Archive` contains the canonical internal calendar and schedule layer
- external providers sync into that model
- `Nucleus` reconciles it into actions and projections
- frontend renders locally without losing canonical timezone/source semantics

That gives Symbiotic what it actually needs:

- full restore from Archive
- schedule-aware orchestration
- user-calendar interoperability
- correct future handling for quiet hours, delivery availability, recurring
  work, and optional operational audiences
