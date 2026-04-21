# Disaster Recovery Strategy: The "Sovereign Soul" Protocol


**Status**: Architectural Pillar (target state)
**Scope**: Host-wide Failure, Nucleus Corruption, Data Loss

**Implementation note (2026-04-20):** Reconciler + Archive + Vault primitives exist, but two load-bearing pieces of this protocol are not shipped: the Sovereign Sync off-site backup mechanism (no automated archive/vault backup/restore code path), and the `com.symbiotic.managed=true` label-based reattach routine described in §6 (the label is not referenced in daemon bootstrap). Treat this as target-state design until those pieces land.

This document specifies how to recover a Symbiotic system from zero, assuming the host VPS is completely lost or corrupted. 

The recovery protocol relies on the **Declarative Control Plane**—because the "Brain" of the system lives in plain Markdown and a portable SQLite Vault, the "Nucleus" (Daemon) is a disposable motor that can be replaced at any time.

---

## 1. The Recovery Assets (What is needed to restore)

To recover the system, the user must have access to two off-site backup volumes (the **Sovereign Sync**):

1.  **The Archive (`knowledge-base/`)**: All `identity/`, `operations/`, `ledger/`, `threads/`, `archive/`, and `library/` Markdown files.
2.  **The Vault (`data/vault/`)**: The encrypted `vault.db` containing all identity, secrets, and OAuth tokens.

*Note: Ephemeral agent swarms are not backed up. They are re-initialized by the Nucleus upon recovery from the last known goal manifest state.*

---

## 2. Phase 1: Clean Host Provisioning

1.  **Hardware/VPS**: Provision a fresh Linux host (Ubuntu 24.04+ recommended).
2.  **Sysbox**: Install the `sysbox-ce` runtime (mandatory for the Execution Plane).
    ```bash
    wget https://github.com/nestybox/sysbox/releases/download/v0.6.4/sysbox-ce_0.6.4-0.ubuntu24.04_amd64.deb
    sudo apt install ./sysbox-ce_*.deb
    ```
3.  **Docker**: Install standard Docker engine.
4.  **Nucleus**: Download the latest static `symbiotic-daemon` binary.

---

## 3. Phase 2: Restoring the Soul (Data Ingress)

1.  **Restore Archive**: Clone or copy the `knowledge-base/` backup to `/var/symbiotic/knowledge-base/`.
2.  **Restore Vault**: Copy the `vault.db` to `/var/symbiotic/data/vault/vault.db`.
3.  **Configure Systemd**: Create the `symbiotic-daemon.service` pointing to these paths.
    ```ini
    [Service]
    ExecStart=/usr/bin/symbiotic-daemon \
      --archive-path /var/symbiotic/knowledge-base \
      --vault-path /var/symbiotic/data/vault
    ```

---

## 4. Phase 3: The Cold Boot Reconciliation

When the Nucleus starts for the first time on the new host:

1.  **Re-identification**: The Nucleus reads `identity/SOUL.md` and `identity/preferences.md`. It now knows its identity and its LLM API keys (decrypted from the restored Vault).
2.  **Goal Discovery**: The **Reconciler** scans `operations/goals/`.
    *   It sees `goal: algorithmic-trading` with `phase: implementation`.
    *   It checks the host's Docker engine and sees **zero active sandboxes**.
3.  **Task Re-Initialization**:
    *   The Nucleus detects a "mismatch" (Goal says Implementation, but no runtime exists).
    *   It automatically creates a fresh Git Swarm Repo.
    *   It spawns a new **Agent Sandbox** (Sysbox) and asks the agent to resume from the last state described in the Goal's Markdown plan.

---

## 5. Phase 4: Network & Transport Recovery

1.  **Matrix Re-Link**: The Nucleus uses its restored Matrix identity from the Vault to re-connect to the home server.
2.  **UI Sync**: The Flutter app (on the user's phone) automatically detects the new Nucleus connection via Matrix.
3.  **State Catchup**: The app displays the new activity stream showing the Reconciler's cold boot actions.

---

## 6. Key Fail-Safes

### What if a Sandbox is "Orphaned" but the Host persists?
If the Nucleus crashes but the VPS is fine, the Sysbox containers stay running.
*   **The Label Gate**: All Symbiotic-managed containers are labeled with `com.symbiotic.managed=true`.
*   **Re-attachment**: On startup, the Nucleus runs `docker ps --filter "label=com.symbiotic.managed=true"`.
*   **Heartbeat Check**: It attempts to connect to the agent's Unix Socket. If it fails, it calls `docker rm -f` on the orphan and spawns a clean replacement to ensure no "Zombie" agents are running.

### What if the Vault is lost, but the Archive is safe?
*   **Identity Re-boot**: You must generate a new Matrix ID and provide new LLM API keys.
*   **Re-claim**: The Nucleus will re-discover your goals in the Archive, but it won't be able to "Act" (e.g., clone a repo) until you re-authorize the credentials in the new Vault. 

---

## 7. Summary of Resilience

| Failure Type | Recovery Speed | Data Loss |
| :--- | :--- | :--- |
| Process Crash | Instant (systemd) | Zero |
| Binary Corruption | Seconds (re-download) | Zero |
| **Host Wipe** | **Minutes (restore backup)** | **Only in-memory LLM state** |
| Backup Loss | Impossible | Total System Death |
