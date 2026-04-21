# SmartOffice Integration Patterns

**Status**: Proposed Specification
**Epic**: Memory, Self-Improvement (T109, T112)
**Source**: Architectural patterns imported from the `smartoffice` ERP system.

This document specifies how Symbiotic integrates three critical architectural patterns from the `smartoffice` project to stabilize data ingestion, enforce LLM safety, and improve semantic retrieval.

---

## 1. The Injector/Connector Firewall (for T109 OAuth Data Ingestion)

### The Concept
External systems (Notion, GitHub, Jira) have messy, proprietary APIs. If Symbiotic agents query these APIs directly, the LLM context window becomes polluted with raw JSON, and the system becomes tightly coupled to third-party SDKs. 
The SmartOffice pattern states: **External Source → Connector → Injector → Local Vault (Markdown).** 

### Symbiotic Implementation Plan
For **T109 (OAuth Connectors)**, we will build an asynchronous ingestion pipeline.
1. **The Connector Daemon**: A background task in the Nucleus that periodically polls external APIs (e.g., Notion API).
2. **The Translation**: It maps a Notion Page into a standard Symbiotic Markdown file with YAML frontmatter.
3. **The Injector**: It writes this file to a dedicated read-only memory tier: `knowledge-base/external/notion/{page_id}.md`.
4. **The Agent View**: Agents do not have a `search_notion` tool. They only have a `search_memory` tool. When they search for "marketing plan", the vector DB seamlessly returns the content from `external/notion/marketing-plan.md`.

### Problematic Friction Points & Resolutions
*   **Friction**: **Syncing State (Two-Way Sync)**. If an agent wants to *modify* a Notion page, writing to the local Markdown file won't automatically push to Notion.
*   **Resolution (Read-Only External Memory)**: For MVP, the `external/` tier is strictly **Read-Only** for agents. If an agent wants to update an external system, it must use a specific declarative tool (`update_notion_page`), which emits an RPC call to the Nucleus. The Nucleus translates the intent back to the external API. The local Markdown file is updated on the next sync cycle.

---

## 2. Strict Serde Validation for Markdown Frontmatter (for T112 Agent DNA)

### The Concept
SmartOffice enforces schema strictly via Rust `serde` structs (`#[derive(Deserialize)]`) on the YAML frontmatter of every Markdown file. Invalid files are outright rejected by the system.

### Symbiotic Implementation Plan
In **T112 (Evolution Engine)**, agents rewrite their own DNA (`roles/*.json` and `skills/*.md`). Relying on LLMs to output perfect JSON/YAML every time is a guaranteed failure path. 
We will inject a validation binary into the Distillery Sandbox: `symbiotic-validator`.

1. **The Sandbox Test**: When the Process Engineer generates a new `flutter-coder.md` skill, it runs `symbiotic-validator flutter-coder.md` *inside the sandbox*.
2. **The Ping-Pong Loop**: If the YAML frontmatter has a schema error (e.g., missing a `required_tools` field), the validator throws a precise Rust compilation-style error. The LLM reads the error and fixes its own syntax in the sandbox.
3. **The Nucleus Gatekeeper**: When the Distillery pushes the final artifact to the host, the Nucleus runs the exact same `serde` deserialization check. If it fails, the file is rejected and the Proposal is aborted.

### Problematic Friction Points & Resolutions
*   **Friction**: **Brittle Evolution**. If the Nucleus is too strict, the Process Engineer agent will constantly fail to generate valid proposals, clogging the swarm with failed goals.
*   **Resolution (Schema Reflection Tool)**: Provide the agent with a `get_schema(entity_type: "Skill")` tool in the sandbox. This returns the exact JSON Schema derived from the Rust structs, allowing the LLM to understand the required fields *before* it tries to write the file.

---

## 3. Entity ID Path Convention (`{entity_type}/{local_slug}.md`)

### The Concept
SmartOffice does not rely on opaque UUIDs or hidden SQLite databases to resolve references between files. It uses explicit, human-readable paths as the primary key: `project/2024-001.md`.

### Symbiotic Implementation Plan
Currently, LLMs struggle to reference specific memories or goals if the IDs are opaque (e.g., `goal-f47ac10b`). We must transition the `knowledge-base/` and the Vector DB (`sqlite-vec`) to use the **Semantic Path** as the primary key.

1. **The Hierarchy**:
   - `roles/coder.json`
   - `skills/rust/axum-api.md`
   - `goals/active/build-landing-page.md`
2. **The LLM Advantage**: An LLM can easily deduce or hallucinate a correct path. If it needs to reference a skill, it can intuitively link to `skills/python/fastapi.md` within its prompt output, and the Nucleus can resolve that directly on disk.
3. **Vector DB Indexing**: In `sqlite-vec`, the `id` column becomes the literal string path relative to the vault root.

---

## 4. Document History & Hybrid Retrieval (The Best of Both Worlds)

### The Comparison: Symbiotic vs. SmartOffice
When comparing how both systems retrieve data and track history, they have opposite but highly complementary strengths:
*   **Retrieval**: *Symbiotic is vastly superior.* It uses a state-of-the-art Hybrid Vector Search (BM25 + Cosine Similarity via `sqlite-vec`), Context Graphs, and Temporal Decay. *SmartOffice* relies on basic deterministic filtering of loaded Rust structs.
*   **History & Structure**: *SmartOffice is vastly superior.* Because every file is a strict `serde` struct, SmartOffice knows exactly what an entity is, its lifecycle state (Draft → Active), and its structural relations. Symbiotic memories are currently loose and unstructured, relying mostly on semantic similarity to find relations.

### Symbiotic Implementation Plan: The Hybrid Merge
By combining Symbiotic's Vector DB with SmartOffice's strict structural guarantees, we achieve **Deterministic Vector Search** and **Semantic Provenance**.

#### A. Deterministic Metadata Pre-Filtering
Vector search is prone to hallucinations (finding a "semantically similar" document that is actually the wrong entity type). We will import SmartOffice's strict `entity_type` frontmatter into Symbiotic's Archive.
*   **The Injection**: Every Markdown file in the Archive must declare an `entity_type` (e.g., `Goal`, `Skill`, `Person`, `Project`, `Note`) and a `status` in its YAML frontmatter.
*   **The Execution**: When the agent uses `search_memory(query: "Flutter auth", type: "Skill")`, the `sqlite-vec` engine first executes a strict, deterministic `WHERE entity_type = 'Skill'` SQL filter *before* running the cosine similarity vector search. This eliminates 90% of retrieval hallucinations.

#### B. Semantic Provenance (The Matrix Audit Trail)
In Symbiotic, agents constantly overwrite and deduce new memories based on conversations. If a memory is wrong, the user needs to know exactly *what conversation* caused the agent to write it. We will borrow SmartOffice's lifecycle state machines and link them to the Matrix transport layer.
*   **The Injection**: Every generated memory in Symbiotic gets an Epistemic State in its YAML frontmatter. Crucially, the `derived_from` array must contain exact Matrix `event_id`s or Thread Room IDs:
    ```yaml
    provenance:
      source: "thread_distillery" # or agent_deduction, notion_import
      agent_role: "researcher"
      confidence: 0.85
      derived_from: 
        - "matrix://room/!thread-saas-product:server.com/event/$abc123def"
        - "external/notion/456"
    ```
*   **Matrix Interweaving**: This fulfills the `Thread Distillery` specification (`docs/design/thread-architecture.md`). The ultimate history of a document is the Git commit log of the Vault, but the *causality* is tracked via Matrix. When the user taps a memory in the UI, the app can use the `derived_from` URI to instantly jump to the exact chat bubble in the Matrix Thread where the decision was made. If the user edits that chat message later, the Nucleus knows exactly which deduced agent memories need to be invalidated or re-woven.

### Problematic Friction Points & Resolutions
*   **Friction**: **Vector DB Bloat**. Storing hundreds of metadata fields in a SQLite Vector DB slows down retrieval and complicates the schema.
*   **Resolution (Sparse Indexing)**: The `sqlite-vec` database should only index the vector embedding, the semantic path (`id`), and 3-4 core categorical columns (`entity_type`, `source`, `tags`). All other rich Frontmatter data remains exclusively on the disk in the Markdown files. The DB finds the path, and the system reads the file from disk for the full context.