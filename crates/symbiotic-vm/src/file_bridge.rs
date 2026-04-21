//! File Bridge: controlled file transfer between host and VM.
//!
//! Enforces allowlisted paths and size limits for secure file exchange.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::types::{FileTransfer, TransferDirection};

/// Maximum file transfer size: 100 MB.
const MAX_TRANSFER_BYTES: u64 = 100 * 1024 * 1024;

/// File Bridge validates and controls file transfers between host and VM.
pub struct FileBridge {
    /// Allowed source directories for host-to-VM transfers.
    allowed_inject_roots: Vec<PathBuf>,
    /// Base directory for VM-to-host extractions: data/runtime/vm-output/{vm_id}/
    output_base: PathBuf,
    /// Maximum transfer size in bytes.
    max_bytes: u64,
}

impl FileBridge {
    /// Create a new FileBridge.
    ///
    /// `project_root` - the project working directory (inject allowed from here).
    /// `data_dir` - the data directory (inject allowed from here, extract goes to vm-output/).
    pub fn new(project_root: &Path, data_dir: &Path) -> Self {
        Self {
            allowed_inject_roots: vec![project_root.to_path_buf(), data_dir.to_path_buf()],
            output_base: data_dir.join("runtime").join("vm-output"),
            max_bytes: MAX_TRANSFER_BYTES,
        }
    }

    /// Validate a file transfer request.
    ///
    /// For HostToVm: host_path must be under an allowed inject root.
    /// For VmToHost: host_path must be a safe relative path under
    /// output_base/{vm_id}/ and may preserve nested subdirectories.
    pub fn validate(&self, vm_id: &str, transfer: &FileTransfer) -> Result<PathBuf> {
        if vm_id.contains("..") || vm_id.contains('/') || vm_id.contains('\\') {
            return Err(anyhow!("vm_id contains path traversal characters"));
        }
        match transfer.direction {
            TransferDirection::HostToVm => {
                let host_path = Path::new(&transfer.host_path);
                let canonical_host = std::fs::canonicalize(host_path).with_context(|| {
                    format!("file bridge: host path '{}' must exist", transfer.host_path)
                })?;
                let allowed = self
                    .allowed_inject_roots
                    .iter()
                    .filter_map(|root| std::fs::canonicalize(root).ok())
                    .any(|canonical_root| canonical_host.starts_with(&canonical_root));
                if !allowed {
                    return Err(anyhow!(
                        "file bridge: host path '{}' is not under any allowed inject root",
                        transfer.host_path
                    ));
                }
                Ok(canonical_host)
            }
            TransferDirection::VmToHost => {
                let extract_dir = self.output_base.join(vm_id);
                let relative = Path::new(&transfer.host_path);
                if relative.is_absolute() {
                    return Err(anyhow!("extract host_path must be relative"));
                }
                if relative
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
                {
                    return Err(anyhow!(
                        "extract host_path must not contain traversal components"
                    ));
                }
                let host_path = extract_dir.join(relative);
                Ok(host_path)
            }
        }
    }

    /// Check if a transfer size is within limits.
    pub fn check_size(&self, size_bytes: u64) -> Result<()> {
        if size_bytes > self.max_bytes {
            return Err(anyhow!(
                "file bridge: transfer size {} bytes exceeds limit of {} bytes",
                size_bytes,
                self.max_bytes
            ));
        }
        Ok(())
    }

    /// Get the extraction output directory for a VM.
    pub fn output_dir(&self, vm_id: &str) -> Result<PathBuf> {
        if vm_id.contains("..") || vm_id.contains('/') || vm_id.contains('\\') {
            return Err(anyhow!("vm_id contains path traversal characters"));
        }
        Ok(self.output_base.join(vm_id))
    }
}
