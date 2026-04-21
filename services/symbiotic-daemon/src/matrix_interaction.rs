//! MatrixUserInteraction — implements the `UserInteraction` trait from the
//! deliberation pipeline, bridging plan presentation and response collection
//! to the Matrix transport.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use symbiotic_agents::execution_plan::ExecutionPlan;
use symbiotic_agents::pipeline::refinement::{UserInteraction, UserResponse};

use crate::matrix_gate::{ApprovalResult, HumanGate, MatrixHumanGate};

/// Bridges the `UserInteraction` trait to the Matrix transport via `MatrixHumanGate`.
///
/// When the pipeline needs to present a plan to the user, this implementation:
/// 1. Formats the `ExecutionPlan` as readable Markdown.
/// 2. Sends it to the goal's Matrix room via the human gate.
/// 3. Waits for the user to reply with approve/edit/reject.
pub struct MatrixUserInteraction {
    gate: Arc<MatrixHumanGate>,
    /// Messages that were presented (for inspection in tests).
    presented: Mutex<Vec<PresentedPlan>>,
}

/// Record of a plan that was presented to the user.
#[derive(Debug, Clone)]
pub struct PresentedPlan {
    pub goal_id: String,
    pub room_id: String,
    pub plan_name: String,
    pub confidence: f32,
    pub message: String,
}

impl MatrixUserInteraction {
    pub fn new(gate: Arc<MatrixHumanGate>) -> Self {
        Self {
            gate,
            presented: Mutex::new(Vec::new()),
        }
    }

    /// Get the list of presented plans (for testing).
    pub fn presented_plans(&self) -> Vec<PresentedPlan> {
        self.presented
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Format an `ExecutionPlan` as human-readable Markdown for Matrix.
    pub fn format_plan(plan: &ExecutionPlan, confidence: f32) -> String {
        let confidence_pct = (confidence * 100.0) as u32;
        let mut output = format!(
            "**Execution Plan: {}**\nConfidence: {}%\n\n",
            plan.name, confidence_pct
        );

        for (i, phase) in plan.phases.iter().enumerate() {
            output.push_str(&format!(
                "**Phase {} — {}**\n{}\n",
                i + 1,
                phase.name,
                phase.description
            ));
            if !phase.validations.is_empty() {
                output.push_str("Validations:\n");
                for validation in &phase.validations {
                    let label = match validation {
                        symbiotic_agents::execution_plan::Validation::TestPass {
                            description,
                            ..
                        } => format!("Test: {description}"),
                        symbiotic_agents::execution_plan::Validation::LintClean { command } => {
                            format!("Lint: {command}")
                        }
                        symbiotic_agents::execution_plan::Validation::HumanReview {
                            criteria,
                            ..
                        } => format!("Review: {criteria}"),
                        symbiotic_agents::execution_plan::Validation::ExpertAgent {
                            agent_type,
                            ..
                        } => format!("Expert: {agent_type}"),
                        symbiotic_agents::execution_plan::Validation::HumanApproval => {
                            "Human Approval Required".to_string()
                        }
                    };
                    output.push_str(&format!("  - {label}\n"));
                }
            }
            output.push('\n');
        }

        output.push_str("Reply `approve`, `reject <reason>`, or describe edits.");
        output
    }
}

#[async_trait]
impl UserInteraction for MatrixUserInteraction {
    async fn present_plan(
        &self,
        goal_id: &str,
        plan: &ExecutionPlan,
        confidence: f32,
        room_id: &str,
    ) -> Result<()> {
        let message = Self::format_plan(plan, confidence);

        // Record the presentation.
        {
            let mut presented = self
                .presented
                .lock()
                .map_err(|_| anyhow!("failed to lock presented plans"))?;
            presented.push(PresentedPlan {
                goal_id: goal_id.to_string(),
                room_id: room_id.to_string(),
                plan_name: plan.name.clone(),
                confidence,
                message: message.clone(),
            });
        }

        // Send the approval request via the human gate.
        self.gate
            .request_approval(goal_id, room_id, &message)
            .await?;

        Ok(())
    }

    async fn await_response(&self, goal_id: &str, timeout: Duration) -> Result<UserResponse> {
        let request_id = format!("approval-{goal_id}");
        let result = self.gate.await_approval(&request_id, timeout).await?;

        match result {
            ApprovalResult::Approved => Ok(UserResponse::Approve),
            ApprovalResult::Rejected { reason } => Ok(UserResponse::Reject { reason }),
            ApprovalResult::Timeout => Ok(UserResponse::Timeout),
        }
    }
}

/// Parse a user's Matrix message into an `ApprovalResult`.
///
/// Recognized patterns:
/// - `approve` / `yes` / `ok` -> Approved
/// - `reject <reason>` / `no <reason>` -> Rejected
/// - Anything else -> None (not an approval response)
pub fn parse_approval_response(body: &str) -> Option<ApprovalResult> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();

    if lower == "approve" || lower == "yes" || lower == "ok" || lower == "lgtm" {
        return Some(ApprovalResult::Approved);
    }

    if let Some(rest) = lower
        .strip_prefix("reject")
        .or_else(|| lower.strip_prefix("no"))
    {
        let reason = rest.trim().to_string();
        if reason.is_empty() {
            return Some(ApprovalResult::Rejected {
                reason: "Rejected by user".to_string(),
            });
        }
        return Some(ApprovalResult::Rejected { reason });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use symbiotic_agents::execution_plan::{Phase, RollbackStrategy, Validation};

    fn sample_plan() -> ExecutionPlan {
        ExecutionPlan {
            name: "deploy-service".to_string(),
            phases: vec![
                Phase {
                    name: "build".to_string(),
                    description: "Build the service binary".to_string(),
                    validations: vec![
                        Validation::LintClean {
                            command: "cargo clippy".to_string(),
                        },
                        Validation::TestPass {
                            test_pattern: "cargo test".to_string(),
                            description: "All tests pass".to_string(),
                        },
                    ],
                },
                Phase {
                    name: "deploy".to_string(),
                    description: "Deploy to production".to_string(),
                    validations: vec![],
                },
            ],
            rollback_strategy: RollbackStrategy::None,
        }
    }

    #[test]
    fn test_format_plan() {
        let plan = sample_plan();
        let formatted = MatrixUserInteraction::format_plan(&plan, 0.85);

        assert!(formatted.contains("deploy-service"));
        assert!(formatted.contains("85%"));
        assert!(formatted.contains("Phase 1"));
        assert!(formatted.contains("build"));
        assert!(formatted.contains("cargo clippy"));
        assert!(formatted.contains("All tests pass"));
        assert!(formatted.contains("Phase 2"));
        assert!(formatted.contains("deploy"));
        assert!(formatted.contains("approve"));
    }

    #[test]
    fn test_format_plan_zero_confidence() {
        let plan = sample_plan();
        let formatted = MatrixUserInteraction::format_plan(&plan, 0.0);
        assert!(formatted.contains("0%"));
    }

    #[test]
    fn test_format_plan_full_confidence() {
        let plan = sample_plan();
        let formatted = MatrixUserInteraction::format_plan(&plan, 1.0);
        assert!(formatted.contains("100%"));
    }

    #[tokio::test]
    async fn test_present_plan_records_presentation() {
        let gate = Arc::new(MatrixHumanGate::new(Duration::from_secs(300)));
        let interaction = MatrixUserInteraction::new(gate);

        let plan = sample_plan();
        interaction
            .present_plan("goal-1", &plan, 0.9, "!room:test")
            .await
            .unwrap();

        let presented = interaction.presented_plans();
        assert_eq!(presented.len(), 1);
        assert_eq!(presented[0].goal_id, "goal-1");
        assert_eq!(presented[0].room_id, "!room:test");
        assert_eq!(presented[0].plan_name, "deploy-service");
        assert!((presented[0].confidence - 0.9).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_present_then_approve() {
        let gate = Arc::new(MatrixHumanGate::new(Duration::from_secs(300)));
        let interaction = MatrixUserInteraction::new(gate.clone());

        let plan = sample_plan();
        interaction
            .present_plan("goal-2", &plan, 0.9, "!room:test")
            .await
            .unwrap();

        // Simulate user approval.
        gate.submit_response("goal-2", ApprovalResult::Approved)
            .unwrap();

        let response = interaction
            .await_response("goal-2", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(matches!(response, UserResponse::Approve));
    }

    #[tokio::test]
    async fn test_present_then_reject() {
        let gate = Arc::new(MatrixHumanGate::new(Duration::from_secs(300)));
        let interaction = MatrixUserInteraction::new(gate.clone());

        let plan = sample_plan();
        interaction
            .present_plan("goal-3", &plan, 0.5, "!room:test")
            .await
            .unwrap();

        gate.submit_response(
            "goal-3",
            ApprovalResult::Rejected {
                reason: "Not ready".to_string(),
            },
        )
        .unwrap();

        let response = interaction
            .await_response("goal-3", Duration::from_secs(1))
            .await
            .unwrap();
        match response {
            UserResponse::Reject { reason } => assert_eq!(reason, "Not ready"),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_present_then_timeout() {
        let gate = Arc::new(MatrixHumanGate::new(Duration::from_millis(50)));
        let interaction = MatrixUserInteraction::new(gate);

        let plan = sample_plan();
        interaction
            .present_plan("goal-4", &plan, 0.3, "!room:test")
            .await
            .unwrap();

        let response = interaction
            .await_response("goal-4", Duration::from_millis(100))
            .await
            .unwrap();
        assert!(matches!(response, UserResponse::Timeout));
    }

    #[test]
    fn test_parse_approval_response_approve() {
        assert_eq!(
            parse_approval_response("approve"),
            Some(ApprovalResult::Approved)
        );
        assert_eq!(
            parse_approval_response("yes"),
            Some(ApprovalResult::Approved)
        );
        assert_eq!(
            parse_approval_response("ok"),
            Some(ApprovalResult::Approved)
        );
        assert_eq!(
            parse_approval_response("lgtm"),
            Some(ApprovalResult::Approved)
        );
        assert_eq!(
            parse_approval_response("  APPROVE  "),
            Some(ApprovalResult::Approved)
        );
    }

    #[test]
    fn test_parse_approval_response_reject() {
        assert_eq!(
            parse_approval_response("reject too risky"),
            Some(ApprovalResult::Rejected {
                reason: "too risky".to_string()
            })
        );
        assert_eq!(
            parse_approval_response("no not now"),
            Some(ApprovalResult::Rejected {
                reason: "not now".to_string()
            })
        );
        assert_eq!(
            parse_approval_response("reject"),
            Some(ApprovalResult::Rejected {
                reason: "Rejected by user".to_string()
            })
        );
    }

    #[test]
    fn test_parse_approval_response_unknown() {
        assert_eq!(parse_approval_response(""), None);
        assert_eq!(parse_approval_response("hello world"), None);
        assert_eq!(parse_approval_response("maybe later"), None);
    }
}
