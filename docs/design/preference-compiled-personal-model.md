# Preference-Compiled Personal Model

> **Task:** [T131](../../tasks/131-preference-compiled-personal-model/README.md)
> **Depends on:** [T130 preference ledger](grouped-inquisition.md), [opentraces schema](https://github.com/JayFarei/opentraces), [T82 Redaction Policy Engine](../../tasks/82-redaction-policy-engine/), [T83 LLM Runtime Manager](../../tasks/83-llm-runtime-manager/), [T101 AI Provider Management](../../tasks/101-ai-provider-management/), [T120 LLM I/O Audit Trail](../../tasks/120-llm-audit-trail/), [T122 Tool Memory](../../tasks/122-tool-memory/), [T128 model-tier naming](source-archeology.md)
> **Conviction:** ~55% — low. Training infrastructure choices, retraining cadence, and evaluation methodology are all areas where real usage will rewrite the assumptions. Ship minimal viable slices and measure.

---

## 1. Motivation

T130 puts operator preferences into **Markdown files that agents read at context-pack assembly time**. That's context-pack memory — editable, auditable, human-readable. But it has three scaling limits:

1. **Token cost per question.** Every Inquisitor recommendation pays the preference-note injection tax. At 15% of a 2k context pack across thousands of questions, that's meaningful spend.
2. **Retrieval miss on style.** Context-pack retrieval is good at "decision X maps to answer Y" but poor at "operator prefers this *voice*, *aesthetic*, *level of formality* across all outputs." Style is distributed; RAG doesn't capture it.
3. **Cold-agent performance.** A newly-spawned peer agent in §13.1 Step 2 has no preference context until hydrated. Its first-pass quality is the base model's — not the operator's.

Weight memory — fine-tuning the base model on accumulated preference data — solves all three. The common case becomes fast and quiet (model already knows); the rare case still uses context memory for auditability. You want both.

**T131 builds the pipeline that converts T130's preference ledger (plus T120/T122 logs) into a LoRA adapter trained on operator data, hosted on operator-controlled infra, and routed to by the existing model-tier system as a new `personal` tier.**

---

## 2. Privacy-first architecture

Before any pipeline decision, three non-negotiables frame the design:

1. **Training data never leaves operator-chosen infra.** Ledger entries contain deeply personal preferences. The operator picks the compute tier; no default routes data to Symbiotic-hosted infrastructure.
2. **Weights are operator-private.** Trained LoRA adapters are owned by the operator. Never uploaded to shared Symbiotic infrastructure. Never included in base-model redistributions.
3. **Every step is opt-in.** T130 can run forever without T131 being activated. Activation is explicit — operator must configure compute, storage, and model tier before training runs.

This means we can't ship a default "just turn it on" path — training requires setup. Fine. That's the right tradeoff for this sensitivity tier.

### 2.1 Three compute tiers

Operator picks one at T131 setup:

| Tier | Compute | Storage | Trust profile |
|---|---|---|---|
| **Local** | Apple Silicon (MLX) / llama.cpp LoRA on workstation | Local disk | Zero external trust; slowest; caps at ~7B model |
| **User-chosen cloud** | Lambda / RunPod / Together.ai / Nvidia DGX Cloud / personal H100 | Private HuggingFace dataset (operator-owned) | Vendor trust only; operator picks; reasonable speed; up to 70B+ |
| **Self-hosted GPU** | Operator's own GPU server, accessed via private network | Operator's storage | Full operator control; highest effort to operate |

**Deliberately excluded:** a Symbiotic-hosted shared training service. The data sensitivity makes this incompatible with the trust model.

### 2.2 Redaction gates

Before any data leaves the operator's machine for training (even to their own HF private dataset), it passes through a two-pass redaction gate:

1. **Symbiotic redaction** via [T82 Redaction Policy Engine](../../tasks/82-redaction-policy-engine/) — our policy rules, sensitivity-tier-aware
2. **opentraces redaction** — Tier 1a (regex) + Tier 1b (Shannon entropy) + optional Tier 1.5 (TruffleHog) + optional Tier 2 (LLM review)

Both passes must succeed before publish. A failure in either discards the entry and logs to `knowledge-base/self/audit/redaction_failures.jsonl` for operator review.

---

## 3. Data sources → opentraces TraceRecord

The five JSONL sources below feed into one unified training corpus:

| Source | Task | What it contributes |
|---|---|---|
| Preference ledger | T130 | `(question, recommendation, operator_answer)` triples — primary SFT target |
| LLM I/O audit trail | T120 | Prompt/response/thinking traces — adds provenance + reasoning training signal |
| Tool memory | T122 | `(tool_call, outcome)` pairs — tool-use behavior cloning |
| Decision audit divergences | T130 §14.5 | Pairs where autonomous decision ≠ recorded preference — high-signal DPO pairs |
| Thread conversations | T108 | Raw dialogue (post-redaction) — style/tone SFT |

### 3.1 The adapter

A Rust component (`symbiotic-trace-export` crate) reads the internal JSONL files, joins them on `goal_id` / `sub_goal_id` / `thread_id` / `correlation_id`, and emits opentraces `TraceRecord` JSONL. One trace record per goal completion (or per question resolution in the case of standalone ledger entries).

Schema alignment (opentraces side — schema v0.3.0):

| opentraces field | Source |
|---|---|
| `task` | `goal.title` + `goal.description` |
| `agent` | `agent_role` from T120 audit entry |
| `steps[].think` | `reasoning_trace` from LedgerEntry (or T120 thinking trace) |
| `steps[].act` | Tool call from T122 entry |
| `steps[].observe` | Tool observation from T122 entry |
| `tokens` | T120 audit trail |
| `cost` | T120 audit trail |
| `outcome` | `LedgerEntry.downstream_outcome` (T130 §2.5 training-readiness field) |
| `security` | Tier passes from §2.2 above |
| `attribution.git_link` | Thread's bookmarks-sync commit ref (if any) |

We don't invent any new fields — we map into the existing opentraces shape. This keeps our data interoperable with the broader agent-trace ecosystem.

### 3.2 The publishing flow

```
Symbiotic JSONL files
    │
    ▼
symbiotic-trace-export (our Rust crate) — joins + maps to opentraces TraceRecord
    │
    ▼
opentraces CLI (opentraces add --all / opentraces push)
  ├─► Security: regex + entropy (always on)
  ├─► Security: TruffleHog (optional, configured per operator)
  ├─► Security: LLM review (optional)
  └─► Staging → operator review → publish
    │
    ▼
Private HuggingFace dataset (operator-owned repo)
```

Per the opentraces README, the tool is `pipx install opentraces` + `opentraces init` in our repo. We don't need to reimplement — we feed it.

---

## 4. Training pipeline

### 4.1 Two-stage training

**Stage A: SFT (supervised fine-tuning)** on preference ledger + redacted thread conversations.

- Objective: learn operator style, common decision patterns, tool-use habits
- Data: one training example per `LedgerEntry` where `operator_answer` exists, formatted as `(system_prompt + context_pack, question → operator_answer)`
- Trainer: HuggingFace [TRL](https://github.com/huggingface/trl) `SFTTrainer` with PEFT LoRA adapter
- Base model: **operator-selected at T131 setup from a criteria-based compatibility matrix (see §4.5)**; pinned for the lifetime of one adapter generation, can be changed at next retrain
- LoRA config: rank 16–32, alpha 32, dropout 0.05, target modules `q_proj`, `k_proj`, `v_proj`, `o_proj`, `gate_proj`, `up_proj`, `down_proj`
- Epochs: 2–3; early-stop on eval-loss plateau

**Stage B: DPO (Direct Preference Optimization)** on override pairs + decision-audit divergences.

- Objective: teach the model when its base recommendations are wrong for this operator
- Data: pairs where `LedgerEntry.operator_answer.overrode_recommendation = true` → `(chosen = operator_answer, rejected = recommendation)`; plus divergences from T130 §14.5
- Trainer: HuggingFace TRL `DPOTrainer`, reference model = Stage A output (or base if not enough SFT data)
- β (KL penalty): 0.1 — conservative, avoids over-fitting to preferences at cost of generality
- Data volume floor: ≥1000 preference pairs before Stage B activates; fewer → skip, keep Stage A only

### 4.2 Volume floors & cold start

| Stage | Minimum ledger entries | If below |
|---|---|---|
| SFT | 5,000 total entries (all sources combined) | Skip — use T130 context-pack preferences only |
| DPO | 1,000 override pairs | Skip Stage B — SFT-only model is acceptable |

Cold-start months: T130 runs alone, ledger accumulates, no training runs. First training run probably 3–6 months post-T130 activation. This is a feature, not a bug — lets the preference corpus mature before compiling it into weights.

### 4.3 Compute estimates

For a 7B LoRA SFT on 5–20k examples:

- Local (MLX on M2 Max): ~4–8 hours; free
- Cloud A100 single (Lambda / RunPod): ~30–60 min; ~$1–3
- Cloud H100 single (Lambda / Together / DGX Cloud): ~15–30 min; ~$2–5

All three are economically reasonable. **Local tier is the recommended default for MVP** — cheapest, highest trust, no vendor trust needed.

### 4.5 Base model selection — criteria, not versions

We deliberately do **not** pin a specific base model in this design. Model releases move faster than design cycles; whatever's pinned here will be two generations behind by the time this ships. Instead, the operator picks at setup from a living compatibility matrix maintained separately at `knowledge-base/methodology/base-model-compatibility.md`.

**Selection criteria** (all required):

| Criterion | Why it matters |
|---|---|
| Permissive license (Apache 2.0, MIT, or comparable) | No downstream restrictions; operator can host anywhere, train freely |
| 7B–14B parameter range | LoRA fine-tunes efficiently; runs on local M-series Apple Silicon or single A100/H100 |
| HuggingFace TRL + PEFT supported out-of-the-box | No custom trainer plumbing; stable tooling integration |
| Quantization-friendly (bitsandbytes 4-bit + llama.cpp GGUF + MLX) | All three serving backends work; operator not locked to cloud inference |
| Native function-calling / tool-use pretraining | Makes the tool-memory training signal meaningful, not just behavior cloning |
| Active community with documented fine-tunes | Known pitfalls are already debugged |
| Release cadence — prefer models released within last ~6 months at setup time | Benefit of recency without consuming bleeding-edge risk |

**Default recommendation at setup time:** operator picks the newest instruction-tuned model from the Qwen 3.x family (or later), Llama 3.x+, or Mistral's current release — whichever meets the criteria matrix at that moment. The compatibility matrix gets refreshed when new releases land and their tooling stabilizes.

**Upgrade path:** changing base models requires a full retrain (no adapter portability across base-model families). This is a ~30min–2hr operation on the MVP fixture corpus, so upgrading ~twice a year as the model landscape evolves is reasonable.

### 4.6 Training driver

A small Python harness (`training/` directory at repo root; **not** inside a submodule):

```
training/
├── pyproject.toml           # depends on opentraces, trl, peft, transformers, datasets
├── train_sft.py             # runs Stage A
├── train_dpo.py             # runs Stage B
├── pull_dataset.py          # pulls operator's HF private dataset, splits train/eval
├── eval_harness.py          # see §6
├── config/
│   ├── local.yaml
│   ├── cloud-lambda.yaml
│   └── cloud-dgx.yaml
└── README.md
```

Not Rust — training pipelines in Python. This is a seam we don't fight; HuggingFace + PEFT + TRL is the ecosystem we'd have to replicate in Rust and it's not worth it. The Rust side owns **data production** (export adapter); Python owns **training** and the model-serving bridge.

---

## 5. Serving & routing

### 5.1 The `personal` tier

Existing model-tier system from [T128 source-archeology.md](source-archeology.md) defines `fast`, `balanced`, `deep`. T131 adds a fourth tier: **`personal`**.

| Tier | Purpose | Tuned? |
|---|---|---|
| `fast` | Classification, short-task routing, peer-agent consultations | No |
| `balanced` | General conversation, mid-complexity goals | No |
| `deep` | Complex reasoning, council deliberation | No |
| **`personal`** | **Inquisitor recommendations, peer consultations in familiar categories, routine task responses** | **Yes — LoRA adapter on operator data** |

### 5.2 Routing rules

| Call site | Tier |
|---|---|
| Inquisitor drafting `recommendation` field for a `QuestionGroup` | `personal` (falls back to `balanced` if `personal` unavailable) |
| Peer agent in §13.1 Step 2, for category already in preference corpus | `personal` |
| Peer agent for novel category | `balanced` (no personalization signal available) |
| Council tier in §13.1 Step 3 | **Explicitly not** `personal` — council must be base-model-only to avoid echo-chamber consensus |
| `deep`-tier reasoning (complex planning) | `deep` base model — don't overfit reasoning to preferences |

The routing decision lives in T101 (AI Provider Management Layer) — we extend its model-selection config, not reinvent it.

### 5.3 Hosting options

| Option | Infra | Latency |
|---|---|---|
| Local base + adapter via llama.cpp or MLX | Workstation / daemon host | ~100ms first token on 7B |
| HF TGI server on operator cloud | Cloud VM / GPU | ~50ms first token |
| HF Inference Endpoint (private) | Managed HF | ~80ms first token |
| vLLM server on self-hosted GPU | Operator GPU | ~30ms first token |

All four options are supported. Operator picks at setup. Adapter is loaded on top of base model at serve time (PEFT lazy-load); base model can be the same one T83 LLM Runtime Manager already hosts.

### 5.4 Fallback behavior

If `personal` tier is unreachable (network failure, model loading, adapter missing), callers silently fall back to `balanced`. The Inquisitor records the fallback in its reasoning trace so the operator can see when personalization isn't active.

---

## 6. Evaluation harness

Training without eval is faith. Four measurements:

### 6.1 Holdout-ledger replay

On every training run, hold out 10% of ledger entries. Run the tuned model on their questions; compare its answers to the operator's actual historical answers.

- Metric: exact-match accuracy + semantic similarity (embedding cosine)
- Pass threshold: ≥70% exact-match OR ≥85% semantic similarity on the held-out set
- Below threshold: abort deployment of the new adapter; keep previous version

### 6.2 A/B shadow runs

For 48 hours after deployment, the Inquisitor's `personal`-tier recommendations are generated in parallel with `balanced`-tier recommendations (operator sees `personal`; `balanced` is logged silently). Operator override rate is compared.

- Metric: override rate delta between tiers
- Pass criteria: `personal` override rate ≥5 percentage points lower than `balanced`
- Below criteria: rollback to previous adapter; log divergence pattern

### 6.3 Preference-conflict detection

Check whether new adapter's recommendations contradict distilled preference notes in `knowledge-base/methodology/preferences/`. High contradiction rate means the adapter learned something that conflicts with the operator's stated-preference layer — probably a style/surface fluke, not a real preference.

### 6.4 Benchmark regression check

On a fixed set of generic reasoning benchmarks (e.g. a small MMLU-style subset), check that the tuned model doesn't regress significantly from base. Guards against LoRA over-fitting wrecking general capability.

- Metric: ≤3% regression on base-model benchmark score
- Above regression threshold: fail adapter; investigate over-fitting

---

## 7. Versioning + rollback

Every training run produces a new LoRA adapter tagged with:

- Ledger cursor (last entry included in training)
- Base model version
- Training config hash
- Eval scores (§6.1, §6.2, §6.4)

Adapter registry lives at `knowledge-base/self/models/personal-adapters/` as Markdown notes with frontmatter (versioning is just Archive entries). Operator can roll back by pointing the router at any prior adapter.

This reuses the pattern from T102 (Agent Prompt & Role Versioning) — we don't invent a new versioning concept, we apply the same one to adapters.

---

## 8. Retraining cadence

Two triggers:

1. **Periodic.** Default: monthly. Operator-configurable (weekly, monthly, quarterly, manual).
2. **Event-driven.** ≥500 new ledger entries **or** ≥50 new override pairs since last training run.

Monthly with event-driven override is reasonable for most usage. Daily is probably overkill and burns money; quarterly loses responsiveness.

### 8.1 The retraining driver

A scheduled daemon job (T130-style scheduling, per [source-archeology.md](source-archeology.md) §archeology-checkpoint patterns) checks triggers and, when met:

1. Invokes `symbiotic-trace-export` to snapshot current ledger state
2. Calls `opentraces add --all` + `opentraces push` to update HF dataset
3. Triggers training run on configured compute tier
4. On success: evaluates per §6
5. On eval pass: atomically swaps the active adapter in T83 LLM Runtime Manager
6. On eval fail: logs divergence; keeps previous adapter; operator notified

### 8.2 Preference drift handling

Ledger entries decay in training weight over time (config: half-life 6 months default). Prevents lock-in to early preferences that the operator has moved on from.

Explicit-supersede takes priority: if a preference note has `superseded_by`, ledger entries supporting the superseded preference are weighted ≤10% in training.

---

## 9. Opt-in consent model

### 9.1 Category-level opt-in

At T131 setup, operator reviews preference categories present in the ledger and opts in per-category. High-sensitivity categories (finances, health, credentials) are **opt-out by default** — must be explicitly enabled.

### 9.2 Per-entry tagging

Ledger entries already carry `sensitivity_tier` (from [data-sensitivity-tiers.md](data-sensitivity-tiers.md)). Training pipeline respects this:

- Public → always included
- Private → included only if operator opted in
- High-sensitivity → never included in training corpus unless explicit per-category consent

### 9.3 Audit surface

Operator can always see:

- What's in the training corpus: `symbiotic training corpus-summary`
- What was redacted: `knowledge-base/self/audit/redaction_failures.jsonl`
- What's in the last training run: adapter's frontmatter lists evidence IDs

No training is done silently. Every compile is an event.

---

## 9.5 Publish-first retention policy

The local Symbiotic instance runs on finite storage. The HuggingFace private dataset (or operator-chosen cloud equivalent) is effectively unbounded and operator-managed. **The remote is the durable source of truth; the local instance is an ephemeral working set.** Full-fidelity long-term retention happens remotely; local keeps only what it needs for daily operation.

This is the inverse of the typical "local logs → periodic archive" pattern. It's also a better privacy story: less sensitive data sitting on the local machine at rest means a smaller forensic surface if the device is lost, seized, or compromised.

**Critical precondition:** this policy activates **only** after T131 has shipped a first adapter that passed §6 eval AND the publish pipeline (§04) has pushed at least one shard successfully. Before that, logs stay full-fidelity local — the ledger is the only place preferences live.

### 9.5.1 The retention model

```
Agent activity
     │
     ▼
┌─────────────────────┐
│ Pre-publish queue   │  Local, full fidelity
│ (unpublished)       │  Retention: until next scheduled push
└──────────┬──────────┘
           │  opentraces push (operator-reviewed)
           ▼
┌─────────────────────┐          ┌──────────────────────┐
│ Recent cache        │          │ Remote dataset       │
│ (post-publish)      │  ◄─────  │ (HuggingFace private │
│ Local, full fid.    │  pull    │  or operator cloud)  │
│ Retention: 7–30d    │  on      │ Full fidelity        │
└──────────┬──────────┘  demand  │ Retention: as long   │
           │                     │  as operator keeps   │
           ▼  trim after window  │  the dataset         │
       (discarded from local;    └──────────────────────┘
        remote still has it)
```

### 9.5.2 What stays local, forever (all tiny)

- **Distilled preference notes** (`knowledge-base/methodology/preferences/*.md`) — these are what the Recall tier and Inquisitor actually read, not raw ledger. Full supersession history.
- **Divergence flags** — small records that a `decision.audit.divergence` event happened, with the remote shard/entry ID for deep-dive fetching.
- **Publish cursor state** — what's pushed, what's pending, shard hashes.
- **Retention-actions audit log** — `knowledge-base/self/audit/retention_actions.jsonl` recording every trim action.

### 9.5.3 What stays local, short-term

| Content | Default retention | Why |
|---|---|---|
| Pre-publish queue | Until next confirmed push (hours to ~1 week) | Can't trim before it's safely remote |
| Recent-cache of published entries | 7–30d post-publish (operator-configurable) | Rapid debugging + training iteration without HF round-trips |
| Divergence-ancestry full content | Until explicitly acknowledged + published | High-signal; don't lose before push succeeds |

### 9.5.4 Pull-on-demand from remote

Training, eval, audit investigation, and operator retrospective analysis pull from the remote dataset. The local instance never does long-horizon operations over local-only data. HF dataset pulls are fast + bandwidth costs are trivial compared to training compute — this is a good trade.

The Recall tier (§13 in T130) never pulls from remote — it reads distilled preference notes which are already local. Only training + retrospective analysis hit the remote.

### 9.5.5 Storage impact (15-agent full-speed projection)

| Horizon | Prior design (local-durable) | Publish-first |
|---|---|---|
| Day 1 local | 420 MB | 420 MB (pre-publish queue) |
| Week 1 local | ~3 GB | ~600 MB compressed (queue + recent cache) |
| Month 1 local | ~12 GB | **~2 GB compressed (steady state)** |
| Year 1 local | ~30 GB compressed | **~2 GB compressed (flat)** |
| Year 5 local | ~150 GB compressed | **~2 GB (still flat)** |
| Year 5 remote | ~0 | ~150 GB on HF (operator-managed) |

Local footprint is **bounded steady-state**. Remote grows linearly but is operator-managed, HF-backed, and accessible from any device.

### 9.5.6 Atomic publish-then-trim

Trimming is never ahead of publishing. The flow:

1. Stage entries via `opentraces add`
2. Operator reviews (or auto-approves if configured + below threshold)
3. `opentraces push` — await HF confirmation of shard write + hash
4. Update publish cursor with confirmed shard ID + hash
5. Wait for recent-cache retention window to elapse
6. Only then trim locally

A failed push → no trim. Entries stay in the pre-publish queue until next attempt. Pushes are idempotent (content-addressed shards), so re-attempts are safe.

### 9.5.7 Compression at rest

All local JSONL (pre-publish queue + recent cache) is stored with **zstd compression** by default. Typical ratio on Symbiotic's data shape: ~5×. This is additional to the retention policy — retention determines *what* stays; compression determines *how big* it is on disk.

Additionally, the audit trail (T120) uses **content-addressed deduplication** for identical system prompts + context packs: the content is stored once by SHA-256 hash; entries reference by hash. For high-volume agents where 80% of calls share the same system prompt, this cuts T120 volume 3–5× before retention even kicks in.

### 9.5.8 Offline operation

When the remote is unreachable:
- Pre-publish queue grows unbounded (at risk)
- No push attempts until reconnect
- Recent cache remains stable
- Training pipeline cannot run (needs remote pull)
- Eval harness cannot run (needs holdout pull)

Operator is notified when pre-publish queue exceeds a threshold (default: 3 days of accumulated data, or 5 GB compressed). Options: retry push manually, switch to a different remote, or temporarily escalate to `local-heavy` profile.

### 9.5.9 Divergence-ancestry forever rule

The forever-retention rule for divergence-ancestry entries (T130 §14.5) is satisfied by the **remote dataset**, not local. Local keeps only the divergence flag + remote shard reference. When operator investigates a divergence, content is fetched on-demand from HF.

Tracker implementation: when a `decision.audit.divergence` fires, T130 writes a flag at `knowledge-base/self/audit/divergence_flags.jsonl` containing `(event_id, remote_shard_ref, remote_entry_id, acknowledged)`. The flag is tiny (hundreds of bytes). The full content lives on HF.

### 9.5.10 Retention profiles

**Design ceiling: 3 months full-fidelity on device for any profile except `local-only`.** This aligns with the base-model switch cadence (~1–2 switches per year; every switch triggers a full retrain per §4.5). Logs older than 3 months are rarely training-relevant after a model switch, and the remote keeps them for any future need anyway.

| Profile | Recent-cache window | Description |
|---|---|---|
| `cloud-forward` (**default**) | 7 days | Minimum local footprint; HF is source of truth |
| `mixed` | 30 days | Extended offline / fast-access window |
| `local-heavy` | **90 days (3 months)** | Maximum local retention outside regulated contexts; caps at the base-model switch cadence |
| `local-only` | forever local, no remote publishing | Regulated-context operators who cannot send data to cloud; **incompatible with T131 training** unless operator runs a local HF-compatible mirror |

Operator picks at T131 setup. Changing profiles emits a Matrix-visible notification — retention posture is never silently altered.

### 9.5.11 What this changes about the T130 flywheel

The T130 feedback loop (§14) is unchanged. What changes is the retention lifecycle:

- T130 writes locally, as designed (no change)
- Publish pipeline (§04) pushes to remote (no change)
- Retention job (§11) trims local after confirmed push + recent-cache window (new)

The Recall tier still reads local preference notes (Markdown, tiny). The Inquisitor still reads local preference notes into context packs. Training pulls from remote (not local). All the roles of data are preserved; only the *persistence location* of the raw ledger moved from "forever local" to "forever remote, briefly local."

### 9.5.12 What this does NOT change

- **Privacy scoping (§9)** — unchanged. Sensitivity-tier gating applies at publish time regardless of retention profile.
- **Archive notes from sub-goal completions** (T130 §5.3) — Archive entries, not ledger entries; separate lifecycle.
- **Operator Markdown writes** (methodology, preferences) — Markdown files stay local, under operator control.
- **Trust boundary** — remote publishing was already in-scope per §3.2; publish-first retention just leverages it more fully.

---

## 10. Non-goals

- **Full pre-training.** We're doing LoRA adapter fine-tuning, not continued-pretraining. Scope creep at its purest.
- **Multi-modal training.** Text only for MVP.
- **Continuous online learning / RL.** Too risky + expensive + unstable at this scale. Batch retraining only.
- **Shared/federated models across operators.** Each operator gets their own; no cross-contamination.
- **Replacing base models.** `personal` is a tier, not a replacement. `deep` always stays base.
- **Training-time interpretability dashboards.** Operators see eval scores and override-rate deltas. Attention visualization, influence functions, etc. are separate work.
- **Custom base models.** We use existing HF base models; we don't train from scratch.
- **Distillation from `deep` to `personal`.** Tempting (use expensive model's outputs as training signal) but risky for overfitting + cost. Out of scope for MVP.

---

## 11. Open design questions

| # | Question | Leaning |
|---|----------|---------|
| 1 | Default base model at MVP setup | Operator picks from the compatibility matrix (§4.5) — latest Qwen 3.x family, Llama 3.x+, or Mistral current-gen, whichever meets criteria at setup time. No hard pin. |
| 2 | Should tool-use patterns (T122) be trained on, or kept as context-hydration only? | Train on them — they're high-signal for behavior cloning |
| 3 | Include decision-audit divergences (T130 §14.5) in SFT or DPO only? | DPO only — they're explicitly preference pairs |
| 4 | Thread conversations as training data — full messages or summary-only? | Full messages, heavily redacted. Style signal is distributed |
| 5 | Eval pass/fail threshold for §6.1 | 70% / 85% is a starting guess; first two runs will recalibrate |
| 6 | Retraining cadence default | Monthly + event-driven on ≥500 entries |
| 7 | LoRA rank: 16, 32, or dynamic? | Start 16; bump to 32 if eval shows under-fitting |
| 8 | When should operator be *required* to opt-in vs allowed to defer? | Never required. Deferral keeps T131 dormant indefinitely |
| 9 | Should the adapter ship as a single LoRA or multiple (one per agent role)? | Single for MVP; per-role is a v2 optimization if signal diverges across roles |
| 10 | Symbiotic-hosted option at all, ever? | Not in MVP. Revisit only if operator demand is clear AND strong privacy guarantees (full-disk encryption, audited enclaves) are available |
| 11 | Offline-queue threshold for operator notification | 3 days / 5 GB compressed whichever hits first. Real usage will tune |
| 12 | Should `local-only` profile be supported for T131 via a local HF mirror? | Defer — document as incompatible for MVP; revisit if regulated-context demand surfaces |
| 13 | Pre-publish queue — daemon crash recovery | Queue is stored on disk (not in-memory) and replayed on daemon restart; idempotent push semantics handle duplicates safely |

---

## 12. Migration path

1. **Phase 0 (no code):** T130 ships with training-readiness fields. Ledger accumulates silently. No training activity.
2. **Phase 1:** `symbiotic-trace-export` crate — Rust adapter to opentraces TraceRecord. Manual-invocation CLI command; no daemon integration yet.
3. **Phase 2:** Integrate `opentraces push` flow. Operator chooses HF private dataset. First published corpus.
4. **Phase 3:** `training/` Python harness — SFT pipeline via HF TRL + PEFT. Local MLX backend only for MVP. First adapter trained.
5. **Phase 4:** Evaluation harness (§6). Manual adapter swap in T83 LLM Runtime Manager.
6. **Phase 5:** `personal` tier wired into T101 Provider Management. Routing rules from §5.2 live.
7. **Phase 6:** DPO pipeline (Stage B) — only after Stage A is stable and override-pair volume ≥1000.
8. **Phase 7:** Cloud compute backends (Lambda / RunPod / DGX Cloud) — optional, operator-configured.
9. **Phase 8:** Automated retraining cadence (§8) — scheduled daemon job; atomic adapter swap on eval pass.

MVP is Phase 1–5. Phases 6–9 are enhancements that become worth building once Phase 1–5 has proven value.

---

## 13. What stays unchanged

| Component | Why |
|---|---|
| T130 ledger format | Already designed for this consumer |
| T120 audit trail | Consumed unchanged by export adapter |
| T122 tool memory | Consumed unchanged by export adapter |
| T82 Redaction Policy Engine | Consumed unchanged for first redaction pass |
| T83 LLM Runtime Manager | Extended with adapter loading; no new paradigms |
| T101 Provider Management | Extended with `personal` tier; no new paradigms |
| `fast` / `balanced` / `deep` tier naming | Extended by one tier; existing tiers unchanged |
| Gatekeeper / capability model | No new capabilities — training is a local operation on already-authorized data |

---

## Related

- [T130 Grouped Inquisition](grouped-inquisition.md) — produces the preference ledger this task consumes
- [opentraces](https://github.com/JayFarei/opentraces) — the agent-trace schema + CLI we publish to
- [HuggingFace TRL](https://github.com/huggingface/trl) — training library (SFTTrainer + DPOTrainer)
- [HuggingFace PEFT](https://github.com/huggingface/peft) — LoRA / QLoRA adapter library
- [T102 Agent Prompt & Role Versioning](../../tasks/102-agent-prompt-versioning/) — versioning pattern reused for adapters
- [T82 Redaction Policy Engine](../../tasks/82-redaction-policy-engine/) — first redaction pass
- [T128 model-tier naming convention](source-archeology.md) — `fast`/`balanced`/`deep`/`personal`
