pub mod intake;
pub mod memory_space;
pub mod protocol;
pub mod temporal;
pub mod trace;
pub mod types;

pub use memory_space::{MemorySpace, MemorySpaceParseError};

use serde::{Deserialize, Serialize};

/// Content sensitivity level for routing decisions.
///
/// Determines which providers (local vs cloud) may process the content.
/// Used by the provider routing layer to enforce data locality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Shareable,
    Restricted,
    Private,
}

/// Return the current Unix timestamp in seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock must be after epoch")
        .as_secs()
}

/// Set file permissions to the given mode (e.g. `0o600`).
///
/// On non-Unix platforms this is a no-op.
pub fn harden_file_permissions(path: &std::path::Path, mode: u32) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(mode);
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Set directory permissions to the given mode (e.g. `0o700`).
///
/// On non-Unix platforms this is a no-op.
pub fn harden_dir_permissions(path: &std::path::Path, mode: u32) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(mode);
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
