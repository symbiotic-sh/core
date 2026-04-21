//! Process Engineer tools for analyzing agent executions and creating
//! methodology improvements.
//!
//! Five tools implementing the [`Tool`] trait:
//! - `read_executions` — query agent execution records
//! - `read_metrics` — query execution summary statistics
//! - `write_rule` — create methodology rules
//! - `write_skill` — create skill manifests
//! - `propose_prompt` — propose prompt version changes (never auto-activated)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::{Duration, Utc};

use crate::builtin_tools::CapabilityChecker;
use crate::monitoring::{AgentMonitor, ExecutionFilter};
use crate::tools::{Tool, ToolResult};

// ---------------------------------------------------------------------------
// ReadExecutionsTool
// ---------------------------------------------------------------------------

/// Queries AgentMonitor for execution records including tool call details.
pub struct ReadExecutionsTool {
    agent_id: String,
    monitor: Arc<dyn AgentMonitor>,
    caps: Arc<dyn CapabilityChecker>,
}

impl ReadExecutionsTool {
    pub fn new(
        agent_id: String,
        monitor: Arc<dyn AgentMonitor>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            monitor,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ReadExecutionsTool {
    fn name(&self) -> &str {
        "read_executions"
    }

    fn description(&self) -> &str {
        "Query agent execution records with optional filters for goal_type, agent_id, and time window"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "description": "Filter by agent ID"},
                "task_id": {"type": "string", "description": "Filter by task/goal ID"},
                "limit": {"type": "integer", "description": "Maximum records to return (default 20)"},
                "since_hours": {"type": "integer", "description": "Only show executions from the last N hours (default 24)"}
            }
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "archive.read")?;

        let since_hours = params
            .get("since_hours")
            .and_then(|v| v.as_i64())
            .unwrap_or(24);
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as u32;

        let filter = ExecutionFilter {
            agent_id: params
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            task_id: params
                .get("task_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            limit: Some(limit),
            ..Default::default()
        };

        let since = Utc::now() - Duration::hours(since_hours);
        let executions = self.monitor.query_executions(&filter, since)?;

        let output = serde_json::to_string_pretty(&executions)
            .map_err(|e| anyhow!("serialization failed: {e}"))?;

        Ok(ToolResult {
            success: true,
            output,
        })
    }
}

// ---------------------------------------------------------------------------
// ReadMetricsTool
// ---------------------------------------------------------------------------

/// Queries execution summary statistics.
pub struct ReadMetricsTool {
    agent_id: String,
    monitor: Arc<dyn AgentMonitor>,
    caps: Arc<dyn CapabilityChecker>,
}

impl ReadMetricsTool {
    pub fn new(
        agent_id: String,
        monitor: Arc<dyn AgentMonitor>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            monitor,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ReadMetricsTool {
    fn name(&self) -> &str {
        "read_metrics"
    }

    fn description(&self) -> &str {
        "Query execution summary statistics for a time window (success rate, latency, token usage)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "window": {
                    "type": "string",
                    "enum": ["1h", "24h", "7d"],
                    "description": "Time window for metrics aggregation (default: 24h)"
                }
            }
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "archive.read")?;

        let window = params
            .get("window")
            .and_then(|v| v.as_str())
            .unwrap_or("24h");

        let since = match window {
            "1h" => Utc::now() - Duration::hours(1),
            "7d" => Utc::now() - Duration::days(7),
            _ => Utc::now() - Duration::hours(24),
        };

        let summary = self.monitor.summary(since)?;
        let output = serde_json::to_string_pretty(&summary)
            .map_err(|e| anyhow!("serialization failed: {e}"))?;

        Ok(ToolResult {
            success: true,
            output,
        })
    }
}

// ---------------------------------------------------------------------------
// WriteRuleTool
// ---------------------------------------------------------------------------

/// Writes a Markdown rule to the methodology knowledge base.
pub struct WriteRuleTool {
    agent_id: String,
    methodology_root: PathBuf,
    caps: Arc<dyn CapabilityChecker>,
}

impl WriteRuleTool {
    pub fn new(
        agent_id: String,
        methodology_root: PathBuf,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            methodology_root,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for WriteRuleTool {
    fn name(&self) -> &str {
        "write_rule"
    }

    fn description(&self) -> &str {
        "Write a process rule as Markdown to the methodology knowledge base. Rules are auto-loaded into agent context on subsequent runs."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "category": {"type": "string", "description": "Rule category subdirectory (e.g. 'process-rules', 'error-prevention')"},
                "filename": {"type": "string", "description": "Filename without extension (e.g. 'avoid-duplicate-recalls')"},
                "content": {"type": "string", "description": "Markdown content of the rule"}
            },
            "required": ["category", "filename", "content"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "archive.write")?;

        let category = params
            .get("category")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: category"))?;
        let filename = params
            .get("filename")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: filename"))?;
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: content"))?;

        // Sanitize path components to prevent directory traversal.
        if category.contains("..") || filename.contains("..") {
            return Err(anyhow!("path traversal not allowed"));
        }

        let dir = self.methodology_root.join(category);
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("failed to create rule directory: {e}"))?;

        let path = dir.join(format!("{filename}.md"));
        std::fs::write(&path, content).map_err(|e| anyhow!("failed to write rule file: {e}"))?;

        Ok(ToolResult {
            success: true,
            output: format!("Rule written to {}", path.display()),
        })
    }
}

// ---------------------------------------------------------------------------
// WriteSkillTool
// ---------------------------------------------------------------------------

/// Writes a TOML skill manifest and prompt file.
pub struct WriteSkillTool {
    agent_id: String,
    skills_root: PathBuf,
    caps: Arc<dyn CapabilityChecker>,
}

impl WriteSkillTool {
    pub fn new(agent_id: String, skills_root: PathBuf, caps: Arc<dyn CapabilityChecker>) -> Self {
        Self {
            agent_id,
            skills_root,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for WriteSkillTool {
    fn name(&self) -> &str {
        "write_skill"
    }

    fn description(&self) -> &str {
        "Write a skill manifest (TOML) and prompt file to the skills directory. Skills are auto-loaded on subsequent agent runs."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name (used as directory name)"},
                "description": {"type": "string", "description": "Human-readable description"},
                "version": {"type": "string", "description": "Semantic version (e.g. '1.0.0')"},
                "keywords": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Keywords for skill discovery"
                },
                "prompt_content": {"type": "string", "description": "Prompt template content (Markdown)"}
            },
            "required": ["name", "description", "version", "prompt_content"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "archive.write")?;

        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: name"))?;
        let description = params
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: description"))?;
        let version = params
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: version"))?;
        let prompt_content = params
            .get("prompt_content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: prompt_content"))?;
        let keywords: Vec<String> = params
            .get("keywords")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if name.contains("..") || name.contains('/') {
            return Err(anyhow!("invalid skill name"));
        }

        let skill_dir = self.skills_root.join(name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| anyhow!("failed to create skill directory: {e}"))?;

        // Write TOML manifest.
        let keywords_toml: Vec<String> = keywords.iter().map(|k| format!("\"{k}\"")).collect();
        let manifest = format!(
            "[skill]\n\
             name = \"{name}\"\n\
             description = \"{description}\"\n\
             version = \"{version}\"\n\
             keywords = [{}]\n\
             trust_level = \"ArchiveWrite\"\n\
             prompt_file = \"prompt.md\"\n",
            keywords_toml.join(", ")
        );

        let manifest_path = skill_dir.join("manifest.toml");
        std::fs::write(&manifest_path, &manifest)
            .map_err(|e| anyhow!("failed to write manifest: {e}"))?;

        // Write prompt file.
        let prompt_path = skill_dir.join("prompt.md");
        std::fs::write(&prompt_path, prompt_content)
            .map_err(|e| anyhow!("failed to write prompt: {e}"))?;

        Ok(ToolResult {
            success: true,
            output: format!("Skill '{name}' written to {}", skill_dir.display()),
        })
    }
}

// ---------------------------------------------------------------------------
// ProposePromptTool
// ---------------------------------------------------------------------------

/// Proposes a new prompt version for a role. Does NOT activate it.
pub struct ProposePromptTool {
    agent_id: String,
    role_dir: PathBuf,
    caps: Arc<dyn CapabilityChecker>,
}

impl ProposePromptTool {
    pub fn new(agent_id: String, role_dir: PathBuf, caps: Arc<dyn CapabilityChecker>) -> Self {
        Self {
            agent_id,
            role_dir,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ProposePromptTool {
    fn name(&self) -> &str {
        "propose_prompt"
    }

    fn description(&self) -> &str {
        "Propose a new prompt version for a role. The proposal is saved but NOT activated — a human must review and activate it."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "role_name": {"type": "string", "description": "Name of the role to propose changes for"},
                "new_prompt": {"type": "string", "description": "The proposed new system prompt text"},
                "changelog": {"type": "string", "description": "Description of what changed and why"}
            },
            "required": ["role_name", "new_prompt", "changelog"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "archive.write")?;

        let role_name = params
            .get("role_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: role_name"))?;
        let new_prompt = params
            .get("new_prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: new_prompt"))?;
        let changelog = params
            .get("changelog")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: changelog"))?;

        if role_name.contains("..") || role_name.contains('/') {
            return Err(anyhow!("invalid role name"));
        }

        // Write the proposal as a standalone file (NOT overwriting the active role).
        let proposals_dir = self.role_dir.join("proposals");
        std::fs::create_dir_all(&proposals_dir)
            .map_err(|e| anyhow!("failed to create proposals directory: {e}"))?;

        let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
        let proposal_filename = format!("{role_name}-{timestamp}.toml");
        let proposal_path = proposals_dir.join(&proposal_filename);

        let proposal_content = format!(
            "# Prompt Proposal (NOT ACTIVE)\n\
             # To activate: copy the [[versions]] section into the role's main TOML\n\
             # and update active_version.\n\n\
             [role]\n\
             name = \"{role_name}\"\n\
             status = \"proposed\"\n\n\
             [[versions]]\n\
             version = \"proposed-{timestamp}\"\n\
             changelog = \"{changelog}\"\n\
             system_prompt = \"\"\"\n\
             {new_prompt}\n\
             \"\"\"\n"
        );

        std::fs::write(&proposal_path, &proposal_content)
            .map_err(|e| anyhow!("failed to write proposal: {e}"))?;

        Ok(ToolResult {
            success: true,
            output: format!(
                "Prompt proposal saved to {} (NOT activated — requires human review)",
                proposal_path.display()
            ),
        })
    }
}

/// Create all PE tools for a given agent.
pub fn create_pe_tools(
    agent_id: &str,
    monitor: Arc<dyn AgentMonitor>,
    caps: Arc<dyn CapabilityChecker>,
    methodology_root: &Path,
    skills_root: &Path,
    role_dir: &Path,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadExecutionsTool::new(
            agent_id.to_string(),
            monitor.clone(),
            caps.clone(),
        )),
        Box::new(ReadMetricsTool::new(
            agent_id.to_string(),
            monitor,
            caps.clone(),
        )),
        Box::new(WriteRuleTool::new(
            agent_id.to_string(),
            methodology_root.to_path_buf(),
            caps.clone(),
        )),
        Box::new(WriteSkillTool::new(
            agent_id.to_string(),
            skills_root.to_path_buf(),
            caps.clone(),
        )),
        Box::new(ProposePromptTool::new(
            agent_id.to_string(),
            role_dir.to_path_buf(),
            caps,
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitoring::{AgentType, ExecutionStatus, FinishRecord, SqliteAgentMonitor};

    // -- Mock backends --

    struct AllowAll;
    impl CapabilityChecker for AllowAll {
        fn check(&self, _agent_id: &str, _scope: &str) -> Result<()> {
            Ok(())
        }
    }

    struct DenyAll;
    impl CapabilityChecker for DenyAll {
        fn check(&self, _agent_id: &str, scope: &str) -> Result<()> {
            Err(anyhow!("capability denied: {scope}"))
        }
    }

    #[tokio::test]
    async fn read_executions_returns_data() {
        let monitor = Arc::new(SqliteAgentMonitor::open_in_memory().unwrap());
        monitor
            .record_start("agent-1", AgentType::React, Some("task-1"), None, None)
            .unwrap();

        let tool = ReadExecutionsTool::new("pe-agent".to_string(), monitor, Arc::new(AllowAll));

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("agent-1"));
    }

    #[tokio::test]
    async fn read_executions_denied_without_capability() {
        let monitor = Arc::new(SqliteAgentMonitor::open_in_memory().unwrap());
        let tool = ReadExecutionsTool::new("pe-agent".to_string(), monitor, Arc::new(DenyAll));

        let err = tool
            .execute(serde_json::json!({}))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("capability denied"));
    }

    #[tokio::test]
    async fn read_metrics_returns_summary() {
        let monitor = Arc::new(SqliteAgentMonitor::open_in_memory().unwrap());
        let exec_id = monitor
            .record_start("agent-1", AgentType::React, None, None, None)
            .unwrap();
        monitor
            .record_finish(&FinishRecord {
                execution_id: &exec_id,
                status: ExecutionStatus::Success,
                iterations: 3,
                tool_call_count: 2,
                tokens_in: Some(1000),
                tokens_out: Some(500),
                error_message: None,
                tool_calls_json: None,
            })
            .unwrap();

        let tool = ReadMetricsTool::new("pe-agent".to_string(), monitor, Arc::new(AllowAll));

        let result = tool
            .execute(serde_json::json!({"window": "1h"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("success_rate"));
    }

    #[tokio::test]
    async fn write_rule_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteRuleTool::new(
            "pe-agent".to_string(),
            tmp.path().to_path_buf(),
            Arc::new(AllowAll),
        );

        let result = tool
            .execute(serde_json::json!({
                "category": "process-rules",
                "filename": "avoid-duplicate-recalls",
                "content": "# Avoid Duplicate Recalls\n\nDo not call recall twice with the same query."
            }))
            .await
            .unwrap();

        assert!(result.success);
        let written =
            std::fs::read_to_string(tmp.path().join("process-rules/avoid-duplicate-recalls.md"))
                .unwrap();
        assert!(written.contains("Avoid Duplicate Recalls"));
    }

    #[tokio::test]
    async fn write_rule_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteRuleTool::new(
            "pe-agent".to_string(),
            tmp.path().to_path_buf(),
            Arc::new(AllowAll),
        );

        let err = tool
            .execute(serde_json::json!({
                "category": "../../../etc",
                "filename": "evil",
                "content": "pwned"
            }))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("path traversal"));
    }

    #[tokio::test]
    async fn write_skill_creates_manifest_and_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = WriteSkillTool::new(
            "pe-agent".to_string(),
            tmp.path().to_path_buf(),
            Arc::new(AllowAll),
        );

        let result = tool
            .execute(serde_json::json!({
                "name": "dedup-checker",
                "description": "Check for duplicate archive entries before storing",
                "version": "1.0.0",
                "keywords": ["dedup", "archive"],
                "prompt_content": "# Dedup Checker\n\nBefore storing, check if an entry with the same URL exists."
            }))
            .await
            .unwrap();

        assert!(result.success);

        let manifest =
            std::fs::read_to_string(tmp.path().join("dedup-checker/manifest.toml")).unwrap();
        assert!(manifest.contains("name = \"dedup-checker\""));
        assert!(manifest.contains("version = \"1.0.0\""));

        let prompt = std::fs::read_to_string(tmp.path().join("dedup-checker/prompt.md")).unwrap();
        assert!(prompt.contains("Dedup Checker"));
    }

    #[tokio::test]
    async fn propose_prompt_creates_proposal_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ProposePromptTool::new(
            "pe-agent".to_string(),
            tmp.path().to_path_buf(),
            Arc::new(AllowAll),
        );

        let result = tool
            .execute(serde_json::json!({
                "role_name": "researcher",
                "new_prompt": "You are an improved research agent with better source citation.",
                "changelog": "Added explicit citation requirements"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("NOT activated"));

        // Verify the proposal file exists.
        let proposals_dir = tmp.path().join("proposals");
        let entries: Vec<_> = std::fs::read_dir(&proposals_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let content = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(content.contains("NOT ACTIVE"));
        assert!(content.contains("improved research agent"));
    }

    #[tokio::test]
    async fn create_pe_tools_returns_five_tools() {
        let monitor = Arc::new(SqliteAgentMonitor::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let tools = create_pe_tools(
            "pe-agent",
            monitor,
            Arc::new(AllowAll),
            &tmp.path().join("operations"),
            &tmp.path().join("skills"),
            &tmp.path().join("roles"),
        );
        assert_eq!(tools.len(), 5);

        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read_executions"));
        assert!(names.contains(&"read_metrics"));
        assert!(names.contains(&"write_rule"));
        assert!(names.contains(&"write_skill"));
        assert!(names.contains(&"propose_prompt"));
    }
}
