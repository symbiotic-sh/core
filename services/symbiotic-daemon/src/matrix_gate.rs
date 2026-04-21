//! MatrixHumanGate — sends approval requests via Matrix and awaits user response.
//!
//! Provides a `HumanGate` trait (local to the daemon) that bridges the
//! deliberation pipeline's need for human approval to the Matrix transport.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

/// Result of a human approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResult {
    Approved,
    Rejected { reason: String },
    Timeout,
}

/// A pending approval request waiting for user response.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub goal_id: String,
    pub room_id: String,
    pub message: String,
    pub created_at: u64,
    pub timeout: Duration,
    pub result: Option<ApprovalResult>,
}

/// Trait for sending approval requests to a human and collecting responses.
///
/// The daemon implements this by sending formatted messages to the goal's
/// Matrix room and waiting for a reply.
#[async_trait]
pub trait HumanGate: Send + Sync {
    /// Send an approval request to the user.
    /// Returns a request ID that can be used to track the response.
    async fn request_approval(&self, goal_id: &str, room_id: &str, message: &str)
        -> Result<String>;

    /// Wait for the user to respond to an approval request.
    async fn await_approval(&self, request_id: &str, timeout: Duration) -> Result<ApprovalResult>;

    /// Submit a user's response to a pending approval.
    fn submit_response(&self, goal_id: &str, result: ApprovalResult) -> Result<()>;
}

/// Matrix-backed human gate that queues approval requests and collects
/// responses from Matrix room messages.
pub struct MatrixHumanGate {
    /// Pending approval requests indexed by goal_id.
    pending: Arc<Mutex<Vec<PendingApproval>>>,
    /// Default timeout for approval requests.
    default_timeout: Duration,
}

impl MatrixHumanGate {
    pub fn new(default_timeout: Duration) -> Self {
        Self {
            pending: Arc::new(Mutex::new(Vec::new())),
            default_timeout,
        }
    }

    /// Check if a goal has a pending approval request.
    pub fn has_pending(&self, goal_id: &str) -> bool {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending
            .iter()
            .any(|p| p.goal_id == goal_id && p.result.is_none())
    }

    /// Get the pending approval for a goal (if any).
    pub fn get_pending(&self, goal_id: &str) -> Option<PendingApproval> {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending
            .iter()
            .find(|p| p.goal_id == goal_id && p.result.is_none())
            .cloned()
    }

    /// Format an approval request message for Matrix.
    pub fn format_approval_message(goal_id: &str, description: &str, confidence: f32) -> String {
        let confidence_pct = (confidence * 100.0) as u32;
        format!(
            "**Approval Required**\n\n\
             Goal: `{goal_id}`\n\
             Confidence: {confidence_pct}%\n\n\
             {description}\n\n\
             Reply `approve` or `reject <reason>` to respond."
        )
    }
}

#[async_trait]
impl HumanGate for MatrixHumanGate {
    async fn request_approval(
        &self,
        goal_id: &str,
        room_id: &str,
        message: &str,
    ) -> Result<String> {
        let request_id = format!("approval-{goal_id}");
        let approval = PendingApproval {
            goal_id: goal_id.to_string(),
            room_id: room_id.to_string(),
            message: message.to_string(),
            created_at: symbiotic_queue::now_unix(),
            timeout: self.default_timeout,
            result: None,
        };

        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow!("failed to lock pending approvals"))?;

        // Remove any existing pending approval for this goal.
        pending.retain(|p| p.goal_id != goal_id);
        pending.push(approval);

        Ok(request_id)
    }

    async fn await_approval(&self, request_id: &str, timeout: Duration) -> Result<ApprovalResult> {
        let goal_id = request_id.strip_prefix("approval-").unwrap_or(request_id);

        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(500);

        loop {
            // Check if we have a result.
            {
                let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(approval) = pending.iter().find(|p| p.goal_id == goal_id) {
                    if let Some(ref result) = approval.result {
                        return Ok(result.clone());
                    }
                }
            }

            if tokio::time::Instant::now() >= deadline {
                // Mark as timed out.
                let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(approval) = pending.iter_mut().find(|p| p.goal_id == goal_id) {
                    approval.result = Some(ApprovalResult::Timeout);
                }
                return Ok(ApprovalResult::Timeout);
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    fn submit_response(&self, goal_id: &str, result: ApprovalResult) -> Result<()> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow!("failed to lock pending approvals"))?;
        if let Some(approval) = pending.iter_mut().find(|p| p.goal_id == goal_id) {
            approval.result = Some(result);
            Ok(())
        } else {
            Err(anyhow!("no pending approval for goal {goal_id}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_approval_message() {
        let msg = MatrixHumanGate::format_approval_message("goal-1", "Deploy to prod", 0.85);
        assert!(msg.contains("goal-1"));
        assert!(msg.contains("85%"));
        assert!(msg.contains("Deploy to prod"));
        assert!(msg.contains("approve"));
        assert!(msg.contains("reject"));
    }

    #[test]
    fn test_matrix_gate_new() {
        let gate = MatrixHumanGate::new(Duration::from_secs(300));
        assert!(!gate.has_pending("goal-1"));
    }

    #[tokio::test]
    async fn test_request_and_submit_approval() {
        let gate = MatrixHumanGate::new(Duration::from_secs(300));

        let request_id = gate
            .request_approval("goal-1", "!room:test", "Please approve")
            .await
            .unwrap();
        assert_eq!(request_id, "approval-goal-1");
        assert!(gate.has_pending("goal-1"));

        // Submit approval.
        gate.submit_response("goal-1", ApprovalResult::Approved)
            .unwrap();

        // Await should return immediately.
        let result = gate
            .await_approval(&request_id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(result, ApprovalResult::Approved);
    }

    #[tokio::test]
    async fn test_request_and_submit_rejection() {
        let gate = MatrixHumanGate::new(Duration::from_secs(300));

        let request_id = gate
            .request_approval("goal-2", "!room:test", "Please approve")
            .await
            .unwrap();

        gate.submit_response(
            "goal-2",
            ApprovalResult::Rejected {
                reason: "Too risky".to_string(),
            },
        )
        .unwrap();

        let result = gate
            .await_approval(&request_id, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(matches!(result, ApprovalResult::Rejected { reason } if reason == "Too risky"));
    }

    #[tokio::test]
    async fn test_approval_timeout() {
        let gate = MatrixHumanGate::new(Duration::from_millis(100));

        let request_id = gate
            .request_approval("goal-3", "!room:test", "Please approve")
            .await
            .unwrap();

        // Don't submit a response — should time out.
        let result = gate
            .await_approval(&request_id, Duration::from_millis(200))
            .await
            .unwrap();
        assert_eq!(result, ApprovalResult::Timeout);
    }

    #[test]
    fn test_submit_response_no_pending() {
        let gate = MatrixHumanGate::new(Duration::from_secs(300));
        let result = gate.submit_response("nonexistent", ApprovalResult::Approved);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no pending"));
    }

    #[tokio::test]
    async fn test_request_replaces_existing() {
        let gate = MatrixHumanGate::new(Duration::from_secs(300));

        gate.request_approval("goal-1", "!room:a", "First request")
            .await
            .unwrap();
        assert!(gate.has_pending("goal-1"));

        // Second request for the same goal replaces the first.
        gate.request_approval("goal-1", "!room:b", "Second request")
            .await
            .unwrap();

        let pending = gate.get_pending("goal-1").unwrap();
        assert_eq!(pending.room_id, "!room:b");
        assert_eq!(pending.message, "Second request");
    }

    #[test]
    fn test_get_pending_none() {
        let gate = MatrixHumanGate::new(Duration::from_secs(300));
        assert!(gate.get_pending("nonexistent").is_none());
    }
}
