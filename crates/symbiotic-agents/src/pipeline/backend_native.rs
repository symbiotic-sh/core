use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::execution_plan::Phase;
use crate::executor::{run_agent_with_config, AgentExecConfig};
use crate::llm::LlmClient;
use crate::tools::Tool;

use super::backend::{GoalExecutionContext, PhaseExecutor};

/// Executes pipeline phases using the native ReAct agent loop.
pub struct NativePhaseExecutor {
    llm: Arc<dyn LlmClient>,
    tools: Vec<Arc<dyn Tool>>,
}

impl NativePhaseExecutor {
    pub fn new(llm: Arc<dyn LlmClient>, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { llm, tools }
    }
}

#[async_trait]
impl PhaseExecutor for NativePhaseExecutor {
    async fn execute_phase(
        &self,
        phase: &Phase,
        context: &GoalExecutionContext,
    ) -> Result<(String, f64)> {
        let config = AgentExecConfig {
            system_prompt: Some(format!(
                "You are executing phase '{}' of goal '{}'. Phase description: {}",
                phase.name, context.goal_id, phase.description
            )),
            max_iterations: Some(10),
            identity_context: None,
            handoff_dir: None,
            agent_id: Some(format!("{}-{}", context.goal_id, phase.name)),
            role: Some(format!("phase-{}", phase.name)),
            redact_output: true,
        };

        let goal = &phase.description;
        let tool_refs: Vec<&dyn Tool> = self.tools.iter().map(|t| t.as_ref()).collect();

        let result =
            run_agent_with_config(goal, "", &tool_refs, self.llm.as_ref(), &config).await?;

        // Quality score based on iterations used (fewer = better).
        let max_iter = 10.0_f64;
        let quality = 1.0 - (result.iterations as f64 / max_iter).min(1.0);
        Ok((result.output, quality))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatMessage;
    use crate::tools::ToolResult;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockLlm {
        responses: Vec<String>,
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(idx)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("mock LLM ran out of responses"))
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
            let text = params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("empty");
            Ok(ToolResult {
                success: true,
                output: format!("echo: {text}"),
            })
        }
    }

    fn make_context() -> GoalExecutionContext {
        GoalExecutionContext {
            goal_id: "goal-test".to_string(),
            plan: crate::execution_plan::ExecutionPlan {
                name: "test-plan".to_string(),
                phases: vec![],
                rollback_strategy: crate::execution_plan::RollbackStrategy::None,
            },
            backend: super::super::backend::ExecutionBackend::Native,
            spawned_agents: vec![],
            phase_results: vec![],
            started_at: 1000,
            completed_at: None,
        }
    }

    #[tokio::test]
    async fn test_native_executor_completes_phase() {
        let llm = Arc::new(MockLlm {
            responses: vec![
                r#"{"done": true, "result": "Phase completed successfully"}"#.to_string(),
            ],
            call_count: AtomicUsize::new(0),
        });

        let executor = NativePhaseExecutor::new(llm, vec![]);
        let phase = Phase {
            name: "implement".to_string(),
            description: "Implement the feature".to_string(),
            validations: vec![],
        };
        let ctx = make_context();

        let (output, quality) = executor.execute_phase(&phase, &ctx).await.unwrap();
        assert_eq!(output, "Phase completed successfully");
        // 1 iteration out of 10 -> quality = 1.0 - 0.1 = 0.9
        assert!((quality - 0.9).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_native_executor_with_tool_call() {
        let llm = Arc::new(MockLlm {
            responses: vec![
                r#"{"tool": "echo", "params": {"text": "hello"}}"#.to_string(),
                r#"{"done": true, "result": "Done after echo"}"#.to_string(),
            ],
            call_count: AtomicUsize::new(0),
        });

        let echo: Arc<dyn Tool> = Arc::new(EchoTool);
        let executor = NativePhaseExecutor::new(llm, vec![echo]);
        let phase = Phase {
            name: "test".to_string(),
            description: "Test the feature".to_string(),
            validations: vec![],
        };
        let ctx = make_context();

        let (output, quality) = executor.execute_phase(&phase, &ctx).await.unwrap();
        assert_eq!(output, "Done after echo");
        // 2 iterations out of 10 -> quality = 1.0 - 0.2 = 0.8
        assert!((quality - 0.8).abs() < f64::EPSILON);
    }
}
