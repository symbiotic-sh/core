# Compiled Project Memory Benchmark

**Status**: Approved design target
**Related Tasks**: T125, T124, T109, T113
**Related Docs**: `docs/design/external-system-eval-2026-04-07.md`, `docs/design/agent-collaboration-conflict-resolution.md`

## Goal

Measure whether a compiled project-memory layer lowers token use and reduces wrong assumptions during real Symbiotic development work.

This benchmark exists to answer a specific product question:

- should Symbiotic development adopt a compiled project-memory layer before deeper implementation continues?

It does **not** exist to crown a fashionable tool.

## Core Question

Given the same Symbiotic codebase and the same development questions:

1. does a compiled project-memory layer improve recall quality?
2. does it reduce token/context burn?
3. does it reduce wrong assumptions and missed canonical sources?
4. does it stay aligned with repo truth well enough to be trusted?

This benchmark now has an additional practical sub-question:

5. how much of the gain comes from `RTK` alone versus compiled project memory on top of RTK?

## Benchmark Arms

The first benchmark matrix should be:

### Arm A: Baseline Without RTK

- fresh context per question
- current workflow only
- no RTK command proxy
- no compiled project-memory tool

### Arm B: Baseline With RTK

- fresh context per question
- same current workflow
- RTK enabled for shell command usage
- no compiled project-memory tool

### Arm C: Compiled Project Memory Without RTK

- fresh context per question
- compiled project-memory tool enabled
- no RTK

### Arm D: Compiled Project Memory With RTK

- fresh context per question
- compiled project-memory tool enabled
- RTK enabled

The benchmark must stay candidate-agnostic.

Historical note:

- one previously evaluated candidate exists in the stored results
- that evidence should be retained
- it should not force the next candidate order

Optional later comparison:

### Arm E: Secondary Derived Layer

- `graphify`
- graph/report output
- optional MCP surface

This exists only after the compiled-project-memory comparison is established.

## Corpus

Use Symbiotic itself.

Include:

- root docs
- `submodules/runtime`
- `submodules/app`
- selected task state
- relevant design and architecture docs

Keep the corpus fixed during each benchmark run.

## Run Protocol

Each question must be run with a **fresh agent/context window**.

That means:

- one question at a time
- no carry-over from previous benchmark questions
- load only the normal required Symbiotic context stack:
  - `CONTEXT.md`
  - `docs/NAMING-CANON.md`
  - `tasks/TASKS.md`
- `tasks/NEXT.md`
- plus whatever additional files the workflow/tool naturally discovers for the answer

This avoids hidden gains from accumulated session context.

### Arm-Specific Discovery Rules

The benchmark is only valid if each arm uses the discovery path it is meant to test.

#### Baseline Arms (`A`, `B`)

- use the normal current workflow
- manual repo search/read flow is allowed
- no compiled project-memory query surface

#### Candidate Arms (`C`, `D`)

- the compiled project-memory query surface must be used **first**
- the exact first-step query depends on the candidate under test
- follow the files/topics returned by the compiled-memory layer as the **primary** discovery path
- direct repo reads are allowed only for:
  - validating the answer against canonical files
  - resolving ambiguity between returned candidates
  - bounded recovery when the compiled-memory layer misses or drifts
- broad manual search before the first compiled-memory query makes the run invalid for candidate comparison

This matters because otherwise the candidate arm collapses back into the baseline workflow with extra overhead, which does not measure whether compiled memory actually changes the retrieval pattern.

## Question Set

The question set must use real development questions with cross-file synthesis pressure.

It should reflect the work we actually do often:

- review and regression hunting
- re-architecture and boundary placement
- simplification and canon-finding
- implementation and change placement
- code/doc alignment checks
- high-level overviews

The set should also mix difficulty:

1. **simple**
   - find the right place to start
   - identify canonical sources quickly
2. **medium**
   - trace one real flow across code and docs
   - connect app/runtime/design/task state
3. **complex**
   - answer architecture-placement questions
   - perform review-oriented synthesis
   - judge code/doc alignment and truth boundaries

The seed set lives in:

- `docs/benchmarks/compiled-project-memory-question-set.jsonl`

The current seed intentionally mixes:

- UI questions
- backend questions
- systemic questions
- codebase/doc alignment questions
- a few simple navigation questions
- a larger set of synthesis-heavy questions

## Metrics

For each run, record:

- correctness
- completeness
- source traceability
- time to answer
- tokens consumed
- number of files/pages consulted

For local Codex runs, `tokens consumed` must come from Codex's own persisted thread state, not a proxy estimate:

- `~/.codex/state_5.sqlite`
- table: `threads`
- field: `tokens_used`

For local Codex runs, `time to answer` should come from the same thread row:

- `threads.updated_at - threads.created_at`

This is second-level wall time, which is sufficient for comparison.

When available, retain the corresponding rollout file for audit/debug:

- `threads.rollout_path`
- local rollout JSONL under `~/.codex/sessions/...`

Also record failure modes:

- wrong canonical source chosen
- stale/generated source preferred over truth
- hallucinated connection
- incomplete cross-boundary trace
- compiled memory drift from repo truth

Also record run configuration:

- `rtk_enabled`
- `compiled_memory_enabled`
- `candidate_name`
- `fresh_context`
- `query_first_required`
- `query_first_satisfied`
- `manual_fallback_used`
- `manual_fallback_reason`
- `invalidated_by_manual_search_before_query`

## Scoring

Suggested rubric:

- `0` incorrect or unusable
- `1` partially correct, missing important cross-boundary truth
- `2` correct but incomplete or weakly sourced
- `3` correct, complete, and well sourced

Each answer should include:

- final answer
- cited files or artifacts used
- token count
- elapsed time
- evaluator notes
- whether the candidate query surface actually found the key canonical files before fallback

## Success Threshold

Consider adoption only if a candidate gives at least one of:

1. materially lower token use without quality regression
2. materially fewer wrong assumptions
3. materially better cross-file recall/traceability

Do **not** adopt if it:

- weakens truth boundaries
- prefers generated abstractions over canonical sources
- introduces high maintenance drift

Treat a candidate run as **invalid** if:

- it does not start with the candidate query surface
- it falls into broad manual grep/read exploration before using the candidate query surface
- it cannot distinguish between candidate-led discovery and manual recovery

## Execution Plan

### Phase 1: Harness Setup

- define question set
- define answer capture format
- define scoring sheet
- choose first candidate tool(s)

### Phase 2: Baseline Run

- run the full question set with current workflow and **RTK off**
- repeat the full question set with current workflow and **RTK on**
- capture time, tokens, and sourcing for both

### Phase 3: Candidate Run

- generate compiled project memory
- run the same question set with **RTK off**
- repeat with **RTK on**
- for each question, start with the compiled-memory query surface first
- use manual repo reads only for validation, ambiguity resolution, or bounded recovery
- explicitly mark any run invalid if manual broad-search happened before the first candidate query
- compare against both baseline arms

### Phase 4: Decision

- summarize benefits and failure modes
- recommend:
  - adopt now
  - adopt partially
  - benchmark another candidate
  - reject

## Output Artifacts

```text
docs/benchmarks/
  compiled-project-memory-question-set.jsonl
  compiled-project-memory-results.md
  compiled-project-memory-baseline-no-rtk.json
  compiled-project-memory-baseline-rtk.json
  compiled-project-memory-candidate-*.json
```

### Answer Capture Contract

Each benchmark run should emit one structured JSON file with:

- run metadata
- per-question scores
- file/artifact citations
- elapsed time
- token count
- failure mode tags

For Codex-local runs, also include:

- `thread_id`
- `rollout_path` when recoverable
- whether the entry was `measured_fresh_context` or only a recovered pilot run

The first templates should exist for:

- baseline without RTK
- baseline with RTK
- candidate without RTK
- candidate with RTK

## Candidate Strategy

Start with compiled project memory before graph-first tooling.

Recommended order:

1. one file-native compiled-memory candidate at a time
2. a simpler hand-rolled `llm-wiki`-style file-native layer if an external candidate proves too opinionated or hard to evaluate cleanly
3. `graphify` only after the compiled-memory comparison exists

### Candidate Selection Rules

A candidate is acceptable only if it is:

- local-first
- file-native
- already organized around compiled project memory rather than graph-first abstractions
- queryable through both ordinary files and a query surface
- fast enough to install and initialize without turning the benchmark into tool-integration work

### Exact First Comparison

The first real benchmark should be:

1. **Baseline, RTK off**
   - current Symbiotic workflow
   - `rg`, direct file reads, docs/tasks/context stack

2. **Baseline, RTK on**
   - same workflow
   - command usage routed through RTK

3. **Candidate A, RTK off**
   - initialize the chosen compiled-memory candidate locally in the Symbiotic repo
   - build or curate its local project-memory surface
   - answer the same seeded question set

4. **Candidate A, RTK on**
   - same compiled project-memory setup
   - RTK enabled

5. **Optional candidate-specific query-surface follow-up**
   - if plain context tree looks promising, add query-surface-specific follow-up

Only after that should we test:

- a hand-rolled `llm-wiki`-style layer
- `graphify`

## Recommendation

Treat this benchmark as a decision gate for development tooling.

If it wins, it should influence:

- repo excavation/indexing
- project-memory design
- Recall tooling for development

If it loses, the result is still valuable: Symbiotic should avoid inheriting complexity that does not pay for itself.
