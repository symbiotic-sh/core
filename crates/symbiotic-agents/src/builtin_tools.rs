//! Built-in tools for agent execution: RecallTool, ArchiveTool, QueueTool.
//!
//! Each tool checks capability tokens via the agent framework before executing.
//! The actual backends are injected as trait objects to avoid tight coupling.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use symbiotic_core::types::question_group::QuestionGroup;

use crate::tools::{Tool, ToolResult};

// ---------------------------------------------------------------------------
// Backend traits (implemented by the actual crates at integration time)
// ---------------------------------------------------------------------------

/// Backend for querying context (maps to Recall Gateway).
#[async_trait::async_trait]
pub trait RecallBackend: Send + Sync {
    async fn query(&self, query: &str, max_items: usize) -> Result<Vec<RecallItem>>;
}

/// A single item returned from context recall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallItem {
    pub id: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
}

/// Backend for writing entries to the Archive.
#[async_trait::async_trait]
pub trait ArchiveBackend: Send + Sync {
    async fn store(&self, title: String, content: String, tags: Vec<String>) -> Result<String>;
}

/// Backend for submitting jobs to the Queue.
#[async_trait::async_trait]
pub trait QueueBackend: Send + Sync {
    async fn enqueue(
        &self,
        job_type: String,
        payload: String,
        idempotency_key: String,
    ) -> Result<String>;
}

/// Backend for dispatching sub-agents from within an orchestrator's ReAct loop.
///
/// Implemented by the daemon so that a running agent can synchronously spawn
/// another agent (different role, new goal) and get its final answer back as a
/// tool observation. Enables true agent-to-agent orchestration without wrapper
/// scripts — the orchestrator's loop becomes the coordination layer.
#[async_trait::async_trait]
pub trait DispatchAgentBackend: Send + Sync {
    async fn dispatch(&self, role: String, goal: String) -> Result<DispatchedAgentResult>;
}

/// Result returned from a sub-agent dispatched via [`DispatchAgentTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchedAgentResult {
    pub output: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Capability checker (injected from the framework)
// ---------------------------------------------------------------------------

/// Checks whether an agent has a specific capability scope.
pub trait CapabilityChecker: Send + Sync {
    fn check(&self, agent_id: &str, scope: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// RecallTool
// ---------------------------------------------------------------------------

/// Queries the Recall Gateway for context relevant to a query.
pub struct RecallTool {
    agent_id: String,
    backend: Arc<dyn RecallBackend>,
    caps: Arc<dyn CapabilityChecker>,
}

impl RecallTool {
    pub fn new(
        agent_id: String,
        backend: Arc<dyn RecallBackend>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            backend,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Query the Archive for context relevant to a search query"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "max_items": {"type": "integer", "description": "Maximum items to return (default 5)"}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "archive.read")?;

        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: query"))?;
        let max_items = params
            .get("max_items")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        let items = self.backend.query(query, max_items).await?;
        let output = serde_json::to_string_pretty(&items)?;
        Ok(ToolResult {
            success: true,
            output,
        })
    }
}

// ---------------------------------------------------------------------------
// ArchiveTool
// ---------------------------------------------------------------------------

/// Writes an entry to the Archive.
pub struct ArchiveTool {
    agent_id: String,
    backend: Arc<dyn ArchiveBackend>,
    caps: Arc<dyn CapabilityChecker>,
}

impl ArchiveTool {
    pub fn new(
        agent_id: String,
        backend: Arc<dyn ArchiveBackend>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            backend,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for ArchiveTool {
    fn name(&self) -> &str {
        "archive"
    }

    fn description(&self) -> &str {
        "Write an entry to the Archive"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Entry title"},
                "content": {"type": "string", "description": "Entry content (markdown)"},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Tags for categorization"}
            },
            "required": ["title", "content"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "archive.write")?;

        let title = params
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: title"))?
            .to_string();
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: content"))?
            .to_string();
        let tags: Vec<String> = params
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let record_id = self.backend.store(title, content, tags).await?;
        Ok(ToolResult {
            success: true,
            output: format!("Stored archive entry: {record_id}"),
        })
    }
}

// ---------------------------------------------------------------------------
// QueueTool
// ---------------------------------------------------------------------------

/// Submits a job to the work queue.
pub struct QueueTool {
    agent_id: String,
    backend: Arc<dyn QueueBackend>,
    caps: Arc<dyn CapabilityChecker>,
}

impl QueueTool {
    pub fn new(
        agent_id: String,
        backend: Arc<dyn QueueBackend>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            backend,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for QueueTool {
    fn name(&self) -> &str {
        "queue"
    }

    fn description(&self) -> &str {
        "Submit a job to the work queue for background processing"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_type": {"type": "string", "description": "Type of job (e.g. 'ingest.fetch', 'archive.review.enqueue')"},
                "payload": {"type": "string", "description": "Job payload (typically a URL or JSON)"},
                "idempotency_key": {"type": "string", "description": "Key to prevent duplicate submissions"}
            },
            "required": ["job_type", "payload", "idempotency_key"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "queue.submit")?;

        let job_type = params
            .get("job_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: job_type"))?
            .to_string();
        let payload = params
            .get("payload")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: payload"))?
            .to_string();
        let idempotency_key = params
            .get("idempotency_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: idempotency_key"))?
            .to_string();

        let job_id = self
            .backend
            .enqueue(job_type, payload, idempotency_key)
            .await?;
        Ok(ToolResult {
            success: true,
            output: format!("Queued job: {job_id}"),
        })
    }
}

// ---------------------------------------------------------------------------
// DispatchAgentTool
// ---------------------------------------------------------------------------

/// Spawn another agent with a different role + new goal from within this agent's
/// ReAct loop. The call is synchronous from the caller's perspective: the
/// sub-agent runs end-to-end and returns its final output as the tool result,
/// which the caller can then reason about.
pub struct DispatchAgentTool {
    agent_id: String,
    backend: Arc<dyn DispatchAgentBackend>,
    caps: Arc<dyn CapabilityChecker>,
}

impl DispatchAgentTool {
    pub fn new(
        agent_id: String,
        backend: Arc<dyn DispatchAgentBackend>,
        caps: Arc<dyn CapabilityChecker>,
    ) -> Self {
        Self {
            agent_id,
            backend,
            caps,
        }
    }
}

#[async_trait::async_trait]
impl Tool for DispatchAgentTool {
    fn name(&self) -> &str {
        "dispatch_agent"
    }

    fn description(&self) -> &str {
        "Spawn a sub-agent with a different role and a specific goal. Blocks until \
         the sub-agent finishes and returns its final output. Use this to delegate \
         focused tasks (research, analysis, critique) to specialist roles \
         instead of trying to do everything yourself."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "role": {
                    "type": "string",
                    "description": "Registered agent role name (e.g. 'researcher', 'security-auditor')."
                },
                "goal": {
                    "type": "string",
                    "description": "Specific, actionable goal for the sub-agent. Include context and desired output format."
                }
            },
            "required": ["role", "goal"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        self.caps.check(&self.agent_id, "agent.dispatch")?;

        let role = params
            .get("role")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: role"))?
            .to_string();
        let goal = params
            .get("goal")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: goal"))?
            .to_string();

        let result = self.backend.dispatch(role.clone(), goal).await?;
        let success = result.status == "completed";
        Ok(ToolResult {
            success,
            output: format!(
                "[sub-agent role={role} status={status}]\n{output}",
                status = result.status,
                output = result.output,
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// AskUserTool
// ---------------------------------------------------------------------------

/// A pending question from an agent to the user, with optional quick-reply suggestions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingQuestion {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick_replies: Option<Vec<String>>,
}

/// Tool that allows an agent to ask the user a clarifying question.
///
/// When called, stores the question in shared state and instructs the agent
/// to wrap up. The workflow runner will emit the question as a `goal.question`
/// event and pause execution until the user responds.
pub struct AskUserTool {
    pending_question: Arc<Mutex<Option<PendingQuestion>>>,
}

impl AskUserTool {
    /// Create a new AskUserTool and return the shared pending-question handle.
    pub fn new() -> (Self, Arc<Mutex<Option<PendingQuestion>>>) {
        let pending = Arc::new(Mutex::new(None));
        (
            AskUserTool {
                pending_question: pending.clone(),
            },
            pending,
        )
    }
}

#[async_trait::async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a clarifying question. Execution will pause until they respond. Use when you need information to proceed."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user"
                },
                "quick_replies": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of suggested quick-reply options for the user"
                }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let question = params
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("Could you provide more details?");
        let quick_replies: Option<Vec<String>> = params
            .get("quick_replies")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        *self
            .pending_question
            .lock()
            .map_err(|e| anyhow!("lock poisoned: {e}"))? = Some(PendingQuestion {
            text: question.to_string(),
            quick_replies,
        });
        Ok(ToolResult {
            success: true,
            output: format!(
                "Your question has been sent to the user: \"{question}\"\n\
                 Execution will pause until they respond. \
                 Summarize your progress so far and finish with \
                 {{\"done\": true, \"result\": \"<your progress summary>\"}}."
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// AskUserGroupTool (T130 §03 — batched Inquisitor emission)
// ---------------------------------------------------------------------------

/// Tool that allows an agent to emit a full `QuestionGroup` in one call.
///
/// This is the batched counterpart to [`AskUserTool`]. Instead of surfacing a
/// single clarifying question, the Inquisitor emits all questions for the
/// current phase as one group sharing an [`UnblockKey`][`symbiotic_core::types::question_group::UnblockKey`].
/// The group is stored in a shared pending-handle using the same pattern as
/// `AskUserTool` + `GeneratePlanTool`; the daemon consumes it after the agent
/// loop wraps up and emits a `goal.question_group` event.
///
/// Availability is gated by the `SYMBIOTIC_BATCH_INQUISITOR` feature flag at
/// the daemon level — this tool is only registered into the Inquisitor's
/// tool-set when the flag is on.
pub struct AskUserGroupTool {
    pending_group: Arc<Mutex<Option<QuestionGroup>>>,
}

impl AskUserGroupTool {
    /// Create a new `AskUserGroupTool` and return the shared pending-group handle.
    pub fn new() -> (Self, Arc<Mutex<Option<QuestionGroup>>>) {
        let pending = Arc::new(Mutex::new(None));
        (
            AskUserGroupTool {
                pending_group: pending.clone(),
            },
            pending,
        )
    }
}

#[async_trait::async_trait]
impl Tool for AskUserGroupTool {
    fn name(&self) -> &str {
        "ask_user_group"
    }

    fn description(&self) -> &str {
        "Emit a batch of clarifying questions as a single QuestionGroup. Use when multiple \
         independent questions share an unblock key (e.g., all design-phase questions for one \
         sub-goal). Each question MUST carry recommendation + confidence + severity. Execution \
         will pause until the group resolves."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        // Schema mirrors the `QuestionGroup` + `AnnotatedQuestion` shape in
        // `symbiotic_core::types::question_group`. We document the wire shape
        // here rather than deriving from the Rust type because JSON schema
        // emission for tagged enums would be verbose and noisy for the LLM.
        serde_json::json!({
            "type": "object",
            "properties": {
                "group_id": {
                    "type": "string",
                    "description": "Stable ID within the parent goal (e.g. 'design-phase')."
                },
                "parent_goal_id": {
                    "type": "string",
                    "description": "Goal ID owning this group. Optional — injected by the daemon when omitted."
                },
                "unblock_key": {
                    "type": "object",
                    "description": "Routing label. `type` picks a variant: \
                        Exploratory { topic }, AttachedRepo { repo_id, branch_hint, requires_approval }, \
                        ResearchOnly { question }, Composite { children }.",
                    "properties": {
                        "type": {"type": "string", "enum": ["Exploratory", "AttachedRepo", "ResearchOnly", "Composite"]}
                    },
                    "required": ["type"]
                },
                "resolution_mode": {
                    "type": "object",
                    "description": "How many questions must resolve before the group fires. \
                        { mode: 'AllRequired' } | { mode: 'AnyOne' } | { mode: 'MajoritySignal', n: N }.",
                    "properties": {
                        "mode": {"type": "string", "enum": ["AllRequired", "AnyOne", "MajoritySignal"]}
                    },
                    "required": ["mode"]
                },
                "questions": {
                    "type": "array",
                    "description": "Annotated questions. Each MUST include recommendation, confidence, severity.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                            "quick_replies": {"type": "array", "items": {"type": "string"}},
                            "recommendation": {"type": "string"},
                            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                            "severity": {"type": "string", "enum": ["Trivial", "Informational", "Decision", "Critical"]},
                            "expected_answer_type": {"type": "string", "enum": ["FreeText", "SingleChoice", "Boolean", "Scalar"]}
                        },
                        "required": ["text", "confidence", "severity", "expected_answer_type"]
                    }
                },
                "created_at": {
                    "type": "string",
                    "description": "ISO 8601 UTC timestamp. Optional — daemon stamps one when omitted."
                }
            },
            "required": ["group_id", "unblock_key", "resolution_mode", "questions"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        // Validate + deserialize into the canonical `QuestionGroup`. We accept
        // an LLM-friendly JSON shape with some fields defaulted so the model
        // does not have to re-derive e.g. `created_at` or `parent_goal_id`
        // every emission.
        let mut params_obj = params
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow!("ask_user_group expects a JSON object"))?;

        // Default created_at if missing — keep types crate deterministic by
        // stamping it here rather than in symbiotic-core.
        if !params_obj.contains_key("created_at") {
            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            params_obj.insert("created_at".to_string(), serde_json::Value::String(now));
        }

        // Default parent_goal_id to an empty string; the daemon layer is
        // responsible for injecting the real goal ID before emitting the event.
        if !params_obj.contains_key("parent_goal_id") {
            params_obj.insert(
                "parent_goal_id".to_string(),
                serde_json::Value::String(String::new()),
            );
        }

        let group: QuestionGroup = serde_json::from_value(serde_json::Value::Object(params_obj))
            .map_err(|e| anyhow!("invalid question group payload: {e}"))?;

        if group.questions.is_empty() {
            return Err(anyhow!(
                "ask_user_group requires at least one question in the group"
            ));
        }

        let group_id = group.group_id.clone();
        let count = group.questions.len();

        *self
            .pending_group
            .lock()
            .map_err(|e| anyhow!("lock poisoned: {e}"))? = Some(group);

        Ok(ToolResult {
            success: true,
            output: format!(
                "Question group \"{group_id}\" submitted with {count} question(s). \
                 Execution will pause until the group resolves. \
                 Summarize your progress so far and finish with \
                 {{\"done\": true, \"result\": \"<your progress summary>\"}}."
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// GeneratePlanTool
// ---------------------------------------------------------------------------

/// A single step in a proposed execution plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskKind {
    Execution,
    Review,
    Distillation,
    Coordination,
    Waiting,
    Approval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskDriver {
    Agent,
    Declared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskEscalationPolicy {
    NotifyOperator,
    RaiseAlert,
    AutoReplan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskEscalationSeverity {
    Normal,
    High,
    Urgent,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskLatenessBasis {
    WallClock,
    DeliveryWindowElapsed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskDeliveryWindowMode {
    Anytime,
    OutsideQuietHours,
    WorkingHours,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskWeekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanDeclaredContext {
    #[serde(default)]
    pub review_target: Option<String>,
    #[serde(default)]
    pub waiting_for: Option<String>,
    #[serde(default)]
    pub coordination_target: Option<String>,
    #[serde(default)]
    pub external_dependency: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanTaskPolicy {
    #[serde(default)]
    pub escalation: Option<PlanTaskEscalationConfig>,
    #[serde(default)]
    pub timing: Option<PlanTaskTimingConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanTaskTimingConfig {
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub lateness_basis: Option<PlanTaskLatenessBasis>,
    #[serde(default)]
    pub delivery_window: Option<PlanTaskDeliveryWindowConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTaskDeliveryWindowConfig {
    pub mode: PlanTaskDeliveryWindowMode,
    #[serde(default)]
    pub quiet_hours: Option<PlanTaskQuietHoursWindow>,
    #[serde(default)]
    pub working_hours: Option<PlanTaskWorkingHoursWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTaskQuietHoursWindow {
    pub start_local: String,
    pub end_local: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTaskWorkingHoursWindow {
    #[serde(default)]
    pub weekdays: Vec<PlanTaskWeekday>,
    pub start_local: String,
    pub end_local: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTaskEscalationConfig {
    pub mode: PlanTaskEscalationPolicy,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub severity: Option<PlanTaskEscalationSeverity>,
    #[serde(default)]
    pub on_enter_blocked: bool,
    #[serde(default)]
    pub after_secs: Option<u64>,
    #[serde(default)]
    pub max_count: Option<u32>,
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
}

impl Default for PlanTaskEscalationConfig {
    fn default() -> Self {
        Self {
            mode: PlanTaskEscalationPolicy::NotifyOperator,
            audience: None,
            severity: None,
            on_enter_blocked: true,
            after_secs: None,
            max_count: None,
            cooldown_secs: None,
        }
    }
}

impl PlanTaskDriver {
    pub fn spawns_workflow_step(self) -> bool {
        matches!(self, Self::Agent)
    }
}

impl PlanTaskKind {
    pub fn default_driver(self) -> PlanTaskDriver {
        match self {
            Self::Execution | Self::Distillation => PlanTaskDriver::Agent,
            Self::Review | Self::Coordination | Self::Waiting | Self::Approval => {
                PlanTaskDriver::Declared
            }
        }
    }

    pub fn allows_driver(self, driver: PlanTaskDriver) -> bool {
        match self {
            Self::Execution | Self::Distillation => matches!(driver, PlanTaskDriver::Agent),
            Self::Waiting | Self::Approval => matches!(driver, PlanTaskDriver::Declared),
            Self::Review | Self::Coordination => true,
        }
    }
}

fn validate_declared_context(
    task_kind: PlanTaskKind,
    task_driver: PlanTaskDriver,
    declared_context: &PlanDeclaredContext,
) -> Result<()> {
    if !matches!(task_driver, PlanTaskDriver::Declared) {
        return Ok(());
    }

    match task_kind {
        PlanTaskKind::Review | PlanTaskKind::Approval => {
            if declared_context.review_target.is_none() {
                return Err(anyhow!(
                    "declared {task_kind:?} task requires declared_context.review_target"
                ));
            }
        }
        PlanTaskKind::Waiting => {
            if declared_context.waiting_for.is_none()
                && declared_context.external_dependency.is_none()
            {
                return Err(anyhow!(
                    "declared waiting task requires declared_context.waiting_for or declared_context.external_dependency"
                ));
            }
        }
        PlanTaskKind::Coordination => {
            if declared_context.coordination_target.is_none()
                && declared_context.external_dependency.is_none()
            {
                return Err(anyhow!(
                    "declared coordination task requires declared_context.coordination_target or declared_context.external_dependency"
                ));
            }
        }
        PlanTaskKind::Execution | PlanTaskKind::Distillation => {}
    }

    Ok(())
}

fn validate_task_policy(task_driver: PlanTaskDriver, policy: &PlanTaskPolicy) -> Result<()> {
    if let Some(escalation) = policy.escalation.as_ref() {
        if !matches!(task_driver, PlanTaskDriver::Declared) {
            return Err(anyhow!(
                "policy.escalation is only valid for task_driver=declared"
            ));
        }
        if !escalation.on_enter_blocked
            && escalation.after_secs.is_none()
            && escalation.max_count.is_some()
        {
            return Err(anyhow!(
                "policy.escalation.max_count without a trigger is invalid"
            ));
        }
        if let Some(audience) = escalation.audience.as_deref() {
            if audience.trim().is_empty() {
                return Err(anyhow!("policy.escalation.audience must not be blank"));
            }
        }
    }
    if let Some(timing) = policy.timing.as_ref() {
        if let Some(timezone) = timing.timezone.as_deref() {
            if timezone.trim().is_empty() {
                return Err(anyhow!("policy.timing.timezone must not be blank"));
            }
        }
    }
    Ok(())
}

/// A single step in a proposed execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub task_id: String,
    pub task_slug: String,
    pub task_kind: PlanTaskKind,
    pub task_driver: PlanTaskDriver,
    pub owner_hint: Option<String>,
    pub declared_context: PlanDeclaredContext,
    pub policy: PlanTaskPolicy,
    pub role: Option<String>,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub derived_from: Vec<String>,
    #[serde(default)]
    pub replaces: Vec<String>,
}

/// A proposed execution plan generated by the inquisition agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedPlan {
    pub steps: Vec<PlanStep>,
    pub confidence: f64,
    pub summary: String,
}

/// Tool that allows the inquisition agent to finalize a plan for user approval.
///
/// When called, stores the proposed plan in shared state. The workflow runner
/// will emit it as a `goal.plan.proposed` event for the user to approve or reject.
pub struct GeneratePlanTool {
    pending_plan: Arc<Mutex<Option<ProposedPlan>>>,
}

impl GeneratePlanTool {
    /// Create a new GeneratePlanTool and return the shared pending-plan handle.
    pub fn new() -> (Self, Arc<Mutex<Option<ProposedPlan>>>) {
        let pending = Arc::new(Mutex::new(None));
        (
            GeneratePlanTool {
                pending_plan: pending.clone(),
            },
            pending,
        )
    }
}

#[async_trait::async_trait]
impl Tool for GeneratePlanTool {
    fn name(&self) -> &str {
        "generate_plan"
    }

    fn description(&self) -> &str {
        "Finalize the execution plan and propose it for user approval. Only call this after the user confirms they have nothing else to add."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "A concise summary of what the plan will accomplish"
                },
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task_id": {"type": "string", "description": "Stable task identity preserved across replans (e.g. search-flights)"},
                            "task_slug": {"type": "string", "description": "Human-facing mutable slug/path label for the task doc"},
                            "task_kind": {"type": "string", "enum": ["execution", "review", "distillation", "coordination", "waiting", "approval"], "description": "Semantic task category"},
                            "task_driver": {"type": "string", "enum": ["agent", "declared"], "description": "Optional runtime projection override. Defaults come from task_kind: execution/distillation -> agent, review/coordination/waiting/approval -> declared."},
                            "owner_hint": {"type": "string", "description": "Ownership target for the task, such as @operator:test or role:reviewer. Prefer this for declared tasks."},
                            "declared_context": {
                                "type": "object",
                                "description": "Structured context for declared or human-facing tasks",
                                "properties": {
                                    "review_target": {"type": "string", "description": "What should be reviewed or approved"},
                                    "waiting_for": {"type": "string", "description": "Condition or signal this task is waiting on"},
                                    "coordination_target": {"type": "string", "description": "Team, system, or surface this coordination task is aimed at"},
                                    "external_dependency": {"type": "string", "description": "External system or dependency blocking/owning progress"}
                                }
                            },
                            "policy": {
                                "type": "object",
                                "description": "Runtime policy for Archive-native task handling",
                                "properties": {
                                    "escalation": {
                                        "type": "object",
                                        "properties": {
                                            "mode": {"type": "string", "enum": ["notify_operator", "raise_alert", "auto_replan"], "description": "How blocked declared work escalates"},
                                            "audience": {"type": "string", "description": "Who should receive the escalation, for example operator, team:infra, or oncall:infra"},
                                            "severity": {"type": "string", "enum": ["normal", "high", "urgent", "critical"], "description": "How urgently the escalation should be treated"},
                                            "on_enter_blocked": {"type": "boolean", "description": "Whether to escalate immediately when the task explicitly enters blocked"},
                                            "after_secs": {"type": "integer", "description": "Optional SLA before escalating stalled work"},
                                            "max_count": {"type": "integer", "description": "Optional maximum number of escalations"},
                                            "cooldown_secs": {"type": "integer", "description": "Optional cooldown between repeated escalations"}
                                        },
                                        "required": ["mode"]
                                    },
                                    "timing": {
                                        "type": "object",
                                        "description": "Timezone-aware delivery and lateness policy for declared work",
                                        "properties": {
                                            "timezone": {"type": "string", "description": "IANA timezone for local delivery semantics, for example Europe/Bratislava"},
                                            "lateness_basis": {"type": "string", "enum": ["wall_clock", "delivery_window_elapsed"], "description": "Whether after_secs counts raw elapsed time or only time inside the delivery window"},
                                            "delivery_window": {
                                                "type": "object",
                                                "properties": {
                                                    "mode": {"type": "string", "enum": ["anytime", "outside_quiet_hours", "working_hours", "custom"]},
                                                    "quiet_hours": {
                                                        "type": "object",
                                                        "properties": {
                                                            "start_local": {"type": "string"},
                                                            "end_local": {"type": "string"}
                                                        }
                                                    },
                                                    "working_hours": {
                                                        "type": "object",
                                                        "properties": {
                                                            "weekdays": {"type": "array", "items": {"type": "string", "enum": ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]}},
                                                            "start_local": {"type": "string"},
                                                            "end_local": {"type": "string"}
                                                        }
                                                    }
                                                },
                                                "required": ["mode"]
                                            }
                                        }
                                    }
                                }
                            },
                            "role": {"type": "string", "description": "Agent role when task_driver=agent. Do not set this for declared tasks."},
                            "description": {"type": "string", "description": "What this step does"},
                            "timeout_s": {"type": "integer", "description": "Optional timeout in seconds"},
                            "depends_on": {"type": "array", "items": {"type": "string"}, "description": "Stable task IDs that must complete before this task can run"},
                            "derived_from": {"type": "array", "items": {"type": "string"}, "description": "Ancestor task IDs when this task was split or materially derived from earlier work"},
                            "replaces": {"type": "array", "items": {"type": "string"}, "description": "Prior active task IDs that this task supersedes in the new plan"}
                        },
                        "required": ["task_id", "task_slug", "task_kind", "description"]
                    },
                    "description": "Ordered list of execution steps"
                },
                "confidence": {
                    "type": "number",
                    "description": "Confidence in this plan (0.0 to 1.0)"
                }
            },
            "required": ["summary", "steps"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let summary = params
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing required parameter: summary"))?
            .to_string();

        let steps_value = params
            .get("steps")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("missing required parameter: steps"))?;

        let mut steps = Vec::new();
        for sv in steps_value {
            let task_id = sv
                .get("task_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let task_slug = sv
                .get("task_slug")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let owner_hint = sv
                .get("owner_hint")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let declared_context = sv
                .get("declared_context")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| anyhow!("invalid declared_context: {error}"))?
                .unwrap_or_default();
            let policy = sv
                .get("policy")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| anyhow!("invalid policy: {error}"))?
                .unwrap_or_default();
            let role = sv
                .get("role")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let task_kind: PlanTaskKind = serde_json::from_value(
                sv.get("task_kind")
                    .cloned()
                    .ok_or_else(|| anyhow!("plan step missing required parameter: task_kind"))?,
            )
            .map_err(|error| anyhow!("invalid task_kind: {error}"))?;
            let task_driver = match sv.get("task_driver").cloned() {
                Some(value) => serde_json::from_value(value)
                    .map_err(|error| anyhow!("invalid task_driver: {error}"))?,
                None => task_kind.default_driver(),
            };
            let description = sv
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let timeout_s = sv.get("timeout_s").and_then(|v| v.as_u64());
            let depends_on = sv
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let derived_from = sv
                .get("derived_from")
                .and_then(|v| v.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let replaces = sv
                .get("replaces")
                .and_then(|v| v.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if task_id.trim().is_empty() {
                return Err(anyhow!("plan step missing required parameter: task_id"));
            }
            if task_slug.trim().is_empty() {
                return Err(anyhow!("plan step missing required parameter: task_slug"));
            }
            if !task_kind.allows_driver(task_driver) {
                return Err(anyhow!(
                    "invalid task_driver={task_driver:?} for task_kind={task_kind:?}"
                ));
            }
            if matches!(task_driver, PlanTaskDriver::Agent) && role.is_none() {
                return Err(anyhow!(
                    "plan step missing required parameter: role for task_driver=agent"
                ));
            }
            if matches!(task_driver, PlanTaskDriver::Declared) && role.is_some() {
                return Err(anyhow!(
                    "plan step must not set role for task_driver=declared; use owner_hint instead"
                ));
            }
            validate_declared_context(task_kind, task_driver, &declared_context)?;
            validate_task_policy(task_driver, &policy)?;
            steps.push(PlanStep {
                task_id,
                task_slug,
                task_kind,
                task_driver,
                owner_hint,
                declared_context,
                policy,
                role,
                description,
                timeout_s,
                depends_on,
                derived_from,
                replaces,
            });
        }

        if steps.is_empty() {
            return Err(anyhow!("plan must have at least one step"));
        }

        let confidence = params
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.8)
            .clamp(0.0, 1.0);

        let plan = ProposedPlan {
            steps,
            confidence,
            summary: summary.clone(),
        };

        *self
            .pending_plan
            .lock()
            .map_err(|e| anyhow!("lock poisoned: {e}"))? = Some(plan);

        Ok(ToolResult {
            success: true,
            output: format!(
                "Plan generated: \"{summary}\"\n\
                 The plan has been submitted for user review. \
                 Wrap up with {{\"done\": true, \"result\": \"Plan proposed for approval.\"}}."
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Mock backends --

    struct MockRecall;

    #[async_trait::async_trait]
    impl RecallBackend for MockRecall {
        async fn query(&self, query: &str, max_items: usize) -> Result<Vec<RecallItem>> {
            Ok(vec![RecallItem {
                id: "r1".to_string(),
                title: format!("Result for: {query}"),
                snippet: "Some context...".to_string(),
                score: 0.9,
            }]
            .into_iter()
            .take(max_items)
            .collect())
        }
    }

    struct MockArchive;

    #[async_trait::async_trait]
    impl ArchiveBackend for MockArchive {
        async fn store(
            &self,
            title: String,
            _content: String,
            _tags: Vec<String>,
        ) -> Result<String> {
            Ok(format!("arc_{}", title.len()))
        }
    }

    struct MockQueue;

    #[async_trait::async_trait]
    impl QueueBackend for MockQueue {
        async fn enqueue(
            &self,
            _job_type: String,
            _payload: String,
            idempotency_key: String,
        ) -> Result<String> {
            Ok(format!("job_{idempotency_key}"))
        }
    }

    // -- Mock capability checkers --

    struct AllowAll;
    impl CapabilityChecker for AllowAll {
        fn check(&self, _agent_id: &str, _scope: &str) -> Result<()> {
            Ok(())
        }
    }

    struct DenyAll;
    impl CapabilityChecker for DenyAll {
        fn check(&self, _agent_id: &str, scope: &str) -> Result<()> {
            Err(anyhow!("capability denied: {scope}"))
        }
    }

    #[tokio::test]
    async fn recall_tool_returns_items() {
        let tool = RecallTool::new(
            "agent-1".to_string(),
            Arc::new(MockRecall),
            Arc::new(AllowAll),
        );
        let result = tool
            .execute(serde_json::json!({"query": "rust agents"}))
            .await
            .expect("execute");
        assert!(result.success);
        assert!(result.output.contains("Result for: rust agents"));
    }

    #[tokio::test]
    async fn recall_tool_denied_without_capability() {
        let tool = RecallTool::new(
            "agent-1".to_string(),
            Arc::new(MockRecall),
            Arc::new(DenyAll),
        );
        let err = tool
            .execute(serde_json::json!({"query": "test"}))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("capability denied"));
    }

    #[tokio::test]
    async fn archive_tool_stores_entry() {
        let tool = ArchiveTool::new(
            "agent-1".to_string(),
            Arc::new(MockArchive),
            Arc::new(AllowAll),
        );
        let result = tool
            .execute(serde_json::json!({
                "title": "Test Entry",
                "content": "Some content",
                "tags": ["test"]
            }))
            .await
            .expect("execute");
        assert!(result.success);
        assert!(result.output.contains("arc_"));
    }

    #[tokio::test]
    async fn archive_tool_denied_without_capability() {
        let tool = ArchiveTool::new(
            "agent-1".to_string(),
            Arc::new(MockArchive),
            Arc::new(DenyAll),
        );
        let err = tool
            .execute(serde_json::json!({"title": "t", "content": "c"}))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("capability denied"));
    }

    #[tokio::test]
    async fn queue_tool_submits_job() {
        let tool = QueueTool::new(
            "agent-1".to_string(),
            Arc::new(MockQueue),
            Arc::new(AllowAll),
        );
        let result = tool
            .execute(serde_json::json!({
                "job_type": "ingest.fetch",
                "payload": "url=https://example.com",
                "idempotency_key": "key-1"
            }))
            .await
            .expect("execute");
        assert!(result.success);
        assert!(result.output.contains("job_key-1"));
    }

    #[tokio::test]
    async fn queue_tool_denied_without_capability() {
        let tool = QueueTool::new(
            "agent-1".to_string(),
            Arc::new(MockQueue),
            Arc::new(DenyAll),
        );
        let err = tool
            .execute(serde_json::json!({
                "job_type": "x",
                "payload": "y",
                "idempotency_key": "z"
            }))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("capability denied"));
    }

    #[tokio::test]
    async fn recall_tool_missing_query_returns_error() {
        let tool = RecallTool::new(
            "agent-1".to_string(),
            Arc::new(MockRecall),
            Arc::new(AllowAll),
        );
        let err = tool
            .execute(serde_json::json!({}))
            .await
            .expect_err("should fail");
        assert!(err
            .to_string()
            .contains("missing required parameter: query"));
    }

    #[tokio::test]
    async fn ask_user_stores_question_text() {
        let (tool, pending) = AskUserTool::new();
        let result = tool
            .execute(serde_json::json!({"question": "What is your budget?"}))
            .await
            .expect("execute");
        assert!(result.success);
        let pq = pending
            .lock()
            .unwrap()
            .clone()
            .expect("should have pending question");
        assert_eq!(pq.text, "What is your budget?");
        assert!(pq.quick_replies.is_none());
    }

    #[tokio::test]
    async fn ask_user_stores_quick_replies() {
        let (tool, pending) = AskUserTool::new();
        let result = tool
            .execute(serde_json::json!({
                "question": "Experience level?",
                "quick_replies": ["Beginner", "Intermediate", "Advanced"]
            }))
            .await
            .expect("execute");
        assert!(result.success);
        let pq = pending
            .lock()
            .unwrap()
            .clone()
            .expect("should have pending question");
        assert_eq!(pq.text, "Experience level?");
        let replies = pq.quick_replies.expect("should have quick_replies");
        assert_eq!(replies, vec!["Beginner", "Intermediate", "Advanced"]);
    }

    #[tokio::test]
    async fn ask_user_default_question_text() {
        let (tool, pending) = AskUserTool::new();
        let result = tool.execute(serde_json::json!({})).await.expect("execute");
        assert!(result.success);
        let pq = pending
            .lock()
            .unwrap()
            .clone()
            .expect("should have pending question");
        assert_eq!(pq.text, "Could you provide more details?");
    }

    #[tokio::test]
    async fn generate_plan_stores_plan() {
        let (tool, pending) = GeneratePlanTool::new();
        let result = tool
            .execute(serde_json::json!({
                "summary": "Research and implement feature X",
                "steps": [
                    {"task_id": "research-existing-approaches", "task_slug": "research-existing-approaches", "task_kind": "execution", "task_driver": "agent", "role": "researcher", "description": "Research existing approaches"},
                    {"task_id": "implement-solution", "task_slug": "implement-solution", "task_kind": "execution", "task_driver": "agent", "role": "coder", "description": "Implement the solution", "timeout_s": 600, "depends_on": ["research-existing-approaches"]}
                ],
                "confidence": 0.85
            }))
            .await
            .expect("execute");
        assert!(result.success);
        let plan = pending
            .lock()
            .unwrap()
            .clone()
            .expect("should have pending plan");
        assert_eq!(plan.summary, "Research and implement feature X");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].task_id, "research-existing-approaches");
        assert_eq!(plan.steps[1].task_slug, "implement-solution");
        assert!(plan.steps[0].task_driver.spawns_workflow_step());
        assert_eq!(plan.steps[0].role.as_deref(), Some("researcher"));
        assert_eq!(
            plan.steps[1].depends_on,
            vec!["research-existing-approaches"]
        );
        assert_eq!(plan.steps[1].timeout_s, Some(600));
        assert!((plan.confidence - 0.85).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn generate_plan_allows_declared_review_with_owner_hint() {
        let (tool, pending) = GeneratePlanTool::new();
        let result = tool
            .execute(serde_json::json!({
                "summary": "Wait for operator review",
                "steps": [{
                    "task_id": "review-plan",
                    "task_slug": "review-plan",
                    "task_kind": "review",
                    "task_driver": "declared",
                    "owner_hint": "@operator:test",
                    "declared_context": {
                        "review_target": "Proposed Tokyo budget plan"
                    },
                    "policy": {
                        "escalation": {
                            "mode": "notify_operator",
                            "audience": "oncall:infra",
                            "severity": "urgent",
                            "on_enter_blocked": true
                        },
                        "timing": {
                            "timezone": "Europe/Bratislava",
                            "lateness_basis": "delivery_window_elapsed",
                            "delivery_window": {
                                "mode": "working_hours",
                                "working_hours": {
                                    "weekdays": ["mon", "tue", "wed", "thu", "fri"],
                                    "start_local": "09:00",
                                    "end_local": "18:00"
                                }
                            }
                        }
                    },
                    "description": "Wait for human review of the plan"
                }]
            }))
            .await
            .expect("execute");
        assert!(result.success);
        let plan = pending.lock().unwrap().clone().unwrap();
        assert_eq!(plan.steps[0].owner_hint.as_deref(), Some("@operator:test"));
        assert_eq!(plan.steps[0].role, None);
        assert_eq!(
            plan.steps[0].declared_context.review_target.as_deref(),
            Some("Proposed Tokyo budget plan")
        );
        assert_eq!(
            plan.steps[0]
                .policy
                .escalation
                .as_ref()
                .map(|value| value.mode),
            Some(PlanTaskEscalationPolicy::NotifyOperator)
        );
        assert_eq!(
            plan.steps[0]
                .policy
                .escalation
                .as_ref()
                .and_then(|value| value.audience.as_deref()),
            Some("oncall:infra")
        );
        assert_eq!(
            plan.steps[0]
                .policy
                .escalation
                .as_ref()
                .and_then(|value| value.severity),
            Some(PlanTaskEscalationSeverity::Urgent)
        );
        assert_eq!(
            plan.steps[0]
                .policy
                .timing
                .as_ref()
                .and_then(|value| value.timezone.as_deref()),
            Some("Europe/Bratislava")
        );
        assert_eq!(
            plan.steps[0]
                .policy
                .timing
                .as_ref()
                .and_then(|value| value.lateness_basis),
            Some(PlanTaskLatenessBasis::DeliveryWindowElapsed)
        );
    }

    #[tokio::test]
    async fn generate_plan_rejects_invalid_kind_driver_pair() {
        let (tool, _pending) = GeneratePlanTool::new();
        let err = tool
            .execute(serde_json::json!({
                "summary": "Bad plan",
                "steps": [{
                    "task_id": "wait-for-budget",
                    "task_slug": "wait-for-budget",
                    "task_kind": "waiting",
                    "task_driver": "agent",
                    "role": "researcher",
                    "description": "Wait for the budget signal"
                }]
            }))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("invalid task_driver"));
    }

    #[tokio::test]
    async fn generate_plan_rejects_declared_role_usage() {
        let (tool, _pending) = GeneratePlanTool::new();
        let err = tool
            .execute(serde_json::json!({
                "summary": "Bad declared plan",
                "steps": [{
                    "task_id": "approve-budget",
                    "task_slug": "approve-budget",
                    "task_kind": "approval",
                    "task_driver": "declared",
                    "role": "reviewer",
                    "description": "Wait for approval"
                }]
            }))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("must not set role"));
    }

    #[tokio::test]
    async fn generate_plan_rejects_declared_review_without_review_target() {
        let (tool, _pending) = GeneratePlanTool::new();
        let err = tool
            .execute(serde_json::json!({
                "summary": "Bad review plan",
                "steps": [{
                    "task_id": "review-plan",
                    "task_slug": "review-plan",
                    "task_kind": "review",
                    "task_driver": "declared",
                    "owner_hint": "@operator:test",
                    "description": "Wait for human review"
                }]
            }))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("review_target"));
    }

    #[tokio::test]
    async fn generate_plan_rejects_declared_waiting_without_condition() {
        let (tool, _pending) = GeneratePlanTool::new();
        let err = tool
            .execute(serde_json::json!({
                "summary": "Bad waiting plan",
                "steps": [{
                    "task_id": "await-budget",
                    "task_slug": "await-budget",
                    "task_kind": "waiting",
                    "task_driver": "declared",
                    "owner_hint": "@operator:test",
                    "description": "Wait for operator budget signal"
                }]
            }))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("waiting_for"));
    }

    #[tokio::test]
    async fn generate_plan_rejects_empty_steps() {
        let (tool, _pending) = GeneratePlanTool::new();
        let err = tool
            .execute(serde_json::json!({
                "summary": "Empty plan",
                "steps": []
            }))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("at least one step"));
    }

    #[tokio::test]
    async fn generate_plan_defaults_confidence() {
        let (tool, pending) = GeneratePlanTool::new();
        let result = tool
            .execute(serde_json::json!({
                "summary": "Simple plan",
                "steps": [{"task_id": "s1", "task_slug": "s1", "task_kind": "execution", "task_driver": "agent", "role": "coder", "description": "Do the thing"}]
            }))
            .await
            .expect("execute");
        assert!(result.success);
        let plan = pending.lock().unwrap().clone().unwrap();
        assert!((plan.confidence - 0.8).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn generate_plan_defaults_driver_from_task_kind() {
        let (tool, pending) = GeneratePlanTool::new();
        let result = tool
            .execute(serde_json::json!({
                "summary": "Default driver plan",
                "steps": [
                    {
                        "task_id": "implement",
                        "task_slug": "implement",
                        "task_kind": "execution",
                        "role": "coder",
                        "description": "Implement the thing"
                    },
                    {
                        "task_id": "review-plan",
                        "task_slug": "review-plan",
                        "task_kind": "review",
                        "owner_hint": "@operator:test",
                        "declared_context": {
                            "review_target": "Implementation plan"
                        },
                        "description": "Wait for plan review"
                    }
                ]
            }))
            .await
            .expect("execute");
        assert!(result.success);
        let plan = pending.lock().unwrap().clone().unwrap();
        assert_eq!(plan.steps[0].task_driver, PlanTaskDriver::Agent);
        assert_eq!(plan.steps[1].task_driver, PlanTaskDriver::Declared);
        assert_eq!(plan.steps[1].owner_hint.as_deref(), Some("@operator:test"));
        assert_eq!(
            plan.steps[1].declared_context.review_target.as_deref(),
            Some("Implementation plan")
        );
    }

    #[tokio::test]
    async fn generate_plan_clamps_confidence() {
        let (tool, pending) = GeneratePlanTool::new();
        let _ = tool
            .execute(serde_json::json!({
                "summary": "Over-confident plan",
                "steps": [{"task_id": "s1", "task_slug": "s1", "task_kind": "execution", "task_driver": "agent", "role": "coder", "description": "Do it"}],
                "confidence": 1.5
            }))
            .await
            .expect("execute");
        let plan = pending.lock().unwrap().clone().unwrap();
        assert!((plan.confidence - 1.0).abs() < f64::EPSILON);
    }

    // -- AskUserGroupTool tests (T130 §03) --

    fn minimal_ask_user_group_payload() -> serde_json::Value {
        serde_json::json!({
            "group_id": "design-phase",
            "parent_goal_id": "goal-build-frontend",
            "unblock_key": {"type": "Exploratory", "topic": "frontend-framework-choice"},
            "resolution_mode": {"mode": "AllRequired"},
            "questions": [
                {
                    "text": "Framework preference: Vue, React, or Svelte?",
                    "quick_replies": ["Vue", "React", "Svelte"],
                    "recommendation": "Vue",
                    "confidence": 0.72,
                    "severity": "Decision",
                    "expected_answer_type": "SingleChoice"
                },
                {
                    "text": "SSR required for initial load?",
                    "quick_replies": ["Yes", "No"],
                    "recommendation": "Yes",
                    "confidence": 0.85,
                    "severity": "Decision",
                    "expected_answer_type": "Boolean"
                }
            ],
            "created_at": "2026-04-18T10:42:00Z"
        })
    }

    #[tokio::test]
    async fn ask_user_group_new_returns_tool_and_handle() {
        let (_tool, pending) = AskUserGroupTool::new();
        assert!(pending.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn ask_user_group_stores_valid_group() {
        let (tool, pending) = AskUserGroupTool::new();
        let result = tool
            .execute(minimal_ask_user_group_payload())
            .await
            .expect("execute");
        assert!(result.success);
        assert!(result.output.contains("design-phase"));
        assert!(result.output.contains("2 question(s)"));

        let group = pending
            .lock()
            .unwrap()
            .clone()
            .expect("pending group must be set");
        assert_eq!(group.group_id, "design-phase");
        assert_eq!(group.parent_goal_id, "goal-build-frontend");
        assert_eq!(group.questions.len(), 2);
        assert_eq!(group.questions[0].recommendation.as_deref(), Some("Vue"));
        assert!((group.questions[0].confidence - 0.72).abs() < 1e-6);
    }

    #[tokio::test]
    async fn ask_user_group_invalid_json_returns_error_and_leaves_handle_empty() {
        let (tool, pending) = AskUserGroupTool::new();
        // Missing required `unblock_key` + `questions`
        let err = tool
            .execute(serde_json::json!({
                "group_id": "bad",
                "resolution_mode": {"mode": "AllRequired"}
            }))
            .await
            .expect_err("should fail");
        assert!(
            err.to_string().contains("invalid question group payload"),
            "got: {err}"
        );
        assert!(
            pending.lock().unwrap().is_none(),
            "handle must stay empty after error"
        );
    }

    #[tokio::test]
    async fn ask_user_group_empty_questions_rejected() {
        let (tool, pending) = AskUserGroupTool::new();
        let err = tool
            .execute(serde_json::json!({
                "group_id": "empty",
                "parent_goal_id": "goal-x",
                "unblock_key": {"type": "ResearchOnly", "question": "?"},
                "resolution_mode": {"mode": "AllRequired"},
                "questions": []
            }))
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("at least one question"));
        assert!(pending.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn ask_user_group_defaults_created_at_when_missing() {
        let (tool, pending) = AskUserGroupTool::new();
        let mut payload = minimal_ask_user_group_payload();
        payload.as_object_mut().unwrap().remove("created_at");
        let result = tool.execute(payload).await.expect("execute");
        assert!(result.success);
        let group = pending.lock().unwrap().clone().unwrap();
        assert!(
            !group.created_at.is_empty(),
            "created_at must be auto-stamped"
        );
    }

    #[tokio::test]
    async fn ask_user_group_roundtrip_matches_core_type() {
        // This test proves the tool's accepted JSON payload deserializes into
        // the canonical `symbiotic_core::types::question_group::QuestionGroup`.
        // When symbiotic-core changes the shape, this test fails — catching
        // cross-boundary drift at build time per CONTEXT.md Strict Typing Rule.
        use symbiotic_core::types::question_group::{
            AnswerType, QuestionGroup, QuestionSeverity, ResolutionMode, UnblockKey,
        };

        let (tool, pending) = AskUserGroupTool::new();
        let _ = tool
            .execute(minimal_ask_user_group_payload())
            .await
            .expect("execute");
        let group: QuestionGroup = pending.lock().unwrap().clone().unwrap();

        match &group.unblock_key {
            UnblockKey::Exploratory { topic } => {
                assert_eq!(topic, "frontend-framework-choice");
            }
            other => panic!("unexpected unblock_key variant: {other:?}"),
        }
        assert!(matches!(group.resolution_mode, ResolutionMode::AllRequired));
        assert!(matches!(
            group.questions[0].severity,
            QuestionSeverity::Decision
        ));
        assert!(matches!(
            group.questions[1].expected_answer_type,
            AnswerType::Boolean
        ));

        // Round-trip through JSON and make sure the canonical type
        // serializes back identically.
        let json = serde_json::to_string(&group).expect("serialize");
        let parsed: QuestionGroup = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, group);
    }

    #[tokio::test]
    async fn ask_user_group_tool_name_is_ask_user_group() {
        let (tool, _) = AskUserGroupTool::new();
        assert_eq!(tool.name(), "ask_user_group");
    }
}
