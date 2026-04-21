//! Tool metrics query bridge.
//!
//! Exposes a dedicated runner-side tool for querying daemon tool reliability
//! stats without going through the generic `tool.execute` path.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use symbiotic_core::protocol::{Tool, ToolResult};

use crate::BridgeClient;

#[derive(Debug, Clone)]
struct ToolStatsParams {
    tool_name: String,
    window_size: usize,
}

impl ToolStatsParams {
    fn from_value(value: serde_json::Value) -> Result<Self> {
        let tool_name = value
            .get("tool_name")
            .or_else(|| value.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing required parameter: tool_name"))?;
        let window_size = value
            .get("window_size")
            .and_then(|v| v.as_u64())
            .map(|value| value as usize)
            .unwrap_or(50);

        Ok(Self {
            tool_name,
            window_size,
        })
    }
}

#[derive(Debug, Clone)]
struct ToolAffinityParams {
    agent_id: Option<String>,
    window_size: usize,
}

impl ToolAffinityParams {
    fn from_value(value: serde_json::Value) -> Result<Self> {
        let agent_id = value
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let window_size = value
            .get("window_size")
            .and_then(|v| v.as_u64())
            .map(|value| value as usize)
            .unwrap_or(50);

        Ok(Self {
            agent_id,
            window_size,
        })
    }
}

pub fn tool_stats_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tool_name": {
                "type": "string",
                "description": "Tool name to inspect"
            },
            "window_size": {
                "type": "integer",
                "description": "How many recent invocations to consider (default: 50)"
            }
        },
        "required": ["tool_name"]
    })
}

pub fn tool_affinity_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "description": "Agent id to inspect. Defaults to the current runner agent."
            },
            "window_size": {
                "type": "integer",
                "description": "How many recent invocations per tool to consider (default: 50)"
            }
        }
    })
}

pub struct ToolStatsTool {
    bridge: Arc<BridgeClient>,
}

impl ToolStatsTool {
    pub fn new(bridge: Arc<BridgeClient>) -> Self {
        Self { bridge }
    }
}

pub struct ToolAffinityTool {
    bridge: Arc<BridgeClient>,
}

impl ToolAffinityTool {
    pub fn new(bridge: Arc<BridgeClient>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl Tool for ToolStatsTool {
    fn name(&self) -> &str {
        "tool_stats"
    }

    fn description(&self) -> &str {
        "Query recent reliability stats for a tool from the Nucleus."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        tool_stats_schema()
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let params = ToolStatsParams::from_value(params)?;
        let result = self
            .bridge
            .tool_stats(&params.tool_name, params.window_size)
            .await?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&result)
                .map_err(|e| anyhow!("failed to serialize tool stats: {e}"))?,
        })
    }
}

#[async_trait::async_trait]
impl Tool for ToolAffinityTool {
    fn name(&self) -> &str {
        "tool_affinity"
    }

    fn description(&self) -> &str {
        "Rank tools by how effectively an agent has used them recently."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        tool_affinity_schema()
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let params = ToolAffinityParams::from_value(params)?;
        let result = self
            .bridge
            .tool_affinity(params.agent_id.as_deref(), params.window_size)
            .await?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&result)
                .map_err(|e| anyhow!("failed to serialize tool affinity: {e}"))?,
        })
    }
}
