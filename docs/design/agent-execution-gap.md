# Agent Execution Gap Analysis


**Status**: Research (Session 70, 2026-03-17)
**Context**: E2E test shows goal.completed for "Build me a landing page" but no actual output produced.
**Reference**: OpenClaw (openclaw/openclaw), OpenHands (arxiv 2407.16741)

## The Problem

Symbiotic's agent orchestration **designs** are sound (agent-orchestration.md, vm-sandboxing.md, agent-swarms.md). The deliberation pipeline classifies goals correctly, generates plans, and routes to execution. But execution is hollow — agents can only think, recall, and archive. They cannot create files, run commands, or interact with the outside world.

### Current Agent Tool Inventory

| Tool | What it does | Creates real output? |
|------|-------------|---------------------|
| `recall` | Query Archive for context | No — read-only |
| `archive` | Write entries to Archive | Markdown notes only |
| `queue` | Submit async jobs | Delegates, doesn't execute |
| `ask_user` | Ask clarifying question | Pauses execution |
| `generate_plan` | Propose plan for approval | Plan artifact only |
| `liveness` | Heartbeat | No |

**Result**: Agent "executes" a goal by reasoning about it, then reports "done."

## What OpenClaw Does Differently

OpenClaw's agents have typed, first-class tools that interact with the real environment:

### Filesystem (group:fs)
- `read` — read files
- `write` — create/overwrite files
- `edit` — modify existing files (line-level edits)
- `apply_patch` — multi-hunk structured patches

### Runtime (group:runtime)
- `exec` — run shell commands (sync or background, with timeout, PTY support)
- `process` — manage background processes (poll, log, write stdin, kill)

### Web (group:web)
- `web_search` — search via Brave/Perplexity/etc.
- `web_fetch` — fetch URL → markdown

### Browser (group:ui)
- `browser` — full Playwright browser: snapshot, screenshot, act (click/type/hover), navigate, console, pdf, upload
- `canvas` — visual workspace for the user

### Agent Coordination
- `sessions_send` / `sessions_spawn` — delegate to sub-agents
- Multi-agent sandbox with per-agent tool profiles

### Key Architecture Patterns
- **Tool profiles**: `minimal`, `coding`, `messaging`, `full` — scoped per agent or per provider
- **Tool groups**: `group:fs`, `group:runtime`, `group:web`, etc. for bulk allow/deny
- **Elevated mode**: sandboxed agents can request host-level exec via approval gate
- **Loop detection**: tracks repetitive tool calls, blocks no-progress loops
- **Workspace-rooted**: agents operate in a workspace directory, file tools are workspace-contained by default

## What Symbiotic Already Has (Designed, Not Wired to Agents)

| Capability | Design Doc | Implementation | Wired to Agents? |
|-----------|-----------|----------------|-------------------|
| VM Sandboxing | `vm-sandboxing.md` | `symbiotic-vm` crate (mock) | No |
| Browser Automation | T79 | `symbiotic-browser` crate | No |
| Task Graph | `agent-orchestration.md` | Designed (Rust types) | No |
| Verification Pipeline | `agent-orchestration.md` | Designed (3-layer) | No |
| Session Proxy | `agent-orchestration.md` | Designed | No |
| Model Router | `agent-orchestration.md` | Designed (tiered) | No |
| Gatekeeper | `symbiotic-trust` | Implemented (capability tokens) | Partially |

## The Gap: Missing Agent Tools

To make Symbiotic's agents actually execute goals, they need these tools added to `builtin_tools.rs`:

### Priority 1 — Minimum Viable Execution
```
shell_exec    — Run command in workspace/sandbox, return stdout+stderr+exit_code
file_read     — Read file contents
file_write    — Create/overwrite file
file_edit     — Line-level edit (or apply_patch)
```

With just these 4, agents could: scaffold projects, write code, run builds, check results.

### Priority 2 — Web & Research
```
web_search    — Search via configured provider
web_fetch     — Fetch URL → markdown
```

### Priority 3 — Browser & Interaction
```
browser_*     — Wire symbiotic-browser crate as agent tools
```

### Priority 4 — Coordination
```
sub_agent     — Spawn scoped sub-agent for subtask
```

## Execution Model Change

Current: `ReAct loop → LLM thinks → calls abstract tool → thinks more → "done"`

Needed: `ReAct loop → LLM thinks → shell_exec("npm create vite@latest") → file_edit(App.tsx) → shell_exec("npm run build") → verify output → done`

The ReAct executor (`executor.rs`) already supports the loop. The change is **adding real tools** and **wiring them through Gatekeeper capability checks**.

## Security Model (Already Designed)

Symbiotic's Gatekeeper + CapabilityToken system is the right security layer for this:
- Agent requests `shell_exec("npm install")` → Gatekeeper checks capability scope
- Trust level determines auto-approve vs user-prompt
- VM sandbox isolates untrusted execution (vm-sandboxing.md)
- Session Proxy handles authenticated actions (no raw creds to agents)

This is actually **more secure** than OpenClaw's approach (which uses simple allow/deny lists). The infrastructure exists — it just needs the tool implementations wired through it.

## Recommendation

Don't rebuild the orchestration layer. The designs are right. Focus on:

1. **Implement 4 core tools** (shell_exec, file_read, file_write, file_edit) in `builtin_tools.rs`
2. **Wire through Gatekeeper** for capability-gated execution
3. **Use workspace directory** as agent sandbox (like OpenClaw's `agents.defaults.workspace`)
4. **Wire verification pipeline** to actually run test commands after agent produces output
5. **Later**: connect `symbiotic-browser` and `symbiotic-vm` as agent tools

## References

- [OpenClaw Tools Docs](https://docs.openclaw.ai/tools)
- [OpenClaw GitHub](https://github.com/openclaw/openclaw)
- [OpenHands Paper](https://arxiv.org/abs/2407.16741) — event-stream agent architecture, Docker sandbox
- [OpenClaw-RL](https://github.com/Gen-Verse/OpenClaw-RL) — RL training for personalized agents
