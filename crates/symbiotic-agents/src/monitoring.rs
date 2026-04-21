//! Agent execution monitoring: tracks agent runs with start/stop lifecycle,
//! tool call counts, token usage, and status.
//!
//! The [`AgentMonitor`] trait defines the monitoring interface. The
//! [`SqliteAgentMonitor`] stores records in a local SQLite database with
//! configurable rolling retention.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from the agent monitoring layer.
#[derive(Debug, Error)]
pub enum MonitorError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("not found: {0}")]
    NotFound(String),
}

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Status of an agent execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionStatus {
    /// Agent is currently running.
    Running,
    /// Agent completed successfully.
    Success,
    /// Agent failed with an error.
    Failed,
    /// Agent was cancelled.
    Cancelled,
    /// Agent hit the context handoff limit.
    Handoff,
}

impl ExecutionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Success => "Success",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Handoff => "Handoff",
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

impl std::str::FromStr for ExecutionStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Running" => Ok(Self::Running),
            "Success" => Ok(Self::Success),
            "Failed" => Ok(Self::Failed),
            "Cancelled" => Ok(Self::Cancelled),
            "Handoff" => Ok(Self::Handoff),
            other => Err(format!("unknown execution status: {other}")),
        }
    }
}

impl std::fmt::Display for ExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Type of agent being monitored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentType {
    /// Single ReAct-loop agent.
    React,
    /// Swarm orchestrator.
    Swarm,
    /// Worker within a swarm.
    Worker,
}

impl AgentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::React => "React",
            Self::Swarm => "Swarm",
            Self::Worker => "Worker",
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "React" => Ok(Self::React),
            "Swarm" => Ok(Self::Swarm),
            "Worker" => Ok(Self::Worker),
            other => Err(format!("unknown agent type: {other}")),
        }
    }
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single agent execution record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentExecution {
    /// Unique execution ID.
    pub execution_id: String,
    /// Agent identifier (e.g., `agent_<hash>`).
    pub agent_id: String,
    /// Type of agent.
    pub agent_type: AgentType,
    /// Task or goal identifier.
    pub task_id: Option<String>,
    /// Parent execution ID (for sub-agents within a swarm).
    pub parent_id: Option<String>,
    /// When the agent started.
    pub started_at: DateTime<Utc>,
    /// When the agent finished (None if still running).
    pub finished_at: Option<DateTime<Utc>>,
    /// Current status.
    pub status: ExecutionStatus,
    /// Number of ReAct loop iterations completed.
    pub iterations: u32,
    /// Number of tool calls made.
    pub tool_call_count: u32,
    /// Input tokens consumed.
    pub tokens_in: Option<u64>,
    /// Output tokens consumed.
    pub tokens_out: Option<u64>,
    /// Error message if status is Failed.
    pub error_message: Option<String>,
    /// LLM model used.
    pub model: Option<String>,
    /// JSON-serialized tool call records (for Process Engineer analysis).
    /// Contains an array of `ToolCallRecord` objects when available.
    pub tool_calls_json: Option<String>,
}

/// Filter criteria for querying executions.
#[derive(Debug, Clone, Default)]
pub struct ExecutionFilter {
    pub agent_id: Option<String>,
    pub agent_type: Option<AgentType>,
    pub task_id: Option<String>,
    pub status: Option<ExecutionStatus>,
    pub limit: Option<u32>,
}

/// Summary statistics for agent executions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSummary {
    /// Total executions in the window.
    pub total: u64,
    /// Currently running agents.
    pub active: u64,
    /// Successfully completed.
    pub succeeded: u64,
    /// Failed.
    pub failed: u64,
    /// Cancelled.
    pub cancelled: u64,
    /// Handed off due to context limits.
    pub handoffs: u64,
    /// Average duration in milliseconds (completed only).
    pub avg_duration_ms: f64,
    /// Total input tokens.
    pub total_tokens_in: u64,
    /// Total output tokens.
    pub total_tokens_out: u64,
    /// Success rate (0.0 to 1.0, excluding running).
    pub success_rate: f64,
}

/// Configuration for the agent monitor.
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// How many days of data to retain. Default: 30.
    pub retention_days: u32,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self { retention_days: 30 }
    }
}

// ---------------------------------------------------------------------------
// AgentMonitor trait
// ---------------------------------------------------------------------------

/// Data for recording the finish of an agent execution.
#[derive(Debug, Clone)]
pub struct FinishRecord<'a> {
    pub execution_id: &'a str,
    pub status: ExecutionStatus,
    pub iterations: u32,
    pub tool_call_count: u32,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub error_message: Option<&'a str>,
    /// Optional JSON-serialized tool call records for PE analysis.
    pub tool_calls_json: Option<&'a str>,
}

/// Trait for agent execution monitoring.
///
/// Implementations record agent lifecycle events and provide query capabilities
/// for dashboards and CLI tools.
pub trait AgentMonitor: Send + Sync {
    /// Record the start of an agent execution. Returns the execution ID.
    fn record_start(
        &self,
        agent_id: &str,
        agent_type: AgentType,
        task_id: Option<&str>,
        parent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<String, MonitorError>;

    /// Record the completion of an agent execution.
    fn record_finish(&self, record: &FinishRecord<'_>) -> Result<(), MonitorError>;

    /// Query executions with filters.
    fn query_executions(
        &self,
        filter: &ExecutionFilter,
        since: DateTime<Utc>,
    ) -> Result<Vec<AgentExecution>, MonitorError>;

    /// Get a single execution by ID.
    fn get_execution(&self, execution_id: &str) -> Result<Option<AgentExecution>, MonitorError>;

    /// Get summary statistics for a time window.
    fn summary(&self, since: DateTime<Utc>) -> Result<ExecutionSummary, MonitorError>;

    /// Get currently running executions.
    fn active_executions(&self) -> Result<Vec<AgentExecution>, MonitorError>;
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

/// SQLite-backed agent monitor.
///
/// The connection is wrapped in a `Mutex` to satisfy the `Send + Sync`
/// requirements of the `AgentMonitor` trait, allowing the monitor to be
/// shared across async tasks.
pub struct SqliteAgentMonitor {
    conn: Mutex<Connection>,
}

impl SqliteAgentMonitor {
    /// Open (or create) a monitor database at the given path.
    pub fn open(path: &Path, config: &MonitorConfig) -> Result<Self, MonitorError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MonitorError::Storage(format!("failed to create directory: {e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| MonitorError::Storage(format!("failed to open database: {e}")))?;
        let monitor = Self {
            conn: Mutex::new(conn),
        };
        monitor.init_schema()?;
        monitor.cleanup_old_records(config.retention_days)?;
        Ok(monitor)
    }

    /// Create an in-memory monitor (for testing).
    pub fn open_in_memory() -> Result<Self, MonitorError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| MonitorError::Storage(format!("failed to open in-memory db: {e}")))?;
        let monitor = Self {
            conn: Mutex::new(conn),
        };
        monitor.init_schema()?;
        Ok(monitor)
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, MonitorError> {
        self.conn
            .lock()
            .map_err(|_| MonitorError::Storage("connection lock poisoned".to_string()))
    }

    fn init_schema(&self) -> Result<(), MonitorError> {
        let conn = self.lock_conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_executions (
                    execution_id TEXT PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    agent_type TEXT NOT NULL,
                    task_id TEXT,
                    parent_id TEXT,
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    status TEXT NOT NULL,
                    iterations INTEGER NOT NULL DEFAULT 0,
                    tool_call_count INTEGER NOT NULL DEFAULT 0,
                    tokens_in INTEGER,
                    tokens_out INTEGER,
                    error_message TEXT,
                    model TEXT,
                    tool_calls_json TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_exec_started ON agent_executions(started_at);
                CREATE INDEX IF NOT EXISTS idx_exec_status ON agent_executions(status);
                CREATE INDEX IF NOT EXISTS idx_exec_agent ON agent_executions(agent_id);
                CREATE INDEX IF NOT EXISTS idx_exec_task ON agent_executions(task_id);
                CREATE INDEX IF NOT EXISTS idx_exec_parent ON agent_executions(parent_id);
                ",
        )
        .map_err(|e| MonitorError::Storage(format!("schema init failed: {e}")))?;

        // Migration: add tool_calls_json column if missing (existing databases).
        let has_column: bool = conn
            .prepare("SELECT tool_calls_json FROM agent_executions LIMIT 0")
            .is_ok();
        if !has_column {
            let _ =
                conn.execute_batch("ALTER TABLE agent_executions ADD COLUMN tool_calls_json TEXT;");
        }

        Ok(())
    }

    fn cleanup_old_records(&self, retention_days: u32) -> Result<(), MonitorError> {
        let cutoff = Utc::now() - Duration::days(retention_days as i64);
        self.lock_conn()?
            .execute(
                "DELETE FROM agent_executions WHERE started_at < ?1 AND status != 'Running'",
                params![cutoff.to_rfc3339()],
            )
            .map_err(|e| MonitorError::Storage(format!("cleanup failed: {e}")))?;
        Ok(())
    }

    fn generate_execution_id() -> String {
        let ts = Utc::now().timestamp_millis();
        let rand_part: u32 = rand::random();
        format!("exec_{ts:x}_{rand_part:08x}")
    }

    fn row_to_execution(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentExecution> {
        let execution_id: String = row.get(0)?;
        let agent_id: String = row.get(1)?;
        let agent_type_str: String = row.get(2)?;
        let task_id: Option<String> = row.get(3)?;
        let parent_id: Option<String> = row.get(4)?;
        let started_at_str: String = row.get(5)?;
        let finished_at_str: Option<String> = row.get(6)?;
        let status_str: String = row.get(7)?;
        let iterations: i32 = row.get(8)?;
        let tool_call_count: i32 = row.get(9)?;
        let tokens_in: Option<i64> = row.get(10)?;
        let tokens_out: Option<i64> = row.get(11)?;
        let error_message: Option<String> = row.get(12)?;
        let model: Option<String> = row.get(13)?;
        let tool_calls_json: Option<String> = row.get(14)?;

        let agent_type: AgentType = agent_type_str.parse().unwrap_or(AgentType::React);
        let started_at = DateTime::parse_from_rfc3339(&started_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let finished_at = finished_at_str.and_then(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&Utc))
                .ok()
        });
        let status: ExecutionStatus = status_str.parse().unwrap_or(ExecutionStatus::Failed);

        Ok(AgentExecution {
            execution_id,
            agent_id,
            agent_type,
            task_id,
            parent_id,
            started_at,
            finished_at,
            status,
            iterations: iterations as u32,
            tool_call_count: tool_call_count as u32,
            tokens_in: tokens_in.map(|v| v as u64),
            tokens_out: tokens_out.map(|v| v as u64),
            error_message,
            model,
            tool_calls_json,
        })
    }
}

impl AgentMonitor for SqliteAgentMonitor {
    fn record_start(
        &self,
        agent_id: &str,
        agent_type: AgentType,
        task_id: Option<&str>,
        parent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<String, MonitorError> {
        let execution_id = Self::generate_execution_id();
        let now = Utc::now();

        self.lock_conn()?
            .execute(
                "INSERT INTO agent_executions
                 (execution_id, agent_id, agent_type, task_id, parent_id,
                  started_at, status, iterations, tool_call_count, model)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, ?8)",
                params![
                    execution_id,
                    agent_id,
                    agent_type.as_str(),
                    task_id,
                    parent_id,
                    now.to_rfc3339(),
                    ExecutionStatus::Running.as_str(),
                    model,
                ],
            )
            .map_err(|e| MonitorError::Storage(format!("insert failed: {e}")))?;

        Ok(execution_id)
    }

    fn record_finish(&self, record: &FinishRecord<'_>) -> Result<(), MonitorError> {
        let now = Utc::now();
        let changed = self
            .lock_conn()?
            .execute(
                "UPDATE agent_executions SET
                    finished_at = ?1,
                    status = ?2,
                    iterations = ?3,
                    tool_call_count = ?4,
                    tokens_in = ?5,
                    tokens_out = ?6,
                    error_message = ?7,
                    tool_calls_json = ?8
                 WHERE execution_id = ?9",
                params![
                    now.to_rfc3339(),
                    record.status.as_str(),
                    record.iterations as i32,
                    record.tool_call_count as i32,
                    record.tokens_in.map(|v| v as i64),
                    record.tokens_out.map(|v| v as i64),
                    record.error_message,
                    record.tool_calls_json,
                    record.execution_id,
                ],
            )
            .map_err(|e| MonitorError::Storage(format!("update failed: {e}")))?;

        if changed == 0 {
            return Err(MonitorError::NotFound(format!(
                "execution {} not found",
                record.execution_id
            )));
        }
        Ok(())
    }

    fn query_executions(
        &self,
        filter: &ExecutionFilter,
        since: DateTime<Utc>,
    ) -> Result<Vec<AgentExecution>, MonitorError> {
        let mut sql = String::from(
            "SELECT execution_id, agent_id, agent_type, task_id, parent_id,
                    started_at, finished_at, status, iterations, tool_call_count,
                    tokens_in, tokens_out, error_message, model, tool_calls_json
             FROM agent_executions WHERE started_at >= ?1",
        );
        let mut bind_values: Vec<String> = vec![since.to_rfc3339()];
        let mut param_idx = 2u32;

        if let Some(ref agent_id) = filter.agent_id {
            sql.push_str(&format!(" AND agent_id = ?{param_idx}"));
            bind_values.push(agent_id.clone());
            param_idx += 1;
        }
        if let Some(ref agent_type) = filter.agent_type {
            sql.push_str(&format!(" AND agent_type = ?{param_idx}"));
            bind_values.push(agent_type.as_str().to_string());
            param_idx += 1;
        }
        if let Some(ref task_id) = filter.task_id {
            sql.push_str(&format!(" AND task_id = ?{param_idx}"));
            bind_values.push(task_id.clone());
            param_idx += 1;
        }
        if let Some(ref status) = filter.status {
            sql.push_str(&format!(" AND status = ?{param_idx}"));
            bind_values.push(status.as_str().to_string());
            // param_idx not used after this, but keep pattern consistent
            let _ = param_idx;
        }

        sql.push_str(" ORDER BY started_at DESC");

        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| MonitorError::Storage(format!("query prepare failed: {e}")))?;

        let params_refs: Vec<&dyn rusqlite::types::ToSql> = bind_values
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();

        let rows = stmt
            .query_map(params_refs.as_slice(), Self::row_to_execution)
            .map_err(|e| MonitorError::Storage(format!("query failed: {e}")))?;

        let mut executions = Vec::new();
        for row_result in rows {
            let exec =
                row_result.map_err(|e| MonitorError::Storage(format!("row read failed: {e}")))?;
            executions.push(exec);
        }

        Ok(executions)
    }

    fn get_execution(&self, execution_id: &str) -> Result<Option<AgentExecution>, MonitorError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT execution_id, agent_id, agent_type, task_id, parent_id,
                        started_at, finished_at, status, iterations, tool_call_count,
                        tokens_in, tokens_out, error_message, model, tool_calls_json
                 FROM agent_executions WHERE execution_id = ?1",
            )
            .map_err(|e| MonitorError::Storage(format!("query prepare failed: {e}")))?;

        let mut rows = stmt
            .query_map(params![execution_id], Self::row_to_execution)
            .map_err(|e| MonitorError::Storage(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(exec)) => Ok(Some(exec)),
            Some(Err(e)) => Err(MonitorError::Storage(format!("row read failed: {e}"))),
            None => Ok(None),
        }
    }

    fn summary(&self, since: DateTime<Utc>) -> Result<ExecutionSummary, MonitorError> {
        let since_str = since.to_rfc3339();
        let conn = self.lock_conn()?;

        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_executions WHERE started_at >= ?1",
                params![since_str],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("count query failed: {e}")))?;

        let active: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_executions WHERE status = 'Running'",
                [],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("active count failed: {e}")))?;

        let succeeded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_executions WHERE started_at >= ?1 AND status = 'Success'",
                params![since_str],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("success count failed: {e}")))?;

        let failed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_executions WHERE started_at >= ?1 AND status = 'Failed'",
                params![since_str],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("failed count failed: {e}")))?;

        let cancelled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_executions WHERE started_at >= ?1 AND status = 'Cancelled'",
                params![since_str],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("cancelled count failed: {e}")))?;

        let handoffs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_executions WHERE started_at >= ?1 AND status = 'Handoff'",
                params![since_str],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("handoff count failed: {e}")))?;

        // Average duration for completed executions
        let avg_duration_ms: f64 = conn
            .query_row(
                "SELECT COALESCE(AVG(
                    (julianday(finished_at) - julianday(started_at)) * 86400000
                 ), 0.0)
                 FROM agent_executions
                 WHERE started_at >= ?1 AND finished_at IS NOT NULL",
                params![since_str],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("avg duration failed: {e}")))?;

        // Token totals
        let total_tokens_in: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(tokens_in), 0) FROM agent_executions WHERE started_at >= ?1",
                params![since_str],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("token sum failed: {e}")))?;

        let total_tokens_out: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(tokens_out), 0) FROM agent_executions WHERE started_at >= ?1",
                params![since_str],
                |row: &rusqlite::Row<'_>| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("token sum failed: {e}")))?;

        let terminal_count = succeeded + failed + cancelled + handoffs;
        let success_rate = if terminal_count > 0 {
            succeeded as f64 / terminal_count as f64
        } else {
            0.0
        };

        Ok(ExecutionSummary {
            total: total as u64,
            active: active as u64,
            succeeded: succeeded as u64,
            failed: failed as u64,
            cancelled: cancelled as u64,
            handoffs: handoffs as u64,
            avg_duration_ms,
            total_tokens_in: total_tokens_in as u64,
            total_tokens_out: total_tokens_out as u64,
            success_rate,
        })
    }

    fn active_executions(&self) -> Result<Vec<AgentExecution>, MonitorError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT execution_id, agent_id, agent_type, task_id, parent_id,
                        started_at, finished_at, status, iterations, tool_call_count,
                        tokens_in, tokens_out, error_message, model, tool_calls_json
                 FROM agent_executions WHERE status = 'Running'
                 ORDER BY started_at ASC",
            )
            .map_err(|e| MonitorError::Storage(format!("query prepare failed: {e}")))?;

        let rows = stmt
            .query_map([], Self::row_to_execution)
            .map_err(|e| MonitorError::Storage(format!("query failed: {e}")))?;

        let mut executions = Vec::new();
        for row_result in rows {
            let exec =
                row_result.map_err(|e| MonitorError::Storage(format!("row read failed: {e}")))?;
            executions.push(exec);
        }

        Ok(executions)
    }
}

// ---------------------------------------------------------------------------
// CLI formatting helpers
// ---------------------------------------------------------------------------

/// Format the execution summary for CLI display.
pub fn format_execution_summary(summary: &ExecutionSummary, window_label: &str) -> String {
    let mut out = String::new();

    let header = format!("Agent Execution Summary ({window_label})");
    out.push_str(&header);
    out.push('\n');
    out.push_str(&"=".repeat(header.len()));
    out.push('\n');

    out.push_str(&format!("Total executions:  {}\n", summary.total));
    out.push_str(&format!("Active (running):  {}\n", summary.active));
    out.push_str(&format!("Succeeded:         {}\n", summary.succeeded));
    out.push_str(&format!("Failed:            {}\n", summary.failed));
    out.push_str(&format!("Cancelled:         {}\n", summary.cancelled));
    out.push_str(&format!("Handoffs:          {}\n", summary.handoffs));
    out.push_str(&format!(
        "Success rate:      {:.1}%\n",
        summary.success_rate * 100.0
    ));
    out.push_str(&format!(
        "Avg duration:      {}\n",
        format_duration_ms(summary.avg_duration_ms as u64)
    ));
    out.push_str(&format!(
        "Tokens (in/out):   {} / {}\n",
        format_tokens(summary.total_tokens_in),
        format_tokens(summary.total_tokens_out),
    ));

    out
}

/// Format a list of executions for CLI display.
pub fn format_execution_list(executions: &[AgentExecution]) -> String {
    if executions.is_empty() {
        return "No agent executions found.\n".to_string();
    }

    let mut out = String::new();

    // Header
    out.push_str(&format!(
        "{:<24} {:<8} {:<10} {:<10} {:<6} {:<6} {:<12}\n",
        "AGENT", "TYPE", "STATUS", "TASK", "ITER", "TOOLS", "DURATION"
    ));
    out.push_str(&"-".repeat(78));
    out.push('\n');

    for exec in executions {
        let duration = match (exec.finished_at, exec.status) {
            (Some(finished), _) => {
                let dur = finished - exec.started_at;
                format_duration_ms(dur.num_milliseconds().max(0) as u64)
            }
            (None, ExecutionStatus::Running) => {
                let dur = Utc::now() - exec.started_at;
                format!(
                    "{}...",
                    format_duration_ms(dur.num_milliseconds().max(0) as u64)
                )
            }
            _ => "-".to_string(),
        };

        let agent_display = if exec.agent_id.len() > 22 {
            format!("{}...", &exec.agent_id[..19])
        } else {
            exec.agent_id.clone()
        };

        let task_display = exec
            .task_id
            .as_deref()
            .map(|t| {
                if t.len() > 8 {
                    format!("{}...", &t[..5])
                } else {
                    t.to_string()
                }
            })
            .unwrap_or_else(|| "-".to_string());

        out.push_str(&format!(
            "{:<24} {:<8} {:<10} {:<10} {:<6} {:<6} {:<12}\n",
            agent_display,
            exec.agent_type.as_str(),
            exec.status.as_str(),
            task_display,
            exec.iterations,
            exec.tool_call_count,
            duration,
        ));
    }

    out
}

/// Format a list of executions as JSON.
pub fn format_execution_list_json(executions: &[AgentExecution]) -> Result<String, MonitorError> {
    serde_json::to_string_pretty(executions).map_err(|e| MonitorError::Serialization(e.to_string()))
}

/// Format execution summary as JSON.
pub fn format_execution_summary_json(summary: &ExecutionSummary) -> Result<String, MonitorError> {
    serde_json::to_string_pretty(summary).map_err(|e| MonitorError::Serialization(e.to_string()))
}

fn format_duration_ms(ms: u64) -> String {
    if ms >= 60_000 {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{mins}m{secs}s")
    } else if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        format!("{count}")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_status_roundtrip() {
        for status in [
            ExecutionStatus::Running,
            ExecutionStatus::Success,
            ExecutionStatus::Failed,
            ExecutionStatus::Cancelled,
            ExecutionStatus::Handoff,
        ] {
            let s = status.as_str();
            let parsed: ExecutionStatus = s.parse().expect("should parse");
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn execution_status_terminal() {
        assert!(!ExecutionStatus::Running.is_terminal());
        assert!(ExecutionStatus::Success.is_terminal());
        assert!(ExecutionStatus::Failed.is_terminal());
        assert!(ExecutionStatus::Cancelled.is_terminal());
        assert!(ExecutionStatus::Handoff.is_terminal());
    }

    #[test]
    fn agent_type_roundtrip() {
        for at in [AgentType::React, AgentType::Swarm, AgentType::Worker] {
            let s = at.as_str();
            let parsed: AgentType = s.parse().expect("should parse");
            assert_eq!(parsed, at);
        }
    }

    #[test]
    fn record_start_and_get() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");
        let exec_id = monitor
            .record_start(
                "agent-1",
                AgentType::React,
                Some("task-1"),
                None,
                Some("claude-opus"),
            )
            .expect("record_start");

        let exec = monitor
            .get_execution(&exec_id)
            .expect("get_execution")
            .expect("should exist");

        assert_eq!(exec.agent_id, "agent-1");
        assert_eq!(exec.agent_type, AgentType::React);
        assert_eq!(exec.task_id.as_deref(), Some("task-1"));
        assert_eq!(exec.status, ExecutionStatus::Running);
        assert_eq!(exec.iterations, 0);
        assert_eq!(exec.model.as_deref(), Some("claude-opus"));
        assert!(exec.finished_at.is_none());
    }

    #[test]
    fn record_start_and_finish() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");
        let exec_id = monitor
            .record_start("agent-1", AgentType::React, None, None, None)
            .expect("record_start");

        monitor
            .record_finish(&FinishRecord {
                execution_id: &exec_id,
                status: ExecutionStatus::Success,
                iterations: 5,
                tool_call_count: 3,
                tokens_in: Some(1000),
                tokens_out: Some(500),
                error_message: None,
                tool_calls_json: None,
            })
            .expect("record_finish");

        let exec = monitor
            .get_execution(&exec_id)
            .expect("get_execution")
            .expect("should exist");

        assert_eq!(exec.status, ExecutionStatus::Success);
        assert_eq!(exec.iterations, 5);
        assert_eq!(exec.tool_call_count, 3);
        assert_eq!(exec.tokens_in, Some(1000));
        assert_eq!(exec.tokens_out, Some(500));
        assert!(exec.finished_at.is_some());
    }

    #[test]
    fn record_finish_with_error() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");
        let exec_id = monitor
            .record_start("agent-1", AgentType::React, None, None, None)
            .expect("record_start");

        monitor
            .record_finish(&FinishRecord {
                execution_id: &exec_id,
                status: ExecutionStatus::Failed,
                iterations: 2,
                tool_call_count: 1,
                tokens_in: Some(500),
                tokens_out: Some(100),
                error_message: Some("max iterations exceeded"),
                tool_calls_json: None,
            })
            .expect("record_finish");

        let exec = monitor
            .get_execution(&exec_id)
            .expect("get_execution")
            .expect("should exist");

        assert_eq!(exec.status, ExecutionStatus::Failed);
        assert_eq!(
            exec.error_message.as_deref(),
            Some("max iterations exceeded")
        );
    }

    #[test]
    fn record_finish_nonexistent_returns_not_found() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");
        let result = monitor.record_finish(&FinishRecord {
            execution_id: "nonexistent",
            status: ExecutionStatus::Success,
            iterations: 0,
            tool_call_count: 0,
            tokens_in: None,
            tokens_out: None,
            error_message: None,
            tool_calls_json: None,
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, MonitorError::NotFound(_)));
    }

    #[test]
    fn active_executions_returns_only_running() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");

        let exec1 = monitor
            .record_start("agent-1", AgentType::React, None, None, None)
            .expect("start");
        let _exec2 = monitor
            .record_start("agent-2", AgentType::Worker, None, None, None)
            .expect("start");

        // Finish one
        monitor
            .record_finish(&FinishRecord {
                execution_id: &exec1,
                status: ExecutionStatus::Success,
                iterations: 3,
                tool_call_count: 2,
                tokens_in: None,
                tokens_out: None,
                error_message: None,
                tool_calls_json: None,
            })
            .expect("finish");

        let active = monitor.active_executions().expect("active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].agent_id, "agent-2");
    }

    #[test]
    fn query_with_filters() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");

        let exec1 = monitor
            .record_start("agent-1", AgentType::React, Some("task-a"), None, None)
            .expect("start");
        let _exec2 = monitor
            .record_start("agent-2", AgentType::Worker, Some("task-b"), None, None)
            .expect("start");

        monitor
            .record_finish(&FinishRecord {
                execution_id: &exec1,
                status: ExecutionStatus::Success,
                iterations: 3,
                tool_call_count: 2,
                tokens_in: None,
                tokens_out: None,
                error_message: None,
                tool_calls_json: None,
            })
            .expect("finish");

        let since = Utc::now() - Duration::hours(1);

        // Filter by agent_id
        let filter = ExecutionFilter {
            agent_id: Some("agent-1".to_string()),
            ..Default::default()
        };
        let results = monitor.query_executions(&filter, since).expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_id, "agent-1");

        // Filter by agent_type
        let filter = ExecutionFilter {
            agent_type: Some(AgentType::Worker),
            ..Default::default()
        };
        let results = monitor.query_executions(&filter, since).expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_id, "agent-2");

        // Filter by status
        let filter = ExecutionFilter {
            status: Some(ExecutionStatus::Success),
            ..Default::default()
        };
        let results = monitor.query_executions(&filter, since).expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_id, "agent-1");

        // Filter by task_id
        let filter = ExecutionFilter {
            task_id: Some("task-b".to_string()),
            ..Default::default()
        };
        let results = monitor.query_executions(&filter, since).expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_id, "agent-2");
    }

    #[test]
    fn query_with_limit() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");

        for i in 0..5 {
            monitor
                .record_start(&format!("agent-{i}"), AgentType::React, None, None, None)
                .expect("start");
        }

        let since = Utc::now() - Duration::hours(1);
        let filter = ExecutionFilter {
            limit: Some(3),
            ..Default::default()
        };
        let results = monitor.query_executions(&filter, since).expect("query");
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn summary_computation() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");

        // 3 success, 1 failed, 1 running
        for i in 0..5 {
            let exec_id = monitor
                .record_start(&format!("agent-{i}"), AgentType::React, None, None, None)
                .expect("start");

            if i < 3 {
                monitor
                    .record_finish(&FinishRecord {
                        execution_id: &exec_id,
                        status: ExecutionStatus::Success,
                        iterations: 5,
                        tool_call_count: 3,
                        tokens_in: Some(1000),
                        tokens_out: Some(500),
                        error_message: None,
                        tool_calls_json: None,
                    })
                    .expect("finish");
            } else if i == 3 {
                monitor
                    .record_finish(&FinishRecord {
                        execution_id: &exec_id,
                        status: ExecutionStatus::Failed,
                        iterations: 2,
                        tool_call_count: 1,
                        tokens_in: Some(300),
                        tokens_out: Some(100),
                        error_message: Some("error"),
                        tool_calls_json: None,
                    })
                    .expect("finish");
            }
            // i == 4 stays running
        }

        let since = Utc::now() - Duration::hours(1);
        let summary = monitor.summary(since).expect("summary");

        assert_eq!(summary.total, 5);
        assert_eq!(summary.active, 1);
        assert_eq!(summary.succeeded, 3);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.cancelled, 0);
        assert_eq!(summary.handoffs, 0);
        // 3 success out of 4 terminal = 75%
        assert!((summary.success_rate - 0.75).abs() < 0.01);
        assert_eq!(summary.total_tokens_in, 3300);
        assert_eq!(summary.total_tokens_out, 1600);
    }

    #[test]
    fn parent_child_relationship() {
        let monitor = SqliteAgentMonitor::open_in_memory().expect("open");

        let parent_id = monitor
            .record_start("swarm-1", AgentType::Swarm, Some("big-task"), None, None)
            .expect("start");

        let child_id = monitor
            .record_start(
                "worker-1",
                AgentType::Worker,
                Some("chunk-1"),
                Some(&parent_id),
                None,
            )
            .expect("start");

        let child = monitor
            .get_execution(&child_id)
            .expect("get")
            .expect("exists");
        assert_eq!(child.parent_id.as_deref(), Some(parent_id.as_str()));
    }

    #[test]
    fn format_summary_output() {
        let summary = ExecutionSummary {
            total: 10,
            active: 2,
            succeeded: 6,
            failed: 1,
            cancelled: 0,
            handoffs: 1,
            avg_duration_ms: 5500.0,
            total_tokens_in: 15000,
            total_tokens_out: 7500,
            success_rate: 0.75,
        };

        let output = format_execution_summary(&summary, "24h");
        assert!(output.contains("Agent Execution Summary (24h)"));
        assert!(output.contains("Total executions:  10"));
        assert!(output.contains("Active (running):  2"));
        assert!(output.contains("Succeeded:         6"));
        assert!(output.contains("Failed:            1"));
        assert!(output.contains("Success rate:      75.0%"));
        assert!(output.contains("Avg duration:      5.5s"));
        assert!(output.contains("Tokens (in/out):   15.0k / 7.5k"));
    }

    #[test]
    fn format_list_empty() {
        let output = format_execution_list(&[]);
        assert!(output.contains("No agent executions found"));
    }

    #[test]
    fn format_list_with_data() {
        let executions = vec![AgentExecution {
            execution_id: "exec_1".to_string(),
            agent_id: "agent-search".to_string(),
            agent_type: AgentType::React,
            task_id: Some("t42".to_string()),
            parent_id: None,
            started_at: Utc::now() - Duration::seconds(30),
            finished_at: Some(Utc::now()),
            status: ExecutionStatus::Success,
            iterations: 5,
            tool_call_count: 3,
            tokens_in: Some(1000),
            tokens_out: Some(500),
            error_message: None,
            model: Some("claude-opus".to_string()),
            tool_calls_json: None,
        }];

        let output = format_execution_list(&executions);
        assert!(output.contains("agent-search"));
        assert!(output.contains("React"));
        assert!(output.contains("Success"));
        assert!(output.contains("t42"));
    }

    #[test]
    fn format_duration_formatting() {
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(1500), "1.5s");
        assert_eq!(format_duration_ms(65000), "1m5s");
        assert_eq!(format_duration_ms(125000), "2m5s");
    }

    #[test]
    fn format_tokens_formatting() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn json_serialization() {
        let summary = ExecutionSummary {
            total: 5,
            active: 1,
            succeeded: 3,
            failed: 1,
            cancelled: 0,
            handoffs: 0,
            avg_duration_ms: 3000.0,
            total_tokens_in: 5000,
            total_tokens_out: 2000,
            success_rate: 0.75,
        };

        let json = format_execution_summary_json(&summary).expect("json");
        assert!(json.contains("\"total\": 5"));
        assert!(json.contains("\"success_rate\": 0.75"));
    }

    #[test]
    fn execution_json_roundtrip() {
        let exec = AgentExecution {
            execution_id: "exec_1".to_string(),
            agent_id: "agent-1".to_string(),
            agent_type: AgentType::React,
            task_id: Some("task-1".to_string()),
            parent_id: None,
            started_at: Utc::now(),
            finished_at: None,
            status: ExecutionStatus::Running,
            iterations: 0,
            tool_call_count: 0,
            tokens_in: None,
            tokens_out: None,
            error_message: None,
            model: None,
            tool_calls_json: None,
        };

        let json = serde_json::to_string(&exec).expect("serialize");
        let parsed: AgentExecution = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.execution_id, exec.execution_id);
        assert_eq!(parsed.agent_type, AgentType::React);
        assert_eq!(parsed.status, ExecutionStatus::Running);
    }
}
