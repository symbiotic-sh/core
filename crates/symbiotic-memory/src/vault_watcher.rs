//! Filesystem watcher for the Vault-as-Truth architecture.
//!
//! Detects when vault Markdown files change (e.g., user edits in Obsidian)
//! and triggers incremental re-indexing via [`VaultIndexer`].
//!
//! Uses a polling approach with `tokio::time::interval()` — consistent with
//! the daemon's existing reconciler loop patterns (see `control_plane.rs`).
//!
//! See `docs/design/vault-as-truth.md` for the architecture.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::vault_indexer::{IndexStats, VaultIndexer};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for the vault watcher.
#[derive(Debug, Clone)]
pub struct VaultWatcherConfig {
    /// Root path to the vault (`knowledge-base/`).
    pub vault_root: PathBuf,
    /// How often to poll for changes, in seconds. Default: 30.
    pub poll_interval_secs: u64,
}

impl Default for VaultWatcherConfig {
    fn default() -> Self {
        Self {
            vault_root: PathBuf::from("knowledge-base"),
            poll_interval_secs: 30,
        }
    }
}

/// Handle for controlling a running vault watcher.
pub struct VaultWatcherHandle {
    cancel: tokio::sync::watch::Sender<bool>,
}

impl VaultWatcherHandle {
    /// Stop the watcher gracefully.
    pub fn stop(&self) {
        let _ = self.cancel.send(true);
    }
}

/// Callback type for when indexing completes.
pub type OnIndexed = Box<dyn Fn(&IndexStats) + Send + Sync>;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start a vault watcher that polls for file changes on an interval.
///
/// Returns a handle that can be used to stop the watcher.
///
/// The watcher runs as a background tokio task. On each tick:
/// 1. Calls `VaultIndexer::update()` to detect changed files
/// 2. If files were re-indexed, calls the optional `on_indexed` callback
///
/// The watcher stops when the handle is dropped or `stop()` is called.
pub fn start_watcher(
    indexer: Arc<Mutex<VaultIndexer>>,
    config: VaultWatcherConfig,
    on_indexed: Option<OnIndexed>,
) -> VaultWatcherHandle {
    let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(config.poll_interval_secs));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let idx = indexer.lock().await;
                    match idx.update(&config.vault_root).await {
                        Ok(stats) => {
                            if stats.files_indexed > 0 || stats.files_removed > 0 {
                                tracing::info!(
                                    indexed = stats.files_indexed,
                                    removed = stats.files_removed,
                                    unchanged = stats.files_unchanged,
                                    "Vault watcher: re-indexed {} file(s)",
                                    stats.files_indexed
                                );
                                if let Some(ref cb) = on_indexed {
                                    cb(&stats);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Vault watcher: indexing failed: {}", e);
                        }
                    }
                }
                _ = cancel_rx.changed() => {
                    tracing::info!("Vault watcher: stopped");
                    break;
                }
            }
        }
    });

    VaultWatcherHandle { cancel: cancel_tx }
}

/// Perform a one-shot scan (useful for daemon startup or CLI `reindex` command).
///
/// This is a convenience wrapper around `VaultIndexer::update()`.
pub async fn scan_once(
    indexer: &VaultIndexer,
    vault_root: &Path,
) -> Result<IndexStats, crate::vault_indexer::VaultIndexError> {
    indexer.update(vault_root).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteMemoryStore;

    async fn setup() -> (SqliteMemoryStore, Arc<Mutex<VaultIndexer>>) {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();
        let indexer = VaultIndexer::new(&store);
        indexer.initialize().await.unwrap();
        (store, Arc::new(Mutex::new(indexer)))
    }

    fn write_entity(vault: &Path, filename: &str, content: &str) {
        let dir = vault.join("ledger/tools/test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(filename), content).unwrap();
    }

    const TEST_ENTITY: &str = "\
---
id: test
type: concept
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Test

## Facts
- A test fact [source: manual]
";

    #[tokio::test]
    async fn scan_once_detects_new_files() {
        let (_store, indexer) = setup().await;
        let vault = tempfile::TempDir::new().unwrap();

        write_entity(vault.path(), "test.md", TEST_ENTITY);

        let idx = indexer.lock().await;
        let stats = scan_once(&idx, vault.path()).await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.entities_upserted, 1);
    }

    #[tokio::test]
    async fn scan_once_skips_unchanged() {
        let (_store, indexer) = setup().await;
        let vault = tempfile::TempDir::new().unwrap();

        write_entity(vault.path(), "test.md", TEST_ENTITY);

        let idx = indexer.lock().await;
        scan_once(&idx, vault.path()).await.unwrap();

        // Second scan — unchanged
        let stats = scan_once(&idx, vault.path()).await.unwrap();
        assert_eq!(stats.files_indexed, 0);
        assert_eq!(stats.files_unchanged, 1);
    }

    #[tokio::test]
    async fn watcher_can_be_stopped() {
        let (_store, indexer) = setup().await;
        let vault = tempfile::TempDir::new().unwrap();

        let config = VaultWatcherConfig {
            vault_root: vault.path().to_path_buf(),
            poll_interval_secs: 1,
        };

        let handle = start_watcher(indexer, config, None);

        // Let it run briefly
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Stop should not panic
        handle.stop();

        // Give it a moment to process the stop signal
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn watcher_calls_callback_on_changes() {
        let (_store, indexer) = setup().await;
        let vault = tempfile::TempDir::new().unwrap();

        write_entity(vault.path(), "test.md", TEST_ENTITY);

        let callback_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_flag = Arc::clone(&callback_called);

        let config = VaultWatcherConfig {
            vault_root: vault.path().to_path_buf(),
            poll_interval_secs: 1,
        };

        let on_indexed: OnIndexed = Box::new(move |stats| {
            if stats.files_indexed > 0 {
                callback_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let handle = start_watcher(indexer, config, Some(on_indexed));

        // Wait for at least one tick
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        handle.stop();

        assert!(
            callback_called.load(std::sync::atomic::Ordering::SeqCst),
            "callback should have been called"
        );
    }

    #[test]
    fn default_config_values() {
        let config = VaultWatcherConfig::default();
        assert_eq!(config.poll_interval_secs, 30);
    }
}
