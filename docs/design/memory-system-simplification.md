# Architecture Simplification: Living Memory System

> **Replaces/Updates:** Sections of `docs/design/memory-system.md`, `docs/architecture/vector-search.md`
> **See also:** `docs/design/vault-as-truth.md` — the broader architectural pivot making Markdown the single source of truth (SQLite is a derived index)
> **Context:** Realignment to the "Extended Arm / Declarative Cognitive Control Plane" vision, eliminating over-engineered local processing while preserving the core Distillery -> Reweave -> Archive pipeline.

## Overview
The T109 implementation successfully built the 5-layer Duality Architecture (Markdown Vault + SQLite Neural Graph). However, certain components (specifically the brute-force JSON vector search and the deterministic Rust-based "Cascading Staleness" logic) are computationally heavy, over-engineered, and misaligned with local-first performance. 

This document defines the simplification of Phase 1 (Vector Storage) and Phase 2 (Temporal Modeling/Decay) to keep the system fast, agentic, and aligned with our knowledge-base research on human-inspired memory engines.

## Phase 1: Vector Storage Consolidation (`sqlite-vec`)

### Problem
The current Semantic Search relies on `symbiotic-context/src/vector_index.rs`, which implements a brute-force cosine similarity search over a `vector_index.json` file. As the Archive grows, this JSON file must be fully deserialized and iterated over in memory for every query, breaking local scaling constraints.

### Solution
We will deprecate `vector_index.json` and migrate to `sqlite-vec` (the modern successor to `sqlite-vss`).

**Implementation Details:**
1. **Single Database Duality**: The Neural Graph (`entities`, `relationships`, `memories`) and FTS5 keyword indices are already in SQLite. We will add `sqlite-vec` virtual tables to the exact same database.
2. **Schema Update**:
   ```sql
   CREATE VIRTUAL TABLE vec_entries USING vec0(
     entry_id TEXT PRIMARY KEY,
     embedding float[768] -- Assuming nomic-embed-text dimensions
   );
   ```
3. **Hybrid Queries**: This allows single-query hybrid search (BM25 + Cosine) natively within SQLite, drastically reducing IPC and memory overhead.

**Justification from KB:** 
As noted in `legacy/knowledge-base/articles/bcbff4b4-architecting-a-memory-engine-inspired-by-the-human.md`, standalone vector databases "get too expensive, or too slow as they grow". `analysis/tldrs/50a0fc18.md` evaluated SQLite+vss against external solutions like Qdrant; `sqlite-vec` provides the optimal balance of zero-infrastructure local deployment with high performance.

---

## Phase 2: Temporal Modeling & Smart Forgetting

### Problem
The current `symbiotic-memory/src/staleness.rs` contains highly complex deterministic logic for "Cascading Staleness" and "Somatic Markers" (valence/arousal). This attempts to build a rigid rules engine in Rust for something that is inherently semantic and fluid. It causes unnecessary CPU overhead and fragile graph invalidation.

### Solution
We will strip the deterministic Rust staleness engine and replace it with two simpler, more resilient mechanisms: **Graph Decay** and **LLM-driven Reweave (Visible Archival)**.

**1. Mathematical Graph Decay (Read-Time)**
Instead of actively invalidating facts via background workers, decay is calculated at retrieval time.
- During BFS Graph traversal (`symbiotic-context/src/graph.rs`), apply a simple half-life decay function based on the edge/fact timestamp.
- **Recency & Relevance Bias**: Recently accessed or heavily connected nodes resist decay longer.

**2. LLM-driven Reweave & Visible Archival (Write-Time)**
We shift the cognitive burden of state change from Rust rules to the LLM during the `Reweave` stage.
- The LLM explicitly issues `ADD`, `UPDATE`, or `ARCHIVE` commands for facts based on new information.
- **CRITICAL: No hard deletes of superseded knowledge.** When the LLM determines a fact is no longer valid or true (for example, "The user no longer uses React"), the fact is not erased from canonical memory.
- Instead, the canonical Markdown note moves that fact into a visible final `## History` section under `### Archived Facts`, and the derived SQLite layer indexes it as archived state.
- **Recall Gateway Policy**: Archived facts are excluded from standard active context retrieval (Core Memory) to prevent hallucination of outdated facts, but they remain available for historical auditing, retroactive timeline reconstruction, or explicit deep-search queries.

**Justification from KB:**
`legacy/knowledge-base/articles/1b852ec3-memory-vs-rag-understanding-the-difference.md` emphasizes that memory must track *when* facts became invalid and understand causal chains. `bcbff4b4` describes "Smart Forgetting" where irrelevant info fades naturally without being erased completely. Visible archival via the Reweave stage implements this human-like memory model while preserving the complete historical timeline.
