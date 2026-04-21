//! Agent swarm orchestrator for parallel task execution.
//!
//! Implements the design from `docs/design/agent-swarms.md`:
//! - `SwarmConfig`: configuration for swarm execution runs
//! - `SwarmOrchestrator`: manages parallel agent instances via channel coordination
//! - `SwarmTask`: priority-based task with dependencies and agent assignment
//! - `AgentMessage` / `OrchestratorMessage`: typed channel messages
//! - `ReviewQueue`: aggregates completed chunks for human review/approval

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for a swarm execution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmConfig {
    /// Maximum agents running in parallel.
    pub max_parallel: usize,
    /// Maximum total agents to spawn for this swarm.
    pub max_total_agents: usize,
    /// How often to poll for completed agents (seconds).
    pub poll_interval_secs: u64,
    /// Whether to auto-commit after each chunk completion.
    pub auto_commit: bool,
    /// Coordination channel buffer size.
    pub channel_buffer_size: usize,
    /// Auto-approve threshold for review items (0.0 - 1.0).
    pub auto_approve_threshold: f64,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            max_parallel: 3,
            max_total_agents: 10,
            poll_interval_secs: 5,
            auto_commit: true,
            channel_buffer_size: 64,
            auto_approve_threshold: 0.85,
        }
    }
}

// ---------------------------------------------------------------------------
// Task types
// ---------------------------------------------------------------------------

/// Priority for a swarm task (lower numeric value = higher priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TaskPriority {
    Critical = 0,
    High = 1,
    Medium = 2,
    Low = 3,
}

/// Status of a swarm task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    PendingReview,
    Approved,
    Rejected,
    Failed,
}

/// A task within a swarm execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmTask {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub priority: TaskPriority,
    pub status: TaskStatus,
    /// IDs of tasks that must complete before this one can start.
    pub depends_on: Vec<String>,
    /// Files this task reads.
    pub reads: Vec<String>,
    /// Files this task writes.
    pub writes: Vec<String>,
    /// Agent ID assigned to this task.
    pub assigned_agent: Option<String>,
    /// Result after completion.
    pub result: Option<TaskResult>,
    /// Number of times this task has been retried.
    pub retry_count: u32,
}

/// Result of a completed task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub output: String,
    pub files_changed: Vec<String>,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Channel messages
// ---------------------------------------------------------------------------

/// Messages sent from agents to the orchestrator.
#[derive(Debug, Clone)]
pub enum AgentMessage {
    /// Agent completed its task.
    Completed { task_id: String, result: TaskResult },
    /// Agent encountered an error.
    Failed { task_id: String, error: String },
    /// Progress heartbeat.
    Progress {
        task_id: String,
        percent: u8,
        message: String,
    },
}

/// Messages sent from orchestrator to agents.
#[derive(Debug, Clone)]
pub enum OrchestratorMessage {
    /// Cancel the agent's current work.
    Cancel { reason: String },
}

// ---------------------------------------------------------------------------
// SwarmOrchestrator
// ---------------------------------------------------------------------------

/// Manages parallel agent execution for a set of tasks.
pub struct SwarmOrchestrator {
    config: SwarmConfig,
    tasks: Vec<SwarmTask>,
    /// Maps task_id -> agent assignment info.
    active_agents: HashMap<String, ActiveAgent>,
    /// Total agents spawned so far.
    total_spawned: usize,
    /// Sender half for agents to send messages to orchestrator.
    agent_tx: mpsc::Sender<AgentMessage>,
    /// Receiver half for orchestrator to collect agent messages.
    agent_rx: mpsc::Receiver<AgentMessage>,
    /// Per-agent command channels (task_id -> sender).
    agent_commands: HashMap<String, mpsc::Sender<OrchestratorMessage>>,
    /// Review queue for completed tasks.
    review_queue: ReviewQueue,
}

/// Tracks an active agent assignment.
struct ActiveAgent {
    _task_id: String,
    _agent_id: String,
}

impl SwarmOrchestrator {
    /// Create a new orchestrator with the given config and tasks.
    pub fn new(config: SwarmConfig, tasks: Vec<SwarmTask>) -> Self {
        let (agent_tx, agent_rx) = mpsc::channel(config.channel_buffer_size);
        Self {
            review_queue: ReviewQueue::in_memory(),
            config,
            tasks,
            active_agents: HashMap::new(),
            total_spawned: 0,
            agent_tx,
            agent_rx,
            agent_commands: HashMap::new(),
        }
    }

    /// Returns the orchestrator config.
    pub fn config(&self) -> &SwarmConfig {
        &self.config
    }

    /// Returns all tasks.
    pub fn tasks(&self) -> &[SwarmTask] {
        &self.tasks
    }

    /// Returns a mutable reference to the review queue.
    pub fn review_queue_mut(&mut self) -> &mut ReviewQueue {
        &mut self.review_queue
    }

    /// Returns a reference to the review queue.
    pub fn review_queue(&self) -> &ReviewQueue {
        &self.review_queue
    }

    /// Get a sender for agents to report back to the orchestrator.
    pub fn agent_sender(&self) -> mpsc::Sender<AgentMessage> {
        self.agent_tx.clone()
    }

    /// Get tasks that are ready to execute: pending status, all dependencies met.
    pub fn get_available_tasks(&self) -> Vec<&SwarmTask> {
        let completed_ids: std::collections::HashSet<&str> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Approved)
            .map(|t| t.id.as_str())
            .collect();

        let mut available: Vec<&SwarmTask> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .filter(|t| {
                t.depends_on
                    .iter()
                    .all(|dep| completed_ids.contains(dep.as_str()))
            })
            .collect();

        // Sort: priority ASC (Critical first), then by id for determinism
        available.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
        available
    }

    /// Check if a set of tasks can safely run in parallel (no write conflicts).
    pub fn can_parallelize(&self, task_ids: &[&str]) -> bool {
        for (i, &id_a) in task_ids.iter().enumerate() {
            let task_a = match self.tasks.iter().find(|t| t.id == id_a) {
                Some(t) => t,
                None => return false,
            };
            for &id_b in &task_ids[i + 1..] {
                let task_b = match self.tasks.iter().find(|t| t.id == id_b) {
                    Some(t) => t,
                    None => return false,
                };
                // Check write-write and write-read conflicts
                for w in &task_a.writes {
                    if task_b.writes.contains(w) || task_b.reads.contains(w) {
                        return false;
                    }
                }
                for w in &task_b.writes {
                    if task_a.reads.contains(w) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Assign a task to an agent. Returns a receiver for orchestrator commands.
    pub fn assign_task(
        &mut self,
        task_id: &str,
        agent_id: &str,
    ) -> Result<mpsc::Receiver<OrchestratorMessage>> {
        if self.total_spawned >= self.config.max_total_agents {
            return Err(anyhow!(
                "max total agents ({}) reached",
                self.config.max_total_agents
            ));
        }
        if self.active_agents.len() >= self.config.max_parallel {
            return Err(anyhow!(
                "max parallel agents ({}) reached",
                self.config.max_parallel
            ));
        }

        let task = self
            .tasks
            .iter_mut()
            .find(|t| t.id == task_id)
            .ok_or_else(|| anyhow!("task not found: {task_id}"))?;

        if task.status != TaskStatus::Pending {
            return Err(anyhow!(
                "task {task_id} is not pending (status: {:?})",
                task.status
            ));
        }

        task.status = TaskStatus::InProgress;
        task.assigned_agent = Some(agent_id.to_string());

        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        self.active_agents.insert(
            task_id.to_string(),
            ActiveAgent {
                _task_id: task_id.to_string(),
                _agent_id: agent_id.to_string(),
            },
        );
        self.agent_commands.insert(task_id.to_string(), cmd_tx);
        self.total_spawned += 1;

        Ok(cmd_rx)
    }

    /// Process a single agent message. Returns true if a task completed.
    pub fn handle_message(&mut self, msg: AgentMessage) -> Result<bool> {
        match msg {
            AgentMessage::Completed { task_id, result } => {
                let task = self
                    .tasks
                    .iter_mut()
                    .find(|t| t.id == task_id)
                    .ok_or_else(|| anyhow!("task not found: {task_id}"))?;

                let auto_approve = result.score >= self.config.auto_approve_threshold;

                if auto_approve {
                    task.status = TaskStatus::Approved;
                } else {
                    task.status = TaskStatus::PendingReview;
                    self.review_queue.enqueue(ReviewItem {
                        id: format!("review_{task_id}"),
                        task_id: task_id.clone(),
                        title: task.title.clone(),
                        summary: result.output.clone(),
                        files_changed: result.files_changed.clone(),
                        score: result.score,
                        status: ReviewStatus::Pending,
                        reviewer_notes: None,
                    })?;
                }

                task.result = Some(result);
                self.active_agents.remove(&task_id);
                self.agent_commands.remove(&task_id);
                Ok(true)
            }
            AgentMessage::Failed { task_id, error } => {
                let task = self
                    .tasks
                    .iter_mut()
                    .find(|t| t.id == task_id)
                    .ok_or_else(|| anyhow!("task not found: {task_id}"))?;

                if task.retry_count < 1 {
                    // Retry once
                    task.status = TaskStatus::Pending;
                    task.assigned_agent = None;
                    task.retry_count += 1;
                } else {
                    task.status = TaskStatus::Failed;
                    task.result = Some(TaskResult {
                        output: format!("Failed after retry: {error}"),
                        files_changed: vec![],
                        score: 0.0,
                    });
                }

                self.active_agents.remove(&task_id);
                self.agent_commands.remove(&task_id);
                Ok(true)
            }
            AgentMessage::Progress { .. } => {
                // Progress messages are informational only
                Ok(false)
            }
        }
    }

    /// Cancel a specific agent's work.
    pub async fn cancel_task(&mut self, task_id: &str, reason: &str) -> Result<()> {
        if let Some(cmd_tx) = self.agent_commands.get(task_id) {
            cmd_tx
                .send(OrchestratorMessage::Cancel {
                    reason: reason.to_string(),
                })
                .await
                .map_err(|_| anyhow!("agent channel disconnected for task {task_id}"))?;
        }

        let task = self
            .tasks
            .iter_mut()
            .find(|t| t.id == task_id)
            .ok_or_else(|| anyhow!("task not found: {task_id}"))?;
        task.status = TaskStatus::Pending;
        task.assigned_agent = None;

        self.active_agents.remove(task_id);
        self.agent_commands.remove(task_id);
        Ok(())
    }

    /// Number of currently active agents.
    pub fn active_count(&self) -> usize {
        self.active_agents.len()
    }

    /// Total agents spawned so far.
    pub fn total_spawned(&self) -> usize {
        self.total_spawned
    }

    /// Check if all tasks are complete (approved or failed).
    pub fn is_complete(&self) -> bool {
        self.tasks
            .iter()
            .all(|t| matches!(t.status, TaskStatus::Approved | TaskStatus::Failed))
    }

    /// Try to receive a message from agents (non-blocking).
    pub fn try_recv(&mut self) -> Option<AgentMessage> {
        self.agent_rx.try_recv().ok()
    }

    /// Drain all pending messages and process them.
    pub fn drain_messages(&mut self) -> Result<usize> {
        let mut count = 0;
        while let Some(msg) = self.try_recv() {
            self.handle_message(msg)?;
            count += 1;
        }
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// Review Queue
// ---------------------------------------------------------------------------

/// Status of a review item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Pending,
    Approved,
    Rejected,
    NeedsRevision,
}

/// A review item in the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: String,
    pub task_id: String,
    pub title: String,
    pub summary: String,
    pub files_changed: Vec<String>,
    pub score: f64,
    pub status: ReviewStatus,
    pub reviewer_notes: Option<String>,
}

/// The review queue, optionally persisted as JSON.
pub struct ReviewQueue {
    items: Vec<ReviewItem>,
    path: Option<PathBuf>,
}

impl ReviewQueue {
    /// Create an in-memory review queue (no persistence).
    pub fn in_memory() -> Self {
        Self {
            items: Vec::new(),
            path: None,
        }
    }

    /// Open a persistent review queue from a JSON file.
    pub fn open(path: &Path) -> Result<Self> {
        let items = if path.exists() {
            let data = std::fs::read_to_string(path)?;
            serde_json::from_str(&data)?
        } else {
            Vec::new()
        };
        Ok(Self {
            items,
            path: Some(path.to_path_buf()),
        })
    }

    /// Add a completed chunk to the review queue.
    pub fn enqueue(&mut self, item: ReviewItem) -> Result<()> {
        self.items.push(item);
        self.persist()
    }

    /// List items, optionally filtered by status.
    pub fn list(&self, status: Option<ReviewStatus>) -> Vec<&ReviewItem> {
        match status {
            Some(s) => self.items.iter().filter(|i| i.status == s).collect(),
            None => self.items.iter().collect(),
        }
    }

    /// Approve a review item. Returns the task_id.
    pub fn approve(&mut self, id: &str, notes: Option<String>) -> Result<String> {
        let item = self
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow!("review item not found: {id}"))?;

        if item.status != ReviewStatus::Pending {
            return Err(anyhow!("review item {id} is not pending"));
        }

        item.status = ReviewStatus::Approved;
        item.reviewer_notes = notes;
        let task_id = item.task_id.clone();
        self.persist()?;
        Ok(task_id)
    }

    /// Reject a review item. Returns the task_id.
    pub fn reject(&mut self, id: &str, notes: String) -> Result<String> {
        let item = self
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| anyhow!("review item not found: {id}"))?;

        if item.status != ReviewStatus::Pending {
            return Err(anyhow!("review item {id} is not pending"));
        }

        item.status = ReviewStatus::Rejected;
        item.reviewer_notes = Some(notes);
        let task_id = item.task_id.clone();
        self.persist()?;
        Ok(task_id)
    }

    /// Get count of pending reviews.
    pub fn pending_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == ReviewStatus::Pending)
            .count()
    }

    /// Total items in the queue.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Persist to disk if a path is configured.
    fn persist(&self) -> Result<()> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let data = serde_json::to_string_pretty(&self.items)?;
            std::fs::write(path, data)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Helpers --

    fn make_task(id: &str, priority: TaskPriority, deps: Vec<&str>) -> SwarmTask {
        SwarmTask {
            id: id.to_string(),
            title: format!("Task {id}"),
            instructions: format!("Do {id}"),
            priority,
            status: TaskStatus::Pending,
            depends_on: deps.into_iter().map(String::from).collect(),
            reads: vec![],
            writes: vec![],
            assigned_agent: None,
            result: None,
            retry_count: 0,
        }
    }

    fn make_task_with_files(id: &str, reads: Vec<&str>, writes: Vec<&str>) -> SwarmTask {
        SwarmTask {
            id: id.to_string(),
            title: format!("Task {id}"),
            instructions: format!("Do {id}"),
            priority: TaskPriority::Medium,
            status: TaskStatus::Pending,
            depends_on: vec![],
            reads: reads.into_iter().map(String::from).collect(),
            writes: writes.into_iter().map(String::from).collect(),
            assigned_agent: None,
            result: None,
            retry_count: 0,
        }
    }

    fn make_result(score: f64) -> TaskResult {
        TaskResult {
            output: "done".to_string(),
            files_changed: vec!["file.rs".to_string()],
            score,
        }
    }

    // -- SwarmConfig tests --

    #[test]
    fn default_config_has_sensible_values() {
        let config = SwarmConfig::default();
        assert_eq!(config.max_parallel, 3);
        assert_eq!(config.max_total_agents, 10);
        assert_eq!(config.poll_interval_secs, 5);
        assert!(config.auto_commit);
        assert_eq!(config.channel_buffer_size, 64);
        assert!((config.auto_approve_threshold - 0.85).abs() < f64::EPSILON);
    }

    // -- Priority ordering --

    #[test]
    fn task_priority_ordering() {
        assert!(TaskPriority::Critical < TaskPriority::High);
        assert!(TaskPriority::High < TaskPriority::Medium);
        assert!(TaskPriority::Medium < TaskPriority::Low);
    }

    // -- Available tasks --

    #[test]
    fn get_available_tasks_returns_pending_with_met_deps() {
        let tasks = vec![
            make_task("a", TaskPriority::Medium, vec![]),
            make_task("b", TaskPriority::High, vec!["a"]),
        ];
        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);

        let available = orch.get_available_tasks();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "a");
    }

    #[test]
    fn get_available_tasks_unblocks_after_dep_approved() {
        let mut tasks = vec![
            make_task("a", TaskPriority::Medium, vec![]),
            make_task("b", TaskPriority::High, vec!["a"]),
        ];
        tasks[0].status = TaskStatus::Approved;

        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        let available = orch.get_available_tasks();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "b");
    }

    #[test]
    fn get_available_tasks_sorted_by_priority() {
        let tasks = vec![
            make_task("low", TaskPriority::Low, vec![]),
            make_task("critical", TaskPriority::Critical, vec![]),
            make_task("high", TaskPriority::High, vec![]),
        ];
        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);

        let available = orch.get_available_tasks();
        assert_eq!(available.len(), 3);
        assert_eq!(available[0].id, "critical");
        assert_eq!(available[1].id, "high");
        assert_eq!(available[2].id, "low");
    }

    // -- Parallelization --

    #[test]
    fn can_parallelize_no_conflicts() {
        let tasks = vec![
            make_task_with_files("a", vec!["r1.rs"], vec!["w1.rs"]),
            make_task_with_files("b", vec!["r2.rs"], vec!["w2.rs"]),
        ];
        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        assert!(orch.can_parallelize(&["a", "b"]));
    }

    #[test]
    fn can_parallelize_write_write_conflict() {
        let tasks = vec![
            make_task_with_files("a", vec![], vec!["shared.rs"]),
            make_task_with_files("b", vec![], vec!["shared.rs"]),
        ];
        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        assert!(!orch.can_parallelize(&["a", "b"]));
    }

    #[test]
    fn can_parallelize_write_read_conflict() {
        let tasks = vec![
            make_task_with_files("a", vec![], vec!["shared.rs"]),
            make_task_with_files("b", vec!["shared.rs"], vec![]),
        ];
        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        assert!(!orch.can_parallelize(&["a", "b"]));
    }

    // -- Assignment --

    #[tokio::test]
    async fn assign_task_sets_in_progress() {
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);

        let _rx = orch.assign_task("a", "agent-1").expect("assign");
        assert_eq!(orch.tasks()[0].status, TaskStatus::InProgress);
        assert_eq!(orch.tasks()[0].assigned_agent.as_deref(), Some("agent-1"));
        assert_eq!(orch.active_count(), 1);
        assert_eq!(orch.total_spawned(), 1);
    }

    #[tokio::test]
    async fn assign_task_fails_on_nonexistent() {
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);

        let err = orch.assign_task("nonexistent", "agent-1").unwrap_err();
        assert!(err.to_string().contains("task not found"));
    }

    #[tokio::test]
    async fn assign_task_respects_max_parallel() {
        let config = SwarmConfig {
            max_parallel: 1,
            ..SwarmConfig::default()
        };
        let tasks = vec![
            make_task("a", TaskPriority::Medium, vec![]),
            make_task("b", TaskPriority::Medium, vec![]),
        ];
        let mut orch = SwarmOrchestrator::new(config, tasks);

        let _rx = orch.assign_task("a", "agent-1").expect("first assign");
        let err = orch.assign_task("b", "agent-2").unwrap_err();
        assert!(err.to_string().contains("max parallel"));
    }

    #[tokio::test]
    async fn assign_task_respects_max_total() {
        let config = SwarmConfig {
            max_total_agents: 1,
            ..SwarmConfig::default()
        };
        let tasks = vec![
            make_task("a", TaskPriority::Medium, vec![]),
            make_task("b", TaskPriority::Medium, vec![]),
        ];
        let mut orch = SwarmOrchestrator::new(config, tasks);

        let _rx = orch.assign_task("a", "agent-1").expect("first assign");
        // Complete first task to free the parallel slot
        orch.handle_message(AgentMessage::Completed {
            task_id: "a".to_string(),
            result: make_result(0.9),
        })
        .expect("handle");

        let err = orch.assign_task("b", "agent-2").unwrap_err();
        assert!(err.to_string().contains("max total"));
    }

    // -- Message handling: completion --

    #[tokio::test]
    async fn handle_completed_auto_approves_high_score() {
        let config = SwarmConfig {
            auto_approve_threshold: 0.85,
            ..SwarmConfig::default()
        };
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(config, tasks);
        let _rx = orch.assign_task("a", "agent-1").unwrap();

        let completed = orch
            .handle_message(AgentMessage::Completed {
                task_id: "a".to_string(),
                result: make_result(0.9),
            })
            .expect("handle");

        assert!(completed);
        assert_eq!(orch.tasks()[0].status, TaskStatus::Approved);
        assert_eq!(orch.active_count(), 0);
        assert_eq!(orch.review_queue().pending_count(), 0);
    }

    #[tokio::test]
    async fn handle_completed_sends_to_review_low_score() {
        let config = SwarmConfig {
            auto_approve_threshold: 0.85,
            ..SwarmConfig::default()
        };
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(config, tasks);
        let _rx = orch.assign_task("a", "agent-1").unwrap();

        orch.handle_message(AgentMessage::Completed {
            task_id: "a".to_string(),
            result: make_result(0.5),
        })
        .expect("handle");

        assert_eq!(orch.tasks()[0].status, TaskStatus::PendingReview);
        assert_eq!(orch.review_queue().pending_count(), 1);
    }

    // -- Message handling: failure --

    #[tokio::test]
    async fn handle_failure_retries_once() {
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        let _rx = orch.assign_task("a", "agent-1").unwrap();

        orch.handle_message(AgentMessage::Failed {
            task_id: "a".to_string(),
            error: "timeout".to_string(),
        })
        .expect("handle");

        // Should be re-queued as pending
        assert_eq!(orch.tasks()[0].status, TaskStatus::Pending);
        assert_eq!(orch.tasks()[0].retry_count, 1);
        assert!(orch.tasks()[0].assigned_agent.is_none());
    }

    #[tokio::test]
    async fn handle_failure_fails_after_retry() {
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);

        // First attempt
        let _rx = orch.assign_task("a", "agent-1").unwrap();
        orch.handle_message(AgentMessage::Failed {
            task_id: "a".to_string(),
            error: "timeout".to_string(),
        })
        .unwrap();

        // Second attempt (retry)
        let _rx = orch.assign_task("a", "agent-2").unwrap();
        orch.handle_message(AgentMessage::Failed {
            task_id: "a".to_string(),
            error: "timeout again".to_string(),
        })
        .unwrap();

        assert_eq!(orch.tasks()[0].status, TaskStatus::Failed);
    }

    // -- Progress messages --

    #[tokio::test]
    async fn handle_progress_does_not_complete() {
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        let _rx = orch.assign_task("a", "agent-1").unwrap();

        let completed = orch
            .handle_message(AgentMessage::Progress {
                task_id: "a".to_string(),
                percent: 50,
                message: "halfway".to_string(),
            })
            .unwrap();

        assert!(!completed);
        assert_eq!(orch.tasks()[0].status, TaskStatus::InProgress);
    }

    // -- Channel coordination --

    #[tokio::test]
    async fn channel_coordination_works() {
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        let _rx = orch.assign_task("a", "agent-1").unwrap();

        // Agent sends a message through the channel
        let tx = orch.agent_sender();
        tx.send(AgentMessage::Completed {
            task_id: "a".to_string(),
            result: make_result(0.9),
        })
        .await
        .unwrap();

        // Orchestrator drains messages
        let count = orch.drain_messages().unwrap();
        assert_eq!(count, 1);
        assert_eq!(orch.tasks()[0].status, TaskStatus::Approved);
    }

    // -- Cancel --

    #[tokio::test]
    async fn cancel_task_requeues() {
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let mut orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        let mut rx = orch.assign_task("a", "agent-1").unwrap();

        orch.cancel_task("a", "no longer needed").await.unwrap();

        assert_eq!(orch.tasks()[0].status, TaskStatus::Pending);
        assert!(orch.tasks()[0].assigned_agent.is_none());
        assert_eq!(orch.active_count(), 0);

        // Agent should receive cancel message
        let msg = rx.recv().await.unwrap();
        assert!(matches!(msg, OrchestratorMessage::Cancel { .. }));
    }

    // -- Completion check --

    #[test]
    fn is_complete_when_all_approved() {
        let mut tasks = vec![
            make_task("a", TaskPriority::Medium, vec![]),
            make_task("b", TaskPriority::Medium, vec![]),
        ];
        tasks[0].status = TaskStatus::Approved;
        tasks[1].status = TaskStatus::Approved;

        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        assert!(orch.is_complete());
    }

    #[test]
    fn is_complete_when_mixed_approved_and_failed() {
        let mut tasks = vec![
            make_task("a", TaskPriority::Medium, vec![]),
            make_task("b", TaskPriority::Medium, vec![]),
        ];
        tasks[0].status = TaskStatus::Approved;
        tasks[1].status = TaskStatus::Failed;

        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        assert!(orch.is_complete());
    }

    #[test]
    fn not_complete_when_pending() {
        let tasks = vec![make_task("a", TaskPriority::Medium, vec![])];
        let orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);
        assert!(!orch.is_complete());
    }

    // -- Review Queue tests --

    #[test]
    fn review_queue_enqueue_and_list() {
        let mut queue = ReviewQueue::in_memory();
        queue
            .enqueue(ReviewItem {
                id: "r1".to_string(),
                task_id: "t1".to_string(),
                title: "Task 1".to_string(),
                summary: "Done".to_string(),
                files_changed: vec!["a.rs".to_string()],
                score: 0.7,
                status: ReviewStatus::Pending,
                reviewer_notes: None,
            })
            .unwrap();

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pending_count(), 1);
        assert_eq!(queue.list(None).len(), 1);
        assert_eq!(queue.list(Some(ReviewStatus::Pending)).len(), 1);
        assert_eq!(queue.list(Some(ReviewStatus::Approved)).len(), 0);
    }

    #[test]
    fn review_queue_approve() {
        let mut queue = ReviewQueue::in_memory();
        queue
            .enqueue(ReviewItem {
                id: "r1".to_string(),
                task_id: "t1".to_string(),
                title: "Task 1".to_string(),
                summary: "Done".to_string(),
                files_changed: vec![],
                score: 0.7,
                status: ReviewStatus::Pending,
                reviewer_notes: None,
            })
            .unwrap();

        let task_id = queue.approve("r1", Some("LGTM".to_string())).unwrap();
        assert_eq!(task_id, "t1");
        assert_eq!(queue.pending_count(), 0);
        assert_eq!(queue.list(Some(ReviewStatus::Approved)).len(), 1);
    }

    #[test]
    fn review_queue_reject() {
        let mut queue = ReviewQueue::in_memory();
        queue
            .enqueue(ReviewItem {
                id: "r1".to_string(),
                task_id: "t1".to_string(),
                title: "Task 1".to_string(),
                summary: "Done".to_string(),
                files_changed: vec![],
                score: 0.5,
                status: ReviewStatus::Pending,
                reviewer_notes: None,
            })
            .unwrap();

        let task_id = queue.reject("r1", "needs work".to_string()).unwrap();
        assert_eq!(task_id, "t1");
        assert_eq!(queue.pending_count(), 0);
        assert_eq!(queue.list(Some(ReviewStatus::Rejected)).len(), 1);
    }

    #[test]
    fn review_queue_approve_nonexistent_fails() {
        let mut queue = ReviewQueue::in_memory();
        let err = queue.approve("nonexistent", None).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn review_queue_double_approve_fails() {
        let mut queue = ReviewQueue::in_memory();
        queue
            .enqueue(ReviewItem {
                id: "r1".to_string(),
                task_id: "t1".to_string(),
                title: "Task 1".to_string(),
                summary: "Done".to_string(),
                files_changed: vec![],
                score: 0.8,
                status: ReviewStatus::Pending,
                reviewer_notes: None,
            })
            .unwrap();

        queue.approve("r1", None).unwrap();
        let err = queue.approve("r1", None).unwrap_err();
        assert!(err.to_string().contains("not pending"));
    }

    #[test]
    fn review_queue_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review-queue.json");

        {
            let mut queue = ReviewQueue::open(&path).unwrap();
            queue
                .enqueue(ReviewItem {
                    id: "r1".to_string(),
                    task_id: "t1".to_string(),
                    title: "Task 1".to_string(),
                    summary: "Done".to_string(),
                    files_changed: vec!["a.rs".to_string()],
                    score: 0.7,
                    status: ReviewStatus::Pending,
                    reviewer_notes: None,
                })
                .unwrap();
        }

        // Re-open and verify
        let queue = ReviewQueue::open(&path).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.list(None)[0].id, "r1");
    }

    // -- Multi-task orchestration scenario --

    #[tokio::test]
    async fn full_orchestration_scenario() {
        // Three tasks: a, b (depends on a), c (independent)
        let tasks = vec![
            make_task("a", TaskPriority::High, vec![]),
            make_task("b", TaskPriority::Medium, vec!["a"]),
            make_task("c", TaskPriority::Low, vec![]),
        ];
        let mut orch = SwarmOrchestrator::new(SwarmConfig::default(), tasks);

        // Initially: a and c are available
        let available = orch.get_available_tasks();
        assert_eq!(available.len(), 2);
        assert_eq!(available[0].id, "a"); // Higher priority
        assert_eq!(available[1].id, "c");

        // Assign a and c
        let _rx_a = orch.assign_task("a", "agent-1").unwrap();
        let _rx_c = orch.assign_task("c", "agent-2").unwrap();
        assert_eq!(orch.active_count(), 2);

        // No more available (b is blocked)
        assert!(orch.get_available_tasks().is_empty());

        // Complete a (high score -> auto-approve)
        orch.handle_message(AgentMessage::Completed {
            task_id: "a".to_string(),
            result: make_result(0.9),
        })
        .unwrap();

        // Now b is available
        let available = orch.get_available_tasks();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].id, "b");

        // Assign and complete b
        let _rx_b = orch.assign_task("b", "agent-3").unwrap();
        orch.handle_message(AgentMessage::Completed {
            task_id: "b".to_string(),
            result: make_result(0.95),
        })
        .unwrap();

        // Complete c
        orch.handle_message(AgentMessage::Completed {
            task_id: "c".to_string(),
            result: make_result(0.88),
        })
        .unwrap();

        assert!(orch.is_complete());
        assert_eq!(orch.total_spawned(), 3);
    }
}
