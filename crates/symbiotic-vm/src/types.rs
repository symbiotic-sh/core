//! Core types for VM sandboxing.

use serde::{Deserialize, Serialize};

/// Unique identifier for a VM instance.
pub type VmId = String;

/// VM lifecycle states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Creating,
    Running,
    Stopped,
    Destroying,
    Failed,
}

/// Resource limits for a VM instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmResources {
    /// Number of CPU cores.
    pub cpus: u32,
    /// RAM in megabytes.
    pub memory_mb: u32,
    /// Disk size in megabytes.
    pub disk_mb: u32,
    /// Maximum execution time in seconds (VM is killed after this).
    pub timeout_secs: u64,
}

impl Default for VmResources {
    fn default() -> Self {
        Self {
            cpus: 2,
            memory_mb: 2048,
            disk_mb: 8192,
            timeout_secs: 600,
        }
    }
}

/// Network policy for a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPolicy {
    /// If true, all network access is blocked.
    pub deny_all: bool,
    /// Allowed egress domains (only used if deny_all is false).
    pub allowed_domains: Vec<String>,
    /// Allowed egress ports (only used if deny_all is false).
    pub allowed_ports: Vec<u16>,
    /// DNS servers to use (empty = no DNS).
    pub dns_servers: Vec<String>,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            deny_all: true,
            allowed_domains: Vec::new(),
            allowed_ports: Vec::new(),
            dns_servers: Vec::new(),
        }
    }
}

/// Request to create a VM instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmCreateRequest {
    /// Image to use as the base.
    pub image: String,
    /// Resource limits.
    pub resources: VmResources,
    /// Network policy.
    pub network: NetworkPolicy,
    /// Files to inject into the VM before execution.
    pub inject_files: Vec<FileTransfer>,
    /// Agent ID requesting the VM.
    pub requesting_agent: String,
    /// Purpose label for audit logging.
    pub purpose: String,
    /// Environment variables injected into the container process.
    pub env: Vec<String>,
    /// Host paths bind-mounted into the VM container.
    pub mounts: Vec<BindMount>,
}

/// A host path bind-mounted into the VM container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindMount {
    /// Absolute path on the host.
    pub host_path: String,
    /// Mount path inside the VM/container.
    pub vm_path: String,
    /// Whether the mount should be read-only.
    pub read_only: bool,
}

/// A file to transfer between host and VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTransfer {
    /// Path on the host (for inject) or VM (for extract).
    pub host_path: String,
    /// Path inside the VM.
    pub vm_path: String,
    /// Transfer direction.
    pub direction: TransferDirection,
}

/// Direction of file transfer between host and VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    HostToVm,
    VmToHost,
}

/// A running VM instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInstance {
    pub id: VmId,
    pub image: String,
    pub state: VmState,
    pub resources: VmResources,
    pub network: NetworkPolicy,
    pub requesting_agent: String,
    pub purpose: String,
    pub created_at: u64,
    pub started_at: Option<u64>,
}

/// Result of executing a command in a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Audit entry for VM operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmAuditEntry {
    pub timestamp: u64,
    pub vm_id: VmId,
    pub agent_id: String,
    pub action: VmAction,
    pub details: String,
}

/// Actions recorded in the VM audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmAction {
    Created,
    Started,
    CommandExecuted,
    FileInjected,
    FileExtracted,
    Stopped,
    Destroyed,
    TimedOut,
}

/// A VM image specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmImageSpec {
    /// Image identifier (e.g., "symbiotic-sandbox-v1").
    pub name: String,
    /// Base OS image.
    pub base: BaseImage,
    /// Tools to install on top of base.
    pub tools: Vec<ToolSpec>,
    /// Default resource limits for VMs using this image.
    pub default_resources: VmResources,
    /// Default network policy.
    pub default_network: NetworkPolicy,
}

/// Base image type for a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseImage {
    MacOS { version: String },
    Ubuntu { version: String },
    Alpine { version: String },
    Custom { url: String },
}

/// A tool to install in a VM image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub version: Option<String>,
    pub install_command: String,
}
