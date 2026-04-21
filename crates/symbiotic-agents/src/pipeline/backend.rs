use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::execution_plan::{Phase, PhaseResult};

/// How agent work is actually executed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionBackend {
    /// Current: Rust ReAct loop calling LLM API.
    #[default]
    Native,
    /// Spawn a `claude` CLI process on the VPS.
    ClaudeCode {
        working_dir: PathBuf,
        model: Option<String>,
        timeout_secs: u64,
    },
    /// Spawn a multi-agent Claude Code team.
    Team {
        team_size: usize,
        working_dir: PathBuf,
        model: Option<String>,
        member_timeout_secs: u64,
    },
}

/// Context for executing a goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalExecutionContext {
    pub goal_id: String,
    pub plan: crate::execution_plan::ExecutionPlan,
    pub backend: ExecutionBackend,
    pub spawned_agents: Vec<String>,
    pub phase_results: Vec<PhaseResult>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
}

/// Trait for executing work within a pipeline phase.
#[async_trait]
pub trait PhaseExecutor: Send + Sync {
    /// Execute the work for a single phase.
    /// Returns the output and a quality score (0.0 - 1.0).
    async fn execute_phase(
        &self,
        phase: &Phase,
        context: &GoalExecutionContext,
    ) -> Result<(String, f64)>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_plan::{ExecutionPlan, RollbackStrategy};

    #[test]
    fn test_execution_backend_serde_native() {
        let backend = ExecutionBackend::Native;
        let json = serde_json::to_string(&backend).unwrap();
        let deserialized: ExecutionBackend = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, ExecutionBackend::Native));
    }

    #[test]
    fn test_execution_backend_serde_claude_code() {
        let backend = ExecutionBackend::ClaudeCode {
            working_dir: PathBuf::from("/tmp/work"),
            model: Some("claude-sonnet".to_string()),
            timeout_secs: 300,
        };
        let json = serde_json::to_string(&backend).unwrap();
        let deserialized: ExecutionBackend = serde_json::from_str(&json).unwrap();
        match deserialized {
            ExecutionBackend::ClaudeCode {
                working_dir,
                model,
                timeout_secs,
            } => {
                assert_eq!(working_dir, PathBuf::from("/tmp/work"));
                assert_eq!(model, Some("claude-sonnet".to_string()));
                assert_eq!(timeout_secs, 300);
            }
            other => panic!("expected ClaudeCode, got {other:?}"),
        }
    }

    #[test]
    fn test_execution_backend_serde_team() {
        let backend = ExecutionBackend::Team {
            team_size: 3,
            working_dir: PathBuf::from("/tmp/team"),
            model: None,
            member_timeout_secs: 600,
        };
        let json = serde_json::to_string(&backend).unwrap();
        let deserialized: ExecutionBackend = serde_json::from_str(&json).unwrap();
        match deserialized {
            ExecutionBackend::Team {
                team_size,
                working_dir,
                model,
                member_timeout_secs,
            } => {
                assert_eq!(team_size, 3);
                assert_eq!(working_dir, PathBuf::from("/tmp/team"));
                assert!(model.is_none());
                assert_eq!(member_timeout_secs, 600);
            }
            other => panic!("expected Team, got {other:?}"),
        }
    }

    #[test]
    fn test_execution_backend_default() {
        let backend = ExecutionBackend::default();
        assert!(matches!(backend, ExecutionBackend::Native));
    }

    #[test]
    fn test_goal_execution_context_serde() {
        let ctx = GoalExecutionContext {
            goal_id: "goal-123".to_string(),
            plan: ExecutionPlan {
                name: "test plan".to_string(),
                phases: vec![],
                rollback_strategy: RollbackStrategy::None,
            },
            backend: ExecutionBackend::Native,
            spawned_agents: vec!["agent-1".to_string()],
            phase_results: vec![],
            started_at: 1000,
            completed_at: Some(2000),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: GoalExecutionContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.goal_id, "goal-123");
        assert_eq!(deserialized.plan.name, "test plan");
        assert!(matches!(deserialized.backend, ExecutionBackend::Native));
        assert_eq!(deserialized.spawned_agents.len(), 1);
        assert_eq!(deserialized.started_at, 1000);
        assert_eq!(deserialized.completed_at, Some(2000));
    }
}
