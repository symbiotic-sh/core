//! VM backend trait for platform-specific VM providers.

use anyhow::Result;
use async_trait::async_trait;

use crate::types::{ExecResult, FileTransfer, VmCreateRequest, VmInstance, VmState};

/// Backend trait for VM providers.
/// Implemented separately for Lume (macOS) and QEMU/KVM (Linux).
#[async_trait]
pub trait VmBackend: Send + Sync {
    /// Create a new VM instance from a request.
    async fn create(&self, id: &str, request: &VmCreateRequest) -> Result<VmInstance>;

    /// Start a VM instance.
    async fn start(&self, id: &str) -> Result<()>;

    /// Execute a command inside the VM.
    async fn exec(&self, id: &str, command: &str) -> Result<ExecResult>;

    /// Stop a VM instance (graceful shutdown).
    async fn stop(&self, id: &str) -> Result<()>;

    /// Destroy a VM instance (remove all resources).
    async fn destroy(&self, id: &str) -> Result<()>;

    /// Transfer a file between host and VM.
    async fn transfer(&self, id: &str, transfer: &FileTransfer) -> Result<()>;

    /// Get the current state of a VM.
    async fn get_state(&self, id: &str) -> Result<VmState>;
}
