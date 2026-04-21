//! Goal start/stop/list/status handlers and workflow execution dispatch.
//!
//! Contains workflow queue helpers, goal state queries, workflow job
//! execution, and the workflow payload encode/decode logic.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use symbiotic_agents::pipeline::types::{AutonomyLevel, GoalMetadata, GoalSource, GoalSubmission};
use symbiotic_agents::pipeline::PipelineOutcome;
use symbiotic_control_plane::WorkItemStatus;
use symbiotic_queue::{now_unix, FailOutcome, JobStatus, QueueJob};
use symbiotic_workflows::{StepStatus, WorkflowRunResult, WorkflowStatus};

use crate::events::{DaemonEvent, EventType};
use crate::goal_state::*;
use crate::routing::{room_alias, RoomRole};
use crate::{escape_field, unescape_field, EnqueueRequest, SymbioticDaemon};

// --- Payload types ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowRunPayload {
    pub template: String,
    pub goal_room: Option<String>,
    pub goal_sender: Option<String>,
    pub project_id: Option<String>,
    pub user_answer: Option<String>,
    pub goal_id: Option<String>,
    /// Original user goal text, preserved across inquisition rounds so the
    /// agent always knows what the user asked for (distinct from
    /// `user_answer` which carries the latest Q&A response).
    pub user_goal: Option<String>,
    /// Archive-derived replanning context for blocked-task re-entry into the
    /// planner loop.
    pub replan_context: Option<String>,
}

pub(crate) const DEFAULT_UNSCOPED_PROJECT_ID: &str = "project:inbox";

pub(crate) fn attached_thread_id_for_room(room_id: &str) -> Option<String> {
    let alias = room_alias(room_id);
    if alias.eq_ignore_ascii_case("#stream") || alias.eq_ignore_ascii_case("stream") {
        return Some("_stream".to_string());
    }
    alias
        .strip_prefix('#')
        .unwrap_or(alias)
        .strip_prefix("thread-")
        .map(|suffix| format!("thread-{suffix}"))
}

pub(crate) fn default_unscoped_project_id() -> String {
    DEFAULT_UNSCOPED_PROJECT_ID.to_string()
}

fn replan_context_detail(task: &crate::goal_management::PlannedTaskRecord) -> String {
    let mut parts = Vec::new();
    if let Some(waiting_for) = task.declared_context.waiting_for.as_deref() {
        parts.push(format!("waiting_for={waiting_for}"));
    }
    if let Some(review_target) = task.declared_context.review_target.as_deref() {
        parts.push(format!("review_target={review_target}"));
    }
    if let Some(coordination_target) = task.declared_context.coordination_target.as_deref() {
        parts.push(format!("coordination_target={coordination_target}"));
    }
    if let Some(external_dependency) = task.declared_context.external_dependency.as_deref() {
        parts.push(format!("external_dependency={external_dependency}"));
    }
    if parts.is_empty() {
        "no additional declared context".to_string()
    } else {
        parts.join(", ")
    }
}

// --- Payload codecs ---

pub(crate) fn encode_workflow_payload(payload: &WorkflowRunPayload) -> String {
    let base = format!(
        "{}|{}|{}|{}",
        escape_field(&payload.template),
        payload
            .goal_room
            .as_deref()
            .map(escape_field)
            .unwrap_or_default(),
        payload
            .goal_sender
            .as_deref()
            .map(escape_field)
            .unwrap_or_default(),
        payload
            .project_id
            .as_deref()
            .map(escape_field)
            .unwrap_or_default()
    );
    // Only append the optional 5th+ fields when present.
    if payload.user_answer.is_some()
        || payload.goal_id.is_some()
        || payload.user_goal.is_some()
        || payload.replan_context.is_some()
    {
        format!(
            "{}|{}|{}|{}|{}",
            base,
            payload
                .user_answer
                .as_deref()
                .map(escape_field)
                .unwrap_or_default(),
            payload
                .goal_id
                .as_deref()
                .map(escape_field)
                .unwrap_or_default(),
            payload
                .user_goal
                .as_deref()
                .map(escape_field)
                .unwrap_or_default(),
            payload
                .replan_context
                .as_deref()
                .map(escape_field)
                .unwrap_or_default()
        )
    } else {
        base
    }
}

pub(crate) fn decode_workflow_payload(encoded: &str) -> Result<WorkflowRunPayload> {
    // Split into at most 8 parts for the extended format.
    let parts = encoded.splitn(8, '|').collect::<Vec<_>>();
    if parts.len() == 1 {
        let template = unescape_field(parts[0]);
        if template.trim().is_empty() {
            return Err(anyhow!("workflow payload template cannot be empty"));
        }
        return Ok(WorkflowRunPayload {
            template,
            goal_room: None,
            goal_sender: None,
            project_id: None,
            user_answer: None,
            goal_id: None,
            user_goal: None,
            replan_context: None,
        });
    }
    if parts.len() != 4 && parts.len() != 6 && parts.len() != 7 && parts.len() != 8 {
        return Err(anyhow!(
            "workflow payload must have 1, 4, 6, 7, or 8 parts (got {})",
            parts.len()
        ));
    }
    let template = unescape_field(parts[0]);
    if template.trim().is_empty() {
        return Err(anyhow!("workflow payload template cannot be empty"));
    }
    let goal_room = {
        let value = unescape_field(parts[1]);
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    };
    let goal_sender = {
        let value = unescape_field(parts[2]);
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    };
    let project_id = {
        let value = unescape_field(parts[3]);
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    };
    let user_answer = if parts.len() >= 5 {
        let value = unescape_field(parts[4]);
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    } else {
        None
    };
    let goal_id = if parts.len() >= 6 {
        let value = unescape_field(parts[5]);
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    } else {
        None
    };
    let user_goal = if parts.len() >= 7 {
        let value = unescape_field(parts[6]);
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    } else {
        None
    };
    let replan_context = if parts.len() >= 8 {
        let value = unescape_field(parts[7]);
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    } else {
        None
    };
    Ok(WorkflowRunPayload {
        template,
        goal_room,
        goal_sender,
        project_id,
        user_answer,
        goal_id,
        user_goal,
        replan_context,
    })
}

// --- Goal text persistence (original NL goal survives across inquisition rounds) ---

/// Persist the original goal text so subsequent inquisition rounds can retrieve it.
pub(crate) fn persist_goal_text(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
    text: &str,
) {
    let dir = data_dir.join("goal_texts");
    let _ = std::fs::create_dir_all(&dir);
    let key = format!("{}_{}", crate::room_alias(room), template);
    let _ = std::fs::write(dir.join(key), text);
    // Clear any prior Q&A history when a new goal starts.
    let qa_key = format!("{}_{}_qa", crate::room_alias(room), template);
    let _ = std::fs::remove_file(dir.join(qa_key));
}

/// Read the persisted original goal text for a given room+template.
pub(crate) fn read_goal_text(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
) -> Option<String> {
    let dir = data_dir.join("goal_texts");
    let key = format!("{}_{}", crate::room_alias(room), template);
    std::fs::read_to_string(dir.join(key)).ok()
}

/// Append a question-answer pair to the goal's Q&A history.
pub(crate) fn append_goal_qa(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
    question: &str,
    answer: &str,
) {
    let dir = data_dir.join("goal_texts");
    let _ = std::fs::create_dir_all(&dir);
    let qa_key = format!("{}_{}_qa", crate::room_alias(room), template);
    let mut content = std::fs::read_to_string(dir.join(&qa_key)).unwrap_or_default();
    let round = content.matches("Q: ").count() + 1;
    content.push_str(&format!("Q{round}: {question}\nA{round}: {answer}\n\n"));
    let _ = std::fs::write(dir.join(qa_key), content);
}

/// Persist a proposed plan JSON so the approval handler can read it back.
pub(crate) fn persist_pending_plan(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
    plan_json: &str,
) {
    let dir = data_dir.join("goal_texts");
    let _ = std::fs::create_dir_all(&dir);
    let key = format!("{}_{}_plan", crate::room_alias(room), template);
    let _ = std::fs::write(dir.join(key), plan_json);
}

/// Read a previously stored plan JSON for the approval handler.
pub(crate) fn read_pending_plan_json(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
) -> Option<String> {
    let dir = data_dir.join("goal_texts");
    let key = format!("{}_{}_plan", crate::room_alias(room), template);
    std::fs::read_to_string(dir.join(key)).ok()
}

/// Persist the latest pending question so the answer handler can pair it.
pub(crate) fn persist_pending_question(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
    question: &str,
) {
    let dir = data_dir.join("goal_texts");
    let _ = std::fs::create_dir_all(&dir);
    let key = format!("{}_{}_pending_q", crate::room_alias(room), template);
    let _ = std::fs::write(dir.join(key), question);
}

/// Read and consume the latest pending question.
pub(crate) fn take_pending_question(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
) -> Option<String> {
    let dir = data_dir.join("goal_texts");
    let key = format!("{}_{}_pending_q", crate::room_alias(room), template);
    let path = dir.join(key);
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

pub(crate) fn persist_auth_result(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
    request_id: &str,
    target: &str,
    handle_id: &str,
) {
    let dir = data_dir.join("goal_texts");
    let _ = std::fs::create_dir_all(&dir);
    let key = format!("{}_{}_auth", crate::room_alias(room), template);
    let payload = serde_json::json!({
        "request_id": request_id,
        "target": target,
        "handle_id": handle_id,
        "status": "completed",
    });
    let _ = std::fs::write(dir.join(key), payload.to_string());
}

pub(crate) fn read_auth_result(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
) -> Option<serde_json::Value> {
    let dir = data_dir.join("goal_texts");
    let key = format!("{}_{}_auth", crate::room_alias(room), template);
    let content = std::fs::read_to_string(dir.join(key)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Read the persisted Q&A history for a given room+template.
pub(crate) fn read_goal_qa(
    data_dir: &std::path::Path,
    room: &str,
    template: &str,
) -> Option<String> {
    let dir = data_dir.join("goal_texts");
    let qa_key = format!("{}_{}_qa", crate::room_alias(room), template);
    let content = std::fs::read_to_string(dir.join(qa_key)).ok()?;
    if content.trim().is_empty() {
        None
    } else {
        Some(content)
    }
}

pub(crate) fn questionnaire_context_from_qa(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let mut context = Vec::new();
    let mut pending_question: Option<String> = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(question) = trimmed
            .strip_prefix('Q')
            .and_then(|rest| rest.split_once(':').map(|(_, value)| value.trim()))
        {
            pending_question = Some(question.to_string());
        } else if let Some(answer) = trimmed
            .strip_prefix('A')
            .and_then(|rest| rest.split_once(':').map(|(_, value)| value.trim()))
        {
            match pending_question.take() {
                Some(question) => context.push(format!("{question} -> {answer}")),
                None => context.push(answer.to_string()),
            }
        }
    }
    context
}

/// Generate a unique goal identifier from the current timestamp.
pub(crate) fn generate_goal_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("goal-{:x}", ts)
}

// --- SymbioticDaemon impl ---

impl SymbioticDaemon {
    pub fn list_goal_states(&self) -> Result<Vec<GoalState>> {
        load_goal_states(&self.config.goal_state_file)
    }

    pub fn goal_state(&self, room_id: &str) -> Result<Option<GoalState>> {
        let room_alias_value = room_alias(room_id).to_ascii_lowercase();
        let states = self.list_goal_states()?;
        Ok(states
            .into_iter()
            .filter(|state| room_alias(&state.goal_room).eq_ignore_ascii_case(&room_alias_value))
            .max_by_key(|state| state.updated_at))
    }

    pub(crate) fn resolve_project_id_for_goal_materialization(
        &self,
        explicit_project_id: Option<&str>,
        thread_id: Option<&str>,
    ) -> Result<String> {
        if let Some(project_id) = explicit_project_id.filter(|value| !value.trim().is_empty()) {
            return Ok(project_id.to_string());
        }

        if let Some(thread_id) = thread_id.filter(|value| !value.trim().is_empty()) {
            if let Some(state) = self
                .list_goal_states()?
                .into_iter()
                .filter(|state| state.thread_id.as_deref() == Some(thread_id))
                .max_by_key(|state| state.updated_at)
            {
                return Ok(state.project_id);
            }
        }

        Ok(default_unscoped_project_id())
    }

    pub fn queue_workflow_run(&self, template_name: &str) -> Result<String> {
        let (job_id, _) = self.queue_workflow_run_with_payload(&WorkflowRunPayload {
            template: template_name.to_string(),
            goal_room: None,
            goal_sender: None,
            project_id: None,
            user_answer: None,
            goal_id: None,
            user_goal: None,
            replan_context: None,
        })?;
        Ok(job_id)
    }

    /// Returns `(job_id, goal_id)`.
    pub fn queue_workflow_run_for_goal(
        &self,
        template_name: &str,
        goal_room: &str,
        goal_sender: &str,
    ) -> Result<(String, String)> {
        let goal_id = generate_goal_id();
        let (job_id, _) = self.queue_workflow_run_with_payload(&WorkflowRunPayload {
            template: template_name.to_string(),
            goal_room: Some(goal_room.to_string()),
            goal_sender: Some(goal_sender.to_string()),
            project_id: None,
            user_answer: None,
            goal_id: Some(goal_id.clone()),
            user_goal: None,
            replan_context: None,
        })?;
        Ok((job_id, goal_id))
    }

    /// Returns `(job_id, goal_id)` where `goal_id` is extracted from the payload.
    pub(crate) fn queue_workflow_run_with_payload(
        &self,
        payload: &WorkflowRunPayload,
    ) -> Result<(String, Option<String>)> {
        // Skip active-job dedup when the payload carries a user_answer.
        // A user_answer means this is a re-queue after goal.answer or
        // goal.plan.approved — a new execution round that MUST create a
        // fresh job with the updated inputs.  The previous job should
        // already be Done (ack'd when the question/plan was emitted),
        // but even if a stale job is somehow still Queued/Running, we
        // must not silently swallow the re-queue.
        if payload.user_answer.is_none() && payload.replan_context.is_none() {
            if let Some(goal_room) = payload.goal_room.as_deref() {
                if let Some(existing_job_id) =
                    self.find_active_goal_workflow(goal_room, &payload.template)?
                {
                    return Ok((existing_job_id, payload.goal_id.clone()));
                }
            }
        }
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "workflow.run".to_string(),
            payload: encode_workflow_payload(payload),
            idempotency_key: format!("workflow:{}:{}", payload.template, now_unix()),
            max_attempts: 3,
            next_run_at: now_unix(),
            // force: workflow re-enqueue (after user answer/approval) must always
            // insert a new job, even if a previous run with the same-second key
            // already completed. find_active_goal_workflow() handles active dedup.
            force: true,
        })?;
        Ok((outcome.job_id, payload.goal_id.clone()))
    }

    pub(crate) fn enqueue_pending_archive_replans(&self, now: u64) -> Result<usize> {
        let archive_root = self
            .config
            .archive_path
            .clone()
            .unwrap_or_else(|| self.config.data_dir.join("../knowledge-base"));
        let requests = crate::goal_management::load_pending_goal_replan_requests(&archive_root)?;
        if requests.is_empty() {
            return Ok(0);
        }

        let goal_states = self.list_goal_states().unwrap_or_default();
        let mut enqueued = 0usize;

        for request in requests {
            let template = format!("inquisition:{}", request.goal_id);
            let conflicting_state = goal_states.iter().find(|state| {
                state.template == template
                    && !matches!(state.status.as_str(), "completed" | "cancelled" | "failed")
            });
            if conflicting_state.is_some() {
                continue;
            }

            let Some((project_id, goal_title, _goal_summary, goal_thread_id)) =
                crate::goal_management::load_goal_identity_from_archive(
                    &archive_root,
                    &request.goal_id,
                )?
            else {
                continue;
            };
            let Some(task) = crate::goal_management::load_goal_task_from_archive(
                &archive_root,
                &request.goal_id,
                &request.task_id,
            )?
            else {
                continue;
            };

            let latest_state = goal_states
                .iter()
                .filter(|state| {
                    state.last_run_id.as_deref() == Some(request.goal_id.as_str())
                        || state.template == template
                })
                .max_by_key(|state| state.updated_at);

            let persisted_goal_room = latest_state
                .map(|state| state.goal_room.clone())
                .unwrap_or_else(|| self.resolve_room(RoomRole::Goals).to_string());
            let goal_room = goal_thread_id
                .as_deref()
                .or(request.thread_id.as_deref())
                .and_then(|thread_id| self.resolve_thread_room(thread_id))
                .unwrap_or_else(|| persisted_goal_room.clone());
            let goal_owner = latest_state
                .and_then(|state| state.owner.clone())
                .unwrap_or_else(|| "@symbiotic:nucleus".to_string());
            let user_goal = read_goal_text(&self.config.data_dir, &persisted_goal_room, &template)
                .unwrap_or_else(|| goal_title.clone());
            let replan_context = format!(
                "Replan requested because task '{}' ({}) entered blocked. Detail: {}. Semantic context: {}",
                task.title,
                task.task_id,
                request
                    .detail
                    .clone()
                    .unwrap_or_else(|| "Blocked declared task".to_string()),
                replan_context_detail(&task)
            );

            let payload = WorkflowRunPayload {
                template: template.clone(),
                goal_room: Some(goal_room.clone()),
                goal_sender: Some(goal_owner.clone()),
                project_id: latest_state.map(|state| state.project_id.clone()),
                user_answer: None,
                goal_id: Some(request.goal_id.clone()),
                user_goal: Some(user_goal),
                replan_context: Some(replan_context.clone()),
            };
            let (job_id, _) = self.queue_workflow_run_with_payload(&payload)?;

            let context = crate::goal_management::GoalHierarchyContext {
                project_id: payload.project_id.as_deref().unwrap_or(project_id.as_str()),
                goal_id: &request.goal_id,
                title: &goal_title,
                summary: "Automatic replanning requested from Archive task policy.",
                owner: Some(&goal_owner),
                thread_id: goal_thread_id.as_deref().or(request.thread_id.as_deref()),
                observed_at: now as i64,
            };
            crate::goal_management::append_goal_replan_enqueued_archive(
                &archive_root,
                &context,
                &request.task_id,
                &replan_context,
                Some("nucleus.replan"),
            )?;
            append_goal_log(
                &self.config.goal_log_file,
                GoalLogEntry {
                    ts: now,
                    event: "goal.replan.enqueued",
                    workflow_job_id: &job_id,
                    goal_room: Some(&goal_room),
                    goal_sender: Some(&goal_owner),
                    template: &template,
                    detail: &replan_context,
                },
            )?;
            upsert_goal_state(
                &self.config.goal_state_file,
                GoalState {
                    goal_room,
                    thread_id: goal_thread_id.or(request.thread_id.clone()),
                    project_id: payload
                        .project_id
                        .clone()
                        .unwrap_or_else(default_unscoped_project_id),
                    template,
                    status: "running".to_string(),
                    last_job_id: job_id,
                    last_run_id: Some(request.goal_id.clone()),
                    owner: Some(goal_owner),
                    updated_at: now,
                    complexity: None,
                    pipeline_stage: Some("replanning".to_string()),
                    audit_id: None,
                    plan_id: None,
                },
            )?;
            enqueued += 1;
        }

        Ok(enqueued)
    }

    pub(crate) fn find_active_goal_workflow(
        &self,
        goal_room: &str,
        template: &str,
    ) -> Result<Option<String>> {
        for status in [JobStatus::Queued, JobStatus::Running] {
            let jobs = self.queue.list_by_status(status)?;
            for job in jobs {
                if job.type_name != "workflow.run" {
                    continue;
                }
                let Ok(payload) = decode_workflow_payload(&job.payload) else {
                    continue;
                };
                let Some(existing_goal_room) = payload.goal_room.as_deref() else {
                    continue;
                };
                if room_alias(existing_goal_room).eq_ignore_ascii_case(room_alias(goal_room))
                    && payload.template.eq_ignore_ascii_case(template)
                {
                    return Ok(Some(job.job_id));
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn goal_cancel_requested(&self, goal_room: &str, template: &str) -> Result<bool> {
        let room_key = room_alias(goal_room).to_ascii_lowercase();
        let states = self.list_goal_states()?;
        Ok(states.into_iter().any(|state| {
            room_alias(&state.goal_room).eq_ignore_ascii_case(&room_key)
                && state.template.eq_ignore_ascii_case(template)
                && state.status.eq_ignore_ascii_case("cancel_requested")
        }))
    }

    /// Resolve a workflow template by name, with dynamic fallback for
    /// `agent-execute:*` patterns that construct an inline agent execution
    /// workflow.
    fn resolve_workflow_template(
        &self,
        template_name: &str,
    ) -> Option<symbiotic_workflows::Workflow> {
        self.workflow_registry.get(template_name).or_else(|| {
            if template_name.starts_with("agent-execute:") {
                Some(build_agent_execute_workflow(template_name))
            } else if template_name.starts_with("inquisition:") {
                Some(build_inquisition_workflow(template_name))
            } else {
                None
            }
        })
    }

    /// Resolve a workflow, optionally generating multi-role steps from a
    /// ProposedPlan JSON string. Used when a plan has been approved and the
    /// `user_answer` field carries the plan JSON.
    fn resolve_workflow_template_with_plan(
        &self,
        template_name: &str,
        plan_json: Option<&str>,
    ) -> Option<symbiotic_workflows::Workflow> {
        // Try to parse a ProposedPlan from the plan JSON.
        if let Some(json) = plan_json {
            if let Ok(plan) =
                serde_json::from_str::<symbiotic_agents::builtin_tools::ProposedPlan>(json)
            {
                if !plan.steps.is_empty() {
                    return Some(build_plan_driven_workflow(template_name, &plan));
                }
            }
        }
        self.resolve_workflow_template(template_name)
    }

    pub fn run_workflow_template(&self, template_name: &str) -> Result<WorkflowRunResult> {
        let workflow = self
            .resolve_workflow_template(template_name)
            .ok_or_else(|| anyhow!("workflow template not found: {template_name}"))?;
        self.workflow_runner.run(&workflow)
    }

    /// Run a workflow template with step-level progress events.
    ///
    /// Returns the `WorkflowRunResult` plus a `Vec<DaemonEvent>` containing
    /// `goal.step.started` and `goal.step.completed`/`goal.step.failed` events
    /// for each step in the workflow.
    ///
    /// When `extra_inputs` is provided, the key-value pairs are merged into the
    /// workflow's input map before execution (overwriting any duplicate keys).
    /// This is used to inject the user's answer when resuming an awaiting goal.
    fn run_workflow_template_with_progress(
        &self,
        template_name: &str,
        goal_id: &str,
        goal_project_id: Option<&str>,
        goal_room: Option<&str>,
        goal_template: Option<&str>,
        goal_owner: Option<&str>,
        observed_at: i64,
        extra_inputs: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<(WorkflowRunResult, Vec<DaemonEvent>)> {
        // If extra_inputs contains a user_answer that looks like plan JSON,
        // generate a multi-role workflow from the plan steps.
        let plan_json = extra_inputs
            .and_then(|ei| ei.get("user_answer"))
            .filter(|v| v.contains("\"steps\""))
            .map(|v| v.as_str());
        let mut workflow = self
            .resolve_workflow_template_with_plan(template_name, plan_json)
            .ok_or_else(|| anyhow!("workflow template not found: {template_name}"))?;

        // Merge extra inputs (e.g. user_answer) into the workflow's input map.
        if let Some(inputs) = extra_inputs {
            for (key, value) in inputs {
                workflow.inputs.insert(key.clone(), value.clone());
            }
        }

        let goal_scope = crate::goal_management::stable_goal_scope_id(template_name, goal_id);
        let thread_id = goal_room.and_then(attached_thread_id_for_room);
        let goal_title = extra_inputs
            .and_then(|inputs| inputs.get("user_goal"))
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| template_name.to_string());
        let goal_summary = format!("Execution tracked for workflow `{template_name}`.");
        let execution_tasks = planned_execution_tasks_from_workflow(&workflow);
        let execution_task_map: std::collections::HashMap<
            String,
            crate::goal_management::PlannedTaskRecord,
        > = execution_tasks
            .into_iter()
            .map(|task| (task.task_slug.clone(), task))
            .collect();

        let goal_id_owned = goal_id.to_string();
        let goal_room_owned = goal_room.map(|s| s.to_string());
        let goal_template_owned = goal_template.map(|s| s.to_string());
        let archive_root = self
            .config
            .archive_path
            .clone()
            .unwrap_or_else(|| self.config.data_dir.join("../knowledge-base"));

        let mut step_events: Vec<DaemonEvent> = Vec::new();

        let result = self.workflow_runner.run_with_progress(
            &workflow,
            |step_index, step, total_steps, result| {
                match result {
                    None => {
                        if let Some(task) = execution_task_map.get(&step.id) {
                            let context = crate::goal_management::GoalHierarchyContext {
                                project_id: goal_project_id.unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                                goal_id: &goal_scope,
                                title: &goal_title,
                                summary: &goal_summary,
                                owner: goal_owner,
                                thread_id: thread_id.as_deref(),
                                observed_at,
                            };
                            crate::goal_management::sync_goal_task_execution_status(
                                &self.management_store,
                                Some(&archive_root),
                                &context,
                                task,
                                WorkItemStatus::Running,
                            );
                        }
                        // Step is about to start.
                        step_events.push(DaemonEvent {
                            event_type: EventType::GoalStepStarted,
                            status: "running".to_string(),
                            job_id: None,
                            detail: format!(
                                "step={} type={} index={} total={}",
                                step.id,
                                step.step_type,
                                step_index + 1,
                                total_steps
                            ),
                            goal_room: goal_room_owned.clone(),
                            goal_template: goal_template_owned.clone(),
                            goal_run_id: None,
                            goal_id: Some(goal_id_owned.clone()),
                            intake_run_id: None,
                            url: None,
                            title: None,
                            sensitivity: None,
                            quick_replies: None,
                            thread_id: None,
                        });
                    }
                    Some(step_result) => {
                        if let Some(task) = execution_task_map.get(&step.id) {
                            let context = crate::goal_management::GoalHierarchyContext {
                                project_id: goal_project_id.unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                                goal_id: &goal_scope,
                                title: &goal_title,
                                summary: &goal_summary,
                                owner: goal_owner,
                                thread_id: thread_id.as_deref(),
                                observed_at,
                            };
                            crate::goal_management::sync_goal_task_execution_status(
                                &self.management_store,
                                Some(&archive_root),
                                &context,
                                task,
                                if step_result.status == StepStatus::Success {
                                    WorkItemStatus::Done
                                } else {
                                    WorkItemStatus::Failed
                                },
                            );
                        }
                        // Step just finished.
                        let (evt_type, status) = if step_result.status == StepStatus::Success {
                            (EventType::GoalStepCompleted, "completed")
                        } else {
                            (EventType::GoalStepFailed, "failed")
                        };
                        let detail = if let Some(ref err) = step_result.error {
                            format!(
                                "step={} type={} index={} total={} error={}",
                                step.id,
                                step.step_type,
                                step_index + 1,
                                total_steps,
                                err
                            )
                        } else {
                            format!(
                                "step={} type={} index={} total={}",
                                step.id,
                                step.step_type,
                                step_index + 1,
                                total_steps
                            )
                        };
                        step_events.push(DaemonEvent {
                            event_type: evt_type,
                            status: status.to_string(),
                            job_id: None,
                            detail,
                            goal_room: goal_room_owned.clone(),
                            goal_template: goal_template_owned.clone(),
                            goal_run_id: None,
                            goal_id: Some(goal_id_owned.clone()),
                            intake_run_id: None,
                            url: None,
                            title: None,
                            sensitivity: None,
                            quick_replies: None,
                            thread_id: None,
                        });
                    }
                }
            },
        )?;

        Ok((result, step_events))
    }

    /// Execute a workflow job, returning the final event and any step-level
    /// progress events.  The caller is responsible for emitting all returned
    /// events (step events first, then the final event).
    pub(crate) fn execute_workflow_job(
        &self,
        job: QueueJob,
        now: u64,
    ) -> Result<(DaemonEvent, Vec<DaemonEvent>)> {
        let payload = decode_workflow_payload(&job.payload)
            .with_context(|| format!("invalid workflow payload for job {}", job.job_id))?;

        // Use the goal_id from the payload (set at enqueue time) so retries
        // keep the same ID. Only generate a new one for legacy jobs without it.
        let goal_id = payload.goal_id.clone().unwrap_or_else(generate_goal_id);
        let management_goal_scope =
            crate::goal_management::stable_goal_scope_id(&payload.template, &goal_id);
        let management_thread_id = payload
            .goal_room
            .as_deref()
            .and_then(attached_thread_id_for_room);
        let management_goal_title = payload
            .user_goal
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                payload.goal_room.as_deref().and_then(|goal_room| {
                    read_goal_text(&self.config.data_dir, goal_room, &payload.template)
                })
            })
            .unwrap_or_else(|| payload.template.clone());

        if let Some(goal_room) = payload.goal_room.as_deref() {
            if self.goal_cancel_requested(goal_room, &payload.template)? {
                self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
                append_goal_log(
                    &self.config.goal_log_file,
                    GoalLogEntry {
                        ts: now,
                        event: "goal.cancelled",
                        workflow_job_id: &job.job_id,
                        goal_room: payload.goal_room.as_deref(),
                        goal_sender: payload.goal_sender.as_deref(),
                        template: &payload.template,
                        detail: "cancelled_before_start",
                    },
                )?;
                upsert_goal_state(
                    &self.config.goal_state_file,
                    GoalState {
                        goal_room: goal_room.to_string(),
                        thread_id: attached_thread_id_for_room(goal_room),
                        project_id: payload
                            .project_id
                            .clone()
                            .unwrap_or_else(default_unscoped_project_id),
                        template: payload.template.clone(),
                        status: "cancelled".to_string(),
                        last_job_id: job.job_id.clone(),
                        last_run_id: None,
                        owner: payload.goal_sender.clone(),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: None,
                        audit_id: None,
                        plan_id: None,
                    },
                )?;
                crate::goal_management::sync_goal_work_item(
                    &self.management_store,
                    crate::goal_management::GoalWorkItemUpdate {
                        slug: &management_goal_scope,
                        title: &management_goal_title,
                        project_id: payload
                            .project_id
                            .as_deref()
                            .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                        phase: Some("cancelled"),
                        owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                        thread_id: management_thread_id.as_deref(),
                        priority: crate::goal_management::priority_from_goal_priority(40),
                        status: symbiotic_control_plane::WorkItemStatus::Cancelled,
                        observed_at: now as i64,
                    },
                );
                return Ok((
                    DaemonEvent {
                        event_type: EventType::GoalCancelled,
                        status: "completed".to_string(),
                        job_id: Some(job.job_id),
                        detail: "cancelled_before_start".to_string(),
                        goal_room: payload.goal_room.clone(),
                        goal_template: payload.goal_room.as_ref().map(|_| payload.template.clone()),
                        goal_run_id: None,
                        goal_id: Some(goal_id),
                        intake_run_id: None,
                        url: None,
                        title: None,
                        sensitivity: None,
                        quick_replies: None,
                        thread_id: None,
                    },
                    Vec::new(),
                ));
            }
        }
        if let Some(goal_room) = payload.goal_room.as_deref() {
            upsert_goal_state(
                &self.config.goal_state_file,
                GoalState {
                    goal_room: goal_room.to_string(),
                    thread_id: attached_thread_id_for_room(goal_room),
                    project_id: payload
                        .project_id
                        .clone()
                        .unwrap_or_else(default_unscoped_project_id),
                    template: payload.template.clone(),
                    status: "running".to_string(),
                    last_job_id: job.job_id.clone(),
                    // Preserve the goal_id so the goal.answer matcher can
                    // still find this state if an answer arrives while the
                    // workflow is executing.
                    last_run_id: payload.goal_id.clone(),
                    owner: payload.goal_sender.clone(),
                    updated_at: now,
                    complexity: None,
                    pipeline_stage: Some("executing".to_string()),
                    audit_id: None,
                    plan_id: None,
                },
            )?;
            crate::goal_management::sync_goal_work_item(
                &self.management_store,
                crate::goal_management::GoalWorkItemUpdate {
                    slug: &management_goal_scope,
                    title: &management_goal_title,
                    project_id: payload
                        .project_id
                        .as_deref()
                        .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                    phase: Some("executing"),
                    owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                    thread_id: management_thread_id.as_deref(),
                    priority: crate::goal_management::priority_from_goal_priority(40),
                    status: symbiotic_control_plane::WorkItemStatus::Running,
                    observed_at: now as i64,
                },
            );
        }

        let goal_room_ref = payload.goal_room.as_deref();
        let goal_template_ref = payload
            .goal_room
            .as_ref()
            .map(|_| payload.template.as_str());

        // Build extra inputs when a user answer is present (goal conversation flow).
        let extra_inputs = {
            let mut map = std::collections::HashMap::new();
            if let Some(ref answer) = payload.user_answer {
                map.insert("user_answer".to_string(), answer.clone());
            }
            if let Some(ref gid) = payload.goal_id {
                map.insert("goal_id".to_string(), gid.clone());
            }
            if let Some(ref goal) = payload.user_goal {
                map.insert("user_goal".to_string(), goal.clone());
            }
            if let Some(ref replan_context) = payload.replan_context {
                map.insert("replan_context".to_string(), replan_context.clone());
            }
            // Inject prior Q&A conversation history so the agent has context from
            // previous inquisition rounds.
            if let Some(goal_room) = payload.goal_room.as_deref() {
                if let Some(qa_history) =
                    read_goal_qa(&self.config.data_dir, goal_room, &payload.template)
                {
                    map.insert("prior_qa".to_string(), qa_history);
                }
                if let Some(auth_result) =
                    read_auth_result(&self.config.data_dir, goal_room, &payload.template)
                {
                    if let Some(request_id) = auth_result
                        .get("request_id")
                        .and_then(|value| value.as_str())
                    {
                        map.insert("auth_request_id".to_string(), request_id.to_string());
                    }
                    if let Some(target) = auth_result.get("target").and_then(|value| value.as_str())
                    {
                        map.insert("auth_target".to_string(), target.to_string());
                    }
                    if let Some(handle_id) = auth_result
                        .get("handle_id")
                        .and_then(|value| value.as_str())
                    {
                        map.insert("auth_handle_id".to_string(), handle_id.to_string());
                    }
                    if let Some(status) = auth_result.get("status").and_then(|value| value.as_str())
                    {
                        map.insert("auth_status".to_string(), status.to_string());
                    }
                }
            }
            if map.is_empty() {
                None
            } else {
                Some(map)
            }
        };

        match self.run_workflow_template_with_progress(
            &payload.template,
            &goal_id,
            payload.project_id.as_deref(),
            goal_room_ref,
            goal_template_ref,
            payload.goal_sender.as_deref(),
            now as i64,
            extra_inputs.as_ref(),
        ) {
            Ok((result, mut step_events)) if result.status == WorkflowStatus::Success => {
                let pending_auth_request = result
                    .step_results
                    .iter()
                    .rev()
                    .find_map(|sr| sr.outputs.get("pending_auth_request").cloned())
                    .and_then(|json| {
                        serde_json::from_str::<crate::auth_jobs::PendingAuthRequest>(&json).ok()
                    });

                if let (Some(auth_request), Some(goal_room)) =
                    (pending_auth_request.as_ref(), payload.goal_room.as_deref())
                {
                    if let Ok(mut store) = self.auth_jobs.lock() {
                        if let Some(mut record) = store.get(&auth_request.request_id) {
                            record.goal_room = Some(goal_room.to_string());
                            record.goal_template = Some(payload.template.clone());
                            record.goal_id = Some(goal_id.clone());
                            record.goal_scope = record
                                .goal_scope
                                .clone()
                                .or_else(|| Some(result.run_id.clone()));
                            let _ = store.update(record);
                        }
                    }
                    self.queue.ack(&job.job_id, &self.config.worker_id, now)?;

                    append_goal_log(
                        &self.config.goal_log_file,
                        GoalLogEntry {
                            ts: now,
                            event: "auth.required",
                            workflow_job_id: &job.job_id,
                            goal_room: Some(goal_room),
                            goal_sender: payload.goal_sender.as_deref(),
                            template: &payload.template,
                            detail: &auth_request.request_id,
                        },
                    )?;
                    upsert_goal_state(
                        &self.config.goal_state_file,
                        GoalState {
                            goal_room: goal_room.to_string(),
                            thread_id: attached_thread_id_for_room(goal_room),
                            project_id: payload
                                .project_id
                                .clone()
                                .unwrap_or_else(default_unscoped_project_id),
                            template: payload.template.clone(),
                            status: "awaiting_auth".to_string(),
                            last_job_id: job.job_id.clone(),
                            last_run_id: Some(goal_id.clone()),
                            owner: payload.goal_sender.clone(),
                            updated_at: now,
                            complexity: None,
                            pipeline_stage: Some("awaiting_auth".to_string()),
                            audit_id: None,
                            plan_id: None,
                        },
                    )?;
                    crate::goal_management::sync_goal_work_item(
                        &self.management_store,
                        crate::goal_management::GoalWorkItemUpdate {
                            slug: &management_goal_scope,
                            title: &management_goal_title,
                            project_id: payload
                                .project_id
                                .as_deref()
                                .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                            phase: Some("awaiting_auth"),
                            owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                            thread_id: management_thread_id.as_deref(),
                            priority: crate::goal_management::priority_from_goal_priority(40),
                            status: symbiotic_control_plane::WorkItemStatus::Blocked,
                            observed_at: now as i64,
                        },
                    );

                    let auth_detail = serde_json::json!({
                        "request_id": auth_request.request_id,
                        "target": auth_request.target,
                        "purpose": auth_request.purpose,
                        "phase": "approval",
                        "requested_by": auth_request.requested_by,
                        "goal_scope": auth_request.goal_scope,
                        "thread_id": auth_request.thread_id,
                        "expires_at": auth_request.expires_at,
                        "status": "awaiting_approval",
                    });

                    if auth_request.room_id != goal_room {
                        step_events.push(DaemonEvent {
                            event_type: EventType::AuthRequired,
                            status: "awaiting_approval".to_string(),
                            job_id: Some(job.job_id.clone()),
                            detail: auth_detail.to_string(),
                            goal_room: Some(auth_request.room_id.clone()),
                            goal_template: None,
                            goal_run_id: Some(result.run_id.clone()),
                            goal_id: None,
                            intake_run_id: None,
                            url: None,
                            title: None,
                            sensitivity: None,
                            quick_replies: None,
                            thread_id: None,
                        });
                    }

                    return Ok((
                        DaemonEvent {
                            event_type: EventType::AuthRequired,
                            status: "awaiting_approval".to_string(),
                            job_id: Some(job.job_id),
                            detail: auth_detail.to_string(),
                            goal_room: payload.goal_room.clone(),
                            goal_template: Some(payload.template.clone()),
                            goal_run_id: Some(result.run_id),
                            goal_id: None,
                            intake_run_id: None,
                            url: None,
                            title: None,
                            sensitivity: None,
                            quick_replies: None,
                            thread_id: None,
                        },
                        step_events,
                    ));
                }

                // Check if any step set a pending_question (ask_user tool was called).
                let pending_question = result
                    .step_results
                    .iter()
                    .rev()
                    .find_map(|sr| sr.outputs.get("pending_question").cloned());
                let quick_replies = result
                    .step_results
                    .iter()
                    .rev()
                    .find_map(|sr| sr.outputs.get("pending_question_quick_replies").cloned());

                if let (Some(question), Some(goal_room)) =
                    (&pending_question, payload.goal_room.as_deref())
                {
                    // Persist the question so the answer handler can pair it in the Q&A history.
                    persist_pending_question(
                        &self.config.data_dir,
                        goal_room,
                        &payload.template,
                        question,
                    );

                    // Agent asked a question — pause workflow and wait for user response.
                    self.queue.ack(&job.job_id, &self.config.worker_id, now)?;

                    // Don't emit goal.question as a step event — it's already
                    // emitted as the main DaemonEvent return below. Emitting
                    // it twice causes duplicate messages in the thread.

                    append_goal_log(
                        &self.config.goal_log_file,
                        GoalLogEntry {
                            ts: now,
                            event: "goal.question",
                            workflow_job_id: &job.job_id,
                            goal_room: Some(goal_room),
                            goal_sender: payload.goal_sender.as_deref(),
                            template: &payload.template,
                            detail: question,
                        },
                    )?;
                    upsert_goal_state(
                        &self.config.goal_state_file,
                        GoalState {
                            goal_room: goal_room.to_string(),
                            thread_id: attached_thread_id_for_room(goal_room),
                            project_id: payload
                                .project_id
                                .clone()
                                .unwrap_or_else(default_unscoped_project_id),
                            template: payload.template.clone(),
                            status: "awaiting_input".to_string(),
                            last_job_id: job.job_id.clone(),
                            last_run_id: Some(goal_id.clone()),
                            owner: payload.goal_sender.clone(),
                            updated_at: now,
                            complexity: None,
                            pipeline_stage: Some("awaiting_input".to_string()),
                            audit_id: None,
                            plan_id: None,
                        },
                    )?;
                    crate::goal_management::sync_goal_work_item(
                        &self.management_store,
                        crate::goal_management::GoalWorkItemUpdate {
                            slug: &management_goal_scope,
                            title: &management_goal_title,
                            project_id: payload
                                .project_id
                                .as_deref()
                                .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                            phase: Some("awaiting_input"),
                            owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                            thread_id: management_thread_id.as_deref(),
                            priority: crate::goal_management::priority_from_goal_priority(40),
                            status: symbiotic_control_plane::WorkItemStatus::Blocked,
                            observed_at: now as i64,
                        },
                    );

                    return Ok((
                        DaemonEvent {
                            event_type: EventType::GoalQuestion,
                            status: "awaiting_input".to_string(),
                            job_id: Some(job.job_id),
                            detail: question.clone(),
                            goal_room: payload.goal_room.clone(),
                            goal_template: Some(payload.template.clone()),
                            goal_run_id: Some(result.run_id),
                            goal_id: Some(goal_id),
                            intake_run_id: None,
                            url: None,
                            title: None,
                            sensitivity: None,
                            quick_replies,
                            thread_id: None,
                        },
                        step_events,
                    ));
                }

                // T130 §04a — Batched Inquisitor path: the agent fired
                // `ask_user_group` and the adapter's pending handle now
                // carries a fully-drafted `QuestionGroup`. Emit
                // `goal.question_group` with the JSON-encoded group as
                // detail, and register the group with the daemon's
                // `QuestionResolver` so the sibling `goal.answer` handler in
                // `commands.rs` can submit answers and produce a
                // `goal.unblocked` / `goal.question_group.expired` outcome.
                let pending_group_json = result
                    .step_results
                    .iter()
                    .rev()
                    .find_map(|sr| sr.outputs.get("pending_question_group").cloned());

                if let (Some(group_json), Some(goal_room)) =
                    (&pending_group_json, payload.goal_room.as_deref())
                {
                    match serde_json::from_str::<symbiotic_core::types::question_group::QuestionGroup>(
                        group_json,
                    ) {
                        Ok(mut group) => {
                            // Stamp the parent_goal_id if the agent omitted it
                            // (builtin_tools default is an empty string). The
                            // daemon owns the canonical goal identity here.
                            if group.parent_goal_id.trim().is_empty() {
                                group.parent_goal_id = goal_id.clone();
                            }

                            // Register the group with the resolver so the
                            // matching `goal.answer` handler can route
                            // answers through `submit_answer`.
                            if let Ok(mut resolver) = self.question_resolver.lock() {
                                resolver.register(group.clone());
                            }

                            // T130 §05 — Teach the Sub-Goal Dispatcher about
                            // the group's UnblockKey so the eventual
                            // `goal.unblocked` handler in `commands.rs` can
                            // route without needing to re-parse the group
                            // JSON. No-op if the dispatcher wasn't installed
                            // at boot (e.g. in tests).
                            if let Some(dispatcher) = self.subgoal_dispatcher() {
                                dispatcher.remember_group(
                                    group.group_id.clone(),
                                    group.unblock_key.clone(),
                                );
                            }

                            // Re-serialize with stamped parent_goal_id so the
                            // wire payload is canonical. Fall back to the
                            // original JSON if serialization fails
                            // (structurally impossible given it parsed).
                            let canonical_json = serde_json::to_string(&group)
                                .unwrap_or_else(|_| group_json.clone());

                            // Workflow pauses awaiting operator input.
                            self.queue.ack(&job.job_id, &self.config.worker_id, now)?;

                            append_goal_log(
                                &self.config.goal_log_file,
                                GoalLogEntry {
                                    ts: now,
                                    event: "goal.question_group",
                                    workflow_job_id: &job.job_id,
                                    goal_room: Some(goal_room),
                                    goal_sender: payload.goal_sender.as_deref(),
                                    template: &payload.template,
                                    detail: &canonical_json,
                                },
                            )?;
                            upsert_goal_state(
                                &self.config.goal_state_file,
                                GoalState {
                                    goal_room: goal_room.to_string(),
                                    thread_id: attached_thread_id_for_room(goal_room),
                                    project_id: payload
                                        .project_id
                                        .clone()
                                        .unwrap_or_else(default_unscoped_project_id),
                                    template: payload.template.clone(),
                                    status: "awaiting_input".to_string(),
                                    last_job_id: job.job_id.clone(),
                                    last_run_id: Some(goal_id.clone()),
                                    owner: payload.goal_sender.clone(),
                                    updated_at: now,
                                    complexity: None,
                                    pipeline_stage: Some("awaiting_input".to_string()),
                                    audit_id: None,
                                    plan_id: None,
                                },
                            )?;
                            crate::goal_management::sync_goal_work_item(
                                &self.management_store,
                                crate::goal_management::GoalWorkItemUpdate {
                                    slug: &management_goal_scope,
                                    title: &management_goal_title,
                                    project_id: payload
                                        .project_id
                                        .as_deref()
                                        .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                                    phase: Some("awaiting_input"),
                                    owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                                    thread_id: management_thread_id.as_deref(),
                                    priority: crate::goal_management::priority_from_goal_priority(
                                        40,
                                    ),
                                    status: symbiotic_control_plane::WorkItemStatus::Blocked,
                                    observed_at: now as i64,
                                },
                            );

                            return Ok((
                                DaemonEvent {
                                    event_type: EventType::GoalQuestionGroup,
                                    status: "awaiting_input".to_string(),
                                    job_id: Some(job.job_id),
                                    detail: canonical_json,
                                    goal_room: payload.goal_room.clone(),
                                    goal_template: Some(payload.template.clone()),
                                    goal_run_id: Some(result.run_id),
                                    goal_id: Some(goal_id),
                                    intake_run_id: None,
                                    url: None,
                                    title: None,
                                    sensitivity: None,
                                    quick_replies: None,
                                    thread_id: None,
                                },
                                step_events,
                            ));
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "goals.rs: failed to parse pending_question_group JSON; \
                                 falling through to normal completion path",
                            );
                        }
                    }
                }

                // Check if the agent generated a plan (generate_plan tool was called).
                let pending_plan = result
                    .step_results
                    .iter()
                    .rev()
                    .find_map(|sr| sr.outputs.get("pending_plan").cloned());

                if let (Some(plan_json), Some(goal_room)) =
                    (&pending_plan, payload.goal_room.as_deref())
                {
                    // Persist the plan JSON so the approval handler can
                    // read it back and generate a dynamic workflow.
                    persist_pending_plan(
                        &self.config.data_dir,
                        goal_room,
                        &payload.template,
                        plan_json,
                    );

                    // Agent proposed a plan — pause and wait for user approval.
                    self.queue.ack(&job.job_id, &self.config.worker_id, now)?;

                    // Don't emit goal.plan.proposed as a step event — it's
                    // already emitted as the main DaemonEvent return below.

                    append_goal_log(
                        &self.config.goal_log_file,
                        GoalLogEntry {
                            ts: now,
                            event: "goal.plan.proposed",
                            workflow_job_id: &job.job_id,
                            goal_room: Some(goal_room),
                            goal_sender: payload.goal_sender.as_deref(),
                            template: &payload.template,
                            detail: plan_json,
                        },
                    )?;
                    upsert_goal_state(
                        &self.config.goal_state_file,
                        GoalState {
                            goal_room: goal_room.to_string(),
                            thread_id: attached_thread_id_for_room(goal_room),
                            project_id: payload
                                .project_id
                                .clone()
                                .unwrap_or_else(default_unscoped_project_id),
                            template: payload.template.clone(),
                            status: "awaiting_approval".to_string(),
                            last_job_id: job.job_id.clone(),
                            last_run_id: Some(goal_id.clone()),
                            owner: payload.goal_sender.clone(),
                            updated_at: now,
                            complexity: None,
                            pipeline_stage: Some("awaiting_approval".to_string()),
                            audit_id: None,
                            plan_id: Some(goal_id.clone()),
                        },
                    )?;
                    crate::goal_management::sync_goal_work_item(
                        &self.management_store,
                        crate::goal_management::GoalWorkItemUpdate {
                            slug: &management_goal_scope,
                            title: &management_goal_title,
                            project_id: payload
                                .project_id
                                .as_deref()
                                .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                            phase: Some("awaiting_approval"),
                            owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                            thread_id: management_thread_id.as_deref(),
                            priority: crate::goal_management::priority_from_goal_priority(40),
                            status: symbiotic_control_plane::WorkItemStatus::Blocked,
                            observed_at: now as i64,
                        },
                    );

                    return Ok((
                        DaemonEvent {
                            event_type: EventType::GoalPlanProposed,
                            status: "awaiting_approval".to_string(),
                            job_id: Some(job.job_id),
                            detail: plan_json.clone(),
                            goal_room: payload.goal_room.clone(),
                            goal_template: Some(payload.template.clone()),
                            goal_run_id: Some(result.run_id),
                            goal_id: Some(goal_id),
                            intake_run_id: None,
                            url: None,
                            title: None,
                            sensitivity: None,
                            quick_replies: None,
                            thread_id: None,
                        },
                        step_events,
                    ));
                }

                // Normal completion (no pending question or plan).
                self.queue.ack(&job.job_id, &self.config.worker_id, now)?;

                // Extract the agent's result/answer from step outputs.
                // The agent executor stores output at `{step_id}_output`.
                let result_detail = result
                    .step_results
                    .iter()
                    .rev()
                    .find_map(|sr| sr.outputs.get(&format!("{}_output", sr.step_id)).cloned())
                    .unwrap_or_default();

                // Emit goal.result carrying the actual answer/output
                // before goal.completed which carries only the status.
                if payload.goal_room.is_some() {
                    step_events.push(DaemonEvent {
                        event_type: EventType::GoalResult,
                        status: "completed".to_string(),
                        job_id: Some(job.job_id.clone()),
                        detail: result_detail,
                        goal_room: payload.goal_room.clone(),
                        goal_template: Some(payload.template.clone()),
                        goal_run_id: Some(result.run_id.clone()),
                        goal_id: Some(goal_id.clone()),
                        intake_run_id: None,
                        url: None,
                        title: None,
                        sensitivity: None,
                        quick_replies: None,
                        thread_id: None,
                    });

                    append_goal_log(
                        &self.config.goal_log_file,
                        GoalLogEntry {
                            ts: now,
                            event: "goal.completed",
                            workflow_job_id: &job.job_id,
                            goal_room: payload.goal_room.as_deref(),
                            goal_sender: payload.goal_sender.as_deref(),
                            template: &payload.template,
                            detail: &result.run_id,
                        },
                    )?;

                    // Spawn PE after successful goal completion.
                    self.maybe_spawn_process_engineer(&result.run_id, &payload.template, now);
                    upsert_goal_state(
                        &self.config.goal_state_file,
                        GoalState {
                            goal_room: payload
                                .goal_room
                                .clone()
                                .ok_or_else(|| anyhow!("goal room not set"))?,
                            thread_id: payload
                                .goal_room
                                .as_deref()
                                .and_then(attached_thread_id_for_room),
                            project_id: payload
                                .project_id
                                .clone()
                                .unwrap_or_else(default_unscoped_project_id),
                            template: payload.template.clone(),
                            status: "completed".to_string(),
                            last_job_id: job.job_id.clone(),
                            last_run_id: Some(result.run_id.clone()),
                            owner: payload.goal_sender.clone(),
                            updated_at: now,
                            complexity: None,
                            pipeline_stage: None,
                            audit_id: None,
                            plan_id: None,
                        },
                    )?;
                    crate::goal_management::sync_goal_work_item(
                        &self.management_store,
                        crate::goal_management::GoalWorkItemUpdate {
                            slug: &management_goal_scope,
                            title: &management_goal_title,
                            project_id: payload
                                .project_id
                                .as_deref()
                                .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                            phase: Some("completed"),
                            owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                            thread_id: management_thread_id.as_deref(),
                            priority: crate::goal_management::priority_from_goal_priority(40),
                            status: symbiotic_control_plane::WorkItemStatus::Done,
                            observed_at: now as i64,
                        },
                    );
                }
                Ok((
                    DaemonEvent {
                        event_type: EventType::WorkflowRun,
                        status: "completed".to_string(),
                        job_id: Some(job.job_id),
                        detail: result.run_id.clone(),
                        goal_room: payload.goal_room.clone(),
                        goal_template: payload.goal_room.as_ref().map(|_| payload.template.clone()),
                        goal_run_id: payload.goal_room.as_ref().map(|_| result.run_id),
                        goal_id: Some(goal_id),
                        intake_run_id: None,
                        url: None,
                        title: None,
                        sensitivity: None,
                        quick_replies: None,
                        thread_id: None,
                    },
                    step_events,
                ))
            }
            Ok((result, step_events)) => {
                let outcome = self.queue.fail(
                    &job.job_id,
                    &self.config.worker_id,
                    now,
                    self.config.retry_backoff_seconds,
                    "workflow execution failed",
                )?;
                let goal_status = if outcome == FailOutcome::MovedToDlq {
                    "dlq"
                } else {
                    "retry"
                };
                if payload.goal_room.is_some() {
                    append_goal_log(
                        &self.config.goal_log_file,
                        GoalLogEntry {
                            ts: now,
                            event: "goal.failed",
                            workflow_job_id: &job.job_id,
                            goal_room: payload.goal_room.as_deref(),
                            goal_sender: payload.goal_sender.as_deref(),
                            template: &payload.template,
                            detail: "workflow execution failed",
                        },
                    )?;
                    upsert_goal_state(
                        &self.config.goal_state_file,
                        GoalState {
                            goal_room: payload
                                .goal_room
                                .clone()
                                .ok_or_else(|| anyhow!("goal room not set"))?,
                            thread_id: payload
                                .goal_room
                                .as_deref()
                                .and_then(attached_thread_id_for_room),
                            project_id: payload
                                .project_id
                                .clone()
                                .unwrap_or_else(default_unscoped_project_id),
                            template: payload.template.clone(),
                            status: goal_status.to_string(),
                            last_job_id: job.job_id.clone(),
                            last_run_id: Some(result.run_id.clone()),
                            owner: payload.goal_sender.clone(),
                            updated_at: now,
                            complexity: None,
                            pipeline_stage: None,
                            audit_id: None,
                            plan_id: None,
                        },
                    )?;
                    crate::goal_management::sync_goal_work_item(
                        &self.management_store,
                        crate::goal_management::GoalWorkItemUpdate {
                            slug: &management_goal_scope,
                            title: &management_goal_title,
                            project_id: payload
                                .project_id
                                .as_deref()
                                .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                            phase: Some(goal_status),
                            owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                            thread_id: management_thread_id.as_deref(),
                            priority: crate::goal_management::priority_from_goal_priority(40),
                            status: symbiotic_control_plane::WorkItemStatus::Failed,
                            observed_at: now as i64,
                        },
                    );
                }
                Ok((
                    DaemonEvent {
                        event_type: EventType::WorkflowRun,
                        status: if outcome == FailOutcome::MovedToDlq {
                            "dlq".to_string()
                        } else {
                            "retry".to_string()
                        },
                        job_id: Some(job.job_id.clone()),
                        detail: result.run_id.clone(),
                        goal_room: payload.goal_room.clone(),
                        goal_template: payload.goal_room.as_ref().map(|_| payload.template.clone()),
                        goal_run_id: payload.goal_room.as_ref().map(|_| result.run_id),
                        goal_id: Some(goal_id),
                        intake_run_id: None,
                        url: None,
                        title: None,
                        sensitivity: None,
                        quick_replies: None,
                        thread_id: None,
                    },
                    step_events,
                ))
            }
            Err(err) => {
                let outcome = self.queue.fail(
                    &job.job_id,
                    &self.config.worker_id,
                    now,
                    self.config.retry_backoff_seconds,
                    &err.to_string(),
                )?;
                let goal_status = if outcome == FailOutcome::MovedToDlq {
                    "dlq"
                } else {
                    "retry"
                };
                if payload.goal_room.is_some() {
                    let err_detail = err.to_string();
                    append_goal_log(
                        &self.config.goal_log_file,
                        GoalLogEntry {
                            ts: now,
                            event: "goal.failed",
                            workflow_job_id: &job.job_id,
                            goal_room: payload.goal_room.as_deref(),
                            goal_sender: payload.goal_sender.as_deref(),
                            template: &payload.template,
                            detail: &err_detail,
                        },
                    )?;
                    upsert_goal_state(
                        &self.config.goal_state_file,
                        GoalState {
                            goal_room: payload
                                .goal_room
                                .clone()
                                .ok_or_else(|| anyhow!("goal room not set"))?,
                            thread_id: payload
                                .goal_room
                                .as_deref()
                                .and_then(attached_thread_id_for_room),
                            project_id: payload
                                .project_id
                                .clone()
                                .unwrap_or_else(default_unscoped_project_id),
                            template: payload.template.clone(),
                            status: goal_status.to_string(),
                            last_job_id: job.job_id.clone(),
                            last_run_id: None,
                            owner: payload.goal_sender.clone(),
                            updated_at: now,
                            complexity: None,
                            pipeline_stage: None,
                            audit_id: None,
                            plan_id: None,
                        },
                    )?;
                    crate::goal_management::sync_goal_work_item(
                        &self.management_store,
                        crate::goal_management::GoalWorkItemUpdate {
                            slug: &management_goal_scope,
                            title: &management_goal_title,
                            project_id: payload
                                .project_id
                                .as_deref()
                                .unwrap_or(DEFAULT_UNSCOPED_PROJECT_ID),
                            phase: Some(goal_status),
                            owner: payload.goal_sender.as_deref().unwrap_or("goal-runner"),
                            thread_id: management_thread_id.as_deref(),
                            priority: crate::goal_management::priority_from_goal_priority(40),
                            status: symbiotic_control_plane::WorkItemStatus::Failed,
                            observed_at: now as i64,
                        },
                    );
                }
                Ok((
                    DaemonEvent {
                        event_type: EventType::WorkflowRun,
                        status: if outcome == FailOutcome::MovedToDlq {
                            "dlq".to_string()
                        } else {
                            "retry".to_string()
                        },
                        job_id: Some(job.job_id),
                        detail: err.to_string(),
                        goal_room: payload.goal_room.clone(),
                        goal_template: payload.goal_room.as_ref().map(|_| payload.template.clone()),
                        goal_run_id: None,
                        goal_id: Some(goal_id),
                        intake_run_id: None,
                        url: None,
                        title: None,
                        sensitivity: None,
                        quick_replies: None,
                        thread_id: None,
                    },
                    Vec::new(),
                ))
            }
        }
    }

    /// Process a goal through the deliberation-first pipeline.
    ///
    /// Creates a `GoalSubmission` from the incoming data, routes it through
    /// `DeliberationPipeline::process_goal()`, and maps the `PipelineOutcome`
    /// to a `DaemonEvent` while updating `GoalState` accordingly.
    pub(crate) fn process_goal_through_pipeline(
        &self,
        description: &str,
        room_id: &str,
        sender: &str,
        now: u64,
    ) -> Result<DaemonEvent> {
        let goal_id = generate_goal_id();
        let thread_id = attached_thread_id_for_room(room_id);
        let project_id =
            self.resolve_project_id_for_goal_materialization(None, thread_id.as_deref())?;

        // Build GoalSubmission from the description.
        let submission = GoalSubmission {
            id: goal_id.clone(),
            title: description
                .chars()
                .take(80)
                .collect::<String>()
                .trim()
                .to_string(),
            description: description.to_string(),
            source: GoalSource::Matrix {
                room_id: room_id.to_string(),
                sender: sender.to_string(),
            },
            metadata: Some(GoalMetadata {
                title: description
                    .chars()
                    .take(80)
                    .collect::<String>()
                    .trim()
                    .to_string(),
                description: description.to_string(),
                domains: vec!["general".to_string()],
                phases: vec!["deliberate".to_string(), "execute".to_string()],
                has_known_template: false,
                template_name: None,
                external_dependencies: vec![],
                required_scopes: vec![],
                estimated_cost_usd: None,
                autonomy_level: AutonomyLevel::Semi,
                constraints: None,
            }),
        };
        let submission_title = submission.title.clone();

        // Record initial goal state.
        upsert_goal_state(
            &self.config.goal_state_file,
            GoalState {
                goal_room: room_id.to_string(),
                thread_id: thread_id.clone(),
                project_id: project_id.clone(),
                template: "deliberation".to_string(),
                status: "classifying".to_string(),
                last_job_id: goal_id.clone(),
                last_run_id: None,
                owner: Some(sender.to_string()),
                updated_at: now,
                complexity: None,
                pipeline_stage: Some("classifying".to_string()),
                audit_id: None,
                plan_id: None,
            },
        )?;

        append_goal_log(
            &self.config.goal_log_file,
            GoalLogEntry {
                ts: now,
                event: "goal.deliberation.started",
                workflow_job_id: &goal_id,
                goal_room: Some(room_id),
                goal_sender: Some(sender),
                template: "deliberation",
                detail: &submission.title,
            },
        )?;

        crate::goal_management::sync_goal_work_item(
            &self.management_store,
            crate::goal_management::GoalWorkItemUpdate {
                slug: &goal_id,
                title: &submission_title,
                phase: Some("classifying"),
                owner: sender,
                thread_id: attached_thread_id_for_room(room_id).as_deref(),
                project_id: &project_id,
                priority: crate::goal_management::priority_from_goal_priority(40),
                status: WorkItemStatus::Running,
                observed_at: now as i64,
            },
        );

        // Run the pipeline synchronously by bridging async via a scoped thread.
        // This is the same pattern used by `run_intake_embeddings`.
        let outcome: Result<PipelineOutcome> = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                use std::collections::HashMap;
                use symbiotic_agents::pipeline::audit::AuditLog;
                use symbiotic_agents::pipeline::classifier::{
                    ClassifierConfig, GoalComplexityClassifier,
                };
                use symbiotic_agents::pipeline::plan_gen::PlanGenerator;
                use symbiotic_agents::pipeline::{DeliberationPipeline, PipelineConfig};

                let goals_dir = self
                    .config
                    .goal_log_file
                    .parent()
                    .unwrap_or(std::path::Path::new("data/goals"))
                    .to_path_buf();
                let audit_db_path = goals_dir.join("pipeline-audit.db");
                let audit_log_path = goals_dir.join("pipeline-audit.jsonl");
                let audit = AuditLog::new(audit_db_path, audit_log_path)?;
                let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
                let plan_generator = PlanGenerator::new(HashMap::new());
                let pipeline = DeliberationPipeline::new(
                    classifier,
                    plan_generator,
                    audit,
                    PipelineConfig::default(),
                );

                let llm_client = self.make_llm_client(symbiotic_core::Sensitivity::Shareable);
                std::thread::scope(|s| {
                    s.spawn(move || {
                        let clients: Vec<&dyn symbiotic_agents::llm::LlmClient> = vec![&llm_client];
                        handle.block_on(pipeline.process_goal(&submission, &clients))
                    })
                    .join()
                    .expect("pipeline thread should not panic")
                })
            }
            Err(_) => Err(anyhow!("no tokio runtime available for pipeline execution")),
        };

        // Map PipelineOutcome to DaemonEvent + GoalState updates.
        match outcome {
            Ok(PipelineOutcome::Executed {
                plan_result,
                audit_id,
            }) => {
                let passed = plan_result.passed;
                let status = if passed { "completed" } else { "failed" };

                // Spawn PE after successful pipeline execution.
                if passed {
                    self.maybe_spawn_process_engineer(&audit_id, "deliberation", now);
                }

                upsert_goal_state(
                    &self.config.goal_state_file,
                    GoalState {
                        goal_room: room_id.to_string(),
                        thread_id: attached_thread_id_for_room(room_id),
                        project_id: project_id.clone(),
                        template: "deliberation".to_string(),
                        status: status.to_string(),
                        last_job_id: goal_id.clone(),
                        last_run_id: None,
                        owner: Some(sender.to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("executed".to_string()),
                        audit_id: Some(audit_id.clone()),
                        plan_id: Some(plan_result.plan_name.clone()),
                    },
                )?;
                crate::goal_management::sync_goal_work_item(
                    &self.management_store,
                    crate::goal_management::GoalWorkItemUpdate {
                        slug: &goal_id,
                        title: &submission_title,
                        project_id: &project_id,
                        phase: Some("executed"),
                        owner: sender,
                        thread_id: attached_thread_id_for_room(room_id).as_deref(),
                        priority: crate::goal_management::priority_from_goal_priority(40),
                        status: if passed {
                            WorkItemStatus::Done
                        } else {
                            WorkItemStatus::Failed
                        },
                        observed_at: now as i64,
                    },
                );
                Ok(DaemonEvent {
                    event_type: EventType::GoalDeliberationExecuted,
                    status: status.to_string(),
                    job_id: Some(goal_id.clone()),
                    detail: format!("plan={} passed={}", plan_result.plan_name, passed),
                    goal_room: Some(room_id.to_string()),
                    goal_template: Some("deliberation".to_string()),
                    goal_run_id: Some(audit_id),
                    goal_id: Some(goal_id),
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
            Ok(PipelineOutcome::AwaitingApproval {
                plan: _,
                confidence,
                audit_id,
            }) => {
                let complexity = if confidence >= 0.95 {
                    "simple"
                } else if confidence >= 0.7 {
                    "moderate"
                } else {
                    "complex"
                };

                // Queue inquisition workflow — the inquisitor agent asks
                // clarifying questions, then generates a plan for user approval.
                let inquisition_template = format!("inquisition:{}", goal_id);

                // Persist the original goal text so subsequent inquisition rounds
                // (after goal.answer re-queues) can retrieve it via read_goal_text.
                persist_goal_text(
                    &self.config.data_dir,
                    room_id,
                    &inquisition_template,
                    description,
                );

                let _ = self.queue_workflow_run_with_payload(&WorkflowRunPayload {
                    template: inquisition_template.clone(),
                    goal_room: Some(room_id.to_string()),
                    goal_sender: Some(sender.to_string()),
                    project_id: Some(project_id.clone()),
                    user_answer: None,
                    goal_id: Some(goal_id.clone()),
                    user_goal: Some(description.to_string()),
                    replan_context: None,
                });

                upsert_goal_state(
                    &self.config.goal_state_file,
                    GoalState {
                        goal_room: room_id.to_string(),
                        thread_id: attached_thread_id_for_room(room_id),
                        project_id: project_id.clone(),
                        template: inquisition_template.clone(),
                        status: "running".to_string(),
                        last_job_id: goal_id.clone(),
                        last_run_id: Some(goal_id.clone()),
                        owner: Some(sender.to_string()),
                        updated_at: now,
                        complexity: Some(complexity.to_string()),
                        pipeline_stage: Some("inquisition".to_string()),
                        audit_id: Some(audit_id.clone()),
                        plan_id: None,
                    },
                )?;
                crate::goal_management::sync_goal_work_item(
                    &self.management_store,
                    crate::goal_management::GoalWorkItemUpdate {
                        slug: &goal_id,
                        title: &submission_title,
                        project_id: &project_id,
                        phase: Some("inquisition"),
                        owner: sender,
                        thread_id: attached_thread_id_for_room(room_id).as_deref(),
                        priority: crate::goal_management::priority_from_goal_priority(40),
                        status: WorkItemStatus::Running,
                        observed_at: now as i64,
                    },
                );
                Ok(DaemonEvent {
                    event_type: EventType::GoalInquisitionStarted,
                    status: "running".to_string(),
                    job_id: Some(goal_id.clone()),
                    detail: format!("complexity={} — starting inquisition", complexity),
                    goal_room: Some(room_id.to_string()),
                    goal_template: Some(inquisition_template),
                    goal_run_id: Some(audit_id),
                    goal_id: Some(goal_id),
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
            Ok(PipelineOutcome::Deliberating {
                council_session_id,
                audit_id,
            }) => {
                upsert_goal_state(
                    &self.config.goal_state_file,
                    GoalState {
                        goal_room: room_id.to_string(),
                        thread_id: attached_thread_id_for_room(room_id),
                        project_id: project_id.clone(),
                        template: "deliberation".to_string(),
                        status: "deliberating".to_string(),
                        last_job_id: goal_id.clone(),
                        last_run_id: None,
                        owner: Some(sender.to_string()),
                        updated_at: now,
                        complexity: Some("complex".to_string()),
                        pipeline_stage: Some("council".to_string()),
                        audit_id: Some(audit_id.clone()),
                        plan_id: None,
                    },
                )?;
                crate::goal_management::sync_goal_work_item(
                    &self.management_store,
                    crate::goal_management::GoalWorkItemUpdate {
                        slug: &goal_id,
                        title: &submission_title,
                        project_id: &project_id,
                        phase: Some("council"),
                        owner: sender,
                        thread_id: attached_thread_id_for_room(room_id).as_deref(),
                        priority: crate::goal_management::priority_from_goal_priority(40),
                        status: WorkItemStatus::Running,
                        observed_at: now as i64,
                    },
                );
                Ok(DaemonEvent {
                    event_type: EventType::GoalDeliberationCouncil,
                    status: "deliberating".to_string(),
                    job_id: Some(goal_id.clone()),
                    detail: format!("council_session={council_session_id}"),
                    goal_room: Some(room_id.to_string()),
                    goal_template: Some("deliberation".to_string()),
                    goal_run_id: Some(audit_id),
                    goal_id: Some(goal_id),
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
            Ok(PipelineOutcome::Rejected { reason, audit_id }) => {
                upsert_goal_state(
                    &self.config.goal_state_file,
                    GoalState {
                        goal_room: room_id.to_string(),
                        thread_id: attached_thread_id_for_room(room_id),
                        project_id: project_id.clone(),
                        template: "deliberation".to_string(),
                        status: "rejected".to_string(),
                        last_job_id: goal_id.clone(),
                        last_run_id: None,
                        owner: Some(sender.to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("rejected".to_string()),
                        audit_id: Some(audit_id.clone()),
                        plan_id: None,
                    },
                )?;
                crate::goal_management::sync_goal_work_item(
                    &self.management_store,
                    crate::goal_management::GoalWorkItemUpdate {
                        slug: &goal_id,
                        title: &submission_title,
                        project_id: &project_id,
                        phase: Some("rejected"),
                        owner: sender,
                        thread_id: attached_thread_id_for_room(room_id).as_deref(),
                        priority: crate::goal_management::priority_from_goal_priority(40),
                        status: WorkItemStatus::Cancelled,
                        observed_at: now as i64,
                    },
                );
                Ok(DaemonEvent {
                    event_type: EventType::GoalDeliberationRejected,
                    status: "rejected".to_string(),
                    job_id: Some(goal_id.clone()),
                    detail: reason,
                    goal_room: Some(room_id.to_string()),
                    goal_template: Some("deliberation".to_string()),
                    goal_run_id: Some(audit_id),
                    goal_id: Some(goal_id),
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
            Err(err) => {
                upsert_goal_state(
                    &self.config.goal_state_file,
                    GoalState {
                        goal_room: room_id.to_string(),
                        thread_id: attached_thread_id_for_room(room_id),
                        project_id: project_id.clone(),
                        template: "deliberation".to_string(),
                        status: "failed".to_string(),
                        last_job_id: goal_id.clone(),
                        last_run_id: None,
                        owner: Some(sender.to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("error".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                )?;
                crate::goal_management::sync_goal_work_item(
                    &self.management_store,
                    crate::goal_management::GoalWorkItemUpdate {
                        slug: &goal_id,
                        title: &submission_title,
                        project_id: &project_id,
                        phase: Some("error"),
                        owner: sender,
                        thread_id: attached_thread_id_for_room(room_id).as_deref(),
                        priority: crate::goal_management::priority_from_goal_priority(40),
                        status: WorkItemStatus::Failed,
                        observed_at: now as i64,
                    },
                );
                Ok(DaemonEvent {
                    event_type: EventType::GoalDeliberationFailed,
                    status: "failed".to_string(),
                    job_id: Some(goal_id.clone()),
                    detail: err.to_string(),
                    goal_room: Some(room_id.to_string()),
                    goal_template: Some("deliberation".to_string()),
                    goal_run_id: None,
                    goal_id: Some(goal_id),
                    intake_run_id: None,
                    url: None,
                    title: None,
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: None,
                })
            }
        }
    }

    /// Create a `GoalStateExecutor` that bridges reconciler actions to the
    /// daemon's goal state file, log, and GoalProcessManager.
    ///
    /// This executor is passed to `reconciler::execute_actions()` to wire the
    /// reconciler's declarative actions to:
    /// - **GoalProcessManager** — validated state transitions, metrics, JSON persistence (source of truth)
    /// - **goal_state.tsv** — flat-file audit trail
    /// - **goal_log_file** — event log for start/stop events
    pub fn make_reconciler_executor(&self) -> crate::reconciler::GoalStateExecutor {
        crate::reconciler::GoalStateExecutor {
            goal_state_file: self.config.goal_state_file.clone(),
            goal_log_file: self.config.goal_log_file.clone(),
            goal_process_manager: Some(self.goal_process_manager.clone()),
            management_store: Some(self.management_store.clone()),
        }
    }
}

/// Construct a dynamic `agent-execute` workflow for goal auto-execution.
///
/// When the deliberation pipeline auto-executes a high-confidence goal, it
/// queues `agent-execute:{goal_id}` as the template name.  This is not a
/// static template in the registry — it's a dynamic pattern.  This function
/// builds the workflow inline with a single `agent.execute` step so the
/// `AgentExecuteExecutor` can run the ReAct agent loop.
/// Build an inquisition workflow — single step with the inquisitor role.
/// The inquisitor asks clarifying questions via `ask_user` and generates
/// a plan via `generate_plan`.
fn build_inquisition_workflow(template_name: &str) -> symbiotic_workflows::Workflow {
    use std::collections::HashMap;
    symbiotic_workflows::Workflow {
        id: format!("wf-{template_name}"),
        name: template_name.to_string(),
        version: "1.0".to_string(),
        inputs: HashMap::new(),
        policy: symbiotic_workflows::WorkflowPolicy {
            sensitivity_max: "restricted".to_string(),
            model_class: "cloud".to_string(),
        },
        steps: vec![symbiotic_workflows::WorkflowStep {
            id: "clarify".to_string(),
            step_type: "agent.execute".to_string(),
            config: [("timeout_seconds".to_string(), "300".to_string())]
                .into_iter()
                .collect(),
            agent_role: Some("inquisitor".to_string()),
        }],
    }
}

/// Build an agent execution workflow. If the workflow inputs contain a
/// `user_answer` that parses as a ProposedPlan JSON, generate multi-role
/// steps from the plan. Otherwise fall back to a single researcher step.
fn build_agent_execute_workflow(template_name: &str) -> symbiotic_workflows::Workflow {
    use std::collections::HashMap;
    // Default: single researcher + report (fallback when no plan is available).
    // When executed with a plan in user_answer, the daemon will rebuild the
    // steps dynamically via build_plan_driven_workflow().
    symbiotic_workflows::Workflow {
        id: format!("wf-{template_name}"),
        name: template_name.to_string(),
        version: "1.0".to_string(),
        inputs: HashMap::new(),
        policy: symbiotic_workflows::WorkflowPolicy {
            sensitivity_max: "restricted".to_string(),
            model_class: "cloud".to_string(),
        },
        steps: vec![
            symbiotic_workflows::WorkflowStep {
                id: "execute".to_string(),
                step_type: "agent.execute".to_string(),
                config: [("timeout_seconds".to_string(), "300".to_string())]
                    .into_iter()
                    .collect(),
                agent_role: Some("researcher".to_string()),
            },
            symbiotic_workflows::WorkflowStep {
                id: "report".to_string(),
                step_type: "goal.report".to_string(),
                config: [("timeout_seconds".to_string(), "30".to_string())]
                    .into_iter()
                    .collect(),
                agent_role: None,
            },
        ],
    }
}

/// Build a multi-role workflow from a ProposedPlan. Each plan step becomes
/// an `agent.execute` workflow step with the role specified in the plan.
/// A `reviewer` step is appended if none exists in the plan, and a final
/// `goal.report` step is always added.
pub(crate) fn build_plan_driven_workflow(
    template_name: &str,
    plan: &symbiotic_agents::builtin_tools::ProposedPlan,
) -> symbiotic_workflows::Workflow {
    use std::collections::HashMap;

    let mut steps: Vec<symbiotic_workflows::WorkflowStep> = plan
        .steps
        .iter()
        .filter(|ps| ps.task_driver.spawns_workflow_step())
        .map(|ps| symbiotic_workflows::WorkflowStep {
            id: ps.task_id.clone(),
            step_type: "agent.execute".to_string(),
            config: {
                let mut cfg = HashMap::new();
                cfg.insert(
                    "timeout_seconds".to_string(),
                    ps.timeout_s.unwrap_or(300).to_string(),
                );
                cfg.insert("description".to_string(), ps.description.clone());
                cfg
            },
            agent_role: ps.role.clone(),
        })
        .collect();

    // If no reviewer step exists in the plan, append one for quality check.
    let has_reviewer = plan.steps.iter().any(|step| {
        step.task_kind == symbiotic_agents::builtin_tools::PlanTaskKind::Review
            || step.role.as_deref() == Some("reviewer")
    });
    if !has_reviewer && steps.len() > 1 {
        steps.push(symbiotic_workflows::WorkflowStep {
            id: "review".to_string(),
            step_type: "agent.execute".to_string(),
            config: [("timeout_seconds".to_string(), "120".to_string())]
                .into_iter()
                .collect(),
            agent_role: Some("reviewer".to_string()),
        });
    }

    // Always end with a report step.
    steps.push(symbiotic_workflows::WorkflowStep {
        id: "report".to_string(),
        step_type: "goal.report".to_string(),
        config: [("timeout_seconds".to_string(), "30".to_string())]
            .into_iter()
            .collect(),
        agent_role: None,
    });

    symbiotic_workflows::Workflow {
        id: format!("wf-{template_name}"),
        name: template_name.to_string(),
        version: "1.0".to_string(),
        inputs: HashMap::new(),
        policy: symbiotic_workflows::WorkflowPolicy {
            sensitivity_max: "restricted".to_string(),
            model_class: "cloud".to_string(),
        },
        steps,
    }
}

fn planned_task_summary(step: &symbiotic_agents::builtin_tools::PlanStep) -> String {
    let declared_detail = declared_context_detail(&step.declared_context);
    match (
        step.task_kind,
        step.task_driver,
        step.role.as_deref(),
        step.owner_hint.as_deref(),
    ) {
        (symbiotic_agents::builtin_tools::PlanTaskKind::Review, _, _, Some(owner)) => {
            format!(
                "Review generated work before completion ({owner}).{}",
                declared_detail
            )
        }
        (symbiotic_agents::builtin_tools::PlanTaskKind::Review, _, Some("reviewer"), _) => {
            format!(
                "Review generated work before completion.{}",
                declared_detail
            )
        }
        (_, symbiotic_agents::builtin_tools::PlanTaskDriver::Declared, _, Some(owner)) => {
            format!(
                "Approved declared task '{}' is routed to {}.{}",
                step.task_id, owner, declared_detail
            )
        }
        (_, _, Some(role), _) => {
            format!(
                "Approved plan task '{}' assigned to role '{}'.{}",
                step.task_id, role, declared_detail
            )
        }
        _ => format!("Approved plan task '{}'.{}", step.task_id, declared_detail),
    }
}

fn planned_task_owner_hint(step: &symbiotic_agents::builtin_tools::PlanStep) -> Option<String> {
    step.owner_hint
        .clone()
        .or_else(|| step.role.as_deref().map(|role| format!("role:{role}")))
}

fn planned_task_declared_context(
    step: &symbiotic_agents::builtin_tools::PlanStep,
) -> symbiotic_control_plane::types::GoalTaskDeclaredContext {
    symbiotic_control_plane::types::GoalTaskDeclaredContext {
        review_target: step.declared_context.review_target.clone(),
        waiting_for: step.declared_context.waiting_for.clone(),
        coordination_target: step.declared_context.coordination_target.clone(),
        external_dependency: step.declared_context.external_dependency.clone(),
    }
}

fn planned_task_policy(
    step: &symbiotic_agents::builtin_tools::PlanStep,
) -> symbiotic_control_plane::types::GoalTaskPolicy {
    symbiotic_control_plane::types::GoalTaskPolicy {
        escalation: step.policy.escalation.as_ref().map(|escalation| {
            symbiotic_control_plane::types::GoalTaskEscalationConfig {
                mode: match escalation.mode {
                    symbiotic_agents::builtin_tools::PlanTaskEscalationPolicy::NotifyOperator => {
                        symbiotic_control_plane::types::GoalTaskEscalationPolicy::NotifyOperator
                    }
                    symbiotic_agents::builtin_tools::PlanTaskEscalationPolicy::RaiseAlert => {
                        symbiotic_control_plane::types::GoalTaskEscalationPolicy::RaiseAlert
                    }
                    symbiotic_agents::builtin_tools::PlanTaskEscalationPolicy::AutoReplan => {
                        symbiotic_control_plane::types::GoalTaskEscalationPolicy::AutoReplan
                    }
                },
                audience: escalation.audience.clone(),
                severity: escalation.severity.map(|severity| match severity {
                    symbiotic_agents::builtin_tools::PlanTaskEscalationSeverity::Normal => {
                        symbiotic_control_plane::types::GoalTaskEscalationSeverity::Normal
                    }
                    symbiotic_agents::builtin_tools::PlanTaskEscalationSeverity::High => {
                        symbiotic_control_plane::types::GoalTaskEscalationSeverity::High
                    }
                    symbiotic_agents::builtin_tools::PlanTaskEscalationSeverity::Urgent => {
                        symbiotic_control_plane::types::GoalTaskEscalationSeverity::Urgent
                    }
                    symbiotic_agents::builtin_tools::PlanTaskEscalationSeverity::Critical => {
                        symbiotic_control_plane::types::GoalTaskEscalationSeverity::Critical
                    }
                }),
                on_enter_blocked: escalation.on_enter_blocked,
                after_secs: escalation.after_secs,
                max_count: escalation.max_count,
                cooldown_secs: escalation.cooldown_secs,
            }
        }),
        timing: step.policy.timing.as_ref().map(|timing| {
            symbiotic_control_plane::types::GoalTaskTimingConfig {
                timezone: timing.timezone.clone(),
                lateness_basis: timing.lateness_basis.map(|value| match value {
                    symbiotic_agents::builtin_tools::PlanTaskLatenessBasis::WallClock => {
                        symbiotic_control_plane::types::GoalTaskLatenessBasis::WallClock
                    }
                    symbiotic_agents::builtin_tools::PlanTaskLatenessBasis::DeliveryWindowElapsed => {
                        symbiotic_control_plane::types::GoalTaskLatenessBasis::DeliveryWindowElapsed
                    }
                }),
                delivery_window: timing.delivery_window.as_ref().map(|window| {
                    symbiotic_control_plane::types::GoalTaskDeliveryWindowConfig {
                        mode: match window.mode {
                            symbiotic_agents::builtin_tools::PlanTaskDeliveryWindowMode::Anytime => {
                                symbiotic_control_plane::types::GoalTaskDeliveryWindowMode::Anytime
                            }
                            symbiotic_agents::builtin_tools::PlanTaskDeliveryWindowMode::OutsideQuietHours => {
                                symbiotic_control_plane::types::GoalTaskDeliveryWindowMode::OutsideQuietHours
                            }
                            symbiotic_agents::builtin_tools::PlanTaskDeliveryWindowMode::WorkingHours => {
                                symbiotic_control_plane::types::GoalTaskDeliveryWindowMode::WorkingHours
                            }
                            symbiotic_agents::builtin_tools::PlanTaskDeliveryWindowMode::Custom => {
                                symbiotic_control_plane::types::GoalTaskDeliveryWindowMode::Custom
                            }
                        },
                        quiet_hours: window.quiet_hours.as_ref().map(|quiet_hours| {
                            symbiotic_control_plane::types::GoalTaskQuietHoursWindow {
                                start_local: quiet_hours.start_local.clone(),
                                end_local: quiet_hours.end_local.clone(),
                            }
                        }),
                        working_hours: window.working_hours.as_ref().map(|working_hours| {
                            symbiotic_control_plane::types::GoalTaskWorkingHoursWindow {
                                weekdays: working_hours
                                    .weekdays
                                    .iter()
                                    .copied()
                                    .map(|weekday| match weekday {
                                        symbiotic_agents::builtin_tools::PlanTaskWeekday::Mon => {
                                            symbiotic_control_plane::types::GoalTaskWeekday::Mon
                                        }
                                        symbiotic_agents::builtin_tools::PlanTaskWeekday::Tue => {
                                            symbiotic_control_plane::types::GoalTaskWeekday::Tue
                                        }
                                        symbiotic_agents::builtin_tools::PlanTaskWeekday::Wed => {
                                            symbiotic_control_plane::types::GoalTaskWeekday::Wed
                                        }
                                        symbiotic_agents::builtin_tools::PlanTaskWeekday::Thu => {
                                            symbiotic_control_plane::types::GoalTaskWeekday::Thu
                                        }
                                        symbiotic_agents::builtin_tools::PlanTaskWeekday::Fri => {
                                            symbiotic_control_plane::types::GoalTaskWeekday::Fri
                                        }
                                        symbiotic_agents::builtin_tools::PlanTaskWeekday::Sat => {
                                            symbiotic_control_plane::types::GoalTaskWeekday::Sat
                                        }
                                        symbiotic_agents::builtin_tools::PlanTaskWeekday::Sun => {
                                            symbiotic_control_plane::types::GoalTaskWeekday::Sun
                                        }
                                    })
                                    .collect(),
                                start_local: working_hours.start_local.clone(),
                                end_local: working_hours.end_local.clone(),
                            }
                        }),
                    }
                }),
            }
        }),
    }
}

fn declared_context_detail(
    context: &symbiotic_agents::builtin_tools::PlanDeclaredContext,
) -> String {
    let mut parts = Vec::new();
    if let Some(review_target) = context.review_target.as_deref() {
        parts.push(format!("review target: {review_target}"));
    }
    if let Some(waiting_for) = context.waiting_for.as_deref() {
        parts.push(format!("waiting for: {waiting_for}"));
    }
    if let Some(coordination_target) = context.coordination_target.as_deref() {
        parts.push(format!("coordination target: {coordination_target}"));
    }
    if let Some(external_dependency) = context.external_dependency.as_deref() {
        parts.push(format!("external dependency: {external_dependency}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

pub(crate) fn planned_execution_tasks_from_workflow(
    workflow: &symbiotic_workflows::Workflow,
) -> Vec<crate::goal_management::PlannedTaskRecord> {
    let step_ids: Vec<String> = workflow
        .steps
        .iter()
        .filter(|step| step.step_type == "agent.execute")
        .map(|step| step.id.clone())
        .collect();

    workflow
        .steps
        .iter()
        .filter(|step| step.step_type == "agent.execute")
        .enumerate()
        .map(|(index, step)| {
            let title = step
                .config
                .get("description")
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| match step.agent_role.as_deref() {
                    Some("reviewer") => "Review generated work".to_string(),
                    Some(role) => format!("Execute planned {role} step"),
                    None => format!("Execute {}", step.id),
                });
            let summary = match step.agent_role.as_deref() {
                Some(role) => format!(
                    "Approved plan step '{}' assigned to role '{}'.",
                    step.id, role
                ),
                None => format!("Approved plan step '{}'.", step.id),
            };
            crate::goal_management::PlannedTaskRecord {
                task_id: step.id.clone(),
                task_slug: step.id.clone(),
                task_kind: match step.agent_role.as_deref() {
                    Some("reviewer") => symbiotic_control_plane::types::GoalTaskKind::Review,
                    _ => symbiotic_control_plane::types::GoalTaskKind::Execution,
                },
                task_driver: symbiotic_control_plane::types::GoalTaskDriver::Agent,
                title,
                summary,
                state: symbiotic_control_plane::types::GoalTaskPlanState::Active,
                execution_status: symbiotic_control_plane::types::GoalTaskStatus::Planned,
                role: step.agent_role.clone(),
                depends_on: if index == 0 {
                    Vec::new()
                } else {
                    vec![step_ids[index - 1].clone()]
                },
                questionnaire_context: Vec::new(),
                owner_hint: step
                    .agent_role
                    .as_deref()
                    .map(|role| format!("role:{role}")),
                declared_context: symbiotic_control_plane::types::GoalTaskDeclaredContext::default(
                ),
                policy: symbiotic_control_plane::types::GoalTaskPolicy::default(),
                retry_count: 0,
                reopen_count: 0,
                last_status_change_at: None,
                plan_version: 1,
                superseded_by: Vec::new(),
                derived_from: Vec::new(),
                replaces: Vec::new(),
            }
        })
        .collect()
}

pub(crate) fn planned_execution_tasks_from_plan_json(
    template_name: &str,
    plan_json: &str,
) -> Option<Vec<crate::goal_management::PlannedTaskRecord>> {
    let plan =
        serde_json::from_str::<symbiotic_agents::builtin_tools::ProposedPlan>(plan_json).ok()?;
    if plan.steps.is_empty() {
        return None;
    }
    let _workflow = build_plan_driven_workflow(template_name, &plan);
    let mut tasks: Vec<crate::goal_management::PlannedTaskRecord> = plan
        .steps
        .iter()
        .map(|step| crate::goal_management::PlannedTaskRecord {
            task_id: step.task_id.clone(),
            task_slug: step.task_slug.clone(),
            task_kind: match step.task_kind {
                symbiotic_agents::builtin_tools::PlanTaskKind::Execution => {
                    symbiotic_control_plane::types::GoalTaskKind::Execution
                }
                symbiotic_agents::builtin_tools::PlanTaskKind::Review => {
                    symbiotic_control_plane::types::GoalTaskKind::Review
                }
                symbiotic_agents::builtin_tools::PlanTaskKind::Distillation => {
                    symbiotic_control_plane::types::GoalTaskKind::Distillation
                }
                symbiotic_agents::builtin_tools::PlanTaskKind::Coordination => {
                    symbiotic_control_plane::types::GoalTaskKind::Coordination
                }
                symbiotic_agents::builtin_tools::PlanTaskKind::Waiting => {
                    symbiotic_control_plane::types::GoalTaskKind::Waiting
                }
                symbiotic_agents::builtin_tools::PlanTaskKind::Approval => {
                    symbiotic_control_plane::types::GoalTaskKind::Approval
                }
            },
            task_driver: match step.task_driver {
                symbiotic_agents::builtin_tools::PlanTaskDriver::Agent => {
                    symbiotic_control_plane::types::GoalTaskDriver::Agent
                }
                symbiotic_agents::builtin_tools::PlanTaskDriver::Declared => {
                    symbiotic_control_plane::types::GoalTaskDriver::Declared
                }
            },
            title: step.description.clone(),
            summary: planned_task_summary(step),
            state: symbiotic_control_plane::types::GoalTaskPlanState::Active,
            execution_status: symbiotic_control_plane::types::GoalTaskStatus::Planned,
            role: step.role.clone(),
            depends_on: step.depends_on.clone(),
            questionnaire_context: Vec::new(),
            owner_hint: planned_task_owner_hint(step),
            declared_context: planned_task_declared_context(step),
            policy: planned_task_policy(step),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: step.derived_from.clone(),
            replaces: step.replaces.clone(),
        })
        .collect();
    if !plan
        .steps
        .iter()
        .any(|step| step.task_kind == symbiotic_agents::builtin_tools::PlanTaskKind::Review)
        && plan.steps.len() > 1
    {
        let depends_on = plan
            .steps
            .last()
            .map(|step| vec![step.task_id.clone()])
            .unwrap_or_default();
        tasks.push(crate::goal_management::PlannedTaskRecord {
            task_id: "review".to_string(),
            task_slug: "review".to_string(),
            task_kind: symbiotic_control_plane::types::GoalTaskKind::Review,
            task_driver: symbiotic_control_plane::types::GoalTaskDriver::Agent,
            title: "Review generated work".to_string(),
            summary: "Quality-check the approved execution output before completion.".to_string(),
            state: symbiotic_control_plane::types::GoalTaskPlanState::Active,
            execution_status: symbiotic_control_plane::types::GoalTaskStatus::Planned,
            role: Some("reviewer".to_string()),
            depends_on,
            questionnaire_context: Vec::new(),
            owner_hint: Some("role:reviewer".to_string()),
            declared_context: symbiotic_control_plane::types::GoalTaskDeclaredContext::default(),
            policy: symbiotic_control_plane::types::GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        });
    }
    if tasks.is_empty() {
        None
    } else {
        Some(tasks)
    }
}
