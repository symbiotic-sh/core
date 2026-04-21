# Grouped Inquisition & Sub-Goal Spawn

> **Task:** [T130](../../tasks/130-grouped-inquisition/README.md)
> **Depends on:** [thread-architecture.md](thread-architecture.md), [deliberation-first-pipeline.md](deliberation-first-pipeline.md), [source-archeology.md](source-archeology.md) (Triage decision-tree pattern)
> **Composes with:** T116 (Internal Git Swarm), T126 (Repo Manifest + ApprovalGate), T127 (Project Bootstrap)
> **Extends (non-breaking):** [AskUserTool](../../submodules/runtime/crates/symbiotic-agents/src/builtin_tools.rs), `GoalProcess` in `symbiotic-control-plane::goals`, Inquisitor system prompt
> **Conviction:** ~65% — low, per [CONTEXT.md:87 conviction rubric](../../CONTEXT.md). Core abstractions span 4 crates; multiple valid shapes for the merge-back channel arbitration.

---

## 1. Motivation

Today's Inquisitor (per [deliberation-first-pipeline.md](deliberation-first-pipeline.md)) runs **one question at a time** against a single active goal. This works for simple clarification but has three structural limits:

1. **Sequential blocking.** User answers Q1 → Inquisitor plans → emits Q2 → user answers. If Q1, Q2, Q3 are independent (each unblocks a distinct phase of work), the system still serializes them, wasting wall-clock time and forcing the user to context-switch on each individual question.

2. **No "conviction dial."** Every question is equal-weight. The user cannot say "answer all design-phase questions with the defaults, but escalate any security-phase question for explicit review." The Inquisitor emits neither a `recommendation` nor a `confidence` annotation.

3. **No parallel push-forward.** When a question *is* answered, execution still routes back through the single active-goal pipeline. There's no mechanism for "this group of 3 answers unblocks a sub-goal that can run in parallel in an isolated git branch, while the main goal keeps planning."

The operator request: **treat questions as dependency-carrying artifacts that group together, spawn parallel sub-goals when their group is resolved, and merge results back via three channels (event / Archive / thread) with different audiences.**

This maps cleanly onto existing infrastructure: T116's internal git server + PR workflow for ephemeral sub-goal work, T126's `ApprovalGate` state machine for safety-gated merges into attached user repos, and the Thread Memory Document from [thread-architecture.md §5](thread-architecture.md) for durable merge-back.

---

## 2. Core Abstractions

### 2.1 `QuestionGroup`

A batch of independent questions that share an **unblock key**. When every question in the group has a resolved answer, a `goal.unblocked` event fires, which the Sub-Goal Dispatcher consumes to spawn work.

```rust
pub struct QuestionGroup {
    pub group_id: String,                 // stable within a goal, e.g. "design-phase"
    pub parent_goal_id: String,
    pub unblock_key: UnblockKey,          // what this group unblocks
    pub questions: Vec<AnnotatedQuestion>,
    pub created_at: OffsetDateTime,
    pub resolved_at: Option<OffsetDateTime>,
    pub resolution_mode: ResolutionMode,  // AllRequired | AnyOne | MajoritySignal
}

pub struct AnnotatedQuestion {
    pub text: String,
    pub quick_replies: Option<Vec<String>>,
    pub recommendation: Option<String>,   // default answer if user says "accept defaults"
    pub confidence: f32,                  // Inquisitor's confidence in the recommendation
    pub severity: QuestionSeverity,       // Trivial | Informational | Decision | Critical
    pub expected_answer_type: AnswerType, // FreeText | SingleChoice | Boolean | Scalar
}

pub enum QuestionSeverity {
    Trivial,        // default-everything category; ~2min grace
    Informational,  // low-impact / easy-to-reverse; ~30min grace
    Decision,       // normal choices; ~10min grace
    Critical,       // irreversible or high-impact; no grace — always operator
}

pub enum ResolutionMode {
    AllRequired,      // every question must be answered
    AnyOne,           // one answer is enough (the group is a disjunction)
    MajoritySignal,   // ≥N answers, used for "polling" groups
}
```

### 2.2 `UnblockKey`

A structured routing label. The Sub-Goal Dispatcher uses this to decide **which backend** executes the unblocked work and **what scope** that work has.

```rust
pub enum UnblockKey {
    /// Exploratory / throwaway work. Routes to T116 internal git swarm.
    /// Outputs become Archive notes via Distillery; no persistent branch.
    Exploratory { topic: String },

    /// Work targeting a user-attached project repo. Routes to T126
    /// mirror_push_with_approval with ApprovalGate. Requires explicit
    /// agent_scopes.push_external in the RepoManifest.
    AttachedRepo {
        repo_id: String,
        branch_hint: String,
        requires_approval: bool,
    },

    /// Archive-only research. Routes to a sandboxed researcher agent;
    /// output lands in knowledge-base/semantic/ as a finding note.
    ResearchOnly { question: String },

    /// Composite — spawns multiple sub-goals, one per inner key.
    /// Parent completes when all children reach terminal state: either
    /// completed (success/partial/failed) OR explicitly flagged obsolete
    /// and killed by the parent. No "any-child-complete" shortcut —
    /// either the child finishes its work, or the parent owns the decision
    /// to declare it obsolete.
    Composite { children: Vec<UnblockKey> },
}
```

### 2.3 `GoalDAG`

Extend `GoalProcess` with minimal dependency vocabulary:

```rust
pub struct GoalProcess {
    // ... existing fields ...

    pub parent_goal_id: Option<String>,
    pub unblock_key: Option<UnblockKey>,       // set on child goals
    pub blocked_by_groups: Vec<String>,        // group_ids that must resolve before this goal runs
    pub spawns_on_unblock: Vec<PlannedSpawn>,  // pre-planned sub-goals
}

pub struct PlannedSpawn {
    pub unblock_key: UnblockKey,
    pub when_group_resolved: String,   // group_id trigger
    pub initial_prompt: String,        // seed for the sub-goal's Inquisitor
}
```

Critically, `blocked_by` **already exists at the work-item layer** ([work_items.rs:23](../../submodules/runtime/crates/symbiotic-control-plane/src/work_items.rs:23)). This design lifts that vocabulary up to goals without inventing new terminology.

### 2.4 `EscalationPath` & `ResolutionTrail`

Before any question reaches the operator, the sub-agent walks a four-tier escalation ladder (full protocol in §13). Each tier attempted produces a trail fragment; by the time a question surfaces to the operator, it carries the full reasoning history:

```rust
pub enum EscalationTier {
    Recall,   // looked up in knowledge-base/methodology/preferences/
    Peer,     // consulted a cheap read-only peer agent
    Council,  // T60 Multi-LLM Council deliberation
    Operator, // escalated to the human
}

pub struct ResolutionTrail {
    pub recall_attempts: Vec<RecallAttempt>,
    pub peer_opinions: Vec<PeerOpinion>,
    pub council_result: Option<CouncilResult>,
    pub final_tier_reached: EscalationTier,
    pub escalation_reason: String,
}
```

`AnnotatedQuestion` gains a `resolution_trail: Option<ResolutionTrail>` — `None` when emitted fresh, populated by the time the operator sees it.

### 2.5 `PreferenceLedger` & `PreferenceDigest`

Every operator-answered question (and its resolution trail) lands in an append-only ledger. A periodic Distillery pass converts raw ledger entries into structured preference notes that feed back into Recall, Inquisitor context, and agent system-prompt hydration (full protocol in §14):

```rust
pub struct LedgerEntry {
    pub id: String,
    pub emitted_at: OffsetDateTime,
    pub thread_id: String,
    pub goal_id: String,
    pub sub_goal_id: Option<String>,
    pub question: AnnotatedQuestion,
    pub resolution_trail: ResolutionTrail,
    pub operator_answer: OperatorAnswer,
    pub tags: Vec<String>,
}

pub struct OperatorAnswer {
    pub text: String,
    pub overrode_recommendation: bool,
    pub free_text_note: Option<String>,
    pub autonomy_tolerance_active: AutonomyTolerance,
}
```

Ledger entries are **immutable once written**. Supersession happens at the distilled-preference level, preserving the raw evidence chain.

---

## 3. Batched Inquisitor Flow

> **Precondition:** The sub-agent has already walked the bottom-up escalation ladder in §13. By the time this section applies, Recall / Peer / Council tiers have all failed to resolve autonomously (or the question's severity mandates operator input per §13.3). Batched emission is the *last* resort — not the first.


### 3.1 Emission (replacing sequential `ask_user` loops)

The Inquisitor's system prompt changes from "emit one question and wait" to "emit all questions for the current phase as a single group." Tool shape:

```json
{
  "tool": "ask_user_group",
  "arguments": {
    "group_id": "design-phase",
    "unblock_key": {"type": "Exploratory", "topic": "frontend-framework-choice"},
    "resolution_mode": "AllRequired",
    "questions": [
      {
        "text": "Framework preference: Vue, React, or Svelte?",
        "quick_replies": ["Vue", "React", "Svelte", "No preference"],
        "recommendation": "Vue",
        "confidence": 0.72,
        "severity": "Decision",
        "expected_answer_type": "SingleChoice"
      },
      {
        "text": "SSR required for initial load?",
        "quick_replies": ["Yes", "No"],
        "recommendation": "Yes",
        "confidence": 0.85,
        "severity": "Decision",
        "expected_answer_type": "Boolean"
      },
      {
        "text": "Target browser support (e.g. 'last 2 versions', 'ES2020+')",
        "quick_replies": null,
        "recommendation": "last 2 versions + Firefox ESR",
        "confidence": 0.60,
        "severity": "Informational",
        "expected_answer_type": "FreeText"
      }
    ]
  }
}
```

### 3.2 Resolution — backend contract

This design fixes the **backend/data contract** for group resolution (§3.2.1 within-group async, §3.2.2 cross-group async). The specific **widget form** — how questions collapse on mobile, how focus + context are balanced, how the pending-group stack renders — is deliberately left as a separate UX design pass and not prescribed here.

**Backend constraints (hard deliverables in T130):**

- All N questions in a group are **addressable simultaneously** — the operator can answer any question without being forced through a sequential flow
- Per-question state machine: `Unanswered | Drafted | Submitted | Skipped`
- Free-order answering: daemon tolerates any submission order
- Drafts live **client-local only** — never crossed on the wire
- Only submitted answers cross Matrix (`goal.answer { question_index, answer }`)
- Submitted answers are immutable after submit
- Multiple groups pending simultaneously across sub-goals — each resolves independently
- Group resolution fires when `resolution_mode` is satisfied (last-submitted question wins)

**Widget form (deferred):**

The rich UX — layout, collapsing strategies on phone, focus patterns, accept-all interactions, pending-group navigation — is drafted in a separate future UX task. For T130's purposes, the app ships a minimal widget meeting the backend constraints above (e.g. stacked expandable cards, one question focused at a time with "next/prev" sibling navigation, simple draft persistence). Later iterations add the polished mobile-aware form.

**Rationale:** UX refinement on mobile (context-aware collapsing, focus management, information density) is orthogonal to the backend async contract. Coupling them in T130 would bloat scope and couple two different review cycles. T130 ships the backend + a minimal viable widget; a follow-up UX task owns the polished design.

When every question reaches a resolved state per the group's `resolution_mode`, the daemon emits:

```json
{
  "sym": {
    "t": "goal.unblocked",
    "s": "completed",
    "d": {
      "goal_id": "goal-build-frontend",
      "group_id": "design-phase",
      "thread_id": "thread-saas-product",
      "unblock_key": {"type": "Exploratory", "topic": "frontend-framework-choice"},
      "answers": { /* question_index → answer */ }
    }
  }
}
```

### 3.2.1 Within-group async answering (backend contract)

All `N` questions in a group are addressable simultaneously — the widget must not force a sequential flow. Whether that renders as all-visible-at-once (desktop) or focus-with-siblings-collapsed (phone) is a widget-form concern deferred to a separate UX task. What this section fixes is the **data + wire contract** the widget must respect:

**Per-question states (client-local; only submitted state crosses the wire):**

| State | Meaning | Wire effect |
|---|---|---|
| `Unanswered` | Initial — no interaction yet | Nothing sent |
| `Drafted` | Operator has typed/picked an answer but not submitted | Nothing sent; draft persists in local app state |
| `Submitted` | Operator confirmed the answer | `goal.answer { question_index, answer }` event emitted to Matrix |
| `Skipped` | Operator explicitly skipped (only legal for `ResolutionMode::AnyOne` / `MajoritySignal`) | `goal.answer { question_index, skipped: true }` emitted |

**Key properties (backend contract):**

- **Free order** — daemon tolerates submissions in any sequence; no enforced ordering
- **Editable drafts** — a drafted answer can be changed until Submit; submitted answers are immutable (audit integrity; enforced server-side by rejecting duplicate `goal.answer` events for the same `question_index`)
- **Partial submission is legal** — `Submitted` state fires immediately per question; group resolution fires when `resolution_mode` is met
- **Drafts persist locally** — client stores drafts so navigation doesn't lose work; drafts are **not** sent to the daemon (no wire pollution, no draft-sync protocol needed)
- **Accept-all is a drafting action, not a submit** — populates every question's draft slot with its `recommendation`; still requires explicit Submit
- **Critical-severity bulk-accept exclusion** — `Critical` questions cannot be bulk-accepted; require per-question explicit confirmation (hard-enforced in the widget + verified by the daemon rejecting any `goal.answer` on a Critical question that wasn't individually confirmed via a per-question interaction marker)

**Widget implementation notes (advisory only — deferred to UX task):**
- Desktop / wide-screen: all questions likely visible at once with individual answer controls
- Phone / narrow-screen: questions likely collapse; focus one question at a time with quick siblings navigation; context panels (resolution trail, recommendation rationale) expandable on demand
- Navigation affordances, information density, focus transitions: **all deliberately unspecified here**

**Why drafts stay local:**

If drafts crossed the wire, we'd either need a new `goal.draft` event type (protocol bloat) or we'd be persisting speculative state on the daemon (sync burden, weird failure modes). Keeping drafts local means the Rust wire protocol is unchanged from the non-async case — the daemon only sees `goal.answer` events, exactly as designed in §04. Drafts are a pure client-side UX enhancement.

**Draft persistence survives app restart:**

The app persists drafts to local storage (scoped per thread + question_group_id), so closing and reopening the app doesn't lose a half-answered group. This is the same mechanism the existing `ExpandableInputBar` uses for message drafts in [thread-architecture.md](thread-architecture.md).

### 3.2.2 Cross-group async concurrency (backend contract)

**Multiple `QuestionGroup`s can be pending simultaneously.** No enforced order at the group level either — but again, the specific UI surface for cross-group navigation (drawer, stack, inbox, etc.) is a widget-form concern deferred to the UX task.

Scenarios this needs to handle cleanly:

| Scenario | Expected behavior |
|---|---|
| 3 sub-goals each emit a `QuestionGroup` around the same time | All 3 groups appear in a pending-questions surface; operator picks any to work on first |
| Group A has been sitting unanswered for an hour; Group C just arrived | Operator can answer C first, leave A open indefinitely (subject to `AutonomyTolerance` grace-period timeout per §3.3) |
| Operator drafts 2 answers in Group A, switches to Group B, answers B completely, comes back to A | Drafts in A are preserved; answering B resolves B's goal; A's goal stays paused until its group resolves |
| Operator dismisses Group A without answering | Group A stays unresolved; its sub-goal's ladder didn't cross tier 4 for a reason, so this is a meaningful operator choice — surfaced in mission-control UI as "awaiting operator" amber pill |

**Implementation constraint: no group blocks another.** The daemon-side `QuestionResolver` (§04) tracks resolution per-group independently. The Dispatcher (§04 / §05–§07) doesn't care about answer order across groups — each `goal.unblocked` event fires when its specific group's `resolution_mode` is met, regardless of sibling groups' states.

**Discovery surface requirements (what the widget must enable, not how):**

- Operator must be able to **discover** all pending groups across all sub-goals at any time
- Operator must be able to see **per-group summary** (question count, completion state, severity, age) without opening the card
- Operator must be able to **switch between groups** without losing draft state in either
- Default presentation ordering: severity × recency (`Critical` > `Decision` > `Informational`; newest first within tier)

Exactly how this renders — persistent drawer, thread inline cards, notification center, dedicated inbox — is left to the UX task. T130's backend contract only requires the above capabilities be *possible* via the data model.

**Architectural implications of full async:**

1. **Sub-goals keep running** during pending-question time (bounded by `AutonomyTolerance` grace period) — a pending group does not block other goals' execution
2. **The agent behind Group A doesn't wait on the agent behind Group C** — they're independent sub-goals with independent state
3. **The escalation ladder in §13 is still sequential per-question** (Recall → Peer → Council → Operator) — only the operator-tier is async; lower tiers don't surface partial state to the UI (they either resolve or escalate)
4. **Cancellation is per-group** (§9 cascade) — timing out or cancelling one group does not cascade to unrelated sibling groups of other sub-goals

### 3.3 Conviction × severity → grace period + auto-accept/fail

Grace length and the auto-accept/auto-fail decision are driven by **both** the question's severity AND the Inquisitor's `confidence` score. Not one or the other — both dimensions matter.

**The rule:**

- `confidence < 0.70` (configurable threshold) → **auto-fail** regardless of severity; question escalates to operator immediately
- `confidence ≥ 0.70` → auto-accept after a severity-scaled grace period (if no operator action)
- `severity = Critical` → **no grace period ever**; always waits for operator, regardless of confidence

**Grace periods by severity (defaults; operator-configurable):**

| Severity | Grace (if confidence ≥ 0.70) | Rationale |
|---|---|---|
| `Critical` | No grace — always wait | Irreversible or high-impact decisions need explicit operator |
| `Decision` | 10 minutes | Most common category; balanced latency vs attention |
| `Informational` | 30 minutes | Low-impact / easy-to-reverse; OK to auto-accept after a longer wait |
| `Trivial` (new) | 2 minutes | Default-everything category; quick auto-accept |

**Auto-accept / auto-fail logging:**

Every auto-passed or auto-failed question writes a ledger entry with:

```rust
pub struct AutoResolutionRecord {
    pub question_id: String,
    pub severity: QuestionSeverity,
    pub confidence: f32,
    pub grace_period_used: Duration,
    pub decision: AutoDecision, // Accepted | Failed | EscalatedToOperator
    pub resolved_at: OffsetDateTime,
}
```

These records feed two things:
1. **Operator review surface** — filterable log of "what did the system decide for me while I wasn't paying attention?" Operator can review and retroactively flag any that were wrong (becomes a DPO training pair per T131)
2. **Calibration loop** — if auto-accepted decisions show a pattern of being wrong (high retroactive-flag rate), the confidence threshold for that category auto-tightens; if they're consistently right, stays stable

**AutonomyTolerance replaced by this model** — the prior `Strict | Balanced | Autonomous` enum is subsumed by the severity × confidence × grace dimensions. Operator can still set a global "strict mode" that extends every grace period to infinity (effectively disabling auto-accept) if they want the old Strict behavior, but the severity+confidence dial is the primary control.

**Configurable thresholds:**

| Setting | Default | Effect |
|---|---|---|
| `fail_threshold` | 0.70 | Below this confidence, always escalate; no auto-accept possible |
| `grace_critical` | N/A (always wait) | Hard rule; not configurable down to a grace period |
| `grace_decision` | 10 min | Severity=Decision grace length |
| `grace_informational` | 30 min | Severity=Informational grace length |
| `grace_trivial` | 2 min | Severity=Trivial grace length |
| `strict_mode` | false | If true, extends all grace periods to infinity (operator must always answer) |

`Critical`-severity questions keep the hard-coded "always escalate" rule from before — this is unchanged. Even `strict_mode=false` + `confidence=0.99` + `severity=Critical` still waits for operator. This mirrors [source-archeology.md](source-archeology.md)'s "Critical severity → always escalate" rule.

---

## 4. Sub-Goal Dispatcher

### 4.0 Content firewall precondition

**Every backend in the routing tree below consumes or produces content that eventually enters agent context windows.** That content passes through the [Content Firewall](content-firewall.md) **at ingest — before Archive write**, not at context-assembly time. Archive is trusted because every entry carries a firewall verdict; context assembly runs only two cheap last-line checks (capability-boundary + annotation wrap) but does not re-scan.

Where the firewall is called in each backend:

| Backend | Ingest-time firewall call sites |
|---|---|
| `Exploratory` (T116 swarm) | Tool observations scan on return from sandbox (before observation log write); distillery-extracted swarm artifacts scan before Archive write |
| `AttachedRepo` (T126 push) | Pulled repo content scans before being written to the per-session cache; commit diffs scan before operator-approval queue |
| `ResearchOnly` | Any web-fetch tool response scans before being cached / archived; re-used Archive content uses stored verdict (no re-scan on each reuse) |
| `Composite` | Inherits ingest-time firewall calls of its children |

**Context-assembly defenses** (cheap, always run):
- Stage D capability-boundary check — per consuming agent's scope
- Stage E annotation wrap — frames content as external for the LLM

If the firewall subsystem is unavailable, the Dispatcher **fails closed on ingest**: new content cannot enter Archive, sub-goal execution that depends on new ingestion pauses until firewall is back. Archive reads still work (existing verdicts are valid), so sub-goals that only need already-archived content can continue. See [content-firewall.md §7](content-firewall.md) for the full failure-mode matrix.

### 4.1 Decision tree

On `goal.unblocked`:

```
switch unblock_key:
  Exploratory { topic } →
    // T116 swarm — ephemeral bare repo per sub-goal
    repo = daemon.create_swarm_repo("subgoal-{sub_goal_id}")
    sandbox = runner.spawn(
      prompt = seed_from_parent + group_answers,
      tools = [file_edit, git_push_swarm, ask_user_group, request_review],
      repo_endpoint = repo.internal_url,
    )
    child_goal = GoalProcess {
      parent_goal_id,
      unblock_key,
      phase: AgentExecute,
    }

  AttachedRepo { repo_id, branch_hint, requires_approval } →
    // T126 mirror+push with ApprovalGate
    manifest = registry.load(repo_id)
    assert manifest.agent_scopes.push_external
    session = GitPushSession::open(repo_id, branch_hint)
    sandbox = runner.spawn(
      prompt = seed_from_parent + group_answers,
      tools = [file_edit, git_push_attached(session), ask_user_group],
    )
    // On completion: mirror_push_with_approval gates the push via Matrix approval

  ResearchOnly { question } →
    // Lightweight — no git, no sandbox; routes through Recall Gateway + LLM
    agent = ResearcherAgent::new(question)
    archive_note = agent.execute(context_pack)
    emit goal.subgoal.completed { outcome: Success, artifact_refs: [archive_note] }

  Composite { children } →
    // Fan out — each child spawns independently; parent tracks all
    for child_key in children:
      recurse with unblock_key = child_key
```

### 4.2 Capacity control

The Dispatcher consults a daemon-wide concurrency cap before spawning:

```rust
pub struct SpawnBudget {
    pub max_concurrent_subgoals: usize,          // default 4
    pub max_concurrent_per_attached_repo: usize, // default 1 (serialize pushes)
    pub max_research_only: usize,                // default 8 (cheap)
}
```

If the budget is exceeded, the sub-goal enters `queued` status and the Dispatcher retries on the next child-completion event.

---

## 5. Three-Channel Merge-Back

On sub-goal completion (success, partial, or failure), the daemon emits to **three distinct channels** with **different content contracts**. Each channel has a different audience and failure mode.

### 5.1 Channel contracts

| Channel | Content | Audience | On Failure |
|---|---|---|---|
| **Event** `goal.subgoal.completed` | Structured enum outcome, artifact refs, unblock_key echo | Parent goal's Inquisitor (machine) | Parent hangs — event is the canonical signal |
| **Archive note** | Full durable artifact (research, methodology, decision trail) | Future agents via Recall Gateway | Silently skipped; not blocking |
| **Thread message** | Terse human summary, rendered as mission-control pill | User (UI) | Logged, best-effort |

### 5.2 Event shape

```json
{
  "sym": {
    "t": "goal.subgoal.completed",
    "s": "completed",
    "d": {
      "parent_goal_id": "goal-build-frontend",
      "sub_goal_id": "subgoal-auth-research",
      "unblock_key": {"type": "Exploratory", "topic": "oauth-library-choice"},
      "outcome": "Success",
      "summary_ref": "archive://semantic/oauth-libs-survey-2026-04-18.md",
      "artifact_refs": ["archive://episodic/subgoals/sg-auth-research/result.md"],
      "followups": [
        { "group_id": "implementation-phase", "reason": "research-surfaced-tradeoffs" }
      ]
    }
  }
}
```

`followups` allows a completed sub-goal to **register new question groups** for the parent — e.g. research surfaces a tradeoff worth escalating to the operator before coding begins.

### 5.3 Archive note shape

Written to `knowledge-base/episodic/subgoals/{sub_goal_id}/result.md`:

```markdown
---
type: episodic_note
sub_goal_id: subgoal-auth-research
parent_goal_id: goal-build-frontend
thread_id: thread-saas-product
unblock_key: { type: Exploratory, topic: oauth-library-choice }
outcome: Success
started_at: 2026-04-18T09:42:00Z
completed_at: 2026-04-18T10:07:31Z
---

# Sub-goal: OAuth library research

## Context
Parent goal asked: "which OAuth library should we use for the SaaS auth flow?"

## Approach
...

## Findings
- `oauth2` crate is the idiomatic Rust choice for PKCE flows
- `oauth2-axum` adds the route handlers but introduces a `hyper 1.x` dep conflict
- ...

## Recommendation
Use `oauth2` + hand-rolled axum handlers; avoid `oauth2-axum` until hyper alignment resolves.

## Evidence
- https://docs.rs/oauth2
- git: feature/oauth-research (branch in swarm repo, now archived)
```

`Failure`-outcome sub-goals write an Archive note **only if** the sub-agent's final reflection contains a non-trivial learning (flag set by the agent itself). Transient errors write nothing durable.

### 5.4 Thread message shape

```json
{
  "sym": {
    "t": "subgoal.completed",
    "s": "completed",
    "d": {
      "parent_goal_id": "goal-build-frontend",
      "sub_goal_id": "subgoal-auth-research",
      "outcome": "Success",
      "render_hint": "pill"
    }
  },
  "body": "✅ Sub-goal auth-research complete — recommended `oauth2` crate (branch auth-research-01)"
}
```

The `render_hint: "pill"` tells the app to render this as a mission-control status pill in the thread, not a full chat bubble — consistent with [CONTEXT.md:22 "Mission Control UX"](../../CONTEXT.md).

### 5.4a Preference feedback (fourth channel, async)

Operator-answered question groups additionally feed the **Preference Ledger** per §14 — an async fourth channel that does not fit the "parent needs this to continue" pattern of §5.1's primary three. Every resolved group appends to `knowledge-base/self/preferences/ledger.jsonl`; a scheduled Distillery pass later distills ledger entries into structured preference notes in `knowledge-base/methodology/preferences/`, which then feed back into Step 1 (Recall) of the next question's escalation ladder. This is what closes the flywheel — fewer questions over time.

### 5.5 Progress events (no durable emit)

During sub-goal execution, the parent thread receives `subgoal.progress` events periodically (configurable; default every 30s if work ongoing). These events:

- **Do not** write to Archive (too noisy; not durable)
- **Do not** emit `goal.unblocked` to the parent planner (parent keeps waiting)
- Render in the thread as a transient amber pill: `🟡 subgoal-auth-research: investigating oauth2-axum version conflicts...`

---

## 6. Integration With Existing Thread Architecture

### 6.1 Thread relationship

Per [thread-architecture.md §3.2](thread-architecture.md), threads already support the `split` lifecycle (parent → child thread). Sub-goals **do not automatically split threads**; the default is that all sub-goal progress and results emit to the **parent goal's thread**.

A sub-goal can opt-in to thread-splitting only if:

1. Its unblock_key is `Composite` with ≥3 children, **and**
2. Any child is expected to generate ≥20 messages (heuristic threshold)

In that case the Dispatcher emits a `routing.split` event per [thread-architecture.md §3.2](thread-architecture.md) and routes child-sub-goal events to the child thread.

### 6.2 Thread Memory Document integration

Sub-goal Archive notes are indexed by the Thread Distillery per [thread-architecture.md §5](thread-architecture.md). A completed sub-goal enriches the parent thread's memory document with:

- A `FINDING` fact per sub-goal outcome
- A `METHODOLOGY` fact if the sub-agent's reflection surfaced a reusable pattern
- An `EPISODE` fact for the execution itself (decayable)

This threads (pun intended) the sub-goal system into the existing compounding-flywheel model — parallel execution enriches durable memory at the same rate as sequential execution, just with higher throughput.

---

## 7. Event Protocol Additions

Add to `symbiotic-core::events::EventType`:

```rust
// Question grouping
GoalQuestionGroup,           // "goal.question_group"           — emitted by Inquisitor
GoalQuestionGroupRejected,   // "goal.question_group.rejected"  — user rejected group shape
GoalUnblocked,               // "goal.unblocked"                — all questions in group resolved

// Sub-goal lifecycle
GoalSubgoalSpawned,          // "goal.subgoal.spawned"          — Dispatcher created sub-goal
SubgoalProgress,             // "subgoal.progress"              — periodic, UI-only
GoalSubgoalCompleted,        // "goal.subgoal.completed"        — merge-back event channel
GoalSubgoalFailed,           // "goal.subgoal.failed"           — terminal failure
```

All thread-scoped events carry `thread_id` per the existing [thread-architecture.md §6](thread-architecture.md) envelope convention.

---

## 8. Security & Capability Model

### 8.1 Gatekeeper checks

No new capabilities are introduced; all work flows through existing primitives:

- `Exploratory` sub-goals: require `spawn_swarm_repo` capability (already exists in T116)
- `AttachedRepo` sub-goals: require `push_external` per `RepoManifest.agent_scopes` (already enforced in T126)
- `ResearchOnly` sub-goals: require `recall_query` (default-granted) + `archive_write_semantic` (default-granted)
- `Composite` sub-goals: require the **union** of inner capabilities; checked per-child

### 8.2 The `recommendation` field is not a capability bypass

Auto-accepting a recommendation does **not** elevate the sub-goal's capabilities. An `AutonomyTolerance::Autonomous` setting means "user trusts the agent's *answer*"; it does not mean "the agent can push to an attached repo without approval." The `requires_operator_approval_for` on the repo manifest still gates the actual push.

This is deliberate — the autonomy dial governs *question answering*, not *capability elevation*. The ApprovalGate remains the sole arbiter of destructive actions on attached repos.

---

## 9. Failure Modes

| Failure | Response |
|---|---|
| Group's `resolution_mode` unsatisfied after configurable timeout | Emit `goal.question_group.expired`; Inquisitor re-asks or escalates to operator |
| Spawn budget exceeded | Sub-goal enters `queued`; Dispatcher retries on next child-completion |
| Sandbox runner crashes mid-execution | T113 bridge surfaces `agent_crashed`; Dispatcher marks sub-goal `Failed`, emits three-channel merge-back with outcome=Failed |
| ApprovalGate denies push (T126) | Sub-goal completes with outcome=`Partial`; Archive note still written; thread message says "push denied, awaiting operator" |
| Parent goal cancelled while sub-goals running | Dispatcher emits `subgoal.cancelled` to each; sandboxes torn down; swarm repos destroyed per T116 cleanup rules |
| Archive write fails | Event channel still fires; Archive write retried with exponential backoff; thread message annotates "durable memory pending" |
| Event emission fails | **Parent hangs** — this is the canonical signal, so we do NOT degrade it to best-effort; bubble error to daemon supervisor, which restarts the dispatcher |

---

## 10. Open Design Questions

| # | Question | Leaning |
|---|----------|---------|
| 1 | Default grace period for `AutonomyTolerance::Balanced` auto-accept? | 5 min — long enough for the user to notice the notification, short enough to not bottleneck parallel work |
| 2 | Should the user be able to retroactively downgrade a completed sub-goal's result? (e.g., "that research was wrong, archive note should be superseded") | Yes — via existing superseding chain in [thread-architecture.md §5 "Superseding, Not Deleting"](thread-architecture.md) |
| 3 | Can a sub-goal spawn further sub-sub-goals? | Yes, but limit depth to 3 (config) to prevent runaway fan-out |
| 4 | How does `Composite` interact with partial child completion? | Parent waits for **all** children; if any child fails terminally, parent completes with `outcome=Partial` |
| 5 | Should `ResearchOnly` sub-goals bypass the sandbox entirely? | Yes for MVP — they're read-only over Recall Gateway + LLM. Revisit if prompt-injection risk surfaces |
| 6 | Council tier (§13.1 Step 3): consensus definition — strict majority, supermajority (2/3), or unanimity? | Start with strict majority for MVP; revisit if we see false-confidence consensus on contested questions |
| 7 | Preference distillation cadence (§14.2) — 4h default, or event-driven on N new ledger entries? | 4h periodic + event-driven on ≥5 new entries in one category; prevents both staleness and thrash |
| 8 | Decision audit sample size (§14.5) — % of autonomous decisions sampled nightly? | Start with 100% for first month, tune down to 10% once confidence is established; sampling rate is operator-visible |
| 9 | Should the preference ledger be per-operator or per-agent-role? | Per-operator only for MVP — one ledger per Symbiotic instance. Multi-operator / multi-persona is a later concern |
| 10 | Can the Inquisitor itself be subject to the bottom-up resolution? (i.e., does the Inquisitor consult Recall before drafting recommendations?) | Yes — the Inquisitor's `recommendation` field is already a Recall-informed output. This is implicit, not a new mechanism |
| 11 | Max draft-persistence lifetime before auto-discard? | 30 days — longer than any reasonable `AutonomyTolerance` grace period; shorter than device-lifetime footprint |
| 12 | Should drafts sync across devices via Matrix? (same operator on phone + desktop) | Defer to post-MVP — introduces sync complexity; MVP drafts are device-local |
| 13 | How should the pending-questions drawer sort by default? | Severity × recency — `Critical` before `Decision` before `Informational`; within severity tier, newest first |
| 14 | Should Accept-all-recommendations show a confirmation for `Critical`-severity questions? | Yes — `Critical` always requires explicit per-question confirmation, never bulk-accepted |

---

## 11. Migration Path

This design is **additive**. The existing sequential Inquisitor flow keeps working; batched emission is an opt-in upgrade driven by the Inquisitor's system prompt. No existing event types change shape; only new ones are added.

Phased rollout:

1. §01 types land (`QuestionGroup`, `UnblockKey`, `GoalProcess` extensions)
2. §02 batched emission behind feature flag (`SYMBIOTIC_BATCH_INQUISITOR=1`)
3. §03 goal DAG + unblock events; Dispatcher routing stubbed
4. §04 Dispatcher live for `ResearchOnly` (cheapest, lowest risk)
5. §05 Dispatcher live for `Exploratory` (T116 swarm — existing infra)
6. §06 Dispatcher live for `AttachedRepo` (T126 ApprovalGate — most sensitive)
7. §07 bottom-up escalation ladder (§13 protocol); initially only Recall + Peer tiers — Council tier deferred until preference corpus is non-trivial
8. §08 preference ledger writes (§14.1) + Recall feedback (§14.3); distillation still manual
9. §09 preference distillation job + automatic preference-note regeneration (§14.2); Inquisitor context hydration (§14.4)
10. §10 decision audit loop (§14.5) + agent system-prompt hydration (§14.6)
11. §11 Council tier activated once ≥20 preference notes exist across ≥5 categories (gives Council something to compare against)
12. §12 end-to-end integration test covering all four `UnblockKey` variants + all four escalation tiers + preference ledger roundtrip

Each phase is independently shippable — the feature flag stays on through step 3; steps 4-6 go live as we gain confidence per backend; steps 7-10 layer in the learning flywheel; step 11 unlocks the most expensive resolution tier only after it has data to reason over.

---

## 12. What Stays Unchanged

| Component | Why |
|---|---|
| `AskUserTool` (sequential) | Kept as-is; batched tool is additive |
| Deliberation Pipeline complexity levels | Orthogonal — Inquisitor still picks depth; grouping is orthogonal to depth |
| T116 swarm primitives | Consumed unchanged |
| T126 `ApprovalGate` | Consumed unchanged |
| Thread room model | No new rooms; sub-goal events emit to parent's thread (or child thread via existing split) |
| Archive sync | Sub-goal Archive notes are normal Archive entries |
| Gatekeeper / capability model | No new capabilities; reuses existing ones |
| Archive FTS5 + vector index | Preference notes are normal Archive entries under `knowledge-base/methodology/preferences/` |
| Distillery pipeline | Preference distillation reuses existing Reduce → Reflect → Reweave stages; no new pipeline stages |
| T60 Multi-LLM Council | Consumed unchanged for the Council escalation tier (§13.1 Step 3) |

---

## 13. Bottom-Up Resolution Protocol

When a sub-agent encounters an unresolved question during execution, it does **not** immediately emit an `ask_user_group`. Instead, it walks a four-tier escalation ladder, consulting cheaper resolution channels first. Only when all four fail — or when severity mandates it (§13.3) — does the question reach the operator.

By then, it carries the full trail of what was tried. The operator sees the *most informed* version of the question, not the rawest.

### 13.1 The escalation ladder

```
  Sub-agent hits uncertainty
          │
          ▼
  ┌───────────────────────────────────────────────────────────┐
  │ Step 1 — Recall Gateway lookup                            │
  │   Query knowledge-base/methodology/preferences/*.md       │
  │     + Neural Graph for matching past decisions            │
  │   If match_confidence ≥ recall_threshold (default 0.85):  │
  │     → Use recalled answer                                 │
  │     → Annotate ResolutionTrail.recall_attempts            │
  │     → CONTINUE (no escalation)                            │
  │   Else: fall through                                      │
  └────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
  ┌───────────────────────────────────────────────────────────┐
  │ Step 2 — Peer consultation                                │
  │   Spawn read-only Peer Agent with question + context      │
  │   Peer returns: { answer, confidence, rationale }         │
  │   If peer_confidence ≥ peer_threshold (default 0.75):     │
  │     → Use peer's answer                                   │
  │     → Annotate ResolutionTrail.peer_opinions              │
  │     → CONTINUE                                            │
  │   Else: fall through                                      │
  └────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
  ┌───────────────────────────────────────────────────────────┐
  │ Step 3 — Council deliberation                             │
  │   (severity=Decision or Critical only; skip for Info)     │
  │   Invoke T60 Multi-LLM Council — independent deliberation │
  │   Council returns: { consensus, confidence, dissenting }  │
  │   If council_confidence ≥ council_threshold (0.80):       │
  │     → Use consensus                                       │
  │     → Annotate ResolutionTrail.council_result             │
  │     → CONTINUE                                            │
  │   Else: fall through                                      │
  └────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
  ┌───────────────────────────────────────────────────────────┐
  │ Step 4 — Escalate to operator                             │
  │   Emit QuestionGroup with full ResolutionTrail attached   │
  │   UI renders the trail: "peer thought X, council split on │
  │     Y vs Z, here is why neither tier resolved"            │
  └───────────────────────────────────────────────────────────┘
```

### 13.2 Why four tiers, not two

A naive version would be "recall if known, else ask." The four-tier design handles four distinct uncertainty types:

| Tier | Handles |
|---|---|
| Recall | Known answer from past operator decisions — fast, zero marginal cost |
| Peer | Unknown but inferable from current-goal context — one agent's judgment suffices |
| Council | Known to be contentious or high-stakes — multiple perspectives warranted, cost justified |
| Operator | Genuinely new or contested ground — human judgment required |

Skipping the Council tier would mean either escalating all contentious questions (too noisy) or trusting a single peer on high-stakes calls (too risky).

### 13.3 When escalation is mandatory

Regardless of tier confidence, the following **always** reach the operator:

- `severity: Critical` questions (hard rule, per design §3.3)
- Questions that would elevate agent capability scope (anything touching `ApprovalGate`)
- Questions about **operator preferences themselves** (can't infer preferences from past preferences — feedback loop needs grounding)
- Cold-start: first-time encounter in a previously-unseen `unblock_key` category (establishes a baseline)
- Preference conflicts detected during distillation (§14.7)

### 13.4 The `ResolutionTrail` surfaces to the UI

When a question reaches the operator, the UI renders not just the question but the trail:

```
❓ Framework preference: Vue, React, or Svelte?

Resolution attempts:
  ▸ Recall — matched past preference "frontend/framework-choice" (conf 0.71)
      ↳ below threshold 0.85 — preference corpus still thin for SSR-specific case
  ▸ Peer — peer-agent-22 recommends Vue (conf 0.68)
      ↳ below threshold 0.75 — cited only one article, no cross-check
  ▸ Council — 2 models say Vue, 1 says React (conf 0.72)
      ↳ below threshold 0.80 — dissent on "team familiarity with React"

Inquisitor recommendation: Vue (conf 0.72)
  [Accept]  [React]  [Svelte]  [Explain more]
```

This is the "control the level of conviction" dial the operator originally asked for — rendered as visibility into the agent's reasoning, not just a blind question.

### 13.5 Peer agent shape

The peer agent in Step 2 is deliberately minimal:

- Read-only capabilities (no tool use beyond Recall + LLM reasoning)
- One-shot — no back-and-forth dialogue with the spawning agent
- Cheap model tier (`fast` per the T128 tier naming — [source-archeology.md](source-archeology.md) model-tier convention)
- Response bounded: ≤500 tokens, must include `confidence` + `rationale`

The peer is explicitly **not** a full sub-agent — no sandbox, no git, no worktree. It's a one-shot consulting call.

### 13.6 Council invocation rules

Council tier (T60 Multi-LLM Council) is expensive. Invoked only when:

- Peer tier returned `confidence < peer_threshold` **and**
- Question severity is `Decision` or `Critical` **and**
- Council budget for the parent goal is not exhausted (default: 3 council invocations per goal)

Council budget prevents runaway cost on thrashy questions. When budget is exhausted, questions that would have used Council escalate directly to the operator (with the trail noting the budget exhaustion as the escalation reason).

---

## 14. Operator Preference Feedback Loop

Every operator answer — whether accepting a recommendation, overriding one, or providing free-text — enters a durable preference ledger. Periodic Distillery passes convert raw ledger entries into structured preference notes in `knowledge-base/methodology/preferences/`, which feed back into **four channels**:

1. **Recall Gateway** — so Step 1 of the escalation ladder (§13.1) can find past preferences
2. **Inquisitor prompt context** — so recommendations are pre-tuned to operator style
3. **Decision audit loop** — so autonomous agent decisions can be checked against preference history after the fact
4. **Agent system-prompt hydration** — so agent rules themselves evolve with operator feedback

### 14.1 The preference ledger

Append-only JSONL at `knowledge-base/self/preferences/ledger.jsonl`. One entry per resolved question (across all tiers — including Recall/Peer/Council resolutions, tagged with the tier that resolved them).

```json
{
  "id": "pref-2026-04-18-0001",
  "emitted_at": "2026-04-18T10:42:00Z",
  "thread_id": "thread-saas-product",
  "goal_id": "goal-build-frontend",
  "sub_goal_id": "subgoal-design-phase",
  "question": {
    "text": "Framework preference: Vue, React, or Svelte?",
    "severity": "Decision",
    "category": "frontend/framework-choice"
  },
  "recommendation": {
    "text": "Vue",
    "confidence": 0.72,
    "authored_by": "inquisitor"
  },
  "resolution_trail": { "final_tier_reached": "Operator", "...": "..." },
  "operator_answer": {
    "text": "Vue",
    "overrode_recommendation": false,
    "free_text_note": null,
    "autonomy_tolerance_active": "Balanced"
  },
  "tags": ["decision", "frontend", "framework", "saas"]
}
```

Ledger entries are **immutable**. Supersession happens at the distilled-preference level, preserving the raw evidence trail.

### 14.2 Distillation into methodology preference notes

A scheduled job (default: 4h cadence + event-driven trigger on ≥5 new entries in one category) runs a constrained Distillery pass over new ledger entries. Groups by `category`; writes/updates `knowledge-base/methodology/preferences/{category-slug}.md`:

```markdown
---
type: preference
category: frontend/framework-choice
tags: [decision, frontend, framework]
confidence: 0.95
evidence: [pref-2026-04-18-0001, pref-2026-03-10-0003, pref-2026-02-14-0012]
last_updated: 2026-04-18T14:00:00Z
superseded_by: null
---

# Preference: Frontend Framework Choice

Operator has consistently chosen Vue over React for new projects
(3 decisions across 2 months, no overrides).

## Context cues
- Cited rationale: "better SSR story", "simpler reactivity model"
- Always paired with Nuxt when SSR is required
- Operator accepts React when legacy team constraint mandates it

## When to apply
Default recommendation for category `frontend/framework-choice` is **Vue** unless:
- Team-already-uses-X constraint in context
- Framework-specific library requirement rules Vue out

## Supersedes
None.
```

Distillation rules:

| Signal | Effect |
|---|---|
| ≥3 consistent answers in category | Preference confidence 0.85 |
| ≥5 consistent answers | Preference confidence 0.95 |
| Any override ≥1 year old | Counted 50% weight |
| Any override ≥2 years old | Counted 25% weight |
| Conflicting recent answers | Emit `preference.conflict`; note enters `superseded_by: <new-id>` state (§14.7) |

### 14.3 Feedback channel 1: Recall Gateway

The `knowledge-base/methodology/preferences/` directory is indexed by the existing Archive FTS5 + vector index. Step 1 of the escalation ladder queries this index with the question text + category + tags. A hit with `confidence ≥ 0.85` short-circuits the ladder.

**No new infrastructure** — this is pure Archive-entry discovery via existing Recall Gateway primitives.

### 14.4 Feedback channel 2: Inquisitor prompt context

When the Inquisitor generates recommendations for a new `QuestionGroup`, its context pack (assembled by Recall Gateway per [thread-architecture.md §5 class budgets](thread-architecture.md)) includes:

- The top-N matching preference notes (class budget: 15% of the pack, reused from the existing `PREFERENCE` class budget in Thread Memory Document recall)
- Recent ledger entries in the same thread (recency bias)

This tunes the Inquisitor's `recommendation` field toward operator style. An Inquisitor that "knows" the operator prefers Vue emits `recommendation: "Vue"` with higher confidence than one operating cold — and the Recall tier in the next escalation can match on that recommendation directly.

### 14.5 Feedback channel 3: Decision audit loop with secondary-model gate

Separately from question-time feedback, every **autonomous agent decision** (made without operator input via Recall/Peer/Council resolution) is logged to `knowledge-base/self/audit/decisions.jsonl`. A **continuous** audit job (not nightly — we operate on much shorter time bases than traditional review paradigms):

1. Samples N recent autonomous decisions (start 100% sampling; tune down)
2. Cross-references against preferences: would this have been escalated under stricter thresholds?
3. Flags deviations where the agent picked X but recorded preferences would've suggested Y
4. **Secondary-model audit gate** (§14.5.1) — before the divergence feeds back into agent hydration, a different-model audit pass evaluates whether the divergence represents a real correction signal or noise
5. Routes the audited divergence: high-confidence corrections propagate same-day to Channel 4 hydration; low-confidence divergences emit `decision.audit.divergence` events into an **hourly digest** (not daily — rate matches the short time bases we operate on); Critical-severity divergences escalate immediately regardless of confidence

This is the **checking of the processes and decisions** the operator requested. It composes with T120 (LLM I/O Audit Trail) rather than replacing it — T130 writes the decision entries; T120 owns the provenance + model-integrity guarantees around them.

### 14.5.1 Secondary-model audit gate (the oscillation-preventer)

Raw same-day feedback from audit divergences into agent hydration creates a control-loop oscillation risk:

> Agent makes decision X → audit flags divergent → agent hydrates "X was wrong" → next decision swings to not-X → possibly overcorrects → new divergence → reversal → …

The traditional fix is an operator-in-the-loop gate (daily digest, operator confirms before feedback propagates). That's too slow for our timescale. The alternative: a **secondary-model audit gate** — before feedback propagates, a different LLM (same-or-higher capability tier than the one that made the original decision) reviews whether the divergence is real.

**The mechanism:**

```
Autonomous decision made by Agent-A using Model-M1
         │
         ▼
Audit cross-reference finds divergence from preference
         │
         ▼
┌──────────────────────────────────────────────────────────┐
│ Secondary-model audit gate                               │
│                                                          │
│ 1. Select Model-M2:                                      │
│    - M2 ≠ M1 (different provider / different base)       │
│    - M2 capability tier ≥ M1 tier                        │
│    - For Personal-tier decisions: M2 = Deep tier (base)  │
│    - For Deep-tier decisions: M2 = Council (multi-model) │
│                                                          │
│ 2. Prompt M2 with:                                       │
│    - The original decision + its context                 │
│    - The recorded preference it supposedly diverged from │
│    - Question: "Is this divergence genuinely a correction│
│      signal, or reasonable alternative / noise?"         │
│                                                          │
│ 3. M2 returns:                                           │
│    - Verdict: CORRECTION | REASONABLE_ALTERNATIVE | NOISE│
│    - Confidence: 0.0–1.0                                 │
│    - Rationale (for audit log)                           │
└──────────────────┬───────────────────────────────────────┘
                   │
     ┌─────────────┼─────────────┐
     ▼             ▼             ▼
 Confidence    Confidence    Confidence
 ≥ 0.80        0.50–0.80     < 0.50
 │             │             │
 Same-day      Hourly digest Flag for operator
 feedback to   to operator;  review; no auto
 hydration;    feedback      feedback; low-confidence
 logged as     ships with    signal logged only
 "corrected    low-confidence
 via M2 audit" flag
```

**Why this works:**

- **Different-model requirement:** prevents the original model from auditing itself (would systematically agree with its own past decisions)
- **Same-or-higher capability:** the auditor is never weaker than the original, so it can't miss subtle correctness
- **Three-way verdict:** not binary; "reasonable alternative" is the escape hatch for cases where the original decision was defensible even if it diverged from the logged preference (preferences drift)
- **Confidence gating:** high-confidence verdicts flow through fast; low-confidence flags for operator — maintains quick feedback while preserving the safety gate
- **Bounded cost:** one secondary call per divergence. Divergences should be rare by design (if they're common, something is wrong at the preference layer). Budget: daemon caps secondary-audit calls per hour to prevent runaway cost if divergence detection misbehaves.

**Model selection matrix:**

| Original decision tier | Secondary audit tier | Rationale |
|---|---|---|
| `personal` | `deep` (base, non-personal) | Catches overfitting of personal adapter |
| `balanced` | `deep` | Higher capability review |
| `deep` | Council (multi-model consensus) | Can't just use deeper; use diversity |
| `fast` | `balanced` | Don't over-invest; fast decisions are low-stakes anyway |

If Council is unavailable for `deep`-tier audits (e.g., Council not yet activated per §13.6), fall back to a two-different-deep-models audit. Never skip the gate.

**Logged in audit trail:**

```json
{
  "event": "decision.audit.divergence.gated",
  "original_decision_id": "...",
  "original_model": "qwen3-personal-adapter-v3",
  "secondary_model": "claude-opus-4-7",
  "secondary_verdict": "CORRECTION",
  "secondary_confidence": 0.87,
  "secondary_rationale": "Operator's preference for Vue was recently superseded...",
  "feedback_propagated": true,
  "feedback_latency_ms": 1200
}
```

Operator can review the audit trail retroactively; every divergence decision is forensically traceable.

### 14.6 Feedback channel 4: Agent system-prompt hydration

Per [CONTEXT.md §Declarative Cognitive Control Plane](../../CONTEXT.md:723), the agent's *Desire* (rules, behaviors) lives in Markdown files in the vault. On agent spawn, the runner hydrates the system prompt from:

- Role-specific prompt template (e.g. `knowledge-base/methodology/agent-rules/inquisitor.md`)
- Top-N preference notes (size-bounded, rotated per-spawn to avoid stale context)
- Recent `decision.audit.divergence` flags relevant to this role

This means the agent's **rules themselves** evolve with operator feedback. A pattern of overrides in a category triggers a suggestion to add a new rule to the role's methodology file — which the operator approves or rejects via a separate question group (recursive but bounded by the cold-start escalation rule in §13.3).

### 14.7 Preference conflicts + supersession

When a new ledger entry contradicts an established preference:

1. Distillation emits `preference.conflict` event with both preferences + new evidence
2. Operator reviews via a `QuestionGroup` (bootstrap case — the system can't decide whether a preference has *changed* without asking)
3. Operator picks: supersede / ignore one-off / split category
4. Preference note updates with `supersedes` chain preserved

This mirrors [thread-architecture.md §5 "Superseding, Not Deleting"](thread-architecture.md) — preference history is never lost, only layered.

### 14.8 Privacy & scope boundary — private by default

**The default privacy posture for every ledger entry, preference note, and training corpus entry is `Private`.** Operator must take an explicit action to upgrade anything to `Public`. There is no implicit public tier, no opt-out, no automatic classification that opens scope. Private-first is the invariant.

Three tiers — only two are reachable by default:

| Tier | How reached | External-LLM eligibility | Training corpus eligibility (T131) |
|---|---|---|---|
| **Private** (default) | Automatic on every write | Never, unless capability explicitly granted per-category | Never, unless explicit opt-in per-category |
| **Public** | Operator explicit action (`symbiotic privacy mark <id> public`) | Freely usable | Freely usable |
| **Sensitive** (stronger than Private) | Operator explicit flag OR auto-detection of credential/health/financial content | Never; not even with capability grant | Never; not included in any training corpus |

Every context pack assembly + every training corpus build applies:

1. **Default deny** — any entry not explicitly `Public` is treated as `Private` and gated by capability
2. **Capability check** — for `Private` content in external-LLM contexts, the consuming agent must hold a capability token scoped to the specific category
3. **Hard block for `Sensitive`** — no capability grant bypasses it; operator must upgrade to `Private` or `Public` first (explicit action), which in itself is an auditable event

**Why private-first, not tier-first:**

The prior design had a three-tier system (Public / Private / High-sensitivity) where tier was *assigned* at write time. The problem: auto-classification is brittle. A preference that *looked* public-ish could get auto-marked Public and leak. Private-first inverts the failure mode — the brittle case is now "content that should be Public stays Private until operator marks it," which is annoying but safe. You'd rather see friction than leakage.

**Operator controls:**

| Action | Effect |
|---|---|
| `symbiotic privacy mark <entry-id> public` | Upgrades one entry to `Public` |
| `symbiotic privacy mark-category <category> public` | Bulk upgrades all entries in a category |
| `symbiotic privacy mark <entry-id> sensitive` | Downgrades to `Sensitive` (strictest) |
| `symbiotic privacy audit` | Shows the current tier distribution + recent upgrades |
| `symbiotic privacy grant-capability <agent-role> <category>` | Grants a specific agent role access to a specific category of `Private` content |

All privacy-level changes emit Matrix events so operator always sees when the scope changes.

**Sensitive auto-detection:**

Even though default is Private (which is itself safe), we additionally auto-flag likely-sensitive content to prevent operator from accidentally upgrading it to Public without review:

- Credential-looking strings (API keys, tokens, passwords) — flagged via existing Content Firewall Stage D patterns
- Health-related terms (medical terms, prescriptions, symptoms)
- Financial identifiers (account numbers, transaction data, balance references)
- Government/identity identifiers (SSN-like patterns, passport, license)

Auto-flagged `Sensitive` entries cannot be bulk-upgraded; operator must explicitly review + reclassify each one. This is belt-and-suspenders — even if the operator runs `mark-category public`, Sensitive-flagged entries within that category stay Sensitive until explicitly handled.

**Relationship to Content Firewall:** The firewall ([content-firewall.md](content-firewall.md)) handles *inbound* content going into agent context. The privacy tier handles *outbound* scope — can this content be sent externally. They're orthogonal boundaries, both enforced.

### 14.9 The flywheel

```
Operator answers a question
    │
    ├─► Ledger entry written
    │        │
    │        ▼
    │   Distillery extracts preference note (4h / event)
    │        │
    │        ▼
    │   Preference note enters Archive (FTS5 + vector index)
    │        │
    │        ├─► Recall tier of next escalation finds it → skips ask
    │        ├─► Inquisitor context pack includes it → better recommendations
    │        ├─► Agent prompt hydration includes it → role rules evolve
    │        └─► Audit loop checks autonomous decisions against it
    │
    └─► Fewer questions over time, better-tuned when they do escalate
```

This is the self-improving-graph pattern from [thread-architecture.md §5 "Self-Improving Graph"](thread-architecture.md) applied to operator input specifically. The more the operator answers, the less they have to answer.

---

## Related Documents

- [thread-architecture.md](thread-architecture.md) — Thread lifecycle + Thread Memory Document
- [deliberation-first-pipeline.md](deliberation-first-pipeline.md) — Inquisitor + complexity levels
- [source-archeology.md](source-archeology.md) — Triage decision tree pattern (reused for question severity)
- `tasks/60-multi-llm-planning-council/` — Council tier backend for §13.1 Step 3
- `tasks/116-internal-git-swarm/` — Swarm backend for `Exploratory` sub-goals
- `tasks/120-llm-audit-trail/` — Audit-loop substrate for §14.5 decision divergence tracking
- `tasks/126-repo-manifest/` — `ApprovalGate` backend for `AttachedRepo` sub-goals
- `tasks/127-project-bootstrap-process/` — RepoManifest + agent_scopes vocabulary
- `tasks/109-living-memory-system/` — Methodology-space writes for preference notes (§14.2)
- `tasks/121-mycelium-memory/` — Adaptive knowledge graph that preference notes enrich
- `tasks/122-tool-memory/` — Parallel feedback loop (per-tool success/failure); preference ledger is the per-question analogue
