//! Rollback safety for the distillery pipeline.
//!
//! Implements transaction-like semantics for pipeline operations. If a later
//! stage fails, earlier side effects (partial Archive writes, note rewrites)
//! can be rolled back using a staging area pattern.
//! See `docs/design/distillery-pipeline.md` §Rollback Safety.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Staged write
// ---------------------------------------------------------------------------

/// A single file write that has been staged but not yet committed.
#[derive(Debug, Clone)]
pub struct StagedWrite {
    /// The target file path where the content will be written on commit.
    pub target_path: PathBuf,
    /// The new content to write.
    pub new_content: String,
    /// The original content of the file before modification (None if new file).
    pub original_content: Option<String>,
}

// ---------------------------------------------------------------------------
// Pipeline transaction
// ---------------------------------------------------------------------------

/// A transaction that accumulates staged writes and commits them atomically.
///
/// All file writes during the pipeline go through the transaction. On success,
/// `commit()` writes all staged content to disk. On failure, `rollback()`
/// restores all files to their original state.
///
/// This is not a database-level transaction but a file-system staging pattern
/// that provides best-effort atomicity for the pipeline.
#[derive(Debug)]
pub struct PipelineTransaction {
    /// Staged writes indexed by target path (string form for HashMap key).
    staged: HashMap<String, StagedWrite>,
    /// Whether the transaction has been committed.
    committed: bool,
    /// Whether the transaction has been rolled back.
    rolled_back: bool,
}

impl PipelineTransaction {
    /// Create a new empty transaction.
    pub fn new() -> Self {
        Self {
            staged: HashMap::new(),
            committed: false,
            rolled_back: false,
        }
    }

    /// Stage a write to a file.
    ///
    /// If the file already exists, its current content is captured for rollback.
    /// If the file does not exist, `original_content` is set to `None`.
    ///
    /// Multiple writes to the same path will overwrite the staged content but
    /// preserve the original content from the first staging.
    pub fn stage_write(
        &mut self,
        target_path: &Path,
        new_content: String,
    ) -> Result<(), RollbackError> {
        if self.committed {
            return Err(RollbackError::AlreadyCommitted);
        }
        if self.rolled_back {
            return Err(RollbackError::AlreadyRolledBack);
        }

        let key = target_path.to_string_lossy().to_string();

        // Only capture original content on the first staging of this path
        let original_content = if self.staged.contains_key(&key) {
            // Preserve the original from the first staging
            self.staged
                .get(&key)
                .and_then(|s| s.original_content.clone())
        } else {
            // Capture current file content (None if file doesn't exist)
            std::fs::read_to_string(target_path).ok()
        };

        self.staged.insert(
            key,
            StagedWrite {
                target_path: target_path.to_path_buf(),
                new_content,
                original_content,
            },
        );

        Ok(())
    }

    /// Stage a write that creates a new file (no original content to restore).
    pub fn stage_new_file(
        &mut self,
        target_path: &Path,
        content: String,
    ) -> Result<(), RollbackError> {
        if self.committed {
            return Err(RollbackError::AlreadyCommitted);
        }
        if self.rolled_back {
            return Err(RollbackError::AlreadyRolledBack);
        }

        let key = target_path.to_string_lossy().to_string();
        self.staged.insert(
            key,
            StagedWrite {
                target_path: target_path.to_path_buf(),
                new_content: content,
                original_content: None,
            },
        );

        Ok(())
    }

    /// Commit all staged writes to disk.
    ///
    /// Returns the number of files written. If any write fails, the
    /// transaction attempts to roll back all previously written files in
    /// this commit and returns an error.
    pub fn commit(&mut self) -> Result<usize, RollbackError> {
        if self.committed {
            return Err(RollbackError::AlreadyCommitted);
        }
        if self.rolled_back {
            return Err(RollbackError::AlreadyRolledBack);
        }

        let mut written: Vec<String> = Vec::new();

        for (key, staged) in &self.staged {
            // Ensure parent directory exists
            if let Some(parent) = staged.target_path.parent() {
                if !parent.exists() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        // Roll back what we've written so far
                        self.rollback_partial(&written);
                        return Err(RollbackError::IoError(format!(
                            "failed to create directory {}: {}",
                            parent.display(),
                            e
                        )));
                    }
                }
            }

            if let Err(e) = std::fs::write(&staged.target_path, &staged.new_content) {
                // Roll back what we've written so far
                self.rollback_partial(&written);
                return Err(RollbackError::IoError(format!(
                    "failed to write {}: {}",
                    staged.target_path.display(),
                    e
                )));
            }
            written.push(key.clone());
        }

        self.committed = true;
        info!(
            files_written = written.len(),
            "Pipeline transaction committed"
        );
        Ok(written.len())
    }

    /// Roll back all staged writes, restoring original file content.
    ///
    /// Returns the number of files successfully restored.
    pub fn rollback(&mut self) -> Result<usize, RollbackError> {
        if self.rolled_back {
            return Err(RollbackError::AlreadyRolledBack);
        }

        let mut restored = 0;

        for staged in self.staged.values() {
            match &staged.original_content {
                Some(original) => {
                    // Restore original content
                    if let Err(e) = std::fs::write(&staged.target_path, original) {
                        warn!(
                            path = %staged.target_path.display(),
                            error = %e,
                            "Failed to restore file during rollback"
                        );
                    } else {
                        restored += 1;
                    }
                }
                None => {
                    // File was newly created; remove it
                    if staged.target_path.exists() {
                        if let Err(e) = std::fs::remove_file(&staged.target_path) {
                            warn!(
                                path = %staged.target_path.display(),
                                error = %e,
                                "Failed to remove new file during rollback"
                            );
                        } else {
                            restored += 1;
                        }
                    }
                }
            }
        }

        self.rolled_back = true;
        info!(
            files_restored = restored,
            "Pipeline transaction rolled back"
        );
        Ok(restored)
    }

    /// Roll back a partial set of writes (used during failed commits).
    fn rollback_partial(&self, written_keys: &[String]) {
        for key in written_keys {
            if let Some(staged) = self.staged.get(key) {
                match &staged.original_content {
                    Some(original) => {
                        if let Err(e) = std::fs::write(&staged.target_path, original) {
                            warn!(
                                path = %staged.target_path.display(),
                                error = %e,
                                "Failed to restore file during partial rollback"
                            );
                        }
                    }
                    None => {
                        if staged.target_path.exists() {
                            let _ = std::fs::remove_file(&staged.target_path);
                        }
                    }
                }
            }
        }
    }

    /// Returns the number of staged writes.
    pub fn staged_count(&self) -> usize {
        self.staged.len()
    }

    /// Returns whether the transaction has been committed.
    pub fn is_committed(&self) -> bool {
        self.committed
    }

    /// Returns whether the transaction has been rolled back.
    pub fn is_rolled_back(&self) -> bool {
        self.rolled_back
    }

    /// Returns the staged content for a given path (for inspection/testing).
    pub fn get_staged(&self, path: &Path) -> Option<&StagedWrite> {
        let key = path.to_string_lossy().to_string();
        self.staged.get(&key)
    }
}

impl Default for PipelineTransaction {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the rollback system.
#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    #[error("transaction already committed")]
    AlreadyCommitted,

    #[error("transaction already rolled back")]
    AlreadyRolledBack,

    #[error("I/O error: {0}")]
    IoError(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_transaction_is_empty() {
        let txn = PipelineTransaction::new();
        assert_eq!(txn.staged_count(), 0);
        assert!(!txn.is_committed());
        assert!(!txn.is_rolled_back());
    }

    #[test]
    fn stage_and_commit_new_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("new-note.md");

        let mut txn = PipelineTransaction::new();
        txn.stage_new_file(&file_path, "# New Note\n\nContent here.".to_string())
            .expect("stage");

        assert_eq!(txn.staged_count(), 1);
        // File should NOT exist yet (only staged)
        assert!(!file_path.exists());

        let written = txn.commit().expect("commit");
        assert_eq!(written, 1);
        assert!(file_path.exists());
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read"),
            "# New Note\n\nContent here."
        );
    }

    #[test]
    fn stage_and_commit_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("existing.md");
        std::fs::write(&file_path, "Original content").expect("write");

        let mut txn = PipelineTransaction::new();
        txn.stage_write(&file_path, "Updated content".to_string())
            .expect("stage");

        // File still has original content (not yet committed)
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read"),
            "Original content"
        );

        txn.commit().expect("commit");
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read"),
            "Updated content"
        );
    }

    #[test]
    fn rollback_restores_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("note.md");
        std::fs::write(&file_path, "Original").expect("write");

        let mut txn = PipelineTransaction::new();
        txn.stage_write(&file_path, "Modified".to_string())
            .expect("stage");
        txn.commit().expect("commit");

        // File is now modified
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read"),
            "Modified"
        );

        // Create a new transaction that we will roll back
        let mut txn2 = PipelineTransaction::new();
        txn2.stage_write(&file_path, "Modified again".to_string())
            .expect("stage");
        txn2.commit().expect("commit");

        // Now roll back txn2 — should restore "Modified" (what was there before txn2)
        let restored = txn2.rollback().expect("rollback");
        assert_eq!(restored, 1);
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read"),
            "Modified"
        );
    }

    #[test]
    fn rollback_removes_new_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("new-file.md");

        let mut txn = PipelineTransaction::new();
        txn.stage_new_file(&file_path, "New content".to_string())
            .expect("stage");
        txn.commit().expect("commit");
        assert!(file_path.exists());

        let restored = txn.rollback().expect("rollback");
        assert_eq!(restored, 1);
        assert!(!file_path.exists());
    }

    #[test]
    fn rollback_without_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("note.md");
        std::fs::write(&file_path, "Original").expect("write");

        let mut txn = PipelineTransaction::new();
        txn.stage_write(&file_path, "Should not be written".to_string())
            .expect("stage");

        // Roll back without committing — file should remain unchanged
        let restored = txn.rollback().expect("rollback");
        // original_content was captured, but we didn't commit, so the file
        // was never modified. Rollback will write the original back, which
        // is a no-op in effect.
        assert_eq!(restored, 1);
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("read"),
            "Original"
        );
    }

    #[test]
    fn cannot_commit_twice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("note.md");

        let mut txn = PipelineTransaction::new();
        txn.stage_new_file(&file_path, "content".to_string())
            .expect("stage");
        txn.commit().expect("commit");

        let result = txn.commit();
        assert!(matches!(result, Err(RollbackError::AlreadyCommitted)));
    }

    #[test]
    fn cannot_rollback_twice() {
        let mut txn = PipelineTransaction::new();
        txn.rollback().expect("rollback");

        let result = txn.rollback();
        assert!(matches!(result, Err(RollbackError::AlreadyRolledBack)));
    }

    #[test]
    fn cannot_stage_after_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("note.md");

        let mut txn = PipelineTransaction::new();
        txn.commit().expect("commit empty");

        let result = txn.stage_write(&file_path, "late write".to_string());
        assert!(matches!(result, Err(RollbackError::AlreadyCommitted)));
    }

    #[test]
    fn cannot_stage_after_rollback() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("note.md");

        let mut txn = PipelineTransaction::new();
        txn.rollback().expect("rollback");

        let result = txn.stage_write(&file_path, "late write".to_string());
        assert!(matches!(result, Err(RollbackError::AlreadyRolledBack)));
    }

    #[test]
    fn multiple_staged_writes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file1 = tmp.path().join("a.md");
        let file2 = tmp.path().join("b.md");
        let file3 = tmp.path().join("c.md");

        std::fs::write(&file1, "Original A").expect("write");
        std::fs::write(&file2, "Original B").expect("write");

        let mut txn = PipelineTransaction::new();
        txn.stage_write(&file1, "New A".to_string()).expect("stage");
        txn.stage_write(&file2, "New B".to_string()).expect("stage");
        txn.stage_new_file(&file3, "New C".to_string())
            .expect("stage");

        assert_eq!(txn.staged_count(), 3);

        txn.commit().expect("commit");

        assert_eq!(std::fs::read_to_string(&file1).expect("r"), "New A");
        assert_eq!(std::fs::read_to_string(&file2).expect("r"), "New B");
        assert_eq!(std::fs::read_to_string(&file3).expect("r"), "New C");

        // Roll back all
        let restored = txn.rollback().expect("rollback");
        assert_eq!(restored, 3);

        assert_eq!(std::fs::read_to_string(&file1).expect("r"), "Original A");
        assert_eq!(std::fs::read_to_string(&file2).expect("r"), "Original B");
        assert!(!file3.exists());
    }

    #[test]
    fn overwrite_staged_preserves_original() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("note.md");
        std::fs::write(&file_path, "Original").expect("write");

        let mut txn = PipelineTransaction::new();
        txn.stage_write(&file_path, "First update".to_string())
            .expect("stage1");
        txn.stage_write(&file_path, "Second update".to_string())
            .expect("stage2");

        assert_eq!(txn.staged_count(), 1); // Same key, overwritten

        txn.commit().expect("commit");
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("r"),
            "Second update"
        );

        // Rollback should restore to Original, not "First update"
        txn.rollback().expect("rollback");
        assert_eq!(std::fs::read_to_string(&file_path).expect("r"), "Original");
    }

    #[test]
    fn get_staged_returns_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("note.md");

        let mut txn = PipelineTransaction::new();
        txn.stage_new_file(&file_path, "staged content".to_string())
            .expect("stage");

        let staged = txn.get_staged(&file_path).expect("should find staged");
        assert_eq!(staged.new_content, "staged content");
        assert!(staged.original_content.is_none());
    }

    #[test]
    fn commit_creates_parent_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("sub").join("dir").join("note.md");

        let mut txn = PipelineTransaction::new();
        txn.stage_new_file(&file_path, "deep content".to_string())
            .expect("stage");

        txn.commit().expect("commit");
        assert!(file_path.exists());
        assert_eq!(
            std::fs::read_to_string(&file_path).expect("r"),
            "deep content"
        );
    }

    #[test]
    fn default_creates_new_transaction() {
        let txn = PipelineTransaction::default();
        assert_eq!(txn.staged_count(), 0);
        assert!(!txn.is_committed());
    }
}
