# External Pattern Distillation - 2026-04-07

**Status**: Research note distilled into design pressure
**Related**: `docs/design/agent-collaboration-conflict-resolution.md`, `docs/design/operator-reasoning-distillation.md`, `docs/design/internal-git-swarm.md`

## Purpose

This note is not a tool roundup.

It exists to extract the **few external primitives worth copying** into Symbiotic and to reject the rest.

The systems reviewed are useful only insofar as they sharpen:

- management/orchestration
- project-memory design
- architecture/context tooling
- skill evolution from real runs

## Canonical Conclusion

The external research points to one strong stack:

1. **Management plane**
   - live assignment, checkout, heartbeat, stale-work handling, approvals
2. **Compiled project memory**
   - file-native, lazy, queryable, human-readable
3. **Canonical repo docs**
   - approved implementation truth
4. **Derived acceleration**
   - graph, MCP, indexes, caches

The important lesson is not `use graphs`.

It is:

- coordination state should not be confused with docs
- compiled project memory should not be confused with canonical truth
- graph/MCP should not be confused with either

## Distilled Sources

### 1. Paperclip

Sources:

- `https://github.com/paperclipai/paperclip`
- `https://docs.paperclip.ing/start/core-concepts`
- `https://docs.paperclip.ing/start/architecture`

#### Core primitives worth copying

1. **Atomic checkout**
   - work is claimed explicitly
   - double-claim returns conflict

2. **Heartbeat + lease**
   - work ownership expires without refresh
   - stale work can be reassigned safely

3. **Control plane separate from workers**
   - agents execute
   - control plane coordinates
   - adapters are not the control plane

4. **Operational state separate from output docs**
   - tickets
   - ownership
   - approvals
   - budgets
   - audit

#### What not to copy

- `zero-human company` framing
- org-chart-as-product metaphor
- treating agent chatter as a good primary artifact

#### Symbiotic fit

Paperclip is the strongest pressure on the **management layer**.

What it should change in Symbiotic:

- explicit `WorkItem`
- explicit `ScopeClaim`
- lease expiry and heartbeat rules
- stale-work reassignment
- approval/budget as first-class orchestration state

What it should **not** change:

- internal git swarm as the development-collaboration layer
- Archive as the memory layer

#### End-game role

Paperclip-like ideas should sit **above** the sandbox + internal git swarm flow.

They solve task/project conflicts, not code merge conflicts.

---

### 2. llm-wiki / file-native project memory

Sources:

- `https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f`

#### Core primitives worth copying

1. **Compiled project memory**
   - durable file-native abstraction layer
   - summaries, indexes, concept pages, links

2. **Lazy/on-demand updates**
   - compile only missing or changed knowledge
   - do not rescan everything every turn

3. **Tiered retrieval**
   - cheap retrieval first
   - expensive LLM reasoning only when needed

4. **Human-readable memory artifacts**
   - ordinary files
   - inspectable and diffable

#### What not to copy

- assuming a generic repo-local wiki is enough for Symbiotic
- collapsing control-plane state, canonical docs, and compiled memory into one folder
- letting generated wiki pages silently become truth

#### Symbiotic fit

This is the strongest pressure on **project-memory design**.

What it should change in Symbiotic:

- stronger project-memory layer for development and collaboration
- lazy compilation from repos and KB inputs
- repo-aware excavation/indexing as Recall tooling
- query tiers before expensive model calls

What it should **not** change:

- Archive truth boundaries
- repo docs as canonical implementation truth
- control-plane state as structured runtime data

#### End-game role

This is the best candidate for a future **compiled project-memory layer** sitting between raw repos and Recall.

For Symbiotic specifically, that likely means:

- shared project memory lives in `knowledge-base/operations/projects/...`
- repo docs stay in repos
- derived indexes and caches stay secondary

---

### 3. oh-my-mermaid

Sources:

- `https://github.com/oh-my-mermaid/oh-my-mermaid`
- `https://x.com/aaron_xong/status/2041521966965002678?s=20`

#### Core primitives worth copying

1. **Architecture perspectives**
   - multiple views over the same codebase

2. **Nested diagrams/docs**
   - high-level map -> subsystem map -> local detail

3. **Agent-readable architecture packets**
   - generated context artifacts useful for onboarding and planning

#### What not to copy

- generated diagrams treated as architectural truth
- raw scans promoted without curation

#### Symbiotic fit

This pressures the **context-packet / architecture-tooling layer**.

It is useful for:

- `context.packet.load`
- architecture scans
- onboarding artifacts

It is not the answer to:

- collaboration ownership
- project memory
- memory truth

#### End-game role

Derived architecture/context tooling, likely feeding agents and human operators, but never replacing curated docs.

---

### 4. Hermes / GEPA

Sources:

- `https://nousresearch.com/hermes-agent/`
- `https://hermes-agent.nousresearch.com/docs/reference/skills-catalog/`
- `https://x.com/RajaPatnaik/status/2041305017781833859?s=20`

#### Core primitives worth copying

1. **Trajectory-based improvement**
   - improve from real runs

2. **Skill/tool-description evolution**
   - not just prompt tweaking

3. **Procedural memory as explicit artifacts**
   - skills and execution notes are real assets

#### What not to copy

- opaque autonomous self-modification
- mutation without audit or review

#### Symbiotic fit

This pressures the **learning-from-runs** layer.

It supports:

- operator protocol distillation
- skill promotion
- tool-description refinement

#### End-game role

Bounded improvement loop over:

- skills
- protocol rules
- tool descriptions

fed by real transcripts and verified outcomes.

---

### 5. graphify

Sources:

- `https://github.com/safishamsi/graphify`
- `https://x.com/RoundtableSpace/status/2041421970546528318?s=20`

#### Core primitives worth copying

1. **Uncertainty labeling**
   - `EXTRACTED`
   - `INFERRED`
   - `AMBIGUOUS`

2. **Persistent derived graph/report outputs**

3. **Alternative navigation surfaces**
   - graph
   - wiki
   - report

#### What not to copy

- graph as truth
- graph-first architecture
- token-efficiency claims accepted without local measurement

#### Symbiotic fit

Useful as a **secondary derived acceleration layer** and benchmark candidate.

It should not drive the core architecture.

#### End-game role

Optional derived graph/report tooling behind the Archive and project-memory layers.

## Distilled Symbiotic Model

From these systems, the strongest Symbiotic model is:

### 1. Management

Copy from Paperclip:

- checkout
- lease
- heartbeat
- stale-work recovery
- approvals/budgets/audit

### 2. Development

Keep Symbiotic's own path:

- sandboxed workers
- internal git server
- PR/review/CI/integration

This is not Paperclip's main contribution. This is already Symbiotic's stronger path.

### 3. Project Memory

Copy from llm-wiki / file-native project memory:

- compiled file-native memory
- lazy updates
- query tiers
- inspectable artifacts

But place it inside Symbiotic's KB/repo split, not as a generic repo-local wiki.

### 4. Architecture Context

Copy from oh-my-mermaid:

- derived architecture perspectives
- nested context artifacts

### 5. Learning From Runs

Copy from Hermes / GEPA:

- trajectory-based refinement
- skill/tool evolution with review

### 6. Derived Acceleration

Copy selectively from graphify:

- uncertainty labels
- graph/report outputs

## What Symbiotic Should Build Next

The real question is not:

- `which tool do we adopt?`

It is:

- `which primitives do we internalize, and where do they belong in Symbiotic's stack?`

Recommended order:

1. **Management primitives**
   - implement checkout/lease/heartbeat/stale-work design
2. **Compiled project-memory benchmark**
   - test whether file-native compiled memory lowers token burn and wrong assumptions
3. **Architecture/context tooling**
   - use derived perspectives for context packets
4. **Learning-from-runs promotion**
   - distill protocol/skills from real trajectories

## Benchmark We Actually Need

The benchmark should test whether a compiled project-memory layer improves Symbiotic development.

### Compare

1. baseline repo workflow
   - `rg`
   - direct file reads
   - current docs/tasks/context

2. compiled project-memory workflow
   - wiki/context-tree style layer

3. compiled project-memory + query surface
   - MCP/CLI over that layer

4. optional graph/report layer
   - graphify-style secondary acceleration

### Measure

- correctness
- completeness
- traceability
- time to answer
- tokens consumed
- wrong assumptions
- drift from canonical repo truth

### Success Threshold

Adopt only if it gives:

- materially lower token use, or
- materially fewer wrong assumptions, or
- materially better cross-file recall

without weakening truth boundaries.

## Recommendation

Treat this note as pattern distillation, not tool endorsement.

Current guidance:

1. internalize Paperclip's management primitives
2. benchmark compiled project memory before adopting any specific tool
3. use graph/report tooling only as a secondary layer
4. keep Archive, repos, and control-plane state as separate truth domains
