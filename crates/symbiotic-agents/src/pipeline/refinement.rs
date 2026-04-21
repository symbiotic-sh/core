use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::execution_plan::{ExecutionPlan, Phase, Validation};

/// User's response to a presented plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum UserResponse {
    Approve,
    Edit {
        add_phases: Vec<Phase>,
        remove_phases: Vec<String>,
        modify_validations: HashMap<String, Vec<Validation>>,
        instructions: String,
    },
    Reject {
        reason: String,
    },
    Timeout,
}

/// A single refinement in the history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinementRecord {
    pub timestamp: u64,
    pub actor: String,
    pub action: String,
    pub plan_snapshot: ExecutionPlan,
    pub confidence: f32,
    pub diff_summary: String,
}

/// Tracks the refinement history for a goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinementHistory {
    pub goal_id: String,
    pub records: Vec<RefinementRecord>,
}

impl RefinementHistory {
    pub fn new(goal_id: String) -> Self {
        Self {
            goal_id,
            records: Vec::new(),
        }
    }

    pub fn record(&mut self, record: RefinementRecord) {
        self.records.push(record);
    }

    pub fn latest_plan(&self) -> Option<&ExecutionPlan> {
        self.records.last().map(|r| &r.plan_snapshot)
    }
}

/// Manages user interaction for plan refinement.
#[async_trait]
pub trait UserInteraction: Send + Sync {
    /// Present a plan to the user and wait for their response.
    async fn present_plan(
        &self,
        goal_id: &str,
        plan: &ExecutionPlan,
        confidence: f32,
        room_id: &str,
    ) -> Result<()>;

    /// Wait for user response (approve, edit, reject).
    async fn await_response(&self, goal_id: &str, timeout: Duration) -> Result<UserResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_plan::{Phase, RollbackStrategy, Validation};

    fn sample_plan(name: &str) -> ExecutionPlan {
        ExecutionPlan {
            name: name.to_string(),
            phases: vec![Phase {
                name: "build".to_string(),
                description: "Build it".to_string(),
                validations: vec![Validation::LintClean {
                    command: "cargo clippy".to_string(),
                }],
            }],
            rollback_strategy: RollbackStrategy::None,
        }
    }

    #[test]
    fn test_user_response_serde_approve() {
        let response = UserResponse::Approve;
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: UserResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, UserResponse::Approve));
    }

    #[test]
    fn test_user_response_serde_edit() {
        let response = UserResponse::Edit {
            add_phases: vec![Phase {
                name: "test".to_string(),
                description: "Test phase".to_string(),
                validations: vec![],
            }],
            remove_phases: vec!["old-phase".to_string()],
            modify_validations: {
                let mut m = HashMap::new();
                m.insert(
                    "build".to_string(),
                    vec![Validation::TestPass {
                        test_pattern: "cargo test".to_string(),
                        description: "Tests pass".to_string(),
                    }],
                );
                m
            },
            instructions: "Add testing".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: UserResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            UserResponse::Edit {
                add_phases,
                remove_phases,
                modify_validations,
                instructions,
            } => {
                assert_eq!(add_phases.len(), 1);
                assert_eq!(add_phases[0].name, "test");
                assert_eq!(remove_phases, vec!["old-phase"]);
                assert!(modify_validations.contains_key("build"));
                assert_eq!(instructions, "Add testing");
            }
            other => panic!("expected Edit, got {other:?}"),
        }
    }

    #[test]
    fn test_user_response_serde_reject() {
        let response = UserResponse::Reject {
            reason: "Too complex".to_string(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: UserResponse = serde_json::from_str(&json).unwrap();
        match deserialized {
            UserResponse::Reject { reason } => assert_eq!(reason, "Too complex"),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn test_refinement_history_record_and_latest() {
        let mut history = RefinementHistory::new("goal-1".to_string());
        assert!(history.latest_plan().is_none());

        history.record(RefinementRecord {
            timestamp: 1000,
            actor: "user".to_string(),
            action: "initial".to_string(),
            plan_snapshot: sample_plan("plan-v1"),
            confidence: 0.8,
            diff_summary: "Initial plan".to_string(),
        });

        assert_eq!(history.records.len(), 1);
        let latest = history.latest_plan().expect("should have a plan");
        assert_eq!(latest.name, "plan-v1");

        history.record(RefinementRecord {
            timestamp: 2000,
            actor: "system".to_string(),
            action: "refined".to_string(),
            plan_snapshot: sample_plan("plan-v2"),
            confidence: 0.9,
            diff_summary: "Added test phase".to_string(),
        });

        assert_eq!(history.records.len(), 2);
        let latest = history.latest_plan().expect("should have a plan");
        assert_eq!(latest.name, "plan-v2");
    }

    #[test]
    fn test_refinement_history_empty() {
        let history = RefinementHistory::new("goal-empty".to_string());
        assert!(history.latest_plan().is_none());
        assert!(history.records.is_empty());
        assert_eq!(history.goal_id, "goal-empty");
    }
}
