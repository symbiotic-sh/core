//! Durable per-thread observability projections for truthful Tier 3 UI.
//!
//! Phase 1 introduced the `OperationsPillSummary` needed by the real thread
//! view. The next narrow steps add a truthful per-thread operations snapshot,
//! persisted branch/review/merge artifacts, and a limited chatter lane derived
//! from daemon-owned bridge interaction logs, still without inventing raw
//! reasoning logs that the daemon does not yet persist cleanly.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use symbiotic_control_plane::{
    manifest::ManifestParser, types::GoalEventManifest, CollaborationScope, ManagementStore,
    WorkItem, WorkItemKind, WorkItemStatus,
};
use symbiotic_matrix::events::MatrixEventEnvelope;
use symbiotic_matrix::transport::MatrixTransport;

use crate::agent_runtime_status::{AgentRuntimeStatusKind, AgentRuntimeStatusStore};
use crate::bridge_interactions::{
    AgentRuntimeLogEntryType, AgentRuntimeLogStore, BridgeInteractionKind,
    BridgeInteractionLogStore,
};
use crate::goal_state::{load_goal_log_records, load_goal_states, GoalLogRecord, GoalState};
use crate::SymbioticDaemon;

const SUMMARY_ACTION: &str = "thread.observability.summary";
const SNAPSHOT_ACTION: &str = "thread.observability.snapshot";
const ACTIVE_AGENT_WINDOW_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OperationHeadline {
    pub title: String,
    pub status: OperationHeadlineStatus,
    pub current_step: Option<String>,
    pub progress_percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationHeadlineStatus {
    Running,
    Waiting,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OperationsPillSummary {
    pub thread_id: String,
    pub active_operation_count: u32,
    pub primary_operation: Option<OperationHeadline>,
    pub waiting_for_user: bool,
    pub has_failure: bool,
    pub total_active_agents: Option<u32>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadOperationCard {
    pub operation_id: String,
    pub title: String,
    pub status: OperationHeadlineStatus,
    pub current_step: Option<String>,
    pub owner_label: Option<String>,
    pub progress_percent: Option<u8>,
    pub active_agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadExecutionUpdate {
    pub operation_id: String,
    pub label: String,
    pub detail: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadChatterEvent {
    pub event_id: String,
    pub operation_id: Option<String>,
    pub from_agent: String,
    pub to: String,
    pub kind: ThreadChatterKind,
    pub message: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadChatterKind {
    Question,
    Plan,
    Auth,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadOperationArtifact {
    pub artifact_id: String,
    pub operation_id: String,
    pub kind: ThreadArtifactKind,
    pub status: ThreadArtifactStatus,
    pub label: String,
    pub detail: String,
    pub target: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadAgentLogEntry {
    pub event_id: String,
    pub operation_id: Option<String>,
    pub agent_id: String,
    pub entry_type: ThreadAgentLogEntryType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_params: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadAgentRuntimeStatus {
    pub agent_id: String,
    pub operation_id: Option<String>,
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub status: AgentRuntimeStatusKind,
    pub detail: Option<String>,
    pub current_iteration: Option<u32>,
    pub max_iterations: Option<u32>,
    pub active_tool_name: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadAgentLogEntryType {
    Tool,
    Result,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadArtifactKind {
    Branch,
    Review,
    Merge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadArtifactStatus {
    Active,
    PendingReview,
    Merged,
    Closed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadObservabilitySnapshot {
    pub thread_id: String,
    pub operations: Vec<ThreadOperationCard>,
    pub updates: Vec<ThreadExecutionUpdate>,
    pub chatter: Vec<ThreadChatterEvent>,
    pub agent_statuses: Vec<ThreadAgentRuntimeStatus>,
    pub agent_logs: Vec<ThreadAgentLogEntry>,
    pub artifacts: Vec<ThreadOperationArtifact>,
    pub updated_at: i64,
}

impl OperationsPillSummary {
    pub fn is_empty(&self) -> bool {
        self.active_operation_count == 0
            && self.primary_operation.is_none()
            && !self.waiting_for_user
            && !self.has_failure
            && self.total_active_agents.unwrap_or(0) == 0
    }

    pub fn to_envelope(&self, now: u64) -> MatrixEventEnvelope {
        let body = if self.is_empty() {
            "Thread observability cleared"
        } else {
            "Thread observability updated"
        };
        let mut envelope = MatrixEventEnvelope::state(SUMMARY_ACTION, now, body)
            .with_thread(&self.thread_id)
            .with_detail_field("thread_id", self.thread_id.clone())
            .with_detail_field("active_operation_count", self.active_operation_count)
            .with_detail_field("waiting_for_user", self.waiting_for_user)
            .with_detail_field("has_failure", self.has_failure)
            .with_detail_field("updated_at", self.updated_at);

        if let Some(primary) = &self.primary_operation {
            envelope = envelope.with_detail_field(
                "primary_operation",
                serde_json::to_value(primary).unwrap_or(serde_json::Value::Null),
            );
        } else {
            envelope = envelope.with_detail_field("primary_operation", serde_json::Value::Null);
        }

        if let Some(total_active_agents) = self.total_active_agents {
            envelope = envelope.with_detail_field("total_active_agents", total_active_agents);
        }

        envelope
    }
}

impl ThreadObservabilitySnapshot {
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
            && self.updates.is_empty()
            && self.chatter.is_empty()
            && self.agent_statuses.is_empty()
            && self.agent_logs.is_empty()
            && self.artifacts.is_empty()
    }

    pub fn to_envelope(&self, now: u64) -> MatrixEventEnvelope {
        let body = if self.is_empty() {
            "Thread observability snapshot cleared"
        } else {
            "Thread observability snapshot updated"
        };
        MatrixEventEnvelope::state(SNAPSHOT_ACTION, now, body)
            .with_thread(&self.thread_id)
            .with_detail_field("thread_id", self.thread_id.clone())
            .with_detail_field(
                "operations",
                serde_json::to_value(&self.operations).unwrap_or(serde_json::Value::Array(vec![])),
            )
            .with_detail_field(
                "updates",
                serde_json::to_value(&self.updates).unwrap_or(serde_json::Value::Array(vec![])),
            )
            .with_detail_field(
                "chatter",
                serde_json::to_value(&self.chatter).unwrap_or(serde_json::Value::Array(vec![])),
            )
            .with_detail_field(
                "agent_statuses",
                serde_json::to_value(&self.agent_statuses)
                    .unwrap_or(serde_json::Value::Array(vec![])),
            )
            .with_detail_field(
                "agent_logs",
                serde_json::to_value(&self.agent_logs).unwrap_or(serde_json::Value::Array(vec![])),
            )
            .with_detail_field(
                "artifacts",
                serde_json::to_value(&self.artifacts).unwrap_or(serde_json::Value::Array(vec![])),
            )
            .with_detail_field("updated_at", self.updated_at)
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PersistedThreadObservability {
    #[serde(default)]
    summaries: Vec<OperationsPillSummary>,
    #[serde(default)]
    snapshots: Vec<ThreadObservabilitySnapshot>,
}

#[derive(Debug, Clone)]
pub struct ThreadObservabilityStore {
    data_dir: PathBuf,
    summaries: HashMap<String, OperationsPillSummary>,
    snapshots: HashMap<String, ThreadObservabilitySnapshot>,
}

impl ThreadObservabilityStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            summaries: HashMap::new(),
            snapshots: HashMap::new(),
        }
    }

    fn store_path(&self) -> PathBuf {
        self.data_dir.join("threads").join("observability.json")
    }

    pub fn load(&mut self) -> Result<()> {
        let path = self.store_path();
        if !path.exists() {
            return Ok(());
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let persisted = serde_json::from_str::<PersistedThreadObservability>(&data)
            .or_else(|_| {
                serde_json::from_str::<Vec<OperationsPillSummary>>(&data).map(|summaries| {
                    PersistedThreadObservability {
                        summaries,
                        snapshots: Vec::new(),
                    }
                })
            })
            .with_context(|| format!("failed to decode {}", path.display()))?;
        self.summaries.clear();
        self.snapshots.clear();
        for summary in persisted.summaries {
            self.summaries.insert(summary.thread_id.clone(), summary);
        }
        for snapshot in persisted.snapshots {
            self.snapshots.insert(snapshot.thread_id.clone(), snapshot);
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let path = self.store_path();
        let dir = path
            .parent()
            .expect("thread observability path should have parent");
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let mut summaries: Vec<&OperationsPillSummary> = self.summaries.values().collect();
        summaries.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        let mut snapshots: Vec<&ThreadObservabilitySnapshot> = self.snapshots.values().collect();
        snapshots.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        let json = serde_json::to_string_pretty(&PersistedThreadObservability {
            summaries: summaries.into_iter().cloned().collect(),
            snapshots: snapshots.into_iter().cloned().collect(),
        })
        .with_context(|| format!("failed to encode {}", path.display()))?;
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, json.as_bytes())
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }

    pub fn get(&self, thread_id: &str) -> Option<&OperationsPillSummary> {
        self.summaries.get(thread_id)
    }

    pub fn upsert(&mut self, summary: OperationsPillSummary) -> Result<()> {
        if summary.is_empty() {
            self.summaries.remove(&summary.thread_id);
        } else {
            self.summaries.insert(summary.thread_id.clone(), summary);
        }
        self.save()
    }

    pub fn get_snapshot(&self, thread_id: &str) -> Option<&ThreadObservabilitySnapshot> {
        self.snapshots.get(thread_id)
    }

    pub fn upsert_snapshot(&mut self, snapshot: ThreadObservabilitySnapshot) -> Result<()> {
        if snapshot.is_empty() {
            self.snapshots.remove(&snapshot.thread_id);
        } else {
            self.snapshots.insert(snapshot.thread_id.clone(), snapshot);
        }
        self.save()
    }
}

fn prettify_template(template: &str) -> String {
    let stripped = template
        .strip_prefix("inquisition:")
        .unwrap_or(template)
        .replace(['_', '-'], " ");
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return "Goal".to_string();
    }
    trimmed
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_waiting_status(status: &str) -> bool {
    matches!(
        status,
        "awaiting_input" | "awaiting_approval" | "awaiting_auth"
    )
}

fn is_active_status(status: &str) -> bool {
    matches!(
        status,
        "running"
            | "queued"
            | "awaiting_input"
            | "awaiting_approval"
            | "awaiting_auth"
            | "deliberating"
            | "retry"
    )
}

fn is_failure_status(status: &str) -> bool {
    matches!(status, "failed" | "retry" | "dlq" | "rejected")
}

fn operation_status_for_goal(goal: &GoalState) -> OperationHeadlineStatus {
    if is_waiting_status(&goal.status) {
        OperationHeadlineStatus::Waiting
    } else if is_failure_status(&goal.status) {
        OperationHeadlineStatus::Failed
    } else {
        OperationHeadlineStatus::Running
    }
}

fn goal_attaches_to_thread(goal: &GoalState, thread_id: &str) -> bool {
    goal.thread_id.as_deref() == Some(thread_id) || goal.last_run_id.as_deref() == Some(thread_id)
}

pub fn derive_operations_pill_summary(
    thread_id: &str,
    thread_title: Option<&str>,
    goal_states: &[GoalState],
    management_store: &ManagementStore,
    agent_runtime_status_store: &AgentRuntimeStatusStore,
    agent_runtime_log_store: &AgentRuntimeLogStore,
    updated_at: i64,
) -> OperationsPillSummary {
    let matching: Vec<&GoalState> = goal_states
        .iter()
        .filter(|goal| goal_attaches_to_thread(goal, thread_id))
        .collect();
    let active_agent_count = derive_recent_active_agents(
        thread_id,
        &matching,
        agent_runtime_status_store,
        agent_runtime_log_store,
        updated_at as u64,
    )
    .len() as u32;
    let active_agents_by_scope = derive_recent_active_agents_by_scope(
        &[],
        &matching,
        agent_runtime_log_store,
        updated_at as u64,
    );
    let task_operations = derive_thread_task_operations(
        thread_id,
        &matching,
        management_store,
        &active_agents_by_scope,
    );

    let waiting_for_user = if task_operations.is_empty() {
        matching.iter().any(|goal| is_waiting_status(&goal.status))
    } else {
        task_operations.iter().any(operation_waiting_for_human)
    };
    let has_failure = if task_operations.is_empty() {
        matching.iter().any(|goal| is_failure_status(&goal.status))
    } else {
        task_operations
            .iter()
            .any(|operation| operation.status == OperationHeadlineStatus::Failed)
    };

    let (active_operation_count, primary_operation) = if task_operations.is_empty() {
        let active_operation_count = matching
            .iter()
            .filter(|goal| is_active_status(&goal.status) && !is_failure_status(&goal.status))
            .count() as u32;

        let primary_goal = matching
            .iter()
            .filter(|goal| is_active_status(&goal.status))
            .max_by_key(|goal| goal.updated_at)
            .copied()
            .or_else(|| {
                matching
                    .iter()
                    .filter(|goal| is_failure_status(&goal.status))
                    .max_by_key(|goal| goal.updated_at)
                    .copied()
            });
        let primary_operation = primary_goal.map(|goal| OperationHeadline {
            title: thread_title
                .filter(|title| !title.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| prettify_template(&goal.template)),
            status: operation_status_for_goal(goal),
            current_step: goal.pipeline_stage.clone(),
            progress_percent: None,
        });
        (active_operation_count, primary_operation)
    } else {
        let active_operation_count = task_operations
            .iter()
            .filter(|operation| operation.status != OperationHeadlineStatus::Failed)
            .count() as u32;
        let primary_operation = task_operations.first().map(|operation| OperationHeadline {
            title: operation.title.clone(),
            status: operation.status,
            current_step: operation.current_step.clone(),
            progress_percent: operation.progress_percent,
        });
        (active_operation_count, primary_operation)
    };

    OperationsPillSummary {
        thread_id: thread_id.to_string(),
        active_operation_count,
        primary_operation,
        waiting_for_user,
        has_failure,
        total_active_agents: Some(active_agent_count).filter(|count| *count > 0),
        updated_at,
    }
}

fn operation_waiting_for_human(operation: &ThreadOperationCard) -> bool {
    operation.status == OperationHeadlineStatus::Waiting
        && operation
            .owner_label
            .as_deref()
            .is_some_and(|owner| !owner.starts_with("role:"))
}

pub(crate) fn derive_thread_observability_snapshot(
    thread_id: &str,
    archive_root: &Path,
    goal_states: &[GoalState],
    goal_logs: &[GoalLogRecord],
    management_store: &ManagementStore,
    interaction_log_store: &BridgeInteractionLogStore,
    agent_runtime_status_store: &AgentRuntimeStatusStore,
    agent_runtime_log_store: &AgentRuntimeLogStore,
    updated_at: i64,
) -> ThreadObservabilitySnapshot {
    let matching_goals: Vec<&GoalState> = goal_states
        .iter()
        .filter(|goal| goal_attaches_to_thread(goal, thread_id))
        .collect();
    let agent_statuses =
        derive_thread_agent_statuses(thread_id, &matching_goals, agent_runtime_status_store);
    let active_agents_by_scope = derive_recent_active_agents_by_scope(
        &agent_statuses,
        &matching_goals,
        agent_runtime_log_store,
        updated_at as u64,
    );

    let mut operations = derive_thread_task_operations(
        thread_id,
        &matching_goals,
        management_store,
        &active_agents_by_scope,
    );
    if operations.is_empty() {
        operations = matching_goals
            .iter()
            .copied()
            .filter(|goal| is_active_status(&goal.status) || is_failure_status(&goal.status))
            .map(|goal| ThreadOperationCard {
                operation_id: format!("{}:{}", goal.template, goal.last_job_id),
                title: prettify_template(&goal.template),
                status: operation_status_for_goal(goal),
                current_step: goal.pipeline_stage.clone(),
                owner_label: goal.owner.clone(),
                progress_percent: None,
                active_agents: goal
                    .last_run_id
                    .as_ref()
                    .and_then(|scope| active_agents_by_scope.get(scope))
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        operations.sort_by(|a, b| a.operation_id.cmp(&b.operation_id));
    }

    let matching_keys: Vec<(&str, &str)> = matching_goals
        .iter()
        .map(|goal| (goal.goal_room.as_str(), goal.template.as_str()))
        .collect();
    let mut updates: Vec<ThreadExecutionUpdate> = goal_logs
        .iter()
        .filter(|record| {
            matching_keys.iter().any(|(goal_room, template)| {
                record.goal_room.as_deref() == Some(*goal_room) && record.template == *template
            })
        })
        .filter(|record| {
            matches!(
                record.event.as_str(),
                "goal.started"
                    | "goal.question"
                    | "goal.failed"
                    | "goal.completed"
                    | "goal.step.started"
                    | "goal.step.completed"
                    | "goal.step.failed"
            )
        })
        .map(|record| ThreadExecutionUpdate {
            operation_id: format!("{}:{}", record.template, record.workflow_job_id),
            label: prettify_event_label(&record.event),
            detail: prettify_detail(&record.detail),
            created_at: record.ts as i64,
        })
        .collect();
    updates.extend(derive_archive_goal_updates(
        thread_id,
        archive_root,
        &matching_goals,
        management_store,
    ));
    updates.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    updates.truncate(8);
    let chatter = derive_thread_chatter(thread_id, &matching_goals, interaction_log_store);
    let agent_logs = derive_thread_agent_logs(thread_id, &matching_goals, agent_runtime_log_store);
    let artifacts = derive_thread_artifacts(thread_id, &matching_goals, management_store);

    ThreadObservabilitySnapshot {
        thread_id: thread_id.to_string(),
        operations,
        updates,
        chatter,
        agent_statuses,
        agent_logs,
        artifacts,
        updated_at,
    }
}

fn prettify_event_label(event: &str) -> String {
    event.replace("goal.", "").replace('.', " ")
}

fn prettify_detail(detail: &str) -> String {
    let trimmed = detail.trim();
    if trimmed.is_empty() {
        "Execution update".to_string()
    } else {
        trimmed.replace(['_', '='], " ")
    }
}

fn owner_label_for_work_item(work_item: &WorkItem) -> Option<String> {
    work_item
        .assignee
        .as_ref()
        .map(|assignment| assignment.agent_id.clone())
}

fn operation_status_for_work_item(work_item: &WorkItem) -> Option<OperationHeadlineStatus> {
    match work_item.status {
        WorkItemStatus::Todo
        | WorkItemStatus::Blocked
        | WorkItemStatus::PendingReview
        | WorkItemStatus::Cancelled => Some(OperationHeadlineStatus::Waiting),
        WorkItemStatus::ClaimPending | WorkItemStatus::Claimed | WorkItemStatus::Running => {
            Some(OperationHeadlineStatus::Running)
        }
        WorkItemStatus::Expired | WorkItemStatus::Failed => Some(OperationHeadlineStatus::Failed),
        WorkItemStatus::Done => None,
    }
}

fn derive_thread_task_operations(
    thread_id: &str,
    matching_goals: &[&GoalState],
    management_store: &ManagementStore,
    active_agents_by_scope: &HashMap<String, Vec<String>>,
) -> Vec<ThreadOperationCard> {
    let goal_scopes: Vec<String> = matching_goals
        .iter()
        .filter_map(|goal| goal.last_run_id.clone())
        .collect();
    let mut tasks: Vec<WorkItem> = management_store
        .work_items()
        .into_iter()
        .filter(|work_item| work_item.kind == WorkItemKind::Task)
        .filter(|work_item| {
            work_item.thread_id.as_deref() == Some(thread_id)
                || work_item
                    .initiative_id
                    .as_ref()
                    .is_some_and(|initiative| goal_scopes.iter().any(|scope| scope == initiative))
        })
        .filter(|work_item| operation_status_for_work_item(work_item).is_some())
        .collect();
    tasks.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    tasks
        .into_iter()
        .map(|work_item| ThreadOperationCard {
            operation_id: work_item.id.clone(),
            title: work_item.title.clone(),
            status: operation_status_for_work_item(&work_item)
                .expect("filtered operation status should be present"),
            current_step: Some(work_item.summary.clone()).filter(|value| !value.trim().is_empty()),
            owner_label: owner_label_for_work_item(&work_item),
            progress_percent: None,
            active_agents: work_item
                .initiative_id
                .as_ref()
                .and_then(|scope| active_agents_by_scope.get(scope))
                .cloned()
                .unwrap_or_default(),
        })
        .take(8)
        .collect()
}

fn extract_event_detail(markdown: &str) -> Option<String> {
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        return Some(trimmed.to_string());
    }
    None
}

fn archive_event_to_update(event: GoalEventManifest) -> ThreadExecutionUpdate {
    let operation_id = event
        .task_id
        .as_ref()
        .map(|task_id| format!("goal:{}:task:{task_id}", event.goal_id))
        .unwrap_or_else(|| format!("goal:{}", event.goal_id));
    let label = match event.event_type.as_str() {
        "task_status_changed" => "Task status changed".to_string(),
        "task_owner_changed" => "Task owner changed".to_string(),
        "task_escalated" => "Task escalated".to_string(),
        "task_escalation_deferred" => "Escalation deferred".to_string(),
        "task_escalation_window_opened" => "Escalation window opened".to_string(),
        "task_escalation_suppressed" => "Escalation suppressed".to_string(),
        "task_replan_requested" => "Replan requested".to_string(),
        "task_replan_skipped" => "Replan skipped".to_string(),
        "task_replan_enqueued" => "Replan queued".to_string(),
        "task_condition_set" => "Condition updated".to_string(),
        "task_condition_satisfied" => "Condition satisfied".to_string(),
        "task_dependencies_satisfied" => "Dependencies satisfied".to_string(),
        "plan_reconciled" => "Plan updated".to_string(),
        other => other.replace('_', " "),
    };
    let detail = if let Some(note) = event
        .note
        .as_deref()
        .filter(|value: &&str| !value.trim().is_empty())
    {
        note.to_string()
    } else if let Some(detail) = extract_event_detail(&event.event_markdown) {
        detail
    } else {
        match event.event_type.as_str() {
            "task_status_changed" => match (event.task_id.as_deref(), event.next_status.as_deref())
            {
                (Some(task_id), Some(next_status)) => {
                    format!("{task_id} -> {}", next_status.replace('_', " "))
                }
                _ => "Task status changed".to_string(),
            },
            "task_owner_changed" => match (
                event.task_id.as_deref(),
                event.previous_owner.as_deref(),
                event.next_owner.as_deref(),
            ) {
                (Some(task_id), Some(previous_owner), Some(next_owner)) => {
                    format!("{task_id}: {previous_owner} -> {next_owner}")
                }
                _ => "Task owner changed".to_string(),
            },
            "task_escalated" => {
                match (
                    event.task_id.as_deref(),
                    event.escalation_policy.as_deref(),
                    event.escalation_audience.as_deref(),
                    event.escalation_severity.as_deref(),
                ) {
                    (Some(task_id), Some(policy), Some(audience), Some(severity)) => {
                        format!(
                            "{task_id}: {} -> {audience} ({severity})",
                            policy.replace('_', " ")
                        )
                    }
                    (Some(task_id), Some(policy), Some(audience), None) => {
                        format!("{task_id}: {} -> {audience}", policy.replace('_', " "))
                    }
                    (Some(task_id), Some(policy), None, Some(severity)) => {
                        format!("{task_id}: {} ({severity})", policy.replace('_', " "))
                    }
                    (Some(task_id), Some(policy), None, None) => {
                        format!("{task_id}: {}", policy.replace('_', " "))
                    }
                    _ => "Task escalated".to_string(),
                }
            }
            "task_escalation_deferred" => match event.task_id.as_deref() {
                Some(task_id) => format!("{task_id}: escalation deferred"),
                None => "Escalation deferred".to_string(),
            },
            "task_escalation_window_opened" => match event.task_id.as_deref() {
                Some(task_id) => format!("{task_id}: delivery window opened"),
                None => "Escalation window opened".to_string(),
            },
            "task_escalation_suppressed" => match event.task_id.as_deref() {
                Some(task_id) => format!("{task_id}: escalation suppressed"),
                None => "Escalation suppressed".to_string(),
            },
            "task_replan_requested" => match event.task_id.as_deref() {
                Some(task_id) => format!("{task_id}: replanning requested"),
                None => "Replan requested".to_string(),
            },
            "task_replan_skipped" => match event.task_id.as_deref() {
                Some(task_id) => format!("{task_id}: replanning skipped"),
                None => "Replan skipped".to_string(),
            },
            "task_replan_enqueued" => match event.task_id.as_deref() {
                Some(task_id) => format!("{task_id}: replanning queued"),
                None => "Replan queued".to_string(),
            },
            "task_condition_set" => match (
                event.task_id.as_deref(),
                event.condition_kind.as_deref(),
                event.condition_value.as_deref(),
            ) {
                (Some(task_id), Some(kind), Some(value)) if !value.is_empty() => {
                    format!("{task_id}: {kind} -> {value}")
                }
                (Some(task_id), Some(kind), _) => format!("{task_id}: {kind} updated"),
                _ => "Condition updated".to_string(),
            },
            "task_condition_satisfied" => match (
                event.task_id.as_deref(),
                event.condition_kind.as_deref(),
                event.condition_value.as_deref(),
            ) {
                (Some(task_id), Some(kind), Some(value)) if !value.is_empty() => {
                    format!("{task_id}: {kind} satisfied ({value})")
                }
                (Some(task_id), Some(kind), _) => format!("{task_id}: {kind} satisfied"),
                _ => "Condition satisfied".to_string(),
            },
            "task_dependencies_satisfied" => match event.task_id.as_deref() {
                Some(task_id) => format!("{task_id}: dependencies satisfied"),
                None => "Dependencies satisfied".to_string(),
            },
            "plan_reconciled" => {
                format!(
                    "added {} // preserved {} // deactivated {}",
                    event.added_task_ids.len(),
                    event.preserved_task_ids.len(),
                    event.deactivated_task_ids.len()
                )
            }
            _ => "Archive event".to_string(),
        }
    };

    ThreadExecutionUpdate {
        operation_id,
        label,
        detail,
        created_at: event.observed_at,
    }
}

fn derive_archive_goal_updates(
    thread_id: &str,
    archive_root: &Path,
    matching_goals: &[&GoalState],
    management_store: &ManagementStore,
) -> Vec<ThreadExecutionUpdate> {
    let parser = ManifestParser::new();
    let mut goal_refs = HashSet::new();
    for work_item in management_store.work_items() {
        if work_item.kind != WorkItemKind::Goal || work_item.thread_id.as_deref() != Some(thread_id)
        {
            continue;
        }
        if let Some(initiative_id) = work_item.initiative_id.as_ref() {
            let normalized_project_id = if work_item.project_id.starts_with("project:") {
                work_item.project_id.clone()
            } else {
                format!("project:{}", work_item.project_id)
            };
            goal_refs.insert((normalized_project_id, initiative_id.clone()));
        }
    }
    for goal in matching_goals {
        let normalized_project_id = if goal.project_id.starts_with("project:") {
            goal.project_id.clone()
        } else {
            format!("project:{}", goal.project_id)
        };
        goal_refs.insert((normalized_project_id, goal.template.clone()));
    }
    let mut updates = Vec::new();

    for (project_id, goal_id) in goal_refs {
        let project_component = project_id.strip_prefix("project:").unwrap_or(&project_id);
        let events_dir = archive_root
            .join("operations/projects")
            .join(project_component)
            .join("goals")
            .join(goal_id.strip_prefix("goal:").unwrap_or(&goal_id))
            .join("events");
        let Ok(entries) = std::fs::read_dir(&events_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Ok(event) = parser.parse_goal_event(&path) else {
                continue;
            };
            if event
                .thread_id
                .as_deref()
                .is_some_and(|attached| attached != thread_id)
            {
                continue;
            }
            updates.push(archive_event_to_update(event));
        }
    }

    updates.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    updates.truncate(8);
    updates
}

fn branch_target(work_item: &WorkItem) -> Option<String> {
    work_item
        .requested_scopes
        .iter()
        .find_map(|scope| match &scope.scope {
            CollaborationScope::RepoBranchNamespace { repo_id, pattern } => {
                Some(format!("{repo_id}:{pattern}"))
            }
            _ => None,
        })
}

fn artifact_for_branch_work_item(work_item: &WorkItem) -> Option<ThreadOperationArtifact> {
    if work_item.kind != WorkItemKind::DevelopmentArtifact {
        return None;
    }
    let target = branch_target(work_item)?;
    let (kind, status, label) = match work_item.status {
        WorkItemStatus::ClaimPending | WorkItemStatus::Claimed | WorkItemStatus::Running => (
            ThreadArtifactKind::Branch,
            ThreadArtifactStatus::Active,
            "Branch active",
        ),
        WorkItemStatus::Blocked => (
            ThreadArtifactKind::Branch,
            ThreadArtifactStatus::Failed,
            "Branch blocked",
        ),
        WorkItemStatus::PendingReview => (
            ThreadArtifactKind::Review,
            ThreadArtifactStatus::PendingReview,
            "Awaiting review",
        ),
        WorkItemStatus::Done => (
            ThreadArtifactKind::Merge,
            ThreadArtifactStatus::Merged,
            "Merged",
        ),
        WorkItemStatus::Cancelled => (
            ThreadArtifactKind::Review,
            ThreadArtifactStatus::Closed,
            "Closed without merge",
        ),
        WorkItemStatus::Expired | WorkItemStatus::Failed => (
            ThreadArtifactKind::Branch,
            ThreadArtifactStatus::Failed,
            "Branch failed",
        ),
        WorkItemStatus::Todo => return None,
    };

    Some(ThreadOperationArtifact {
        artifact_id: work_item.id.clone(),
        operation_id: work_item
            .parent_work_item_id
            .clone()
            .or_else(|| work_item.initiative_id.clone())
            .unwrap_or_else(|| work_item.id.clone()),
        kind,
        status,
        label: label.to_string(),
        detail: work_item.title.clone(),
        target,
        updated_at: work_item.updated_at,
    })
}

fn derive_thread_artifacts(
    thread_id: &str,
    matching_goals: &[&GoalState],
    management_store: &ManagementStore,
) -> Vec<ThreadOperationArtifact> {
    let mut artifacts: Vec<ThreadOperationArtifact> = management_store
        .work_items()
        .into_iter()
        .filter(|work_item| work_item.thread_id.as_deref() == Some(thread_id))
        .filter_map(|work_item| artifact_for_branch_work_item(&work_item))
        .collect();
    let fallback: Vec<ThreadOperationArtifact> = matching_goals
        .iter()
        .filter_map(|goal| goal.last_run_id.as_deref())
        .flat_map(|goal_scope| management_store.work_items_for_initiative(goal_scope))
        .filter_map(|work_item| artifact_for_branch_work_item(&work_item))
        .filter(|artifact| {
            !artifacts
                .iter()
                .any(|existing| existing.artifact_id == artifact.artifact_id)
        })
        .collect();
    artifacts.extend(fallback);
    artifacts.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    artifacts.truncate(8);
    artifacts
}

fn derive_thread_chatter(
    thread_id: &str,
    matching_goals: &[&GoalState],
    interaction_log_store: &BridgeInteractionLogStore,
) -> Vec<ThreadChatterEvent> {
    let goal_scopes: Vec<String> = matching_goals
        .iter()
        .filter_map(|goal| goal.last_run_id.clone())
        .collect();
    interaction_log_store
        .recent_for_thread(thread_id, &goal_scopes, 8)
        .into_iter()
        .filter_map(|record| {
            let (kind, to) = match record.kind {
                BridgeInteractionKind::PendingQuestion => (ThreadChatterKind::Question, "Operator"),
                BridgeInteractionKind::ProposedPlan => (ThreadChatterKind::Plan, "Operator"),
                BridgeInteractionKind::PendingAuthRequest => {
                    (ThreadChatterKind::Auth, "Credentials")
                }
                BridgeInteractionKind::ContextPacketLoaded => return None,
            };
            Some(ThreadChatterEvent {
                event_id: record.event_id,
                operation_id: record.goal_scope,
                from_agent: record.agent_id,
                to: to.to_string(),
                kind,
                message: record.detail,
                created_at: record.created_at as i64,
            })
        })
        .collect()
}

fn derive_thread_agent_logs(
    thread_id: &str,
    matching_goals: &[&GoalState],
    agent_runtime_log_store: &AgentRuntimeLogStore,
) -> Vec<ThreadAgentLogEntry> {
    let goal_scopes: Vec<String> = matching_goals
        .iter()
        .filter_map(|goal| goal.last_run_id.clone())
        .collect();
    agent_runtime_log_store
        .recent_for_thread(thread_id, &goal_scopes, 32)
        .into_iter()
        .map(|record| ThreadAgentLogEntry {
            event_id: record.event_id,
            operation_id: record.goal_scope,
            agent_id: record.agent_id,
            entry_type: match record.entry_type {
                AgentRuntimeLogEntryType::Tool => ThreadAgentLogEntryType::Tool,
                AgentRuntimeLogEntryType::Result => ThreadAgentLogEntryType::Result,
                AgentRuntimeLogEntryType::Blocked => ThreadAgentLogEntryType::Blocked,
            },
            content: record.content,
            tool_name: record.tool_name,
            tool_params: record.tool_params,
            created_at: record.created_at as i64,
        })
        .collect()
}

fn derive_thread_agent_statuses(
    thread_id: &str,
    matching_goals: &[&GoalState],
    agent_runtime_status_store: &AgentRuntimeStatusStore,
) -> Vec<ThreadAgentRuntimeStatus> {
    let goal_scopes: Vec<String> = matching_goals
        .iter()
        .filter_map(|goal| goal.last_run_id.clone())
        .collect();
    agent_runtime_status_store
        .statuses_for_thread(thread_id, &goal_scopes, 32)
        .into_iter()
        .map(|status| ThreadAgentRuntimeStatus {
            agent_id: status.agent_id,
            operation_id: status.goal_scope,
            role: status.role,
            sandbox_type: status.sandbox_type,
            model_label: status.model_label,
            status: status.status,
            detail: status.detail,
            current_iteration: status.current_iteration,
            max_iterations: status.max_iterations,
            active_tool_name: status.active_tool_name,
            updated_at: status.updated_at as i64,
        })
        .collect()
}

fn derive_recent_active_agents(
    thread_id: &str,
    matching_goals: &[&GoalState],
    agent_runtime_status_store: &AgentRuntimeStatusStore,
    agent_runtime_log_store: &AgentRuntimeLogStore,
    now: u64,
) -> Vec<String> {
    let agent_statuses =
        derive_thread_agent_statuses(thread_id, matching_goals, agent_runtime_status_store);
    let mut agents: Vec<String> = derive_recent_active_agents_by_scope(
        &agent_statuses,
        matching_goals,
        agent_runtime_log_store,
        now,
    )
    .into_values()
    .flatten()
    .collect();
    agents.sort();
    agents.dedup();
    agents
}

fn derive_recent_active_agents_by_scope(
    agent_statuses: &[ThreadAgentRuntimeStatus],
    matching_goals: &[&GoalState],
    agent_runtime_log_store: &AgentRuntimeLogStore,
    now: u64,
) -> HashMap<String, Vec<String>> {
    let mut by_scope: HashMap<String, Vec<String>> = HashMap::new();
    let cutoff = now.saturating_sub(ACTIVE_AGENT_WINDOW_SECS) as i64;
    for status in agent_statuses.iter().filter(|status| {
        status.updated_at >= cutoff && status.status.is_active() && status.operation_id.is_some()
    }) {
        let scope = status
            .operation_id
            .as_ref()
            .expect("checked operation_id exists")
            .clone();
        let agents = by_scope.entry(scope).or_default();
        if !agents.iter().any(|agent| agent == &status.agent_id) {
            agents.push(status.agent_id.clone());
        }
    }
    if !by_scope.is_empty() {
        for agents in by_scope.values_mut() {
            agents.sort();
        }
        return by_scope;
    }

    let goal_scopes: Vec<String> = matching_goals
        .iter()
        .filter_map(|goal| goal.last_run_id.clone())
        .collect();
    let cutoff = now.saturating_sub(ACTIVE_AGENT_WINDOW_SECS);
    for record in agent_runtime_log_store.recent_for_thread("", &goal_scopes, 128) {
        if record.created_at < cutoff {
            continue;
        }
        let Some(scope) = record.goal_scope else {
            continue;
        };
        let agents = by_scope.entry(scope).or_default();
        if !agents.iter().any(|agent| agent == &record.agent_id) {
            agents.push(record.agent_id);
        }
    }
    for agents in by_scope.values_mut() {
        agents.sort();
    }
    by_scope
}

pub fn summary_action() -> &'static str {
    SUMMARY_ACTION
}

pub fn snapshot_action() -> &'static str {
    SNAPSHOT_ACTION
}

impl SymbioticDaemon {
    fn thread_title_for_observability(&self, thread_id: &str) -> Option<String> {
        self.thread_manager.lock().ok().and_then(|manager| {
            manager.as_ref().and_then(|manager| {
                manager
                    .active_threads()
                    .into_iter()
                    .find(|entry| entry.thread_id == thread_id)
                    .map(|entry| entry.title.clone())
            })
        })
    }

    fn should_refresh_thread_observability(envelope: &MatrixEventEnvelope) -> Option<&str> {
        let thread_id = envelope.sym.t.as_deref()?;
        if matches!(
            envelope.sym.a.as_deref(),
            Some(SUMMARY_ACTION | SNAPSHOT_ACTION)
        ) {
            return None;
        }

        let action_matches = envelope
            .sym
            .a
            .as_deref()
            .map(|action| {
                action.starts_with("goal.")
                    || action.starts_with("auth.")
                    || action.starts_with("routing.")
            })
            .unwrap_or(false);

        let goal_detail_matches = envelope
            .sym
            .d
            .as_ref()
            .and_then(|detail| detail.as_object())
            .is_some_and(|detail| {
                detail.contains_key("goal_id") || detail.contains_key("template")
            });

        if action_matches || goal_detail_matches {
            Some(thread_id)
        } else {
            None
        }
    }

    fn rebuild_thread_observability_projection(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<(OperationsPillSummary, ThreadObservabilitySnapshot)> {
        let goal_states = load_goal_states(&self.config.goal_state_file)
            .with_context(|| "failed to load goal state for thread observability refresh")?;
        let goal_logs = load_goal_log_records(&self.config.goal_log_file)
            .with_context(|| "failed to load goal log for thread observability refresh")?;
        let management_store = self
            .management_store
            .lock()
            .map_err(|_| anyhow!("management store lock poisoned"))?;
        let interaction_log_store = self
            .bridge_interaction_log_store
            .lock()
            .map_err(|_| anyhow!("bridge interaction log store lock poisoned"))?;
        let agent_runtime_status_store = self
            .agent_runtime_status_store
            .lock()
            .map_err(|_| anyhow!("agent runtime status store lock poisoned"))?;
        let agent_runtime_log_store = self
            .agent_runtime_log_store
            .lock()
            .map_err(|_| anyhow!("agent runtime log store lock poisoned"))?;
        let thread_title = self.thread_title_for_observability(thread_id);
        Ok((
            derive_operations_pill_summary(
                thread_id,
                thread_title.as_deref(),
                &goal_states,
                &management_store,
                &agent_runtime_status_store,
                &agent_runtime_log_store,
                now as i64,
            ),
            derive_thread_observability_snapshot(
                thread_id,
                &self.config.archive_root,
                &goal_states,
                &goal_logs,
                &management_store,
                &interaction_log_store,
                &agent_runtime_status_store,
                &agent_runtime_log_store,
                now as i64,
            ),
        ))
    }

    fn persist_thread_observability_summary(&self, summary: OperationsPillSummary) -> Result<()> {
        let mut store = self
            .thread_observability_store
            .lock()
            .map_err(|_| anyhow!("thread observability store lock poisoned"))?;
        store.upsert(summary)
    }

    fn persist_thread_observability_snapshot(
        &self,
        snapshot: ThreadObservabilitySnapshot,
    ) -> Result<()> {
        let mut store = self
            .thread_observability_store
            .lock()
            .map_err(|_| anyhow!("thread observability store lock poisoned"))?;
        store.upsert_snapshot(snapshot)
    }

    pub(crate) async fn send_thread_observability_summary_if_needed<T: MatrixTransport + ?Sized>(
        &self,
        transport: &T,
        room_id: &str,
        envelope: &MatrixEventEnvelope,
        now: u64,
    ) -> Result<()> {
        let Some(thread_id) = Self::should_refresh_thread_observability(envelope) else {
            return Ok(());
        };

        let (summary, snapshot) = self.rebuild_thread_observability_projection(thread_id, now)?;
        self.persist_thread_observability_summary(summary.clone())?;
        self.persist_thread_observability_snapshot(snapshot.clone())?;
        transport
            .send_outgoing(room_id, summary.to_envelope(now))
            .await?;
        transport
            .send_outgoing(room_id, snapshot.to_envelope(now))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runtime_status::{
        AgentRuntimeProfile, AgentRuntimeStatusKind, AgentRuntimeStatusStore,
    };

    fn goal_state(
        template: &str,
        status: &str,
        last_run_id: &str,
        updated_at: u64,
        pipeline_stage: Option<&str>,
    ) -> GoalState {
        GoalState {
            goal_room: "#goals".to_string(),
            thread_id: Some(last_run_id.to_string()),
            project_id: "project:test".to_string(),
            template: template.to_string(),
            status: status.to_string(),
            last_job_id: "job-1".to_string(),
            last_run_id: Some(last_run_id.to_string()),
            owner: Some("@user:test".to_string()),
            updated_at,
            complexity: None,
            pipeline_stage: pipeline_stage.map(str::to_string),
            audit_id: None,
            plan_id: None,
        }
    }

    #[test]
    fn derives_waiting_summary_from_goal_state() {
        let management_store = ManagementStore::new(PathBuf::from("."));
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(Path::new("."));
        let agent_runtime_log_store = AgentRuntimeLogStore::new(Path::new("."));
        let summary = derive_operations_pill_summary(
            "goal-1",
            Some("Fix deploy pipeline"),
            &[goal_state(
                "inquisition:goal-1",
                "awaiting_input",
                "goal-1",
                10,
                Some("awaiting_input"),
            )],
            &management_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            10,
        );
        assert_eq!(summary.active_operation_count, 1);
        assert!(summary.waiting_for_user);
        assert!(!summary.has_failure);
        let primary = summary.primary_operation.expect("primary operation");
        assert_eq!(primary.title, "Fix deploy pipeline");
        assert_eq!(primary.status, OperationHeadlineStatus::Waiting);
        assert_eq!(primary.current_step.as_deref(), Some("awaiting_input"));
        assert_eq!(primary.progress_percent, None);
    }

    #[test]
    fn derives_empty_summary_when_thread_has_no_matching_goal_state() {
        let management_store = ManagementStore::new(PathBuf::from("."));
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(Path::new("."));
        let agent_runtime_log_store = AgentRuntimeLogStore::new(Path::new("."));
        let summary = derive_operations_pill_summary(
            "goal-1",
            None,
            &[goal_state("deliberation", "completed", "goal-2", 10, None)],
            &management_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            10,
        );
        assert!(summary.is_empty());
        assert_eq!(summary.thread_id, "goal-1");
        assert_eq!(summary.active_operation_count, 0);
        assert!(summary.primary_operation.is_none());
    }

    #[test]
    fn derives_task_first_summary_from_thread_attached_task_work_items() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut management_store = ManagementStore::new(dir.path().to_path_buf());
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        management_store
            .upsert_work_item(WorkItem {
                id: "goal:travel:task:await-budget".to_string(),
                project_id: "symbiotic".to_string(),
                initiative_id: Some("travel-plan".to_string()),
                parent_work_item_id: Some("goal:travel".to_string()),
                kind: WorkItemKind::Task,
                thread_id: Some("thread-travel".to_string()),
                title: "Await budget confirmation".to_string(),
                summary:
                    "Wait for operator budget confirmation (waiting for: operator budget approval)"
                        .to_string(),
                status: WorkItemStatus::Blocked,
                priority: symbiotic_control_plane::WorkPriority::P2,
                urgency: symbiotic_control_plane::WorkUrgency::Normal,
                assignment_mode: symbiotic_control_plane::AssignmentMode::SingleOwner,
                requested_scopes: Vec::new(),
                accepted_claim_ids: Vec::new(),
                assignee: Some(symbiotic_control_plane::AgentAssignment {
                    agent_id: "operator".to_string(),
                    runner_id: None,
                    assigned_at: 10,
                }),
                blocked_by: Vec::new(),
                depends_on: Vec::new(),
                review_mode: symbiotic_control_plane::ReviewMode::NoReview,
                cancellation: None,
                created_at: 10,
                updated_at: 20,
            })
            .expect("upsert task work item");

        let summary = derive_operations_pill_summary(
            "thread-travel",
            Some("Travel Plan"),
            &[goal_state(
                "travel-plan",
                "running",
                "thread-travel",
                10,
                Some("planning"),
            )],
            &management_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            20,
        );

        assert_eq!(summary.active_operation_count, 1);
        assert!(summary.waiting_for_user);
        let primary = summary.primary_operation.expect("primary operation");
        assert_eq!(primary.title, "Await budget confirmation");
        assert_eq!(primary.status, OperationHeadlineStatus::Waiting);
    }

    #[test]
    fn derives_snapshot_from_matching_goal_states() {
        let management_store = ManagementStore::new(PathBuf::from("."));
        let interaction_store = BridgeInteractionLogStore::new(Path::new("."));
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(Path::new("."));
        let agent_runtime_log_store = AgentRuntimeLogStore::new(Path::new("."));
        let snapshot = derive_thread_observability_snapshot(
            "goal-1",
            Path::new("."),
            &[
                goal_state("build_api", "running", "goal-1", 10, Some("implementing")),
                goal_state(
                    "review_api",
                    "awaiting_input",
                    "goal-1",
                    20,
                    Some("awaiting_input"),
                ),
                goal_state("ignored", "running", "goal-2", 30, Some("executing")),
            ],
            &[GoalLogRecord {
                ts: 20,
                event: "goal.step.started".to_string(),
                workflow_job_id: "job-1".to_string(),
                goal_room: Some("#goals".to_string()),
                goal_sender: Some("@user:test".to_string()),
                template: "build_api".to_string(),
                detail: "step=implement".to_string(),
            }],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            30,
        );
        assert_eq!(snapshot.thread_id, "goal-1");
        assert_eq!(snapshot.operations.len(), 2);
        assert_eq!(snapshot.operations[0].title, "Build Api");
        assert_eq!(
            snapshot.operations[0].status,
            OperationHeadlineStatus::Running
        );
        assert_eq!(
            snapshot.operations[1].current_step.as_deref(),
            Some("awaiting_input")
        );
        assert_eq!(snapshot.updates.len(), 1);
        assert_eq!(snapshot.updates[0].label, "step started");
        assert!(snapshot.chatter.is_empty());
        assert!(snapshot.agent_statuses.is_empty());
        assert!(snapshot.agent_logs.is_empty());
        assert!(snapshot.artifacts.is_empty());
    }

    #[test]
    fn store_save_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = ThreadObservabilityStore::new(dir.path());
        store
            .upsert(OperationsPillSummary {
                thread_id: "goal-1".to_string(),
                active_operation_count: 1,
                primary_operation: Some(OperationHeadline {
                    title: "Fix deploy pipeline".to_string(),
                    status: OperationHeadlineStatus::Running,
                    current_step: Some("executing".to_string()),
                    progress_percent: None,
                }),
                waiting_for_user: false,
                has_failure: false,
                total_active_agents: None,
                updated_at: 10,
            })
            .expect("upsert");
        store
            .upsert_snapshot(ThreadObservabilitySnapshot {
                thread_id: "goal-1".to_string(),
                operations: vec![ThreadOperationCard {
                    operation_id: "build_api:job-1".to_string(),
                    title: "Build Api".to_string(),
                    status: OperationHeadlineStatus::Running,
                    current_step: Some("executing".to_string()),
                    owner_label: Some("@user:test".to_string()),
                    progress_percent: None,
                    active_agents: Vec::new(),
                }],
                updates: vec![ThreadExecutionUpdate {
                    operation_id: "build_api:job-1".to_string(),
                    label: "step started".to_string(),
                    detail: "step implementing".to_string(),
                    created_at: 10,
                }],
                chatter: vec![ThreadChatterEvent {
                    event_id: "evt-1".to_string(),
                    operation_id: Some("run-1".to_string()),
                    from_agent: "agent-1".to_string(),
                    to: "Operator".to_string(),
                    kind: ThreadChatterKind::Question,
                    message: "Needs budget".to_string(),
                    created_at: 10,
                }],
                agent_statuses: vec![ThreadAgentRuntimeStatus {
                    agent_id: "agent-1".to_string(),
                    operation_id: Some("run-1".to_string()),
                    role: Some("coder".to_string()),
                    sandbox_type: "vm_sandbox".to_string(),
                    model_label: Some("gpt-5.4".to_string()),
                    status: AgentRuntimeStatusKind::Running,
                    detail: Some("Iteration 2/15".to_string()),
                    current_iteration: Some(2),
                    max_iterations: Some(15),
                    active_tool_name: Some("read_file".to_string()),
                    updated_at: 10,
                }],
                agent_logs: vec![ThreadAgentLogEntry {
                    event_id: "log-1".to_string(),
                    operation_id: Some("run-1".to_string()),
                    agent_id: "agent-1".to_string(),
                    entry_type: ThreadAgentLogEntryType::Tool,
                    content: "Calling tool: read_file".to_string(),
                    tool_name: Some("read_file".to_string()),
                    tool_params: Some("{\"path\":\"README.md\"}".to_string()),
                    created_at: 10,
                }],
                artifacts: vec![ThreadOperationArtifact {
                    artifact_id: "artifact-1".to_string(),
                    operation_id: "goal-1".to_string(),
                    kind: ThreadArtifactKind::Review,
                    status: ThreadArtifactStatus::PendingReview,
                    label: "Awaiting review".to_string(),
                    detail: "Implementation branch".to_string(),
                    target: "repo-1:feature/x".to_string(),
                    updated_at: 10,
                }],
                updated_at: 10,
            })
            .expect("upsert snapshot");

        let mut reopened = ThreadObservabilityStore::new(dir.path());
        reopened.load().expect("load");
        assert!(reopened.get("goal-1").is_some());
        assert!(reopened.get_snapshot("goal-1").is_some());
        assert_eq!(
            reopened
                .get("goal-1")
                .and_then(|summary| summary.primary_operation.as_ref())
                .map(|headline| headline.title.as_str()),
            Some("Fix deploy pipeline")
        );
        assert_eq!(
            reopened
                .get_snapshot("goal-1")
                .map(|snapshot| snapshot.chatter.len()),
            Some(1)
        );
        assert_eq!(
            reopened
                .get_snapshot("goal-1")
                .map(|snapshot| snapshot.artifacts.len()),
            Some(1)
        );
        assert_eq!(
            reopened
                .get_snapshot("goal-1")
                .map(|snapshot| snapshot.agent_statuses.len()),
            Some(1)
        );
        assert_eq!(
            reopened
                .get_snapshot("goal-1")
                .map(|snapshot| snapshot.agent_logs.len()),
            Some(1)
        );
    }

    #[test]
    fn derives_artifacts_from_management_branch_items() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut management_store = ManagementStore::new(dir.path().to_path_buf());
        let interaction_store = BridgeInteractionLogStore::new(dir.path());
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        management_store
            .upsert_work_item(WorkItem {
                id: "branch-item".to_string(),
                project_id: "repo-1".to_string(),
                initiative_id: Some("run-1".to_string()),
                parent_work_item_id: None,
                kind: WorkItemKind::DevelopmentArtifact,
                thread_id: Some("goal-1".to_string()),
                title: "Implementation branch: feature/auth".to_string(),
                summary: "tracks branch".to_string(),
                status: WorkItemStatus::PendingReview,
                priority: symbiotic_control_plane::WorkPriority::P2,
                urgency: symbiotic_control_plane::WorkUrgency::Normal,
                assignment_mode: symbiotic_control_plane::AssignmentMode::SingleOwner,
                requested_scopes: vec![symbiotic_control_plane::ScopeRequirement {
                    scope: CollaborationScope::RepoBranchNamespace {
                        repo_id: "repo-1".to_string(),
                        pattern: "feature/auth".to_string(),
                    },
                    mode: symbiotic_control_plane::ScopeMode::ExclusiveWrite,
                    reason: "branch".to_string(),
                }],
                accepted_claim_ids: Vec::new(),
                assignee: None,
                blocked_by: Vec::new(),
                depends_on: Vec::new(),
                review_mode: symbiotic_control_plane::ReviewMode::AutoReviewThenHumanIfNeeded,
                cancellation: None,
                created_at: 10,
                updated_at: 20,
            })
            .expect("upsert work item");

        let snapshot = derive_thread_observability_snapshot(
            "goal-1",
            dir.path(),
            &[GoalState {
                goal_room: "#goals".to_string(),
                thread_id: Some("goal-1".to_string()),
                project_id: "project:test".to_string(),
                template: "build_api".to_string(),
                status: "running".to_string(),
                last_job_id: "job-1".to_string(),
                last_run_id: Some("run-1".to_string()),
                owner: Some("@user:test".to_string()),
                updated_at: 10,
                complexity: None,
                pipeline_stage: Some("implementing".to_string()),
                audit_id: None,
                plan_id: None,
            }],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            30,
        );

        assert_eq!(snapshot.artifacts.len(), 1);
        assert_eq!(snapshot.artifacts[0].label, "Awaiting review");
        assert_eq!(snapshot.artifacts[0].target, "repo-1:feature/auth");
    }

    #[test]
    fn derives_operations_from_thread_attached_task_work_items() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut management_store = ManagementStore::new(dir.path().to_path_buf());
        let interaction_store = BridgeInteractionLogStore::new(dir.path());
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        management_store
            .upsert_work_item(WorkItem {
                id: "goal:travel:task:await-budget".to_string(),
                project_id: "symbiotic".to_string(),
                initiative_id: Some("travel-planning".to_string()),
                parent_work_item_id: Some("goal:travel".to_string()),
                kind: WorkItemKind::Task,
                thread_id: Some("thread-travel".to_string()),
                title: "Await budget confirmation".to_string(),
                summary:
                    "Wait for operator budget confirmation (waiting for: operator budget approval)"
                        .to_string(),
                status: WorkItemStatus::Blocked,
                priority: symbiotic_control_plane::WorkPriority::P2,
                urgency: symbiotic_control_plane::WorkUrgency::Normal,
                assignment_mode: symbiotic_control_plane::AssignmentMode::SingleOwner,
                requested_scopes: Vec::new(),
                accepted_claim_ids: Vec::new(),
                assignee: Some(symbiotic_control_plane::AgentAssignment {
                    agent_id: "operator".to_string(),
                    runner_id: None,
                    assigned_at: 10,
                }),
                blocked_by: Vec::new(),
                depends_on: Vec::new(),
                review_mode: symbiotic_control_plane::ReviewMode::NoReview,
                cancellation: None,
                created_at: 10,
                updated_at: 25,
            })
            .expect("upsert task work item");

        let snapshot = derive_thread_observability_snapshot(
            "thread-travel",
            dir.path(),
            &[GoalState {
                goal_room: "#goals".to_string(),
                thread_id: Some("thread-travel".to_string()),
                project_id: "project:symbiotic".to_string(),
                template: "travel-planning".to_string(),
                status: "running".to_string(),
                last_job_id: "job-1".to_string(),
                last_run_id: Some("travel-planning".to_string()),
                owner: Some("@user:test".to_string()),
                updated_at: 10,
                complexity: None,
                pipeline_stage: Some("deliberating".to_string()),
                audit_id: None,
                plan_id: None,
            }],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            30,
        );

        assert_eq!(snapshot.operations.len(), 1);
        assert_eq!(snapshot.operations[0].title, "Await budget confirmation");
        assert_eq!(
            snapshot.operations[0].status,
            OperationHeadlineStatus::Waiting
        );
        assert_eq!(
            snapshot.operations[0].current_step.as_deref(),
            Some("Wait for operator budget confirmation (waiting for: operator budget approval)")
        );
        assert_eq!(
            snapshot.operations[0].owner_label.as_deref(),
            Some("operator")
        );
    }

    #[test]
    fn derives_recent_updates_from_archive_goal_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut management_store = ManagementStore::new(dir.path().to_path_buf());
        let interaction_store = BridgeInteractionLogStore::new(dir.path());
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        std::fs::create_dir_all(
            dir.path()
                .join("operations/projects/symbiotic/goals/travel-plan/events"),
        )
        .expect("create events dir");
        std::fs::write(
            dir.path().join(
                "operations/projects/symbiotic/goals/travel-plan/events/200-task-owner-changed.md",
            ),
            r#"---
goal_id: "travel-plan"
event_type: "task_owner_changed"
observed_at: 200
plan_version: 3
thread_id: "thread-travel"
task_id: "await-budget"
previous_status: null
next_status: null
previous_owner: "@lead:test"
next_owner: "operator"
actor: "@lead:test"
note: "Hand off budget approval to operator"
added_task_ids: []
preserved_task_ids: ["await-budget"]
deactivated_task_ids: []
supersession_edges: []
owner_change_edges: ["await-budget:@lead:test->operator"]
---

# task owner changed

Hand off budget approval to operator
"#,
        )
        .expect("write event doc");
        management_store
            .upsert_work_item(WorkItem {
                id: "goal:travel-plan".to_string(),
                project_id: "symbiotic".to_string(),
                initiative_id: Some("travel-plan".to_string()),
                parent_work_item_id: None,
                kind: WorkItemKind::Goal,
                thread_id: Some("thread-travel".to_string()),
                title: "Travel Plan".to_string(),
                summary: "Plan a trip".to_string(),
                status: WorkItemStatus::Running,
                priority: symbiotic_control_plane::WorkPriority::P1,
                urgency: symbiotic_control_plane::WorkUrgency::Normal,
                assignment_mode: symbiotic_control_plane::AssignmentMode::ParallelChildren,
                requested_scopes: Vec::new(),
                accepted_claim_ids: Vec::new(),
                assignee: None,
                blocked_by: Vec::new(),
                depends_on: Vec::new(),
                review_mode: symbiotic_control_plane::ReviewMode::NoReview,
                cancellation: None,
                created_at: 10,
                updated_at: 20,
            })
            .expect("upsert goal work item");

        let snapshot = derive_thread_observability_snapshot(
            "thread-travel",
            dir.path(),
            &[GoalState {
                goal_room: "#goals".to_string(),
                thread_id: Some("thread-travel".to_string()),
                project_id: "project:symbiotic".to_string(),
                template: "travel-plan".to_string(),
                status: "running".to_string(),
                last_job_id: "job-1".to_string(),
                last_run_id: Some("travel-plan".to_string()),
                owner: Some("@user:test".to_string()),
                updated_at: 10,
                complexity: None,
                pipeline_stage: Some("deliberating".to_string()),
                audit_id: None,
                plan_id: None,
            }],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            220,
        );

        assert_eq!(snapshot.updates.len(), 1);
        assert_eq!(snapshot.updates[0].label, "Task owner changed");
        assert_eq!(
            snapshot.updates[0].detail,
            "Hand off budget approval to operator"
        );
        assert_eq!(
            snapshot.updates[0].operation_id,
            "goal:travel-plan:task:await-budget"
        );
    }

    #[test]
    fn derives_artifacts_from_explicit_thread_attachment_without_scope_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut management_store = ManagementStore::new(dir.path().to_path_buf());
        let interaction_store = BridgeInteractionLogStore::new(dir.path());
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        management_store
            .upsert_work_item(WorkItem {
                id: "branch-item".to_string(),
                project_id: "repo-1".to_string(),
                initiative_id: Some("other-run".to_string()),
                parent_work_item_id: None,
                kind: WorkItemKind::DevelopmentArtifact,
                thread_id: Some("goal-1".to_string()),
                title: "Implementation branch: feature/explicit-thread".to_string(),
                summary: "tracks branch".to_string(),
                status: WorkItemStatus::Running,
                priority: symbiotic_control_plane::WorkPriority::P2,
                urgency: symbiotic_control_plane::WorkUrgency::Normal,
                assignment_mode: symbiotic_control_plane::AssignmentMode::SingleOwner,
                requested_scopes: vec![symbiotic_control_plane::ScopeRequirement {
                    scope: CollaborationScope::RepoBranchNamespace {
                        repo_id: "repo-1".to_string(),
                        pattern: "feature/explicit-thread".to_string(),
                    },
                    mode: symbiotic_control_plane::ScopeMode::ExclusiveWrite,
                    reason: "branch".to_string(),
                }],
                accepted_claim_ids: Vec::new(),
                assignee: None,
                blocked_by: Vec::new(),
                depends_on: Vec::new(),
                review_mode: symbiotic_control_plane::ReviewMode::AutoReviewThenHumanIfNeeded,
                cancellation: None,
                created_at: 10,
                updated_at: 20,
            })
            .expect("upsert work item");

        let snapshot = derive_thread_observability_snapshot(
            "goal-1",
            dir.path(),
            &[GoalState {
                goal_room: "#goals".to_string(),
                thread_id: Some("goal-1".to_string()),
                project_id: "project:test".to_string(),
                template: "build_api".to_string(),
                status: "running".to_string(),
                last_job_id: "job-1".to_string(),
                last_run_id: Some("run-1".to_string()),
                owner: Some("@user:test".to_string()),
                updated_at: 10,
                complexity: None,
                pipeline_stage: Some("implementing".to_string()),
                audit_id: None,
                plan_id: None,
            }],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            30,
        );

        assert_eq!(snapshot.artifacts.len(), 1);
        assert_eq!(snapshot.artifacts[0].label, "Branch active");
        assert_eq!(
            snapshot.artifacts[0].target,
            "repo-1:feature/explicit-thread"
        );
    }

    #[test]
    fn derives_chatter_from_bridge_interaction_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let management_store = ManagementStore::new(dir.path().to_path_buf());
        let mut interaction_store = BridgeInteractionLogStore::new(dir.path());
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        interaction_store
            .append(crate::bridge_interactions::BridgeInteractionRecord {
                event_id: "evt-1".to_string(),
                token_id: "token-1".to_string(),
                agent_id: "agent-1".to_string(),
                goal_scope: Some("run-1".to_string()),
                thread_id: None,
                kind: crate::bridge_interactions::BridgeInteractionKind::PendingQuestion,
                summary: "Needs input".to_string(),
                detail: "What budget?".to_string(),
                created_at: 10,
                raw_payload: serde_json::json!({"question":"What budget?"}),
            })
            .expect("append interaction");

        let snapshot = derive_thread_observability_snapshot(
            "goal-1",
            dir.path(),
            &[GoalState {
                goal_room: "#goals".to_string(),
                thread_id: Some("goal-1".to_string()),
                project_id: "project:test".to_string(),
                template: "build_api".to_string(),
                status: "running".to_string(),
                last_job_id: "job-1".to_string(),
                last_run_id: Some("run-1".to_string()),
                owner: Some("@user:test".to_string()),
                updated_at: 10,
                complexity: None,
                pipeline_stage: Some("implementing".to_string()),
                audit_id: None,
                plan_id: None,
            }],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            30,
        );

        assert_eq!(snapshot.chatter.len(), 1);
        assert_eq!(snapshot.chatter[0].from_agent, "agent-1");
        assert_eq!(snapshot.chatter[0].to, "Operator");
        assert_eq!(snapshot.chatter[0].message, "What budget?");
    }

    #[test]
    fn derives_agent_logs_from_runtime_log_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let management_store = ManagementStore::new(dir.path().to_path_buf());
        let interaction_store = BridgeInteractionLogStore::new(dir.path());
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let mut agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        agent_runtime_log_store
            .append(crate::bridge_interactions::AgentRuntimeLogRecord {
                event_id: "log-1".to_string(),
                token_id: "token-1".to_string(),
                agent_id: "agent-1".to_string(),
                goal_scope: Some("run-1".to_string()),
                thread_id: None,
                entry_type: crate::bridge_interactions::AgentRuntimeLogEntryType::Tool,
                content: "Calling tool: read_file".to_string(),
                tool_name: Some("read_file".to_string()),
                tool_params: Some("{\"path\":\"README.md\"}".to_string()),
                created_at: 10,
                raw_payload: serde_json::json!({"tool":"read_file"}),
            })
            .expect("append runtime log");

        let snapshot = derive_thread_observability_snapshot(
            "goal-1",
            dir.path(),
            &[GoalState {
                goal_room: "#goals".to_string(),
                thread_id: Some("goal-1".to_string()),
                project_id: "project:test".to_string(),
                template: "build_api".to_string(),
                status: "running".to_string(),
                last_job_id: "job-1".to_string(),
                last_run_id: Some("run-1".to_string()),
                owner: Some("@user:test".to_string()),
                updated_at: 10,
                complexity: None,
                pipeline_stage: Some("implementing".to_string()),
                audit_id: None,
                plan_id: None,
            }],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            30,
        );

        assert_eq!(snapshot.agent_logs.len(), 1);
        assert_eq!(snapshot.agent_logs[0].agent_id, "agent-1");
        assert_eq!(
            snapshot.agent_logs[0].tool_name.as_deref(),
            Some("read_file")
        );
        assert_eq!(
            snapshot.agent_logs[0].entry_type,
            ThreadAgentLogEntryType::Tool
        );
    }

    #[test]
    fn derives_recent_active_agents_for_pill_and_operations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let management_store = ManagementStore::new(dir.path().to_path_buf());
        let interaction_store = BridgeInteractionLogStore::new(dir.path());
        let agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let mut agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        agent_runtime_log_store
            .append(crate::bridge_interactions::AgentRuntimeLogRecord {
                event_id: "log-1".to_string(),
                token_id: "token-1".to_string(),
                agent_id: "agent-alpha".to_string(),
                goal_scope: Some("run-1".to_string()),
                thread_id: Some("goal-1".to_string()),
                entry_type: crate::bridge_interactions::AgentRuntimeLogEntryType::Tool,
                content: "Calling tool: read_file".to_string(),
                tool_name: Some("read_file".to_string()),
                tool_params: None,
                created_at: 100,
                raw_payload: serde_json::json!({"tool":"read_file"}),
            })
            .expect("append active runtime log");

        let goal = GoalState {
            goal_room: "#goals".to_string(),
            thread_id: Some("goal-1".to_string()),
            project_id: "project:test".to_string(),
            template: "build_api".to_string(),
            status: "running".to_string(),
            last_job_id: "job-1".to_string(),
            last_run_id: Some("run-1".to_string()),
            owner: Some("@user:test".to_string()),
            updated_at: 100,
            complexity: None,
            pipeline_stage: Some("implementing".to_string()),
            audit_id: None,
            plan_id: None,
        };

        let summary = derive_operations_pill_summary(
            "goal-1",
            Some("Build API"),
            std::slice::from_ref(&goal),
            &management_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            100,
        );
        assert_eq!(summary.total_active_agents, Some(1));

        let snapshot = derive_thread_observability_snapshot(
            "goal-1",
            dir.path(),
            &[goal],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            100,
        );
        assert_eq!(snapshot.operations.len(), 1);
        assert_eq!(
            snapshot.operations[0].active_agents,
            vec!["agent-alpha".to_string()]
        );
    }

    #[test]
    fn derives_agent_statuses_from_runtime_status_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let management_store = ManagementStore::new(dir.path().to_path_buf());
        let interaction_store = BridgeInteractionLogStore::new(dir.path());
        let mut agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        agent_runtime_status_store
            .record_handshake(
                "agent-alpha".to_string(),
                "token-1".to_string(),
                Some("run-1".to_string()),
                AgentRuntimeProfile {
                    role: Some("coder".to_string()),
                    sandbox_type: "vm_sandbox".to_string(),
                    model_label: Some("gpt-5.4".to_string()),
                    max_iterations: Some(15),
                    thread_id: Some("goal-1".to_string()),
                },
                90,
            )
            .expect("record handshake");
        agent_runtime_status_store
            .record_event(
                "agent-alpha",
                "token-1",
                Some("run-1".to_string()),
                Some("goal-1".to_string()),
                AgentRuntimeStatusKind::Running,
                Some("Iteration 2/15".to_string()),
                Some(2),
                Some(15),
                Some("read_file".to_string()),
                100,
            )
            .expect("record event");

        let snapshot = derive_thread_observability_snapshot(
            "goal-1",
            dir.path(),
            &[GoalState {
                goal_room: "#goals".to_string(),
                thread_id: Some("goal-1".to_string()),
                project_id: "project:test".to_string(),
                template: "build_api".to_string(),
                status: "running".to_string(),
                last_job_id: "job-1".to_string(),
                last_run_id: Some("run-1".to_string()),
                owner: Some("@user:test".to_string()),
                updated_at: 100,
                complexity: None,
                pipeline_stage: Some("implementing".to_string()),
                audit_id: None,
                plan_id: None,
            }],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            100,
        );

        assert_eq!(snapshot.agent_statuses.len(), 1);
        let status = &snapshot.agent_statuses[0];
        assert_eq!(status.agent_id, "agent-alpha");
        assert_eq!(status.operation_id.as_deref(), Some("run-1"));
        assert_eq!(status.role.as_deref(), Some("coder"));
        assert_eq!(status.sandbox_type, "vm_sandbox");
        assert_eq!(status.model_label.as_deref(), Some("gpt-5.4"));
        assert_eq!(status.status, AgentRuntimeStatusKind::Running);
        assert_eq!(status.detail.as_deref(), Some("Iteration 2/15"));
        assert_eq!(status.current_iteration, Some(2));
        assert_eq!(status.max_iterations, Some(15));
        assert_eq!(status.active_tool_name.as_deref(), Some("read_file"));
    }

    #[test]
    fn prefers_runtime_status_store_for_active_agent_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let management_store = ManagementStore::new(dir.path().to_path_buf());
        let goal = GoalState {
            goal_room: "#goals".to_string(),
            thread_id: Some("goal-1".to_string()),
            project_id: "project:test".to_string(),
            template: "build_api".to_string(),
            status: "running".to_string(),
            last_job_id: "job-1".to_string(),
            last_run_id: Some("run-1".to_string()),
            owner: Some("@user:test".to_string()),
            updated_at: 100,
            complexity: None,
            pipeline_stage: Some("implementing".to_string()),
            audit_id: None,
            plan_id: None,
        };
        let mut agent_runtime_status_store = AgentRuntimeStatusStore::new(dir.path());
        let agent_runtime_log_store = AgentRuntimeLogStore::new(dir.path());
        agent_runtime_status_store
            .record_handshake(
                "agent-alpha".to_string(),
                "token-1".to_string(),
                Some("run-1".to_string()),
                AgentRuntimeProfile {
                    role: Some("coder".to_string()),
                    sandbox_type: "vm_sandbox".to_string(),
                    model_label: None,
                    max_iterations: Some(15),
                    thread_id: Some("goal-1".to_string()),
                },
                95,
            )
            .expect("record handshake");
        agent_runtime_status_store
            .record_event(
                "agent-alpha",
                "token-1",
                Some("run-1".to_string()),
                Some("goal-1".to_string()),
                AgentRuntimeStatusKind::Running,
                Some("Iteration 2/15".to_string()),
                Some(2),
                Some(15),
                Some("read_file".to_string()),
                100,
            )
            .expect("record event");

        let summary = derive_operations_pill_summary(
            "goal-1",
            Some("Build API"),
            std::slice::from_ref(&goal),
            &management_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            100,
        );
        assert_eq!(summary.total_active_agents, Some(1));

        let interaction_store = BridgeInteractionLogStore::new(dir.path());
        let snapshot = derive_thread_observability_snapshot(
            "goal-1",
            dir.path(),
            &[goal],
            &[],
            &management_store,
            &interaction_store,
            &agent_runtime_status_store,
            &agent_runtime_log_store,
            100,
        );
        assert_eq!(
            snapshot.operations[0].active_agents,
            vec!["agent-alpha".to_string()]
        );
    }
}
