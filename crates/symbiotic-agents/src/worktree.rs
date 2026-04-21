//! Git worktree isolation for swarm agents.
//!
//! Each agent in a swarm operates in its own git worktree, providing full
//! filesystem isolation. This prevents concurrent agents from interfering with
//! each other's working copy.
//!
//! Worktrees are created at `.claude/worktrees/{agent-id}/` relative to the
//! repository root, with each agent getting a dedicated branch
//! `agent/{agent-id}`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::process::Command;
use tokio::sync::Mutex;

/// Errors that can occur during worktree operations.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// Git is not available on the system.
    #[error("git is not available: {0}")]
    GitNotAvailable(String),

    /// The specified path is not a git repository.
    #[error("not a git repository: {0}")]
    NotAGitRepo(PathBuf),

    /// A worktree for this agent already exists.
    #[error("worktree already exists for agent: {0}")]
    AlreadyExists(String),

    /// A worktree for this agent was not found.
    #[error("worktree not found for agent: {0}")]
    NotFound(String),

    /// A git command failed.
    #[error("git command failed: {0}")]
    GitCommand(String),

    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Information about a managed worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Agent ID that owns this worktree.
    pub agent_id: String,
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Branch name used by this worktree.
    pub branch: String,
    /// The base branch this worktree was created from.
    pub base_branch: String,
}

/// Manages git worktree lifecycle for swarm agents.
///
/// Each agent gets an isolated worktree at `.claude/worktrees/{agent-id}/`
/// with a dedicated branch `agent/{agent-id}`.
#[derive(Clone)]
pub struct WorktreeManager {
    /// Root of the git repository.
    repo_root: PathBuf,
    /// Active worktrees tracked in memory.
    active: Arc<Mutex<HashMap<String, WorktreeInfo>>>,
}

impl WorktreeManager {
    /// Create a new `WorktreeManager` for the given repository root.
    ///
    /// Validates that git is available and the path is a git repository.
    pub async fn new(repo_root: impl Into<PathBuf>) -> Result<Self, WorktreeError> {
        let repo_root = repo_root.into();

        // Verify git is available
        let git_check = Command::new("git")
            .arg("--version")
            .output()
            .await
            .map_err(|e| WorktreeError::GitNotAvailable(e.to_string()))?;

        if !git_check.status.success() {
            return Err(WorktreeError::GitNotAvailable(
                "git --version returned non-zero".to_string(),
            ));
        }

        // Verify the path is a git repo
        let repo_check = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&repo_root)
            .output()
            .await
            .map_err(WorktreeError::Io)?;

        if !repo_check.status.success() {
            return Err(WorktreeError::NotAGitRepo(repo_root));
        }

        Ok(Self {
            repo_root,
            active: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Create a new worktree for the given agent.
    ///
    /// Creates the worktree at `.claude/worktrees/{agent_id}/` with branch
    /// `agent/{agent_id}` based on `base_branch`.
    ///
    /// Returns the absolute path to the worktree.
    pub async fn create_worktree(
        &self,
        agent_id: &str,
        base_branch: &str,
    ) -> Result<PathBuf, WorktreeError> {
        let mut active = self.active.lock().await;

        if active.contains_key(agent_id) {
            return Err(WorktreeError::AlreadyExists(agent_id.to_string()));
        }

        let worktree_path = self.worktree_path(agent_id);
        let branch_name = self.branch_name(agent_id);

        // Resolve the base commit (branch or ref)
        let base_output = Command::new("git")
            .args(["rev-parse", base_branch])
            .current_dir(&self.repo_root)
            .output()
            .await
            .map_err(WorktreeError::Io)?;

        if !base_output.status.success() {
            let stderr = String::from_utf8_lossy(&base_output.stderr);
            return Err(WorktreeError::GitCommand(format!(
                "failed to resolve base branch '{base_branch}': {stderr}"
            )));
        }

        // Create the worktree with a new branch
        let add_output = Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                &branch_name,
                worktree_path
                    .to_str()
                    .ok_or_else(|| WorktreeError::GitCommand("non-UTF8 path".to_string()))?,
                base_branch,
            ])
            .current_dir(&self.repo_root)
            .output()
            .await
            .map_err(WorktreeError::Io)?;

        if !add_output.status.success() {
            let stderr = String::from_utf8_lossy(&add_output.stderr);
            return Err(WorktreeError::GitCommand(format!(
                "git worktree add failed: {stderr}"
            )));
        }

        let info = WorktreeInfo {
            agent_id: agent_id.to_string(),
            path: worktree_path.clone(),
            branch: branch_name,
            base_branch: base_branch.to_string(),
        };
        active.insert(agent_id.to_string(), info);

        tracing::info!(
            agent_id = agent_id,
            path = %worktree_path.display(),
            "created worktree for agent"
        );

        Ok(worktree_path)
    }

    /// Remove a worktree and its branch for the given agent.
    ///
    /// Removes the worktree directory and deletes the `agent/{agent_id}` branch.
    /// Logs errors but does not fail if cleanup is partial.
    pub async fn cleanup_worktree(&self, agent_id: &str) -> Result<(), WorktreeError> {
        let mut active = self.active.lock().await;

        let info = active
            .remove(agent_id)
            .ok_or_else(|| WorktreeError::NotFound(agent_id.to_string()))?;

        // Remove the worktree
        let remove_output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&info.path)
            .current_dir(&self.repo_root)
            .output()
            .await;

        match remove_output {
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    agent_id = agent_id,
                    error = %stderr,
                    "failed to remove worktree via git, attempting manual cleanup"
                );
                // Fallback: try manual directory removal and prune
                if info.path.exists() {
                    if let Err(e) = tokio::fs::remove_dir_all(&info.path).await {
                        tracing::warn!(
                            agent_id = agent_id,
                            error = %e,
                            "failed to manually remove worktree directory"
                        );
                    }
                }
                // Prune stale worktree entries
                let _ = Command::new("git")
                    .args(["worktree", "prune"])
                    .current_dir(&self.repo_root)
                    .output()
                    .await;
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = agent_id,
                    error = %e,
                    "failed to execute git worktree remove"
                );
            }
            Ok(_) => {}
        }

        // Delete the branch
        let branch_output = Command::new("git")
            .args(["branch", "-D", &info.branch])
            .current_dir(&self.repo_root)
            .output()
            .await;

        match branch_output {
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    agent_id = agent_id,
                    branch = %info.branch,
                    error = %stderr,
                    "failed to delete agent branch"
                );
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = agent_id,
                    error = %e,
                    "failed to execute git branch delete"
                );
            }
            Ok(_) => {}
        }

        tracing::info!(agent_id = agent_id, "cleaned up worktree for agent");

        Ok(())
    }

    /// List all active worktrees managed by this instance.
    pub async fn list_worktrees(&self) -> Vec<WorktreeInfo> {
        let active = self.active.lock().await;
        active.values().cloned().collect()
    }

    /// Get the worktree info for a specific agent.
    pub async fn get_worktree(&self, agent_id: &str) -> Option<WorktreeInfo> {
        let active = self.active.lock().await;
        active.get(agent_id).cloned()
    }

    /// Check if a worktree exists for the given agent.
    pub async fn has_worktree(&self, agent_id: &str) -> bool {
        let active = self.active.lock().await;
        active.contains_key(agent_id)
    }

    /// Get the number of active worktrees.
    pub async fn active_count(&self) -> usize {
        let active = self.active.lock().await;
        active.len()
    }

    /// Remove all active worktrees. Errors are logged but do not stop cleanup.
    pub async fn cleanup_all(&self) {
        let agent_ids: Vec<String> = {
            let active = self.active.lock().await;
            active.keys().cloned().collect()
        };

        for agent_id in agent_ids {
            if let Err(e) = self.cleanup_worktree(&agent_id).await {
                tracing::warn!(
                    agent_id = %agent_id,
                    error = %e,
                    "failed to cleanup worktree during cleanup_all"
                );
            }
        }
    }

    /// Return the repository root path.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Compute the worktree directory path for an agent.
    fn worktree_path(&self, agent_id: &str) -> PathBuf {
        self.repo_root
            .join(".claude")
            .join("worktrees")
            .join(agent_id)
    }

    /// Compute the branch name for an agent.
    fn branch_name(&self, agent_id: &str) -> String {
        format!("agent/{agent_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a temporary git repo with an initial commit.
    async fn make_temp_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().to_path_buf();

        // git init
        let output = Command::new("git")
            .args(["init"])
            .current_dir(&path)
            .output()
            .await
            .expect("git init");
        assert!(output.status.success(), "git init failed");

        // Configure user for commits
        let _ = Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&path)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&path)
            .output()
            .await;

        // Create initial commit
        let readme = path.join("README.md");
        tokio::fs::write(&readme, "# Test Repo\n")
            .await
            .expect("write readme");
        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&path)
            .output()
            .await;
        let output = Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&path)
            .output()
            .await
            .expect("git commit");
        assert!(output.status.success(), "initial commit failed");

        (dir, path)
    }

    /// Helper: get the current branch name in a repo.
    async fn current_branch(repo: &Path) -> String {
        let output = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(repo)
            .output()
            .await
            .expect("rev-parse");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Helper: check if a branch exists in a repo.
    async fn branch_exists(repo: &Path, branch: &str) -> bool {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", branch])
            .current_dir(repo)
            .output()
            .await
            .expect("rev-parse");
        output.status.success()
    }

    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn new_succeeds_for_valid_repo() {
        let (_dir, path) = make_temp_repo().await;
        let manager = WorktreeManager::new(&path).await;
        assert!(manager.is_ok());
    }

    #[tokio::test]
    async fn new_fails_for_non_git_directory() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let result = WorktreeManager::new(dir.path()).await;
        assert!(matches!(result, Err(WorktreeError::NotAGitRepo(_))));
    }

    // -----------------------------------------------------------------------
    // Create worktree
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_worktree_creates_directory_and_branch() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        let wt_path = manager
            .create_worktree("agent-001", &main_branch)
            .await
            .expect("create_worktree");

        // Directory exists
        assert!(wt_path.exists());
        assert!(wt_path.join("README.md").exists());

        // Branch was created
        assert!(branch_exists(&path, "agent/agent-001").await);

        // Tracked in memory
        assert!(manager.has_worktree("agent-001").await);
        assert_eq!(manager.active_count().await, 1);
    }

    #[tokio::test]
    async fn create_worktree_uses_correct_path() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        let wt_path = manager
            .create_worktree("test-agent", &main_branch)
            .await
            .expect("create_worktree");

        let expected = path.join(".claude").join("worktrees").join("test-agent");
        assert_eq!(wt_path, expected);
    }

    #[tokio::test]
    async fn create_worktree_rejects_duplicate() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        manager
            .create_worktree("agent-dup", &main_branch)
            .await
            .expect("first create");

        let result = manager.create_worktree("agent-dup", &main_branch).await;

        assert!(matches!(result, Err(WorktreeError::AlreadyExists(_))));
    }

    #[tokio::test]
    async fn create_worktree_fails_for_invalid_base_branch() {
        let (_dir, path) = make_temp_repo().await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        let result = manager
            .create_worktree("agent-bad-base", "nonexistent-branch")
            .await;

        assert!(matches!(result, Err(WorktreeError::GitCommand(_))));
    }

    // -----------------------------------------------------------------------
    // Cleanup worktree
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cleanup_worktree_removes_directory_and_branch() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        let wt_path = manager
            .create_worktree("agent-cleanup", &main_branch)
            .await
            .expect("create_worktree");

        assert!(wt_path.exists());
        assert!(branch_exists(&path, "agent/agent-cleanup").await);

        manager
            .cleanup_worktree("agent-cleanup")
            .await
            .expect("cleanup_worktree");

        // Directory removed
        assert!(!wt_path.exists());

        // Branch deleted
        assert!(!branch_exists(&path, "agent/agent-cleanup").await);

        // No longer tracked
        assert!(!manager.has_worktree("agent-cleanup").await);
        assert_eq!(manager.active_count().await, 0);
    }

    #[tokio::test]
    async fn cleanup_worktree_fails_for_unknown_agent() {
        let (_dir, path) = make_temp_repo().await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        let result = manager.cleanup_worktree("ghost-agent").await;
        assert!(matches!(result, Err(WorktreeError::NotFound(_))));
    }

    // -----------------------------------------------------------------------
    // List / Get
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_worktrees_returns_all_active() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        manager
            .create_worktree("agent-a", &main_branch)
            .await
            .expect("create a");
        manager
            .create_worktree("agent-b", &main_branch)
            .await
            .expect("create b");

        let list = manager.list_worktrees().await;
        assert_eq!(list.len(), 2);

        let ids: Vec<&str> = list.iter().map(|w| w.agent_id.as_str()).collect();
        assert!(ids.contains(&"agent-a"));
        assert!(ids.contains(&"agent-b"));
    }

    #[tokio::test]
    async fn get_worktree_returns_info() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        manager
            .create_worktree("agent-info", &main_branch)
            .await
            .expect("create");

        let info = manager.get_worktree("agent-info").await;
        assert!(info.is_some());
        let info = info.expect("should be some");
        assert_eq!(info.agent_id, "agent-info");
        assert_eq!(info.branch, "agent/agent-info");
        assert_eq!(info.base_branch, main_branch);
    }

    #[tokio::test]
    async fn get_worktree_returns_none_for_unknown() {
        let (_dir, path) = make_temp_repo().await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        assert!(manager.get_worktree("unknown").await.is_none());
    }

    // -----------------------------------------------------------------------
    // Cleanup all
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cleanup_all_removes_everything() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        manager
            .create_worktree("agent-x", &main_branch)
            .await
            .expect("create x");
        manager
            .create_worktree("agent-y", &main_branch)
            .await
            .expect("create y");

        assert_eq!(manager.active_count().await, 2);

        manager.cleanup_all().await;

        assert_eq!(manager.active_count().await, 0);
        assert!(!branch_exists(&path, "agent/agent-x").await);
        assert!(!branch_exists(&path, "agent/agent-y").await);
    }

    // -----------------------------------------------------------------------
    // Concurrent creation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn concurrent_worktree_creation() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        let mut handles = Vec::new();
        for i in 0..3 {
            let mgr = manager.clone();
            let branch = main_branch.clone();
            handles.push(tokio::spawn(async move {
                mgr.create_worktree(&format!("concurrent-{i}"), &branch)
                    .await
            }));
        }

        let mut successes = 0;
        for handle in handles {
            let result = handle.await.expect("task panicked");
            if result.is_ok() {
                successes += 1;
            }
        }

        // All 3 should succeed (git handles concurrent worktree adds)
        assert_eq!(successes, 3);
        assert_eq!(manager.active_count().await, 3);

        // Cleanup
        manager.cleanup_all().await;
        assert_eq!(manager.active_count().await, 0);
    }

    // -----------------------------------------------------------------------
    // Worktree is a valid working copy
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn worktree_has_independent_working_copy() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");

        let wt_path = manager
            .create_worktree("agent-independent", &main_branch)
            .await
            .expect("create_worktree");

        // Write a file in the worktree
        let test_file = wt_path.join("agent-file.txt");
        tokio::fs::write(&test_file, "agent work\n")
            .await
            .expect("write");

        // The file should NOT exist in the main repo
        assert!(!path.join("agent-file.txt").exists());

        // The worktree should report the correct branch
        let wt_branch = current_branch(&wt_path).await;
        assert_eq!(wt_branch, "agent/agent-independent");

        manager.cleanup_all().await;
    }

    // -----------------------------------------------------------------------
    // Manager clone shares state
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cloned_manager_shares_state() {
        let (_dir, path) = make_temp_repo().await;
        let main_branch = current_branch(&path).await;
        let manager = WorktreeManager::new(&path).await.expect("manager");
        let manager2 = manager.clone();

        manager
            .create_worktree("agent-shared", &main_branch)
            .await
            .expect("create");

        // The clone should see the same worktree
        assert!(manager2.has_worktree("agent-shared").await);
        assert_eq!(manager2.active_count().await, 1);

        manager.cleanup_all().await;
    }
}
