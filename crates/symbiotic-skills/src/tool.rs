//! Agent tool wrapper for the Skill Synthesis pipeline.
//!
//! Registers `synthesize_skill` as an agent-callable tool. When an agent
//! encounters a novel problem that no existing skill can solve, it calls
//! this tool to trigger the full synthesis pipeline (code generation,
//! sandbox compilation, testing, archival).

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use symbiotic_agents::builtin_tools::CapabilityChecker;
use symbiotic_agents::tools::{Tool, ToolResult};

use crate::synthesis::SkillSynthesizer;

/// Agent tool that triggers dynamic skill synthesis.
///
/// Requires the `vm.exec` capability scope (skill synthesis runs code in
/// a sandboxed environment). The tool receives a skill name, description,
/// and the requesting agent ID, then runs the full synthesis pipeline.
pub struct SynthesizeTool {
    agent_id: String,
    synthesizer: Arc<Mutex<SkillSynthesizer>>,
    caps: Arc<dyn CapabilityChecker>,
}

impl SynthesizeTool {
    /// Create a new synthesis tool for the given agent.
    pub fn new(
        agent_id: String,
        synthesizer: Arc<Mutex<SkillSynthesizer>>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            synthesizer,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for SynthesizeTool {
    fn name(&self) -> &str {
        "synthesize_skill"
    }

    fn description(&self) -> &str {
        "Synthesize a new skill when no existing skill can solve the problem. \
         Generates source code, compiles in a sandbox, runs tests, and archives \
         the result as a reusable skill."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Kebab-case name for the new skill (e.g. 'json-patcher')"
                },
                "description": {
                    "type": "string",
                    "description": "What the skill should do — detailed enough for code generation"
                }
            },
            "required": ["skill_name", "description"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        // Capability gate: skill synthesis requires vm.exec
        self.caps.check(&self.agent_id, "vm.exec")?;

        let skill_name = params
            .get("skill_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: skill_name"))?
            .to_string();

        let description = params
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: description"))?
            .to_string();

        let request = crate::synthesis::SynthesisRequest::new(
            skill_name.clone(),
            description,
            self.agent_id.clone(),
        );

        let synthesizer = self.synthesizer.lock().await;
        match synthesizer.synthesize(&request).await {
            Ok(result) => {
                let output = serde_json::json!({
                    "status": "completed",
                    "skill_name": result.skill_name,
                    "skill_dir": result.skill_dir.display().to_string(),
                    "compiled_targets": result.compiled_targets,
                    "stages": result.stages_completed.iter()
                        .map(|s| format!("{s:?}"))
                        .collect::<Vec<_>>(),
                });
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&output)?,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: format!("Skill synthesis failed: {e}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis::{SkillArchiver, StubSandboxCompiler};
    use std::sync::Arc;

    struct AllowAll;
    impl CapabilityChecker for AllowAll {
        fn check(&self, _agent_id: &str, _scope: &str) -> Result<()> {
            Ok(())
        }
    }

    struct DenyAll;
    impl CapabilityChecker for DenyAll {
        fn check(&self, _agent_id: &str, scope: &str) -> Result<()> {
            Err(anyhow::anyhow!("capability denied: {scope}"))
        }
    }

    #[tokio::test]
    async fn synthesize_tool_succeeds_with_stub_compiler() {
        let tmp = tempfile::TempDir::new().unwrap();
        let synthesizer = SkillSynthesizer::new(
            Box::new(StubSandboxCompiler),
            SkillArchiver::new(tmp.path()),
        );
        let tool = SynthesizeTool::new(
            "agent-1".to_string(),
            Arc::new(Mutex::new(synthesizer)),
            Arc::new(AllowAll),
        );

        let result = tool
            .execute(serde_json::json!({
                "skill_name": "test-tool",
                "description": "A test tool for testing"
            }))
            .await
            .expect("execute");

        assert!(result.success);
        assert!(result.output.contains("completed"));
        assert!(result.output.contains("test-tool"));
    }

    #[tokio::test]
    async fn synthesize_tool_denied_without_capability() {
        let tmp = tempfile::TempDir::new().unwrap();
        let synthesizer = SkillSynthesizer::new(
            Box::new(StubSandboxCompiler),
            SkillArchiver::new(tmp.path()),
        );
        let tool = SynthesizeTool::new(
            "agent-1".to_string(),
            Arc::new(Mutex::new(synthesizer)),
            Arc::new(DenyAll),
        );

        let err = tool
            .execute(serde_json::json!({
                "skill_name": "blocked-tool",
                "description": "Should not run"
            }))
            .await
            .expect_err("should fail");

        assert!(err.to_string().contains("capability denied"));
    }

    #[tokio::test]
    async fn synthesize_tool_missing_params() {
        let tmp = tempfile::TempDir::new().unwrap();
        let synthesizer = SkillSynthesizer::new(
            Box::new(StubSandboxCompiler),
            SkillArchiver::new(tmp.path()),
        );
        let tool = SynthesizeTool::new(
            "agent-1".to_string(),
            Arc::new(Mutex::new(synthesizer)),
            Arc::new(AllowAll),
        );

        let err = tool
            .execute(serde_json::json!({}))
            .await
            .expect_err("should fail");

        assert!(err.to_string().contains("missing required parameter"));
    }

    #[test]
    fn synthesize_tool_metadata() {
        let tmp = tempfile::TempDir::new().unwrap();
        let synthesizer = SkillSynthesizer::new(
            Box::new(StubSandboxCompiler),
            SkillArchiver::new(tmp.path()),
        );
        let tool = SynthesizeTool::new(
            "agent-1".to_string(),
            Arc::new(Mutex::new(synthesizer)),
            Arc::new(AllowAll),
        );

        assert_eq!(tool.name(), "synthesize_skill");
        assert!(!tool.description().is_empty());

        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
        assert!(schema["required"].as_array().unwrap().len() == 2);
    }
}
