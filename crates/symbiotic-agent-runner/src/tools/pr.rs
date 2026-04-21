//! PR tools for swarm collaboration.
//!
//! These are **remote tools** — they proxy requests to the daemon via the
//! JSON-RPC bridge, where the PRManager handles the actual state.
//!
//! Tools:
//! - `pr_create`: Create a pull request for a feature branch
//! - `pr_comment`: Add a review comment to a PR
//! - `pr_approve`: Approve a PR
//! - `pr_request_changes`: Request changes on a PR
//! - `pr_get_status`: Get the current status of a PR
//! - `pr_close`: Close a pull request without merging
//!
//! Only registered when `SWARM_REPO_ID` env var is set.

use std::sync::Arc;

use anyhow::Result;
use symbiotic_core::protocol::{Tool, ToolResult};

use crate::BridgeClient;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Execute a remote PR tool via the JSON-RPC bridge.
async fn pr_rpc(
    bridge: &BridgeClient,
    method: &str,
    params: serde_json::Value,
) -> Result<ToolResult> {
    let result = bridge.call(method, params).await?;

    let success = result
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap_or("no output")
        .to_string();

    Ok(ToolResult { success, output })
}

fn inject_agent_id(params: &mut serde_json::Value, agent_id: &str) {
    if let Some(obj) = params.as_object_mut() {
        obj.entry("agent_id".to_string())
            .or_insert_with(|| serde_json::json!(agent_id));
    }
}

// ---------------------------------------------------------------------------
// PRCreateTool
// ---------------------------------------------------------------------------

/// Create a pull request for a feature branch.
pub struct PRCreateTool {
    bridge: Arc<BridgeClient>,
    repo_id: String,
    agent_id: String,
    goal_scope: Option<String>,
    thread_id: Option<String>,
}

impl PRCreateTool {
    pub fn new(
        bridge: Arc<BridgeClient>,
        repo_id: String,
        agent_id: String,
        goal_scope: Option<String>,
        thread_id: Option<String>,
    ) -> Self {
        Self {
            bridge,
            repo_id,
            agent_id,
            goal_scope,
            thread_id,
        }
    }
}

#[async_trait::async_trait]
impl Tool for PRCreateTool {
    fn name(&self) -> &str {
        "pr_create"
    }

    fn description(&self) -> &str {
        "Create a pull request to merge your feature branch into main. Returns the PR ID."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "branch": {
                    "type": "string",
                    "description": "Source branch name (e.g. feature/agent-42)"
                },
                "base": {
                    "type": "string",
                    "description": "Target branch (default: main)"
                },
                "title": {
                    "type": "string",
                    "description": "PR title describing the changes"
                },
                "description": {
                    "type": "string",
                    "description": "Detailed description of the changes"
                }
            },
            "required": ["branch", "title"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let mut rpc_params = params.clone();
        if let Some(obj) = rpc_params.as_object_mut() {
            obj.insert("repo_id".to_string(), serde_json::json!(self.repo_id));
            if let Some(goal_scope) = &self.goal_scope {
                obj.entry("goal_scope".to_string())
                    .or_insert_with(|| serde_json::json!(goal_scope));
            }
            if let Some(thread_id) = &self.thread_id {
                obj.entry("thread_id".to_string())
                    .or_insert_with(|| serde_json::json!(thread_id));
            }
        }
        inject_agent_id(&mut rpc_params, &self.agent_id);
        pr_rpc(&self.bridge, "pr.create", rpc_params).await
    }
}

// ---------------------------------------------------------------------------
// PRCommentTool
// ---------------------------------------------------------------------------

/// Add a review comment to a pull request.
pub struct PRCommentTool {
    bridge: Arc<BridgeClient>,
    agent_id: String,
}

impl PRCommentTool {
    pub fn new(bridge: Arc<BridgeClient>, agent_id: String) -> Self {
        Self { bridge, agent_id }
    }
}

#[async_trait::async_trait]
impl Tool for PRCommentTool {
    fn name(&self) -> &str {
        "pr_comment"
    }

    fn description(&self) -> &str {
        "Add a review comment to a pull request, optionally on a specific file and line."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pr_id": {
                    "type": "string",
                    "description": "The PR ID to comment on"
                },
                "file": {
                    "type": "string",
                    "description": "File path the comment relates to"
                },
                "line": {
                    "type": "integer",
                    "description": "Line number in the file (optional)"
                },
                "body": {
                    "type": "string",
                    "description": "Comment text (Markdown supported)"
                }
            },
            "required": ["pr_id", "body"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let mut rpc_params = params.clone();
        inject_agent_id(&mut rpc_params, &self.agent_id);
        pr_rpc(&self.bridge, "pr.comment", rpc_params).await
    }
}

// ---------------------------------------------------------------------------
// PRApproveTool
// ---------------------------------------------------------------------------

/// Approve a pull request (submit a positive review).
pub struct PRApproveTool {
    bridge: Arc<BridgeClient>,
    agent_id: String,
}

impl PRApproveTool {
    pub fn new(bridge: Arc<BridgeClient>, agent_id: String) -> Self {
        Self { bridge, agent_id }
    }
}

#[async_trait::async_trait]
impl Tool for PRApproveTool {
    fn name(&self) -> &str {
        "pr_approve"
    }

    fn description(&self) -> &str {
        "Approve a pull request, indicating the changes are ready to merge."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pr_id": {
                    "type": "string",
                    "description": "The PR ID to approve"
                },
                "comment": {
                    "type": "string",
                    "description": "Optional approval comment"
                }
            },
            "required": ["pr_id"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let mut rpc_params = params.clone();
        inject_agent_id(&mut rpc_params, &self.agent_id);
        pr_rpc(&self.bridge, "pr.approve", rpc_params).await
    }
}

// ---------------------------------------------------------------------------
// PRRequestChangesTool
// ---------------------------------------------------------------------------

/// Request changes on a pull request (submit a negative review with feedback).
pub struct PRRequestChangesTool {
    bridge: Arc<BridgeClient>,
    agent_id: String,
}

impl PRRequestChangesTool {
    pub fn new(bridge: Arc<BridgeClient>, agent_id: String) -> Self {
        Self { bridge, agent_id }
    }
}

#[async_trait::async_trait]
impl Tool for PRRequestChangesTool {
    fn name(&self) -> &str {
        "pr_request_changes"
    }

    fn description(&self) -> &str {
        "Request changes on a pull request with specific feedback."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pr_id": {
                    "type": "string",
                    "description": "The PR ID to review"
                },
                "comments": {
                    "type": "array",
                    "description": "Review comments with file/line references",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": { "type": "string" },
                            "line": { "type": "integer" },
                            "body": { "type": "string" }
                        },
                        "required": ["body"]
                    }
                }
            },
            "required": ["pr_id", "comments"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let mut rpc_params = params.clone();
        inject_agent_id(&mut rpc_params, &self.agent_id);
        pr_rpc(&self.bridge, "pr.request_changes", rpc_params).await
    }
}

// ---------------------------------------------------------------------------
// PRGetStatusTool
// ---------------------------------------------------------------------------

/// Get the current status of a pull request (reviews, checks, merge readiness).
pub struct PRGetStatusTool {
    bridge: Arc<BridgeClient>,
}

impl PRGetStatusTool {
    pub fn new(bridge: Arc<BridgeClient>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl Tool for PRGetStatusTool {
    fn name(&self) -> &str {
        "pr_get_status"
    }

    fn description(&self) -> &str {
        "Get the current status of a pull request: reviews, CI checks, and whether merge rules are satisfied."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pr_id": {
                    "type": "string",
                    "description": "The PR ID to check"
                }
            },
            "required": ["pr_id"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        pr_rpc(&self.bridge, "pr.get_status", params).await
    }
}

// ---------------------------------------------------------------------------
// PRCloseTool
// ---------------------------------------------------------------------------

/// Close a pull request without merging.
pub struct PRCloseTool {
    bridge: Arc<BridgeClient>,
}

impl PRCloseTool {
    pub fn new(bridge: Arc<BridgeClient>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl Tool for PRCloseTool {
    fn name(&self) -> &str {
        "pr_close"
    }

    fn description(&self) -> &str {
        "Close a pull request without merging it and release branch ownership."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pr_id": {
                    "type": "string",
                    "description": "The PR ID to close"
                }
            },
            "required": ["pr_id"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        pr_rpc(&self.bridge, "pr.close", params).await
    }
}

// ---------------------------------------------------------------------------
// CheckReportTool
// ---------------------------------------------------------------------------

/// Report a CI check result back to the swarm daemon for the current PR.
pub struct CheckReportTool {
    bridge: Arc<BridgeClient>,
    pr_id: String,
    check_name: String,
    agent_id: String,
}

impl CheckReportTool {
    pub fn new(
        bridge: Arc<BridgeClient>,
        pr_id: String,
        check_name: String,
        agent_id: String,
    ) -> Self {
        Self {
            bridge,
            pr_id,
            check_name,
            agent_id,
        }
    }
}

#[async_trait::async_trait]
impl Tool for CheckReportTool {
    fn name(&self) -> &str {
        "check_report"
    }

    fn description(&self) -> &str {
        "Report the current CI check result for this PR. Use after running the assigned verification."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["pending", "running", "success", "failure"],
                    "description": "The latest state for this check"
                },
                "output": {
                    "type": "string",
                    "description": "Optional log summary or failure output"
                }
            },
            "required": ["status"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let mut rpc_params = params.clone();
        if let Some(obj) = rpc_params.as_object_mut() {
            obj.insert("pr_id".to_string(), serde_json::json!(self.pr_id));
            obj.insert("check_name".to_string(), serde_json::json!(self.check_name));
        }
        inject_agent_id(&mut rpc_params, &self.agent_id);
        pr_rpc(&self.bridge, "check.report", rpc_params).await
    }
}
