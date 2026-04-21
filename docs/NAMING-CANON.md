# Symbiotic Naming Canon

This is the single naming standard for the product vision, UX, and external communication.

## Core Terms

| Component | Canonical Name | Meaning |
|---|---|---|
| Persistent knowledge layer | **Archive** | Source-backed entries and analysis |
| Unit of captured knowledge | **Entry** | A single captured item (link, thread, note, repo) |
| Compressed analysis artifact | **Brief** | Fast summary with key points, risks, and actions |
| Long-term entity memory | **Neural Graph** | People, projects, entities, and relationships |
| Context retrieval plane | **Recall Gateway** | Policy-aware context assembly for agents/models |
| Content intake system | **Intake** | Raw content capture (URLs, text, files) before processing |
| Knowledge processing pipeline | **Distillery** | Multi-stage processor (Reduce → Reflect → Reweave → Verify) |
| Main runtime service | **Nucleus** | Core orchestration runtime |
| Capability enforcement service | **Gatekeeper** | Authorization and action mediation |
| Credential isolation system | **Vault** | Secret storage and auth/session boundary |
| Work execution queue | **Queue** | Durable job scheduling and retries |
| Matrix transport install step | **Matrix Link** | Matrix device pairing + trust bootstrap |

## Install Wizard Steps

Canonical step names for the setup wizard (see `docs/architecture/install-wizard.md`):

`Signal Online -> Nucleus Boot -> Matrix Link -> Memory Channels -> Provider Keys -> Vault Seal -> Recall Calibration -> System Alive`

## Flow Language

Use this sequence consistently:

`Capture -> Intake -> Distillery -> Archive -> Recall -> Action -> Evolution`

## Model Tiers

Worker agents are described by **capability + speed tier**, never by provider or model name. Canonical tiers:

| Tier | Purpose | Examples of use |
|---|---|---|
| `fast` | Low-latency, low-cost classifier / tagger. Short context, many cheap calls preferred over one deep call. | Source Archeology `excavator`, `dater`, Handoff `reporter`; Project Bootstrap `discovery`. |
| `balanced` | Mid-latency reasoning. Multi-input decisions that don't need deep chain-of-thought. | Reserved; future stages that need light reasoning without full-depth planning. |
| `deep` | Highest-capability reasoning; slowest. Multi-step planning, nuanced classification, structured output under ambiguity. | Source Archeology `diagnostician`, `reconciler`, `scaffolder`, `triager`; Project Bootstrap `analyst`, `ingest`. |

**Do not** use provider-specific class names in design docs, task chunks, or code (`Haiku-class`, `Sonnet-class`, `gpt-4o`, `Opus-class`). The runtime backend maps tiers to model families per deploy; coupling the design to a vendor lineup rots as models ship.

## Style Rules

- Use these names consistently across README, VISION, product copy, and app text.
- Prefer concise nouns over overloaded technical terms.
- Keep names concrete and memorable; avoid extra metaphor layers.
- When referring to implementation paths or event names, append them in backticks after the canonical term (example: `Archive` -> `knowledge-base/` during migration).
