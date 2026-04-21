//! Git tools for swarm collaboration.
//!
//! These tools let agents interact with the swarm git server:
//! - `git_clone`: Clone the swarm repo into the workspace
//! - `git_push`: Commit and push changes to a feature branch
//!
//! Only registered when `SWARM_REPO_ID` and `GIT_SERVER_URL` env vars are set.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use symbiotic_core::protocol::{Tool, ToolResult};

use super::workspace::WorkspaceConfig;
use crate::BridgeClient;

// ---------------------------------------------------------------------------
// GitCloneTool
// ---------------------------------------------------------------------------

/// Clone the swarm repository into the agent's workspace.
pub struct GitCloneTool {
    config: Arc<WorkspaceConfig>,
    git_server_url: String,
    repo_id: String,
}

impl GitCloneTool {
    pub fn new(config: Arc<WorkspaceConfig>, git_server_url: String, repo_id: String) -> Self {
        Self {
            config,
            git_server_url,
            repo_id,
        }
    }

    fn repo_url(&self) -> String {
        format!("{}/{}.git", self.git_server_url, self.repo_id)
    }
}

#[async_trait::async_trait]
impl Tool for GitCloneTool {
    fn name(&self) -> &str {
        "git_clone"
    }

    fn description(&self) -> &str {
        "Clone the swarm git repository into your workspace. Optionally checkout a specific branch."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "branch": {
                    "type": "string",
                    "description": "Branch to checkout after cloning (default: main)"
                },
                "target_dir": {
                    "type": "string",
                    "description": "Directory name to clone into (default: repo)"
                }
            }
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let branch = params
            .get("branch")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        let target_dir = params
            .get("target_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("repo");

        let target_path = self.config.root.join(target_dir);
        let url = self.repo_url();

        let output = tokio::process::Command::new("git")
            .args([
                "clone",
                "--branch",
                branch,
                &url,
                &target_path.to_string_lossy(),
            ])
            .current_dir(&self.config.root)
            .output()
            .await?;

        if output.status.success() {
            // Configure git identity inside the clone
            let _ = tokio::process::Command::new("git")
                .args(["config", "user.email", "agent@symbiotic.sh"])
                .current_dir(&target_path)
                .output()
                .await;
            let _ = tokio::process::Command::new("git")
                .args(["config", "user.name", "Symbiotic Agent"])
                .current_dir(&target_path)
                .output()
                .await;

            Ok(ToolResult {
                success: true,
                output: format!("Cloned {} (branch: {}) into {}", url, branch, target_dir),
            })
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Ok(ToolResult {
                success: false,
                output: format!("git clone failed: {}", stderr.trim()),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// GitPushTool
// ---------------------------------------------------------------------------

/// Commit and push workspace changes to a feature branch on the swarm git server.
pub struct GitPushTool {
    config: Arc<WorkspaceConfig>,
    git_server_url: String,
    repo_id: String,
    agent_id: String,
    goal_scope: Option<String>,
    thread_id: Option<String>,
    bridge: Arc<BridgeClient>,
}

impl GitPushTool {
    pub fn new(
        config: Arc<WorkspaceConfig>,
        git_server_url: String,
        repo_id: String,
        agent_id: String,
        goal_scope: Option<String>,
        thread_id: Option<String>,
        bridge: Arc<BridgeClient>,
    ) -> Self {
        Self {
            config,
            git_server_url,
            repo_id,
            agent_id,
            goal_scope,
            thread_id,
            bridge,
        }
    }

    async fn issue_push_session(&self, branch: &str) -> Result<String> {
        let response = self
            .bridge
            .call(
                "swarm.issue_push_session",
                serde_json::json!({
                    "agent_id": self.agent_id,
                    "repo_id": self.repo_id,
                    "branch": branch,
                    "goal_scope": self.goal_scope,
                    "thread_id": self.thread_id,
                }),
            )
            .await?;

        response
            .get("push_session")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .ok_or_else(|| anyhow!("push session response missing push_session"))
    }
}

#[async_trait::async_trait]
impl Tool for GitPushTool {
    fn name(&self) -> &str {
        "git_push"
    }

    fn description(&self) -> &str {
        "Stage all changes, commit with a message, and push to your feature branch on the swarm git server."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Commit message describing your changes"
                },
                "work_dir": {
                    "type": "string",
                    "description": "Subdirectory containing the git repo (default: repo)"
                }
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("update");
        let work_dir = params
            .get("work_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("repo");

        let repo_path = self.config.root.join(work_dir);
        let branch = format!("feature/agent-{}", self.agent_id);

        // Ensure we're on the right branch
        let checkout = tokio::process::Command::new("git")
            .args(["checkout", "-B", &branch])
            .current_dir(&repo_path)
            .output()
            .await?;

        if !checkout.status.success() {
            let stderr = String::from_utf8_lossy(&checkout.stderr);
            return Ok(ToolResult {
                success: false,
                output: format!("git checkout failed: {}", stderr.trim()),
            });
        }

        // Stage all changes
        let add = tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo_path)
            .output()
            .await?;

        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            return Ok(ToolResult {
                success: false,
                output: format!("git add failed: {}", stderr.trim()),
            });
        }

        // Commit
        let commit = tokio::process::Command::new("git")
            .args(["commit", "-m", message, "--allow-empty"])
            .current_dir(&repo_path)
            .output()
            .await?;

        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            // "nothing to commit" is not a failure
            if stderr.contains("nothing to commit") {
                return Ok(ToolResult {
                    success: true,
                    output: "Nothing to commit — working tree clean".to_string(),
                });
            }
            return Ok(ToolResult {
                success: false,
                output: format!("git commit failed: {}", stderr.trim()),
            });
        }

        // Push to remote
        let remote_url = format!("{}/{}.git", self.git_server_url, self.repo_id);
        let push_session = match self.issue_push_session(&branch).await {
            Ok(push_session) => push_session,
            Err(error) => {
                return Ok(ToolResult {
                    success: false,
                    output: format!("push session request failed: {error}"),
                });
            }
        };

        let push = tokio::process::Command::new("git")
            .args([
                "-c",
                &format!("http.extraHeader=X-Symbiotic-Push-Session: {push_session}"),
                "push",
                &remote_url,
                &format!("{}:{}", branch, branch),
            ])
            .current_dir(&repo_path)
            .output()
            .await?;

        if push.status.success() {
            Ok(ToolResult {
                success: true,
                output: format!("Pushed to branch '{}': {}", branch, message),
            })
        } else {
            let stderr = String::from_utf8_lossy(&push.stderr);
            Ok(ToolResult {
                success: false,
                output: format!("git push failed: {}", stderr.trim()),
            })
        }
    }
}
