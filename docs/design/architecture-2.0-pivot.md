# Architecture 2.0 Pivot: The Distillery & Declarative System

**Status**: Approved (Phase: Execution)
**Replaces**: Legacy single-agent ReAct loop, flat RAG memory, and Material 3 UX.

## 1. The Declarative Cognitive Control Plane (With Strict Imperative Execution)
Symbiotic is no longer an imperative "God Agent" trying to solve tasks in a massive `while` loop. 
- **The Concept**: Symbiotic acts as a Kubernetes-style controller. The desired state of the human's mind and tasks are declared in Markdown files (`knowledge-base/identity/` and `operations/`). The Orchestrator reads the state, compares it to reality, and executes deterministic workflows to reconcile them.
- **The Security Invariant (The Extended Arm)**: Because Symbiotic acts as a 24/7 autonomous agent capable of controlling headless browsers (Playwright), executing code, and accessing sensitive APIs (Vault), **the execution layer MUST remain strictly imperative and sandboxed.**
  - We retain the `CapabilityToken` and `AccessBroker` (Gatekeeper) models in `symbiotic-trust`.
  - A declarative Markdown file cannot grant a cloud LLM access to the user's bank account. The Gatekeeper (`AccessBroker`) ensures that even if a cloud model suffers a Prompt Injection attack from a malicious webpage, it lacks the cryptographic token to execute the payload. Only the Local LLM or the User can grant elevated capability tokens.

## 2. Memory as Identity (The Distillery Pipeline)
Memory is not storage; it is identity construction. We abandon flat Vector DBs (pure RAG) in favor of a **Markdown + Graph Duality**.
- **The Storage Layer (Human UI)**: Plain Markdown files organized in an Obsidian-compatible vault. This guarantees ownership, easy exporting, and native editability.
- **The Index Layer (Agent Brain)**: A local SQLite-backed Property Graph. When a file is read, claims are extracted and written to the graph with **temporal tags** and **emotional/impact weighting** (somatic markers for fast routing).

### The Three Spaces of Memory
1. **Neural Graph (Semantic)**: Atomic notes, wiki-linked. (What the system knows).
2. **Self Space (Episodic)**: The agent's identity, user interaction preferences, and calibrated confidence. (Who the system is).
3. **Methodology (Procedural)**: Active tasks, friction logs, and handoffs. (How the system acts right now).

### The Digestive Pipeline (Unidirectional)
Instead of a monolithic ReAct loop, intake runs through chained, isolated LLM contexts:
1. `Reduce`: Extract atomic claims (strip fluff).
2. `Reflect`: Connect claims to the existing graph.
3. `Reweave`: Update *old* notes based on *new* knowledge.
4. `Verify`: Strict schema validation (the immune system).
5. `Archive`: Move raw source to cold storage.

## 3. Agentic Swarms & The Context Handoff Protocol
To prevent "looping" and context degradation, long-running tasks are handled by swarms, not single agents.
- **Dynamic Skill Routing**: Agents load specific `SKILL.md` graphs dynamically based on intent, rather than stuffing 10 skills into one prompt.
- **Prompt Cache First**: System prompts are strictly ordered. Static rules (SOUL, formatting) at the top; dynamic inputs at the bottom.
- **The 80% Circuit Breaker**: The LLM Runtime Manager monitors token usage. At 80% capacity, the agent is *forced* to write a `YYYY-MM-DD-HHMM-context-handoff.md` file (Objective, Done, Pending, Blockers). 
- **Worktree Isolation**: Sub-agents operate in isolated `git worktrees`. The handoff `.md` file acts as an Async RPC to wake up the next agent in the swarm to continue the task without polluting the main branch.

## 4. Strict Security & Sync Boundaries
- **The Vault vs. The Archive**: Highly sensitive data (Medical records, Passwords, Financials) **NEVER** enter the Markdown Archive or the Neural Graph Index. They live exclusively in the `Credential Gateway` (svlt2 encrypted). Agents must request JIT (Just-In-Time) decryption to use them.
- **Sovereign Sync**: The Archive is synced via Matrix E2EE (for live state) and backed up to a **Self-Hosted Git Server** running on the user's VPS. We do not use public GitHub for personal brains.
- **Universal Intake**: All incoming unstructured files (PDF, Word) are routed through a Python-based Markdown converter (e.g., Markitdown paradigm) before hitting the distillery pipeline.

## 5. The "Identity Stream" UX
The Flutter app (`submodules/app`) drops the utilitarian Material 3 design.
- **The Stream**: The home screen visualizes the distillery pipeline. It is a living stream of thoughts, connected by their impact scores, not just a chronological list.
- **Glassmorphism & Motion**: The UI must feel alive, providing ambient feedback of the Nucleus's background processing.
- **Native Wikilinks**: The UI natively parses `[[links]]` allowing seamless traversal of the Neural Graph.
- **Command Palette**: Form fields are replaced by a universal, contextual command palette for capture and orchestration.
## 6. Adaptive Goal Workflows (The Action Engine)
The Neural Graph is passive; Goals are active. Symbiotic manages user objectives through dynamically compiled, adaptive workflows rather than hardcoded domain agents.

### The Goal Lifecycle
1. **The Inquisition (Setup)**: When a user creates a new Goal (e.g., "Health Tracking" or "Algorithmic Trading"), the Orchestrator does not immediately start executing. It begins a long-form, interactive interview to assess:
   - Current knowledge level.
   - Specific constraints (budget, time, physical limitations).
   - Expected outcomes vs. open-ended research.
2. **The Blueprint (Compilation)**: Based on the Inquisition, the Orchestrator generates a declarative Markdown/JSON workflow plan in `operations/goals/[goal-id]/plan.md`. The user can freely edit this plan before execution.
3. **Execution Phases (Dynamic Adaptation)**: Goals transition through distinct phases, spawning different swarms:
   - *Phase 1: Research*: Spawns Discovery Agents to scour the Neural Graph and external web, summarizing findings into `knowledge/`.
   - *Phase 2: Provisioning*: Spawns Infrastructure Agents to set up isolated Sandbox environments or request JIT Vault credentials (e.g., "Create a crypto wallet in the Goal Sandbox").
   - *Phase 3: Implementation*: Executes the concrete tasks (e.g., "Run the trading backtest script").
   - *Phase 4: Maintenance*: Spawns Chron-Agents for recurring sub-tasks (e.g., "Weekly health metric check-in").

### Goal Sandboxing & Vault Integration
Goals that require external interaction or credential generation (like trading accounts) execute inside a dedicated **Goal Sandbox**.
- The Sandbox has a localized `Vault` namespace. The trading bot cannot access the user's primary email password, only the API keys explicitly provisioned for that specific Goal ID.
- If a Goal requires user input (e.g., "I need you to deposit funds to this address to continue"), it pauses and routes a secure `Action Required` prompt to the user's Identity Stream UX.

## 7. Dynamic Skill Synthesis (The Resourceful Arm)
Symbiotic is not limited to a hardcoded set of tools or an app store. It is an extended arm that works 24/7 on novel problems by writing its own integrations.
- **The Process**: When a Goal workflow requires a tool that does not exist (e.g., "Synthesize voice to call a restaurant" or "Scrape this specific niche dashboard"), the Orchestrator does not fail.
- **The Sandbox**: It spawns a specialized Coder Agent (e.g., OpenClaw protocol) inside a secure, ephemeral Docker sandbox.
- **The Synthesis**: The Coder Agent writes the necessary script (Python/Node.js/Rust) to interact with the target API or headless browser (Playwright), tests it locally in the sandbox, and exposes it as a temporary Model Context Protocol (MCP) tool.
- **The Persistence**: Once the tool succeeds in the Goal workflow, the Orchestrator archives the synthesized script into `operations/skills/` for future use. The system literally teaches itself how to perform new tasks.

## 8. The "Mission Control" UX Paradigm
The Flutter app is not just a note-taking viewer; it is a live command center for a 24/7 autonomous workforce.
- **The Activity Stream**: The home screen visualizes the active Goal execution. Users see live status updates:
  - `🟢 Goal: Algorithmic Trading -> Spawning Sandbox...`
  - `🟡 Goal: Algorithmic Trading -> Awaiting 2FA for Binance login... [Tap to Approve]`
  - `🟢 Goal: Algorithmic Trading -> Backtest complete. Yield: +4%. Deployed to chron-worker.`
- **Transparent Autonomy**: The user can tap into any active workflow to see exactly which agents are running, what code they are synthesizing, and what context they are holding, providing absolute oversight of the "Extended Arm."
