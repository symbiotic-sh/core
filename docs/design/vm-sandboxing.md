# VM Sandboxing — Lume Backend (Planned)

**Task**: T62 (Lume VM Sandboxing)
**Depends on**: current Sysbox implementation (see `docs/architecture/vm-sandboxing.md`)
**Not on immediate roadmap** — post-T1XX follow-up once the core sub-goal / firewall / source-archeology work is in a steady state.

## Scope

This doc covers one thing: adding a `LumeBackend` that implements the existing `VmBackend` trait on macOS hosts. It does **not** re-design the core API — `VmManager`, `VmCreateRequest`, `FileBridge`, capability gating, the audit log, and the hardening floor all stay exactly as they are today.

In particular:

- We are not adding QEMU/KVM. Sysbox already covers Linux deployments.
- We are not changing the file-bridge contract, the scope-to-trust map, or the `VmAuditEntry` shape.
- We are not redesigning images or introducing a `VmImageSpec` registry.

## Rationale

`SysboxBackend` gives containers VM-like isolation via user-namespacing and nested-Docker. That is more than enough for Linux server deployments, but on macOS developer hosts it has two problems:

1. **No native sysbox-runc on macOS.** `SysboxBackend::new()` today falls back to plain Docker on Desktop (runtime = `runc`), which loses the user-namespace isolation Sysbox provides on Linux.
2. **Container-level boundary, not VM-level.** For workloads where we want to tolerate a kernel-level container escape — untrusted code execution, third-party tool evaluation, credential-adjacent flows — a hypervisor boundary is the right floor.

Lume ([trycua/cua](https://github.com/trycua/cua)) wraps Apple's Virtualization Framework and is already a common tool in the macOS VM ecosystem. Shelling out to its CLI is a small, well-understood surface.

## Backend sketch

```rust
// submodules/runtime/crates/symbiotic-vm/src/backends/lume.rs (planned)

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;

use crate::backend::VmBackend;
use crate::types::{ExecResult, FileTransfer, VmCreateRequest, VmInstance, VmState};

pub struct LumeBackend {
    /// Path to the `lume` CLI binary.
    lume_binary: PathBuf,
    /// Directory for VM storage (used for `--data-dir` style flags).
    vm_storage: PathBuf,
}

impl LumeBackend {
    pub fn new(lume_binary: PathBuf, vm_storage: PathBuf) -> Result<Self> { /* ... */ }
    pub async fn lume_available() -> bool { /* probe `lume --version` */ }
}

#[async_trait]
impl VmBackend for LumeBackend {
    async fn create(&self, id: &str, request: &VmCreateRequest) -> Result<VmInstance> {
        // lume create --name {id} --cpu {cpus} --memory {memory_mb}
        //             --disk {disk_mb} --image {request.image}
        // Map request.env + request.mounts into --env / --volume flags.
        // Return VmInstance with VmState::Creating.
    }

    async fn start(&self, id: &str) -> Result<()>      { /* lume start --name {id} */ }
    async fn exec(&self, id: &str, cmd: &str) -> Result<ExecResult> {
        // lume exec --name {id} -- sh -c {wrapped_command}
        // Reuse the same __SYMBIOTIC_EXEC_EXIT_CODE__ marker protocol as SysboxBackend
        // so VmManager::exec() behavior is identical.
    }
    async fn stop(&self, id: &str) -> Result<()>       { /* lume stop --name {id} */ }
    async fn destroy(&self, id: &str) -> Result<()>    { /* lume delete --name {id} --force */ }

    async fn transfer(&self, id: &str, transfer: &FileTransfer) -> Result<()> {
        // HostToVm: lume cp {host_path} {id}:{vm_path}
        // VmToHost: lume cp {id}:{vm_path} {host_path}
        // FileBridge has already rewritten host_path to the vm-output directory.
    }

    async fn get_state(&self, id: &str) -> Result<VmState> {
        // lume list --format json, filter by id, map status -> VmState.
    }
}
```

## What stays the same

- **Trait**: `impl VmBackend for LumeBackend` plugs into the existing `VmManager` with no manager-side changes.
- **Request types**: `VmCreateRequest`, `BindMount`, `NetworkPolicy`, `VmResources` are all backend-agnostic already.
- **File bridge**: `VmManager::transfer_file` calls `FileBridge::validate` before delegating to the backend. Lume never sees unvalidated paths.
- **Audit log**: `VmManager` writes the `VmAuditEntry` before/after backend calls as today.
- **Capability gating**: scope-to-trust is still enforced in the manager.
- **Exec protocol**: reuse `wrap_exec_command` + `extract_exit_code_marker` from `backends/sysbox.rs` (candidate to move to a shared helper module when the second backend lands).

## What needs mapping

| Concern | Sysbox (today) | Lume (planned) |
|---------|----------------|----------------|
| Isolation boundary | Container + user-namespace | Hypervisor (Apple Virtualization Framework) |
| CPU/memory caps | `nano_cpus`, `memory` on `HostConfig` | `--cpu`, `--memory` CLI flags |
| Read-only rootfs | `readonly_rootfs: true` | VM base image is immutable; writable paths via `--volume` |
| Cap drops / no-new-privileges | Docker `cap_drop`, `security_opt` | N/A — hypervisor boundary makes these moot |
| Network deny-all | `network_mode: "none"` | `--network none` (or per-VM bridge disable, TBD per Lume flags) |
| Bind mounts | `HostConfig.binds` | `--volume host:vm[:ro]` |
| Env vars | `Config.env` | `--env KEY=value` |
| PID cap | `pids_limit: 256` | N/A — separate kernel |

The mapping is straightforward. The main open question is how Lume exposes network policy granularity; for a first pass, binary deny-all vs. default-bridge parity with `SysboxBackend` is enough.

## Selecting a backend at runtime

Today `services/symbiotic-daemon/src/lib.rs` constructs `SysboxBackend::new()` directly. Once Lume lands, the daemon bootstrap picks a backend based on platform + availability:

```rust
// Sketch only — not a commitment to this exact shape.
let backend: Box<dyn VmBackend> = if cfg!(target_os = "macos")
    && LumeBackend::lume_available().await
{
    Box::new(LumeBackend::new(lume_path, vm_storage)?)
} else {
    Box::new(SysboxBackend::new()?)
};
```

A `SYMBIOTIC_VM_BACKEND=sysbox|lume` override follows the same pattern as the existing `SYMBIOTIC_VM_USE_SYSBOX` flag for forcing a choice in tests and CI.

## Testing plan

- Unit-test CLI arg construction as pure functions (mirroring `build_host_config` in `backends/sysbox.rs`). No `lume` binary required.
- Integration tests gated behind `#[ignore]` + an availability probe, the same pattern as `distillery_job_exec_extract_and_persist_flow_runs_on_sysbox_backend` in `services/symbiotic-daemon/src/swarm_server.rs`.
- Reuse the existing `VmManager` test suite as-is — any trait-level regressions show up in the same tests.

## Explicit non-goals

- Not replacing `SysboxBackend`. Linux server deployments keep using it.
- Not building a VM image registry, a `VmImageSpec` loader, or YAML image configs. Callers pass image strings today; that does not need to change for the backend to land.
- Not adding a QEMU backend. If Linux-host VM-level isolation becomes a need, revisit then.
- Not changing the audit log format, scope names, or trust mapping.

## Related

- `docs/architecture/vm-sandboxing.md` — current Sysbox implementation and the trait this doc plugs into.
- `docs/architecture/agent-orchestration.md` — capability tokens and trust levels.
- [trycua/cua](https://github.com/trycua/cua) — Lume project.
