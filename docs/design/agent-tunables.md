# Agent Tunables — Configurable Thresholds with Sensible Defaults

**Status**: Proposed pattern (adopted 2026-04-18; applied incrementally as classifier stages land)
**Related Tasks**: T128 Source Archeology (first concrete adopter), T126 Repo Manifest (future `archeology_policy` field), T66 Goal-Driven Multi-Domain System, T105 Deliberation-First Pipeline
**Related Docs**: `docs/design/source-archeology.md`, `docs/architecture/agent-orchestration.md`

## Problem

Symbiotic's classifier agents make escalation decisions: "is the LLM's verdict confident enough to act on, or should the operator be pulled in?" Those decisions are driven by numeric thresholds — confidence floors, severity cutoffs, retry budgets, autonomy gates. Two failure modes are equally bad:

1. **Hard-code the threshold.** The operator can't tune the system's conservatism per project, per agent, or per task type without editing code. Reviewing whether a given threshold is even right becomes an archaeology exercise.
2. **Expose the threshold as a required config parameter.** Every caller has to know and set it, the "correct" value is invented independently each time, and the zero-config path stops being correct.

Both fail because they put the operator in the loop *before* the agent has had a chance to be sensible. We want the zero-config path to be correct by default, and the configurable path to be available when the operator wants more or less conservatism.

## Rule

**Every LLM-decision threshold, confidence floor, retry budget, or tunable that affects agent escalation / verdict selection / when-to-ask-the-operator MUST be**:

1. **Parameterized** — lives on a typed config struct (e.g. `DiagnoseConfig { confidence_floor: f32 }`), never a magic constant at the call site.
2. **Default-ready** — `impl Default` ships a sensible, conservative value. The zero-config path IS the common path; operators should never need to touch config to get correct-enough behavior.
3. **Resolvable per task type and per agent** — the config must be sourceable from the invocation context (task target, agent identity, repo manifest policy, project autonomy level, …) so operators *can* override without editing code when they want to. Hierarchy: hard-coded default → per-agent → per-task-type → per-invocation, with later overriding earlier.

### Applies to

- Confidence thresholds for verdict escalation (Diagnose, Triage, future classifiers).
- Severity floors for auto-apply vs. defer.
- Retry counts for LLM / network calls.
- Autonomy gates (`auto` / `semi` / `manual`).
- Triage decision-tree weights.
- Anywhere a number decides "escalate to human vs. proceed."

### Does NOT apply to

- Performance caps (`MAX_FILES_WALKED`, buffer sizes, token caps on LLM projections). These are a different-axis concern — tuning is legitimately rare and `const` is fine until the cap bites someone.

## Never

- Hard-code a threshold that decides whether to escalate, invoke an LLM, or trigger a costly action. Wrap it in a config struct even if the struct only has one field today.
- Expose a threshold as a required parameter with no default. Required + unnamed defaults push burden onto every caller.
- Hide tunables behind private fields. Readable + overridable by the caller is the whole point.

## Config Hierarchy (resolution order)

When an agent is invoked, a tunable's effective value is resolved by walking the hierarchy from most- to least-specific:

```
per-invocation override (explicit arg)
  ↓ falls through to
per-task-type policy (e.g. archeology_policy on RepoManifest)
  ↓ falls through to
per-agent default (e.g. DiagnoseConfig::default())
  ↓ falls through to
hard-coded baseline (const inside impl Default)
```

The first non-None layer wins. Callers that don't care get the baseline; operators that do get to override at whichever layer fits their workflow (project-level policy file, agent-type default, explicit invocation flag).

## First Adopter: `DiagnoseConfig`

`submodules/runtime/crates/symbiotic-agents/src/source_archeology/diagnose.rs`:

```rust
#[derive(Debug, Clone, Copy)]
pub struct DiagnoseConfig {
    /// Below this, the classifier's verdict is overridden to
    /// `NeedsOperatorInput`. Default: 0.80 — conservative-by-default:
    /// prefer escalating to the operator over picking the wrong branch.
    /// Sourceable from `RepoManifest.archeology_policy` when that field
    /// lands (future T126 chunk).
    pub confidence_floor: f32,
}

impl Default for DiagnoseConfig {
    fn default() -> Self {
        Self { confidence_floor: 0.80 }
    }
}
```

- Default 0.80 matches the design doc §Stage 3 "Diagnose is conservative" intent.
- Struct is public and the field is public — the caller can override.
- Only one field today, but the struct is there so adding a sibling (e.g. `retry_budget`, `escalate_on_low_sample`) is a non-breaking extension.

## Future Adopters (tracked)

- **Triage decision-tree weights (T128 §08)** — severity weighting, goal-alignment weight, peer-review agreement threshold. All on a `TriageConfig` struct.
- **Reconcile severity cutoff** — currently the LLM's raw severity passes through; future cutoff for "below this severity, never auto-apply even post-Triage" lives on `ReconcileConfig`.
- **Autonomy gates globally** — the `auto` / `semi` / `manual` autonomy level on `ArcheologyTarget` + goal metadata is already structured per-invocation; post-MVP this becomes resolvable from `RepoManifest.autonomy` too.
- **Retry budgets for LLM calls** — currently hard-coded "one retry" in every classifier. Unify under a single `LlmRetryConfig` struct once a second tunable (backoff, per-provider cap) shows up.

## When `archeology_policy` Lands on RepoManifest

T126 will add (approximate shape, subject to that chunk's design):

```rust
pub struct ArcheologyPolicy {
    pub diagnose: Option<DiagnoseConfig>,
    pub reconcile: Option<ReconcileConfig>,
    pub triage: Option<TriageConfig>,
    // …
}
```

Stored per-repo. The archeology pipeline resolves each stage's effective config via the hierarchy above — the call site passes `manifest.archeology_policy.diagnose.unwrap_or_default()` (or similar) into the runner. No code changes at the stage level; the struct stays the same, only the source shifts.

## Non-Goals

- Not a general-purpose runtime config system (we're not building TOML schemas, CLI flags, or env var overrides in this pattern). Those are separate concerns — this pattern is specifically about how *agent decision logic* exposes its tunables.
- Not a replacement for per-call parameters where the parameter genuinely varies (e.g. `ArcheologyTarget.repo_id` is per-call data, not a tunable).
- Not about performance caps (`MAX_FILES_WALKED` etc.) — those stay `const` until someone's production use case needs to override them, at which point a dedicated perf-cap config struct lands.
