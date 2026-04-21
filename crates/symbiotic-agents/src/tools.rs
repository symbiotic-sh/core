//! Tool trait and built-in tools for agent execution.

pub use symbiotic_core::protocol::{format_tools_for_prompt, Tool, ToolResult};

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool;

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "dummy"
        }
        fn description(&self) -> &str {
            "A test tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}})
        }
        async fn execute(&self, params: serde_json::Value) -> anyhow::Result<ToolResult> {
            let x = params.get("x").and_then(|v| v.as_str()).unwrap_or("none");
            Ok(ToolResult {
                success: true,
                output: format!("got: {x}"),
            })
        }
    }

    #[tokio::test]
    async fn tool_trait_works() {
        let tool = DummyTool;
        assert_eq!(tool.name(), "dummy");
        assert_eq!(tool.description(), "A test tool");

        let result = tool
            .execute(serde_json::json!({"x": "hello"}))
            .await
            .expect("execute");
        assert!(result.success);
        assert_eq!(result.output, "got: hello");
    }

    #[test]
    fn format_tools_includes_tool_info() {
        let tool = DummyTool;
        let prompt = format_tools_for_prompt(&[&tool]);
        assert!(prompt.contains("dummy"));
        assert!(prompt.contains("A test tool"));
        assert!(prompt.contains("\"tool\""));
        assert!(prompt.contains("\"done\""));
    }
}
