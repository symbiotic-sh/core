# Vault Organization: The Sovereign Data Structure

**Status**: Proposed Specification
**Epic**: Memory, Self-Improvement, Archiving (T111, T116)
**Depends on**: `docs/design/vault-as-truth.md`, `docs/design/entity-artifact-layout.md`, `docs/design/entity-type-and-schema-kind.md`

This document specifies the exact directory structure, file types, and Transition Boundaries for the Symbiotic `knowledge-base/` (The Vault). 

Because the Vault serves as both the **LLM Context Engine** and the **Human-Readable Obsidian Wiki**, it must follow a strict, predictable hierarchy.

---

## 1. The Four Ingestion Tiers

Data enters the Vault through four distinct tiers of "Trust and Distillation." The Nucleus (via the Distillery Sandbox) enforces this boundary.

| Tier | Folder | Trust Level | Description |
| :--- | :--- | :--- | :--- |
| **Tier 0** | `archive/` | **Raw / Immutable** | Untouched receipts of the outside world. Web clippings, raw PDFs, or unedited meeting transcripts. Used for FTS5 (keyword) search only. |
| **Tier 1** | `library/` | **Managed / External** | Structured reference material from external projects (e.g., the Axum docs, Stripe API spec, research papers). Cleaned and flattened by an agent, but not "personal" knowledge. |
| **Tier 2** | `ledger/` | **Structured / Core** | The strict structured record layer. Canonical entity truth lives in `{slug}.md`; generated read artifacts live beside it using explicit suffixes such as `{slug}.brief.md`. The folder path mirrors the runtime `EntityType` taxonomy; future `SchemaKind` specialization does not create a competing top-level folder tree. |
| **Tier 3** | `identity/`, `operations/`, `threads/` | **Distilled / Personal** | The highest value data. Identity, goals, workflows, skills, reports, and living summaries of your Matrix conversations. |

---

## 2. Example Vault Structure

Here is how a complex Symbiotic installation (managing a software project, a marketing campaign, and personal back-office tasks) looks on disk.

```text
knowledge-base/
│
├── archive/                           # [Tier 0] Raw Receipts
│   ├── web/
│   │   └── 2026-03-14-stripe-pricing-update.md
│   └── documents/
│       └── 2026-02-01-acme-corp-nda.pdf.md      # OCR'd text of a PDF
│
├── library/                           # [Tier 1] External Reference
│   ├── axum-framework/                # Agent-flattened docs for a specific tool
│   │   ├── routing.md
│   │   └── middleware.md
│   └── marketing-benchmarks/
│       └── 2026-saas-conversion-rates.md
│
├── ledger/                            # [Tier 2] Structured Database Records
│   ├── projects/
│   │   ├── symbiotic-os/
│   │   │   ├── symbiotic-os.md
│   │   │   └── symbiotic-os.brief.md
│   │   └── project-phoenix/
│   │       ├── project-phoenix.md
│   │       └── project-phoenix.brief.md
│   ├── people/
│   │   └── sarah-acme/
│   │       ├── sarah-acme.md
│   │       └── sarah-acme.brief.md
│   ├── tools/
│   │   └── rust-lang/
│   │       ├── rust-lang.md
│   │       └── rust-lang.brief.md
│   └── organizations/
│       └── acme-corp/
│           ├── acme-corp.md
│           └── acme-corp.brief.md
│
├── threads/                           # [Tier 3] Living Conversation Summaries
│   ├── thread-saas-launch.md
│   └── thread-server-maintenance.md
├── operations/                        # [Tier 3] Goals, workflows, skills, reports
│   ├── skills/
│   │   └── rust-api-development.md
│   └── workflows/
│       └── deploy-to-production.md
└── identity/                          # [Tier 3] Identity and Preferences
    ├── SOUL.md
    └── preferences.md
```

---

## 3. Transition Boundaries (How Data Moves)

Data rarely stays static. The **Thread Distillery** and **Process Engineer** (T112) constantly move and upgrade data across tiers.

### Scenario A: Analyzing an External Project (Tier 0 → Tier 1)
1. **The Goal**: "Agent, analyze the Axum framework documentation."
2. **The Intake**: The agent clones the Axum repo into a Sysbox Sandbox.
3. **The Distillation**: It strips out all the boilerplate, CSS, and navigation logic. It extracts only the Markdown content.
4. **The Write**: It writes the cleaned files into `library/axum-framework/`. 
5. **The Upgrade**: The Nucleus instantly vector-indexes these files. The agent now has near-instant, clean semantic recall of the Axum framework without polluting the `ledger/` folder.

### Scenario B: A Conversation Becomes a Decision (Tier 0 → Tier 3)
1. **The Chat**: In Matrix `#thread-saas-launch`, you say: *"Let's use Stripe instead of LemonSqueezy. The API is cleaner."*
2. **The Distillery**: The background Thread Distillery reads the chat.
3. **The Link**: It edits `ledger/projects/symbiotic-os/symbiotic-os.md`. Under the canonical truth file, it appends:
   `- Payment Gateway: Stripe. Reason: Cleaner API. [[thread-saas-launch#^event-abc123]]`
4. **The Upgrade**: A raw conversation just became an Atomic Fact, linked directly to its Matrix source via a block reference.

### Scenario C: Backoffice "SmartOffice" Operations (Tier 2)
1. **The Request**: "Generate an invoice for Acme Corp."
2. **The Recall**: The agent queries the canonical record `ledger/organizations/acme-corp/acme-corp.md` to find the billing address and VAT number (Strict YAML frontmatter).
3. **The Action**: It generates a PDF and saves a receipt to `archive/documents/invoice-101.md`.
4. **The Link**: It updates `acme-corp.md` to link to the new invoice.

---

## 4. The Rules of the Vault

To maintain this structure, the Nucleus enforces strict rules on the LLM Agents:

1. **No Sandbox Escapes**: Agents in the Sysbox Execution Plane cannot write directly to the Vault. They must output their proposed Markdown files to a shared `/workspace/output/` volume.
2. **The Gatekeeper**: The Nucleus reads the `/workspace/output/` directory. If the agent tries to write to a protected path (e.g., `identity/SOUL.md`) without the correct `CapabilityToken`, the Nucleus rejects the write.
3. **Frontmatter Enforcement**: If an agent writes to `ledger/`, the Nucleus parses the file with Rust `serde`. If the YAML frontmatter is missing the required `entity_type` or `id`, the file is rejected back to the agent for correction.
4. **Symlinks Forbidden**: The Vault must be 100% portable. Absolute paths and symlinks are banned. All links must be relative Obsidian `[[wikilinks]]`.

### Type and Schema Rule

Tier 2 records follow the runtime-owned type model:

- `type:` is the fixed core `EntityType`
- optional `schema:` may add richer validation and product behavior later
- storage folders still mirror the core type, not the schema

Example:

```text
ledger/tasks/5x5-strength/5x5-strength.md
ledger/tasks/5x5-strength/5x5-strength.brief.md
```

even if the note later carries:

```yaml
type: task
schema: training_program
```
