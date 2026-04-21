# Sovereign Sync — Distributed Data Integrity


**Status**: Proposed Specification
**Epic**: Sovereign Sync / Data Integrity (T111, T118)
**Depends on**: `docs/design/vault-as-truth.md`

## Overview

Symbiotic is designed for distributed use across multiple devices (Phone, VPS, Desktop). The **Vault as Truth** model uses Git as the underlying transport and version control system. This document specifies how the system handles background synchronization and resolves conflicts when the user and the agent edit the same data simultaneously.

## Core Mechanisms

### 1. Atomic Auto-Commits
Every mutation performed by the `VaultWriter` (adding a fact, archiving a fact, creating an entity) triggers an immediate Git commit on the host machine.
- **Message Format**: `memory({entity_id}): {action} [source: {id}]`
- **Granularity**: One commit per surgical edit.

### 2. Background Sync Loop
The Symbiotic Daemon runs a background synchronization task (default interval: 5 minutes):
1. **Pull**: Execute `git pull --no-rebase`.
2. **Conflict Detection**: If the pull fails with a merge conflict, enter **Mediation Mode**.
3. **Push**: If the pull succeeds, execute `git push` to broadcast local changes.

## Conflict Mediation (Programmatic Escalation)

When a merge conflict occurs, the Nucleus acts as a **Programmatic Mediator**. This is a deterministic logic layer, not an AI agent, ensuring 100% reliability in presenting the truth to the user.

### Why Conflicts are Rare
Because Symbiotic uses **Surgical Markdown Edits** (one line at a time) rather than full-note rewrites:
- **Auto-Merge Success**: Git's standard merge algorithm handles most simultaneous edits (e.g., user adds a fact at the top, agent adds one at the bottom) without conflict.
- **Edge Case Only**: Mediation is only triggered if both actors edit the *exact same line* or the same frontmatter field simultaneously.

### The Mediation Flow
1. **Detection**: The sync loop identifies files in `Unmerged` state via `git ls-files -u`.
2. **Parsing**: The `GitConflictParser` (programmatic string matching) extracts the conflicting blocks:
    - **Ours (Mine)**: The local/agent version.
    - **Theirs (Yours)**: The remote/user version.
3. **Notification**: The Daemon sends an alert to the Matrix `#stream` room:
    > "Boss, we have a memory conflict on John's profile. Should I keep your edit, my edit, or merge both?"
4. **User Input**: The Matrix message includes Quick Reply chips:
    - `[Keep Mine]` (Agent version)
    - `[Keep Yours]` (User version)
    - `[Merge Both]` (Concatenate)
5. **Resolution**: Upon receiving the user's choice, the Daemon:
    - Rewrites the file with the chosen content.
    - Executes `git add` and `git commit -m "resolve: user mediated conflict"`.
    - Resumes the background sync loop.

## History Caching (Temporal Truth)

To enable high-performance history queries (e.g., "What did this file look like last month?") without slow Git operations, the system maintains a `vault_history` table in SQLite.

### Indexing Process
- The `VaultIndexer` periodically scans the Git log.
- Semantic commit messages are parsed to identify which facts were added or removed.
- The SQLite cache allows the UI to show a "Timeline" view of knowledge evolution instantly.

## Security and Sandboxing

- **Writer Isolation**: Even when an agent is running in a **Sysbox Sandbox**, it never has direct write access to the Git repository.
- **Proxy Pattern**: The sandboxed agent must request mutations via the Unix Domain Socket (UDS). The host-side Daemon validates the request before the `VaultWriter` performs the edit and commit.
- **Signed Commits**: (Future) All agent commits are GPG-signed by the Daemon's identity to ensure provenance.
