use serde::{Deserialize, Serialize};

use crate::claims::ScopeRequirement;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItem {
    pub id: String,
    pub project_id: String,
    pub initiative_id: Option<String>,
    pub parent_work_item_id: Option<String>,
    pub kind: WorkItemKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub status: WorkItemStatus,
    pub priority: WorkPriority,
    pub urgency: WorkUrgency,
    pub assignment_mode: AssignmentMode,
    pub requested_scopes: Vec<ScopeRequirement>,
    pub accepted_claim_ids: Vec<String>,
    pub assignee: Option<AgentAssignment>,
    pub blocked_by: Vec<String>,
    pub depends_on: Vec<String>,
    pub review_mode: ReviewMode,
    pub cancellation: Option<CancellationState>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemKind {
    Goal,
    Task,
    Execution,
    DevelopmentArtifact,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    Todo,
    ClaimPending,
    Claimed,
    Running,
    Blocked,
    PendingReview,
    Done,
    Cancelled,
    Expired,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum WorkPriority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkUrgency {
    Immediate,
    Normal,
    Deferred,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentMode {
    SingleOwner,
    ParallelChildren,
    ReviewOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAssignment {
    pub agent_id: String,
    pub runner_id: Option<String>,
    pub assigned_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    NoReview,
    HumanRequired,
    AutoReviewThenHumanIfNeeded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancellationState {
    pub requested_at: i64,
    pub requested_by: String,
    pub reason: String,
    pub hard_stop: bool,
}

impl WorkItem {
    pub fn touch(&mut self, observed_at: i64) {
        self.updated_at = observed_at;
    }

    pub fn set_status(&mut self, status: WorkItemStatus, observed_at: i64) {
        self.status = status;
        self.touch(observed_at);
    }

    pub fn add_claim(&mut self, claim_id: String, observed_at: i64) {
        if !self.accepted_claim_ids.iter().any(|id| id == &claim_id) {
            self.accepted_claim_ids.push(claim_id);
        }
        self.touch(observed_at);
    }
}
