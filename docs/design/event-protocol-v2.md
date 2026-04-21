# Event Protocol v2 — Simplified Chat Protocol

> **Task:** New (to be created)
> **Supersedes:** `symbiotic-core::EventType` enum (37 stale variants), daemon string-based event types (~29)
> **Related:** `docs/design/ux-specification.md` (sections 7, 8, 15), `docs/design/thread-architecture.md`
> **Shared crate:** Types defined in `symbiotic-core::protocol` — referenced by both daemon and app (via `rust-core` FFI)

---

## 1. Problem

The current event system has 29+ string-based event types encoding daemon internals onto the wire. The canonical `EventType` enum in `symbiotic-core` (37 variants) is unused — the daemon bypasses it entirely. The Flutter app filters out ~19 of the 29 types before rendering. Commands (app-to-daemon) and events (daemon-to-app) share the same namespace with no type-level distinction.

This complexity caused a production bug where `goal.step.completed` was silently remapped to `goal.completed` by a status-based catch-all, because the formatting logic was a 90-line if-else chain matching on string event types.

The UX spec (section 7-8) already describes a simpler model. This design aligns the wire protocol with what the spec intended.

---

## 2. Design Principle

**The wire protocol encodes what the user sees, not what the daemon does internally.**

The user sees three things on screen:

1. **Message** — The daemon says something. Shows as a chat bubble (for replies and results) or a small status pill (for progress updates). Think of it like a text message from a person — it's information flowing to you.

2. **Question** — The daemon needs your input before it can continue. Shows the question text with tappable choice chips (like "Budget / Mid-range / Luxury") or a text input field, or both. Plan approval is a question too — "Here's my plan, approve or reject?" with two buttons.

3. **Notification** — Something happened in a thread you're not currently looking at and needs your attention. Shows as a floating banner that expands from the input bar, with thread context so you know where it came from and can tap to go there.

Everything else — which thread it belongs to, what step the daemon is on, how to route your response back — is metadata attached to one of these three.

---

## 3. Two Message Types

All communication between the daemon and the app flows through Matrix as encrypted messages. We use two custom message types to separate direction:

- **`sym.e`** (event) — Daemon tells the app something. Flows daemon → Matrix → app.
- **`sym.c`** (command) — App tells the daemon to do something. Flows app → Matrix → daemon.

The daemon only listens for `sym.c` messages. The app only listens for `sym.e` messages. No parsing needed to determine direction.

---

## 4. Events (`sym.e`) — What the Daemon Sends

Every event has a **kind** (what type of interaction) and a **status** (what phase it's in). Both are integers for efficiency.

### 4.1 Example Event

Here's the daemon asking "What's your budget?" with three choices:

```json
{
  "msgtype": "sym.e",
  "body": "What's your budget?",
  "sym": {
    "v": 2,
    "k": 1,
    "s": 3,
    "t": "thr-xyz",
    "r": "$parent_event_id",
    "ch": ["Budget", "Mid-range", "Luxury"],
    "d": {},
    "ts": 1710841200
  }
}
```

The `body` field is always human-readable text — it's what you'd see if you opened the Matrix room in a standard client. The `sym` object is the structured data the app actually parses.

### 4.2 Event Fields

| Wire | Meaning | Type | Required | Description |
|------|---------|------|----------|-------------|
| `v` | version | int | yes | Always `2` for this protocol version |
| `k` | kind | int | yes | What type of interaction (see table below) |
| `s` | status | int | for kind 0-2 | What phase it's in (see table below) |
| `t` | thread | string | for kind 0-2 | Which conversation thread this belongs to. When the user responds from the floating input bar on another screen, the app uses this to route the response to the correct thread. |
| `r` | reply_to | string | no | The Matrix event ID (`$...`) of the message this responds to. Creates a visual quote-reply chain like Telegram — you can see which question an answer was for. Also used when tapping a chip to answer a question. |
| `ch` | choices | string[] | no | Tappable options. When present, the app renders choice chips or buttons below the message. Used for quick reply suggestions, plan approve/reject, and multi-choice questions. |
| `d` | detail | object | no | Extra structured data that doesn't fit in `body`. Plan steps, step progress counters, error details, etc. Always a JSON object with string keys. |
| `a` | action | string | for kind 3 | Dotted name describing a state change (e.g. `routing.created`). Only used for state events — the app needs these for internal bookkeeping but never shows them in chat. |
| `ts` | timestamp | int | yes | Unix timestamp in seconds |

### 4.3 Kind — What Type of Interaction

| Value | Name | What it means | How it looks in the thread | How it looks when user is on another screen |
|-------|------|---------------|---------------------------|---------------------------------------------|
| `0` | Message | The daemon is saying something — a reply, a result, a progress update, an error message. | Chat bubble (for substantive content) or small centered pill (for status updates like "Running step 2/3"). | Small toast at top of screen: "✈️ Tokyo Trip: Here's your itinerary..." with an [Open] button to jump to the thread. |
| `1` | Question | The daemon needs input before it can continue. The question text is in `body`, and if there are suggested answers, they're in `ch` (choices). | Chat bubble with the question text, plus tappable choice chips below it. If no choices, the text input field activates. | Floating card expands from the input bar showing the question + choices. User can answer right there without leaving their current screen. |
| `2` | Notification | Something happened in a background thread that needs attention, but doesn't require an inline response. | Not rendered in the chat thread (it's meant for out-of-thread delivery). | Banner with action buttons, like "Goal failed — [Open thread] [Dismiss]". |
| `3` | State | Internal app bookkeeping — thread created, credential stored, daemon health snapshot. | Never shown in chat. | Never shown to user. The app uses these to update its internal state (thread list, vault status, connection indicator). |

### 4.4 Status — What Phase It's In

| Value | Name | What it means | Visual treatment |
|-------|------|---------------|-----------------|
| `0` | Working | The daemon is actively processing something. | Small muted pill with a pulse/spinner animation. Used for step progress ("Running step 2/3: Research"). |
| `1` | Success | Something completed successfully. | Green-accented bubble. Used for results ("Here's your itinerary...") and completion markers ("Goal completed"). |
| `2` | Fail | Something went wrong. | Red-accented bubble or pill. Used for errors ("Step failed: timeout") and goal failures. |
| `3` | Awaiting | The daemon is waiting for the user to respond. | Amber-accented bubble with active choice chips. The input bar highlights to draw attention. |
| `4` | Accepted | The user already responded to this question (chips should be disabled). | Choice chips are grayed out with the selected one highlighted. Prevents double-tapping. |

### 4.5 Notification Priority

The input bar can only show one floating notification at a time. When multiple arrive, they queue by priority:

1. **Questions** (kind 1) — highest. The daemon is blocked waiting for you.
2. **Failures** (kind 0, status 2) — something broke and you should know.
3. **Successes** (kind 0, status 1) — a result is ready.
4. **Notifications** (kind 2) — lowest. Background info.

### 4.6 State Events (kind 3)

State events are never shown in chat. They carry a dotted `a` (action) name that tells the app what to update internally:

```json
{ "k": 3, "a": "routing.created", "t": "thr-new", "d": { "title": "Tokyo Trip" }, "ts": 1710841200 }
```

State actions:
- `routing.created` / `routing.moved` / `routing.split` / `routing.archived` / `routing.promoted` — thread management
- `snapshot` — daemon health for connection indicator
- `credential.*` — vault state changes
- `escrow.*` — key escrow lifecycle
- `install.*` — setup wizard progress

---

## 5. Commands (`sym.c`) — What the App Sends

Commands are how the app tells the daemon to do things. They have a dotted command name and optional data.

```json
{
  "msgtype": "sym.c",
  "body": "",
  "sym": {
    "v": 2,
    "c": "goal.approve_plan",
    "t": "thr-1",
    "r": "$plan_event_id",
    "d": {}
  }
}
```

### 5.1 Command Fields

| Wire | Meaning | Type | Required | Description |
|------|---------|------|----------|-------------|
| `v` | version | int | yes | `2` |
| `c` | command | string | yes | Dotted command name — what the user wants to do |
| `t` | thread | string | when relevant | Which thread this command is for |
| `r` | reply_to | string | no | Matrix event ID of the event this responds to (e.g., the plan that was approved) |
| `d` | detail | object | no | Command parameters |

### 5.2 Command List

| Command | When the user... | Key `d` fields |
|---------|------------------|----------------|
| `goal.deliberate` | Types something classified as a goal | `description` |
| `goal.approve_plan` | Taps "Approve" on a plan card | — |
| `goal.reject_plan` | Taps "Reject" on a plan card | `reason` (optional) |
| `goal.answer` | Answers an agent's question or taps a chip | `text` |
| `goal.stop` | Stops a running goal | — |
| `goal.retry` | Retries a failed goal | — |
| `credential.submit` | Submits an API key or password | `service`, `key` |
| `credential.remove` | Deletes a stored credential | `service` |
| `intake.note` | Captures a text note | `text` |
| `push.register` | App registers push token on startup | `token`, `platform` |

---

## 6. Message Interactions

Every Matrix message gets a server-assigned event ID (`$hash`). This means any message in a chat thread can be referenced — enabling reply, promote, and other actions.

### 6.1 Reply to Message

Long-press a message → "Reply". The app shows the quoted original above your text input (like Telegram/WhatsApp). Your response carries `r` pointing to the original message's Matrix event ID.

```
[$ev1] Agent: "Here are 3 hotel options..."
[$ev2, r: $ev1] User: "What about the second one?"
```

### 6.2 Promote to Thread

Select a message → "Start thread". Creates a new conversation thread seeded from that message. The original thread shows a routing card ("→ Promoted to 🎧 Headphone Research"), and the new thread shows a "Continued from ✈️ Tokyo Trip" link at the top.

```json
{ "k": 3, "a": "routing.promoted", "t": "thr-headphones", "d": { "origin": "$ev_id", "from": "thr-tokyo", "title": "Headphone Research" } }
```

### 6.3 Actions Menu

Long-press any message to see available actions:
- **Reply** — Quote-reply in the current thread
- **Promote** — Start a new thread from this message
- **Copy** — Copy the message text
- **Share** — iOS/Android share sheet
- **Pin** — Pin to the top of the thread for quick reference
- **Forward** — Send this message to a different thread

All actions reference the message by its Matrix event ID — no custom ID system needed.

---

## 7. Correlation — How Messages Link Together

Two fields connect messages into conversation chains:

| Field | Purpose |
|-------|---------|
| `t` (thread) | Groups messages into a conversation. The floating input bar uses this to route your response to the correct thread, even if you're on a different screen. |
| `r` (reply_to) | Links a message to the specific message it's responding to. When you tap a choice chip on a question, your answer carries `r` pointing to that question. This lets the UI show which question was answered. |

Matrix assigns each message a globally unique event ID (`$hash`) on the server — no collision risk, no ID generation logic needed.

### Full Goal Flow Example

```
Thread: "thr-tokyo"

[$e1]          User: "Plan me a trip to Tokyo"
[$e2, r:$e1]   k:1 s:3 "What's your budget?" ch:[Budget, Mid-range, Luxury]
[$e3, r:$e2]   User taps "Mid-range"
[$e4, r:$e1]   k:1 s:3 "How many days?" ch:[3-4, 5-7, 7+]
[$e5, r:$e4]   User taps "5-7"
[$e6, r:$e1]   k:1 s:3 plan card ch:[Approve, Reject]
[$e7, r:$e6]   command: goal.approve_plan
[$e8]          k:0 s:0 "Running step 1/3: Research" (pill)
[$e9]          k:0 s:0 "Running step 2/3: Book flights" (pill)
[$e10]         k:0 s:1 "Here's your 5-day itinerary..." (result bubble)
[$e11]         k:0 s:1 "Goal completed" (pill)
```

### Floating Input Scenario

User is browsing the Memory tab. Event `$e2` arrives from a background goal:

1. The input bar expands upward: "✈️ Tokyo Trip: What's your budget?" with chips [Budget, Mid-range, Luxury]
2. User taps "Mid-range"
3. App sends a command with `t: "thr-tokyo"` and `r: "$e2"` — routing is automatic
4. The bar collapses. The thread card in the Stream list updates.

No event type string matching. The thread ID handles routing. The reply_to links the answer to the question.

---

## 8. What `detail` Carries

The `d` (detail) field carries structured data that varies by context. Always a JSON object — no key=value string parsing.

| Context | What `d` contains |
|---------|-------------------|
| Plan approval card | `{ "plan": { "summary": "...", "confidence": 0.85, "steps": [...] } }` |
| Step progress pill | `{ "step": "research", "i": 2, "n": 3, "role": "researcher" }` |
| Error message | `{ "error": "timeout after 30s", "step": "fetch" }` |
| Goal result | `{ "format": "markdown" }` (the actual content is in `body`) |
| Credential request | `{ "service": "gemini", "field": "api_key" }` |
| Intake capture | `{ "url": "https://...", "title": "Article Title" }` |

---

## 9. Migration Path

### Clean cutover (no dual-emit)

We control both sides (daemon + app) and ship them together. There are no third-party consumers of `org.symbiotic.event`. A dual-emit transition period would add complexity for zero benefit. Instead: replace v1 with v2 in a single coordinated change, verify with E2E tests.

**Implementation order:**

1. **Shared types** — Create `symbiotic-core/src/protocol.rs` with `Kind`, `Status`, `EventPayload`, `CommandPayload`
2. **Daemon** — Replace `org.symbiotic.event` emission with `sym.e`. Delete string-based event type machinery (`DaemonEvent.event_type`, `goal_envelope_fields()`, `format_step_body()`, `GOAL_EVENT_TYPES`)
3. **App** — Replace `org.symbiotic.event` parsing with `sym.e`/`sym.c` msgtype dispatch. Delete `displayBody` switch (30 cases), `_isVisibleMessage()` whitelist, `_classifySender` switch, `EventRouter` type-prefix matching, `_shouldNotify` type checks
4. **Commands** — Replace app command emission with `sym.c`
5. **Dead code** — Delete stale `EventType` enum (37 variants) from `symbiotic-core`
7. **E2E verify** — Run full test suite

**v1→v2 mapping reference** (for the implementation):

| v1 event type | v2 kind + status |
|---------------|-----------------|
| `goal.question` | `k:1 s:3` + `ch` from quick_replies |
| `goal.plan.proposed` | `k:1 s:3` + `ch:["Approve","Reject"]` + plan in `d` |
| `goal.result` | `k:0 s:1` |
| `goal.step.started` | `k:0 s:0` + step info in `d` |
| `goal.step.completed` | `k:0 s:1` + step info in `d` |
| `goal.step.failed` | `k:0 s:2` + error in `d` |
| `goal.completed` | `k:0 s:1` |
| `goal.failed` | `k:0 s:2` |
| `chat.reply` | `k:0 s:1` |
| `task.result` | `k:0 s:1` |
| `goal.deliberation.*` | Not emitted in v2 (daemon-internal, never user-visible) |
| `routing.*` | `k:3` + `a:routing.*` |

**Code deleted** (~950 lines):
- `DaemonEvent.event_type: String` → replaced by `Kind` + `Status` enums
- `EventType` enum in `symbiotic-core` (37 dead variants) → replaced by 4-value `Kind`
- `goal_envelope_fields()`, `format_step_body()`, `GOAL_EVENT_TYPES` → gone
- `ChatView._isVisibleMessage()` whitelist → everything in `sym.e` is visible by definition
- `displayBody` switch (30 cases) → `body` IS the display text
- `_classifySender` switch → direction determines sender
- `EventRouter` type-prefix matching → msgtype dispatch (`sym.e` vs `sym.c`)
- `_shouldNotify` type checks → kind + status priority sort

### Phase 2: Message interactions (future)

- Reply-to support (visual quote-reply chain)
- Message actions menu (reply, promote, copy, share, pin, forward)
- Thread promotion flow

---

## 10. Wire Efficiency

The `sym` payload uses short field names and integer enums to minimize message size. The `body` field stays human-readable as a fallback for raw Matrix log inspection.

### 10.1 Translation Map

**Kind (`k`)** — integer on the wire, enum in code:

| Wire value | Rust | Dart | Meaning |
|-----------|------|------|---------|
| `0` | `Kind::Message` | `Kind.message` | Daemon says something |
| `1` | `Kind::Question` | `Kind.question` | Daemon needs input |
| `2` | `Kind::Notification` | `Kind.notification` | Background attention needed |
| `3` | `Kind::State` | `Kind.state` | App bookkeeping (not rendered) |

**Status (`s`)** — integer on the wire, enum in code:

| Wire value | Rust | Dart | Meaning |
|-----------|------|------|---------|
| `0` | `Status::Working` | `Status.working` | In progress |
| `1` | `Status::Success` | `Status.success` | Completed OK |
| `2` | `Status::Fail` | `Status.fail` | Error |
| `3` | `Status::Awaiting` | `Status.awaiting` | Needs user response |
| `4` | `Status::Accepted` | `Status.accepted` | User responded |

### 10.2 Shared Crate

These types live in `symbiotic-core::protocol` — the shared crate referenced by both the daemon and the app's `rust-core` FFI bridge. One definition, used everywhere:

```rust
// symbiotic-core/src/protocol.rs

use serde_repr::{Serialize_repr, Deserialize_repr};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
pub enum Kind {
    Message = 0,
    Question = 1,
    Notification = 2,
    State = 3,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
pub enum Status {
    Working = 0,
    Success = 1,
    Fail = 2,
    Awaiting = 3,
    Accepted = 4,
}

/// A v2 event payload (inside sym.e Matrix messages).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPayload {
    pub v: u8,
    pub k: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ch: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
    pub ts: u64,
}

/// A v2 command payload (inside sym.c Matrix messages).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPayload {
    pub v: u8,
    pub c: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
}
```

The Dart side mirrors this via the `rust-core` FFI bridge, or with a simple Dart class that reads the same integer values. Either way, both sides share the same enum values — `0` always means `Message`, `1` always means `Question`, etc.

### 10.3 Size Comparison

A step progress message (v1 vs v2):

**v1** (~280 bytes in `sym`):
```json
{ "t": "goal.step.started", "s": "running", "rid": "run-abc", "d": { "detail": "step=research type=agent index=2 total=3", "template": "deliberation", "worker_event": "goal.step.started", "worker_status": "running", "goal_id": "goal-abc", "run_id": "run-abc" }, "ts": 1710841200 }
```

**v2** (~70 bytes in `sym`):
```json
{ "k": 0, "s": 0, "t": "thr-1", "d": { "step": "research", "i": 2, "n": 3 }, "ts": 1710841200 }
```

**~4x smaller.** No redundant fields. No string matching. Integer comparison for routing and rendering.

Fields not present are omitted entirely (`skip_serializing_if`), so a simple status pill is tiny:
```json
{ "k": 0, "s": 1, "t": "thr-1", "ts": 1710841200 }
```

---

## 11. Rendering Rules

**The user should never see daemon internals.** These rules govern what's shown in chat:

### What's visible
- **Questions** (kind=1, status=3) with choices → QuickReplyChips
- **Plan proposals** (kind=1 + `d.plan` or choices=["Approve","Reject"]) → PlanCard with approve/reject
- **Results** (kind=0, status=1 with substantive content) → AgentMessageBubble
- **Errors** (kind=0, status=2) → Red-accented bubble
- **User messages** → UserMessageBubble

### What's hidden or minimized
- **Step progress** (kind=0, status=0/1 with `d.step`) → Small `SystemEventPill` or update the PlanCard in-place. NOT full agent bubbles. These are daemon pipeline internals.
- **Classification results** (`complexity=simple`) → Never shown. This is daemon-internal UX classification output.
- **State events** (kind=3) → Hidden entirely (app bookkeeping only).
- **Deliberation phases** (classifying, auto-executing) → Small pill at most, not full bubbles.

### Progress display preference
Step progress should ideally **update the PlanCard in-place** rather than spawning separate messages. The plan card should show "Running step 2/3: Research" as a live status line, not flood the chat with individual pills.

---

## 12. Type Safety: Shared Protocol Types

The Dart `EventKind`/`EventStatus` constants MUST be generated from the Rust `Kind`/`Status` enums in `symbiotic-core::protocol`, not manually duplicated.

**Required approach**: Use `flutter_rust_bridge` (already in `submodules/app/rust-core`) to expose protocol types via FFI. One definition in Rust, auto-generated Dart bindings.

**Current state (2026-04-20)**: FRB bindings now exist in `symbiotic-mobile-core/src/frb_generated.rs`; the Rust `Kind`/`Status` enums are exposed to Dart via `flutter_rust_bridge` instead of being hand-duplicated.

---

## 13. Open Questions

1. **Should notification (kind 2) be separate, or just message (kind 0) with a priority field?** The UX spec delivers notifications through the same input bar. The distinction is where the user is, not the content type.

2. **Thread promotion UX** — Copy message to new thread, or reference card linking back?

3. **Pin semantics** — At thread top or just marked? Multiple pins?

4. **`d` schema validation** — Validate against `k`+`s` combinations, or leave flexible?

5. **Thread ID format** — Short slugs (`thr-tokyo`) or UUIDs? Slugs are human-readable; UUIDs avoid collisions.
