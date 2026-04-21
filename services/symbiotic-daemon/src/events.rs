//! Event types, construction helpers, and utility functions.
//!
//! Contains `EventType`, `DaemonEvent`, `DaemonStatusSnapshot`,
//! `RoutedMatrixEnvelope`, the `simple_hash` helper, and
//! `extract_title_from_markdown`.

use std::fmt;
use symbiotic_core::protocol::{Kind, Status};
use symbiotic_matrix::events::MatrixEventEnvelope;

// ---------------------------------------------------------------------------
// EventType — typed enum replacing the old `event_type: String`
// ---------------------------------------------------------------------------

/// Every distinct event the daemon can emit.
///
/// Replaces the previous `event_type: String` field on `DaemonEvent`.
/// The wire protocol remains integer-based (`Kind` + `Status`); this enum
/// is daemon-internal for compile-time exhaustiveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    // -- Goal lifecycle --
    GoalCreated,
    GoalStarted,
    GoalCompleted,
    GoalFailed,
    GoalCancelled,
    GoalAnswer,
    GoalResult,
    GoalQuestion,
    GoalProgress,

    // -- Goal plan --
    GoalPlanProposed,

    // -- Goal steps --
    GoalStepStarted,
    GoalStepCompleted,
    GoalStepFailed,

    // -- Goal deliberation --
    GoalDeliberationClassifying,
    GoalDeliberationAwaitingApproval,
    GoalDeliberationAutoExecuting,
    GoalDeliberationExecuted,
    GoalDeliberationCouncil,
    GoalDeliberationRejected,
    GoalDeliberationFailed,

    // -- Goal inquisition --
    GoalInquisitionStarted,

    // -- Grouped inquisition (T130 §02) --
    GoalQuestionGroup,
    GoalQuestionGroupRejected,
    /// Fired when a pending `QuestionGroup` blew past its grace period
    /// without auto-accept being possible (§3.3). Lands on the thread so
    /// the operator sees "we couldn't decide for you" (T130 §04).
    GoalQuestionGroupExpired,
    GoalUnblocked,
    GoalSubgoalSpawned,
    SubgoalProgress,
    GoalSubgoalCompleted,
    GoalSubgoalFailed,

    // -- Routing (thread lifecycle) --
    RoutingCreated,
    RoutingArchived,
    RoutingSplit,
    RoutingMoved,

    // -- Thread --
    ThreadPromotionProposed,
    ThreadPromotionDismissed,
    ThreadPromotionAccepted,
    ThreadDistillery,

    // -- Chat / Short task (T108 new) --
    ChatReply,
    TaskResult,

    // -- Memory --
    MemoryStaleness,

    // -- Intake --
    IngestFetch,
    ArchiveReviewEnqueue,
    ArchiveReview,

    // -- Auth --
    AuthIssue,
    AuthRequired,
    AuthStarted,
    AuthCompleted,
    AuthFailed,

    // -- Workflow --
    WorkflowRun,

    // -- Bookmarks --
    BookmarksSync,

    // -- Install --
    InstallNucleus,
    InstallMatrix,
    InstallRecall,
    InstallAlive,
    InstallProvision,
    InstallBootstrap,
    InstallVerify,
    InstallRun,

    // -- Structural proposals (friction detection) --
    StructuralProposal,

    // -- Fallback --
    JobUnknown,
}

impl EventType {
    /// Stable dotted-string representation (used in push payloads, tracing,
    /// and the `a` field for State events on the wire).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GoalCreated => "goal.created",
            Self::GoalStarted => "goal.started",
            Self::GoalCompleted => "goal.completed",
            Self::GoalFailed => "goal.failed",
            Self::GoalCancelled => "goal.cancelled",
            Self::GoalAnswer => "goal.answer",
            Self::GoalResult => "goal.result",
            Self::GoalQuestion => "goal.question",
            Self::GoalProgress => "goal.progress",
            Self::GoalPlanProposed => "goal.plan.proposed",
            Self::GoalStepStarted => "goal.step.started",
            Self::GoalStepCompleted => "goal.step.completed",
            Self::GoalStepFailed => "goal.step.failed",
            Self::GoalDeliberationClassifying => "goal.deliberation.classifying",
            Self::GoalDeliberationAwaitingApproval => "goal.deliberation.awaiting_approval",
            Self::GoalDeliberationAutoExecuting => "goal.deliberation.auto_executing",
            Self::GoalDeliberationExecuted => "goal.deliberation.executed",
            Self::GoalDeliberationCouncil => "goal.deliberation.council",
            Self::GoalDeliberationRejected => "goal.deliberation.rejected",
            Self::GoalDeliberationFailed => "goal.deliberation.failed",
            Self::GoalInquisitionStarted => "goal.inquisition.started",
            Self::GoalQuestionGroup => "goal.question_group",
            Self::GoalQuestionGroupRejected => "goal.question_group.rejected",
            Self::GoalQuestionGroupExpired => "goal.question_group.expired",
            Self::GoalUnblocked => "goal.unblocked",
            Self::GoalSubgoalSpawned => "goal.subgoal.spawned",
            Self::SubgoalProgress => "subgoal.progress",
            Self::GoalSubgoalCompleted => "goal.subgoal.completed",
            Self::GoalSubgoalFailed => "goal.subgoal.failed",
            Self::RoutingCreated => "routing.created",
            Self::RoutingArchived => "routing.archived",
            Self::RoutingSplit => "routing.split",
            Self::RoutingMoved => "routing.moved",
            Self::ThreadPromotionProposed => "thread.promotion.proposed",
            Self::ThreadPromotionDismissed => "thread.promotion.dismissed",
            Self::ThreadPromotionAccepted => "thread.promotion.accepted",
            Self::ThreadDistillery => "thread.distillery",
            Self::ChatReply => "chat.reply",
            Self::TaskResult => "task.result",
            Self::MemoryStaleness => "memory.staleness",
            Self::IngestFetch => "ingest.fetch",
            Self::ArchiveReviewEnqueue => "archive.review.enqueue",
            Self::ArchiveReview => "archive.review",
            Self::AuthIssue => "auth.issue",
            Self::AuthRequired => "auth.required",
            Self::AuthStarted => "auth.started",
            Self::AuthCompleted => "auth.completed",
            Self::AuthFailed => "auth.failed",
            Self::WorkflowRun => "workflow.run",
            Self::BookmarksSync => "bookmarks.sync",
            Self::InstallNucleus => "install.nucleus",
            Self::InstallMatrix => "install.matrix",
            Self::InstallRecall => "install.recall",
            Self::InstallAlive => "install.alive",
            Self::InstallProvision => "install.provision",
            Self::InstallBootstrap => "install.bootstrap",
            Self::InstallVerify => "install.verify",
            Self::InstallRun => "install.run",
            Self::StructuralProposal => "structural.proposal",
            Self::JobUnknown => "job.unknown",
        }
    }

    /// True for any `goal.step.*` variant.
    pub fn is_step(&self) -> bool {
        matches!(
            self,
            Self::GoalStepStarted | Self::GoalStepCompleted | Self::GoalStepFailed
        )
    }

    /// True for any `install.*` variant.
    pub fn is_install(&self) -> bool {
        matches!(
            self,
            Self::InstallNucleus
                | Self::InstallMatrix
                | Self::InstallRecall
                | Self::InstallAlive
                | Self::InstallProvision
                | Self::InstallBootstrap
                | Self::InstallVerify
                | Self::InstallRun
        )
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EventType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "goal.created" => Ok(Self::GoalCreated),
            "goal.started" => Ok(Self::GoalStarted),
            "goal.completed" => Ok(Self::GoalCompleted),
            "goal.failed" => Ok(Self::GoalFailed),
            "goal.cancelled" => Ok(Self::GoalCancelled),
            "goal.answer" => Ok(Self::GoalAnswer),
            "goal.result" => Ok(Self::GoalResult),
            "goal.question" => Ok(Self::GoalQuestion),
            "goal.progress" => Ok(Self::GoalProgress),
            "goal.plan.proposed" => Ok(Self::GoalPlanProposed),
            "goal.step.started" => Ok(Self::GoalStepStarted),
            "goal.step.completed" => Ok(Self::GoalStepCompleted),
            "goal.step.failed" => Ok(Self::GoalStepFailed),
            "goal.deliberation.classifying" => Ok(Self::GoalDeliberationClassifying),
            "goal.deliberation.awaiting_approval" => Ok(Self::GoalDeliberationAwaitingApproval),
            "goal.deliberation.auto_executing" => Ok(Self::GoalDeliberationAutoExecuting),
            "goal.deliberation.executed" => Ok(Self::GoalDeliberationExecuted),
            "goal.deliberation.council" => Ok(Self::GoalDeliberationCouncil),
            "goal.deliberation.rejected" => Ok(Self::GoalDeliberationRejected),
            "goal.deliberation.failed" => Ok(Self::GoalDeliberationFailed),
            "goal.inquisition.started" => Ok(Self::GoalInquisitionStarted),
            "goal.question_group" => Ok(Self::GoalQuestionGroup),
            "goal.question_group.rejected" => Ok(Self::GoalQuestionGroupRejected),
            "goal.question_group.expired" => Ok(Self::GoalQuestionGroupExpired),
            "goal.unblocked" => Ok(Self::GoalUnblocked),
            "goal.subgoal.spawned" => Ok(Self::GoalSubgoalSpawned),
            "subgoal.progress" => Ok(Self::SubgoalProgress),
            "goal.subgoal.completed" => Ok(Self::GoalSubgoalCompleted),
            "goal.subgoal.failed" => Ok(Self::GoalSubgoalFailed),
            "routing.created" => Ok(Self::RoutingCreated),
            "routing.archived" => Ok(Self::RoutingArchived),
            "routing.split" => Ok(Self::RoutingSplit),
            "routing.moved" => Ok(Self::RoutingMoved),
            "thread.promotion.proposed" => Ok(Self::ThreadPromotionProposed),
            "thread.promotion.dismissed" => Ok(Self::ThreadPromotionDismissed),
            "thread.promotion.accepted" => Ok(Self::ThreadPromotionAccepted),
            "thread.distillery" => Ok(Self::ThreadDistillery),
            "chat.reply" => Ok(Self::ChatReply),
            "task.result" => Ok(Self::TaskResult),
            "memory.staleness" => Ok(Self::MemoryStaleness),
            "ingest.fetch" => Ok(Self::IngestFetch),
            "archive.review.enqueue" => Ok(Self::ArchiveReviewEnqueue),
            "archive.review" => Ok(Self::ArchiveReview),
            "auth.issue" => Ok(Self::AuthIssue),
            "auth.required" => Ok(Self::AuthRequired),
            "auth.started" => Ok(Self::AuthStarted),
            "auth.completed" => Ok(Self::AuthCompleted),
            "auth.failed" => Ok(Self::AuthFailed),
            "workflow.run" => Ok(Self::WorkflowRun),
            "bookmarks.sync" => Ok(Self::BookmarksSync),
            "install.nucleus" => Ok(Self::InstallNucleus),
            "install.matrix" => Ok(Self::InstallMatrix),
            "install.recall" => Ok(Self::InstallRecall),
            "install.alive" => Ok(Self::InstallAlive),
            "install.provision" => Ok(Self::InstallProvision),
            "install.bootstrap" => Ok(Self::InstallBootstrap),
            "install.verify" => Ok(Self::InstallVerify),
            "install.run" => Ok(Self::InstallRun),
            "structural.proposal" => Ok(Self::StructuralProposal),
            "job.unknown" => Ok(Self::JobUnknown),
            _ => Err(format!("unknown event type: {s}")),
        }
    }
}

// ---------------------------------------------------------------------------
// DaemonEvent
// ---------------------------------------------------------------------------

/// An event produced by the daemon's job execution loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonEvent {
    pub event_type: EventType,
    pub status: String,
    pub job_id: Option<String>,
    pub detail: String,
    pub goal_room: Option<String>,
    pub goal_template: Option<String>,
    pub goal_run_id: Option<String>,
    /// Unique goal identifier that persists across all events for a single goal.
    /// Used by the app to group events into a virtual "goal room."
    pub goal_id: Option<String>,
    /// The intake `run_id` that correlates all pipeline events for one submission.
    /// Set on `ingest.fetch`, `archive.review.enqueue`, and `archive.review` events
    /// so the Flutter app can group them into a single entry.
    pub intake_run_id: Option<String>,
    /// Source URL of the ingested content (for feed display).
    pub url: Option<String>,
    /// Title extracted from the ingested content (for feed display).
    pub title: Option<String>,
    /// Content sensitivity tag (shareable|restricted|private).
    /// When set, the daemon's Matrix send path uses this to apply
    /// Tier 3 phone-only filtering (redact Private events to placeholders).
    pub sensitivity: Option<String>,
    /// JSON-encoded quick-reply suggestions for `goal.question` events.
    /// When present, the app renders tappable chips below the question.
    pub quick_replies: Option<String>,
    /// Thread context identifier for routing events to the correct thread.
    /// When set, the Matrix send path includes this in the envelope's thread field.
    pub thread_id: Option<String>,
}

impl DaemonEvent {
    /// Extract a `key=value` pair from the space-separated detail string.
    fn extract_detail_kv<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
        detail.split_whitespace().find_map(|part| {
            let (k, v) = part.split_once('=')?;
            if k == key {
                Some(v)
            } else {
                None
            }
        })
    }

    /// Build a v2 Matrix event envelope from this daemon event.
    pub fn to_envelope(&self, ts: u64) -> MatrixEventEnvelope {
        let (kind, status, body) = self.classify();
        let mut envelope = if matches!(
            self.event_type,
            EventType::AuthRequired
                | EventType::AuthStarted
                | EventType::AuthCompleted
                | EventType::AuthFailed
        ) {
            MatrixEventEnvelope::state(self.event_type.as_str(), ts, &body)
                .with_detail_field("worker_status", self.status.as_str())
        } else {
            MatrixEventEnvelope::new(kind, status, ts, &body)
        };

        if let Some(thread_id) = self.thread_id.as_ref().or(self.goal_id.as_ref()) {
            envelope = envelope.with_thread(thread_id);
        }
        if let Some(ref quick_replies) = self.quick_replies {
            if let Ok(choices) = serde_json::from_str::<Vec<String>>(quick_replies) {
                envelope = envelope.with_choices(choices);
            }
        }
        if let Some(ref sensitivity) = self.sensitivity {
            envelope = envelope.with_sensitivity(sensitivity);
        }
        // Add detail fields
        let mut detail = serde_json::Map::new();
        if let Some(ref goal_id) = self.goal_id {
            detail.insert("goal_id".to_string(), serde_json::json!(goal_id));
        }
        if let Some(ref template) = self.goal_template {
            detail.insert("template".to_string(), serde_json::json!(template));
        }
        if !self.detail.trim().is_empty() {
            if self.event_type == EventType::GoalPlanProposed {
                if let Ok(plan) = serde_json::from_str::<serde_json::Value>(&self.detail) {
                    detail.insert("plan".to_string(), plan);
                }
            }
            // T130 §04a — batched Inquisitor: ship the full QuestionGroup
            // JSON under `group`, and include answers maps for unblocked /
            // expired outcomes so the app can reconcile locally without
            // another round-trip.
            if matches!(
                self.event_type,
                EventType::GoalQuestionGroup
                    | EventType::GoalUnblocked
                    | EventType::GoalQuestionGroupExpired
                    | EventType::GoalQuestionGroupRejected
            ) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&self.detail) {
                    let key = match self.event_type {
                        EventType::GoalQuestionGroup => "group",
                        EventType::GoalUnblocked => "unblock",
                        EventType::GoalQuestionGroupExpired => "expired",
                        EventType::GoalQuestionGroupRejected => "rejected",
                        _ => unreachable!(),
                    };
                    detail.insert(key.to_string(), value);
                }
            }
            if matches!(
                self.event_type,
                EventType::AuthRequired
                    | EventType::AuthStarted
                    | EventType::AuthCompleted
                    | EventType::AuthFailed
            ) {
                if let Ok(auth_detail) = serde_json::from_str::<serde_json::Value>(&self.detail) {
                    if let Some(map) = auth_detail.as_object() {
                        for (key, value) in map {
                            detail.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
            if self.event_type.is_step() {
                if let Some(step) = Self::extract_detail_kv(&self.detail, "step") {
                    detail.insert("step".to_string(), serde_json::json!(step));
                }
                if let Some(index) = Self::extract_detail_kv(&self.detail, "index") {
                    if let Ok(i) = index.parse::<u32>() {
                        detail.insert("i".to_string(), serde_json::json!(i));
                    }
                }
                if let Some(total) = Self::extract_detail_kv(&self.detail, "total") {
                    if let Ok(n) = total.parse::<u32>() {
                        detail.insert("n".to_string(), serde_json::json!(n));
                    }
                }
                if self.event_type == EventType::GoalStepFailed {
                    if let Some(error) = Self::extract_detail_kv(&self.detail, "error") {
                        detail.insert("error".to_string(), serde_json::json!(error));
                    }
                }
            }
        }
        if !detail.is_empty() {
            for (k, v) in detail {
                envelope = envelope.with_detail_field(&k, v);
            }
        }
        envelope
    }

    /// Map event types to v2 Kind + Status + human-readable body.
    pub(crate) fn classify(&self) -> (Kind, Status, String) {
        let template = self.goal_template.as_deref().unwrap_or("unknown");
        match self.event_type {
            EventType::GoalQuestion => (Kind::Question, Status::Awaiting, self.detail.clone()),
            EventType::GoalPlanProposed => {
                let body = match serde_json::from_str::<serde_json::Value>(&self.detail) {
                    Ok(plan) => {
                        let summary = plan
                            .get("summary")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(no summary)");
                        let confidence = plan
                            .get("confidence")
                            .and_then(|v| v.as_f64())
                            .map(|c| format!("{:.0}%", c * 100.0))
                            .unwrap_or_else(|| "unknown".to_string());
                        let step_count = plan
                            .get("steps")
                            .and_then(|v| v.as_array())
                            .map(|a| a.len())
                            .unwrap_or(0);
                        format!(
                            "Plan proposed: {summary} ({confidence} confidence, {step_count} step{})",
                            if step_count == 1 { "" } else { "s" }
                        )
                    }
                    Err(_) => format!("Plan proposed for workflow `{template}`"),
                };
                (Kind::Question, Status::Awaiting, body)
            }
            EventType::GoalResult => (Kind::Message, Status::Success, self.detail.clone()),
            EventType::AuthRequired => (
                Kind::Notification,
                Status::Awaiting,
                auth_body_from_detail(&self.detail, "Approval required"),
            ),
            EventType::AuthStarted => (
                Kind::Notification,
                Status::Working,
                auth_body_from_detail(&self.detail, "Authentication started"),
            ),
            EventType::AuthCompleted => (
                Kind::Notification,
                Status::Success,
                auth_body_from_detail(&self.detail, "Authentication completed"),
            ),
            EventType::AuthFailed => (
                Kind::Notification,
                Status::Fail,
                auth_body_from_detail(&self.detail, "Authentication failed"),
            ),
            EventType::GoalStepStarted => {
                let body = self.format_step_body_v2();
                (Kind::Message, Status::Working, body)
            }
            EventType::GoalStepCompleted => {
                let body = self.format_step_body_v2();
                (Kind::Message, Status::Success, body)
            }
            EventType::GoalStepFailed => {
                let body = self.format_step_body_v2();
                (Kind::Message, Status::Fail, body)
            }
            EventType::GoalCompleted => (
                Kind::Message,
                Status::Success,
                format!("Goal workflow `{template}` completed"),
            ),
            EventType::GoalFailed => (
                Kind::Message,
                Status::Fail,
                format!("Goal workflow `{template}` failed"),
            ),
            EventType::GoalCancelled => (
                Kind::Message,
                Status::Success,
                format!("Goal workflow `{template}` cancelled"),
            ),
            EventType::GoalStarted => (
                Kind::Message,
                Status::Working,
                format!("Goal started (`{template}`)"),
            ),
            EventType::GoalDeliberationClassifying => (
                Kind::Message,
                Status::Working,
                "Classifying goal...".to_string(),
            ),
            EventType::GoalDeliberationAwaitingApproval => (
                Kind::Message,
                Status::Awaiting,
                "Awaiting approval".to_string(),
            ),
            EventType::GoalDeliberationAutoExecuting => {
                let desc = Self::extract_detail_kv(&self.detail, "description").unwrap_or("goal");
                (
                    Kind::Message,
                    Status::Working,
                    format!("Auto-executing: {desc}"),
                )
            }
            EventType::GoalInquisitionStarted => (
                Kind::Message,
                Status::Working,
                "Analyzing goal...".to_string(),
            ),
            EventType::GoalCreated => {
                let title = self.title.as_deref().unwrap_or("new goal");
                (
                    Kind::Message,
                    Status::Working,
                    format!("Goal created: {title}"),
                )
            }
            EventType::GoalAnswer => (Kind::Message, Status::Accepted, self.detail.clone()),

            // T130 §04a — batched Inquisitor surfaces.
            EventType::GoalQuestionGroup => {
                // Body is a short human-readable hint; the full group JSON
                // lives in the detail object under `group` so the app can
                // render the annotated questions without re-parsing body.
                let summary = serde_json::from_str::<serde_json::Value>(&self.detail)
                    .ok()
                    .and_then(|v| {
                        v.get("questions")
                            .and_then(|q| q.as_array())
                            .map(|arr| arr.len())
                    })
                    .map(|n| format!("{n} clarification{}", if n == 1 { "" } else { "s" }))
                    .unwrap_or_else(|| "clarifying questions".to_string());
                (
                    Kind::Question,
                    Status::Awaiting,
                    format!("Needs input: {summary}"),
                )
            }
            EventType::GoalUnblocked => {
                (Kind::Message, Status::Success, "Goal unblocked".to_string())
            }
            EventType::GoalQuestionGroupExpired => (
                Kind::Message,
                Status::Fail,
                "Question group expired without resolution".to_string(),
            ),
            EventType::GoalQuestionGroupRejected => (
                Kind::Message,
                Status::Fail,
                "Question group rejected".to_string(),
            ),

            // Deliberation pipeline outcomes
            EventType::GoalDeliberationExecuted => {
                let plan = Self::extract_detail_kv(&self.detail, "plan").unwrap_or("plan");
                let passed = Self::extract_detail_kv(&self.detail, "passed")
                    .map(|v| v == "true")
                    .unwrap_or(false);
                if passed {
                    (
                        Kind::Message,
                        Status::Success,
                        format!("Plan executed: {plan}"),
                    )
                } else {
                    (Kind::Message, Status::Fail, format!("Plan failed: {plan}"))
                }
            }
            EventType::GoalDeliberationCouncil => (
                Kind::Message,
                Status::Working,
                "Deliberating with planning council...".to_string(),
            ),
            EventType::GoalDeliberationRejected => (
                Kind::Message,
                Status::Fail,
                format!("Goal rejected: {}", self.detail),
            ),
            EventType::GoalDeliberationFailed => (
                Kind::Message,
                Status::Fail,
                format!("Goal failed: {}", self.detail),
            ),

            // Chat / Short task (T108)
            EventType::ChatReply => (Kind::Message, Status::Success, self.detail.clone()),
            EventType::TaskResult => (Kind::Message, Status::Success, self.detail.clone()),

            // Staleness warnings surfaced after distillery extraction
            EventType::MemoryStaleness => {
                (Kind::Notification, Status::Success, self.detail.clone())
            }

            // Auto-promotion events (thread -> goal promotion flow)
            EventType::ThreadPromotionProposed => {
                let title = self.title.as_deref().unwrap_or("goal");
                (
                    Kind::Question,
                    Status::Awaiting,
                    format!("Promote thread to goal: {title}"),
                )
            }
            EventType::ThreadPromotionDismissed => (
                Kind::State,
                Status::Success,
                "promotion dismissed".to_string(),
            ),
            EventType::ThreadPromotionAccepted => {
                (Kind::State, Status::Success, "promoted to goal".to_string())
            }

            // Structural proposals from friction detection
            EventType::StructuralProposal => {
                (Kind::Notification, Status::Success, self.detail.clone())
            }

            // Internal housekeeping -- hidden from user (Kind::State)
            EventType::RoutingCreated
            | EventType::RoutingArchived
            | EventType::RoutingSplit
            | EventType::RoutingMoved => (Kind::State, Status::Success, self.detail.clone()),
            EventType::ArchiveReviewEnqueue | EventType::ArchiveReview => {
                (Kind::State, Status::Success, self.detail.clone())
            }

            // Catch-all for events without specific classification
            // (IngestFetch, WorkflowRun, BookmarksSync, Auth*, Install*, etc.)
            _ => {
                if self.status == "completed" {
                    (
                        Kind::Message,
                        Status::Success,
                        format!("Goal workflow `{template}` completed"),
                    )
                } else {
                    (
                        Kind::Message,
                        Status::Fail,
                        format!("Goal workflow `{template}` failed"),
                    )
                }
            }
        }
    }

    fn format_step_body_v2(&self) -> String {
        let step_name = Self::extract_detail_kv(&self.detail, "step").unwrap_or("unknown");
        let index = Self::extract_detail_kv(&self.detail, "index");
        let total = Self::extract_detail_kv(&self.detail, "total");
        let progress = match (index, total) {
            (Some(i), Some(t)) => format!(" ({i}/{t})"),
            _ => String::new(),
        };
        match self.event_type {
            EventType::GoalStepStarted => format!("Running step: {step_name}{progress}"),
            EventType::GoalStepCompleted => format!("Step completed: {step_name}{progress}"),
            EventType::GoalStepFailed => {
                let error =
                    Self::extract_detail_kv(&self.detail, "error").unwrap_or("unknown error");
                format!("Step failed: {step_name}{progress} \u{2014} {error}")
            }
            _ => self.event_type.to_string(),
        }
    }
}

fn auth_body_from_detail(detail: &str, fallback: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(detail) else {
        return fallback.to_string();
    };
    let target = value.get("target").and_then(|value| value.as_str());
    let purpose = value.get("purpose").and_then(|value| value.as_str());
    match (target, purpose) {
        (Some(target), Some(purpose)) => format!("{fallback}: {target} ({purpose})"),
        (Some(target), None) => format!("{fallback}: {target}"),
        _ => fallback.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a minimal goal DaemonEvent for testing.
    fn goal_event(event_type: EventType, status: &str, detail: &str) -> DaemonEvent {
        DaemonEvent {
            event_type,
            status: status.to_string(),
            job_id: None,
            detail: detail.to_string(),
            goal_room: Some("#test-goal".to_string()),
            goal_template: Some("deliberation".to_string()),
            goal_run_id: None,
            goal_id: Some("goal-123".to_string()),
            intake_run_id: None,
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        }
    }

    #[test]
    fn classify_goal_question() {
        let event = goal_event(EventType::GoalQuestion, "awaiting_input", "What color?");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Question);
        assert_eq!(status, Status::Awaiting);
        assert_eq!(body, "What color?");
    }

    #[test]
    fn classify_goal_result() {
        let event = goal_event(EventType::GoalResult, "completed", "The answer is 42");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Success);
        assert_eq!(body, "The answer is 42");
    }

    #[test]
    fn classify_step_started() {
        let event = goal_event(
            EventType::GoalStepStarted,
            "running",
            "step=classify type=agent index=1 total=3",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Working);
        assert!(body.contains("classify"), "body={body}");
        assert!(body.starts_with("Running step:"), "body={body}");
    }

    #[test]
    fn classify_step_completed() {
        let event = goal_event(
            EventType::GoalStepCompleted,
            "completed",
            "step=execute type=agent index=2 total=3",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Success);
        assert!(body.starts_with("Step completed:"), "body={body}");
    }

    #[test]
    fn classify_step_failed() {
        let event = goal_event(
            EventType::GoalStepFailed,
            "failed",
            "step=classify index=1 total=3 error=timeout",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Fail);
        assert!(body.contains("timeout"), "body={body}");
    }

    #[test]
    fn classify_plan_proposed() {
        let json = r#"{"summary":"Do the thing","confidence":0.9,"steps":[{"id":"s1"}]}"#;
        let event = goal_event(EventType::GoalPlanProposed, "awaiting_approval", json);
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Question);
        assert_eq!(status, Status::Awaiting);
        assert!(body.contains("Do the thing"), "body={body}");
        assert!(body.contains("90%"), "body={body}");
    }

    #[test]
    fn classify_goal_started() {
        let event = goal_event(EventType::GoalStarted, "running", "");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Working);
        assert!(body.contains("deliberation"), "body={body}");
    }

    #[test]
    fn classify_goal_completed() {
        let event = goal_event(EventType::GoalCompleted, "completed", "");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Success);
        assert!(body.contains("completed"), "body={body}");
    }

    #[test]
    fn classify_goal_failed() {
        let event = goal_event(EventType::GoalFailed, "failed", "");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Fail);
        assert!(body.contains("failed"), "body={body}");
    }

    #[test]
    fn classify_goal_cancelled() {
        let event = goal_event(EventType::GoalCancelled, "completed", "");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Success);
        assert!(body.contains("cancelled"), "body={body}");
    }

    #[test]
    fn classify_deliberation_classifying() {
        let event = goal_event(EventType::GoalDeliberationClassifying, "running", "");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Working);
        assert!(body.contains("Classifying"), "body={body}");
    }

    #[test]
    fn classify_deliberation_awaiting_approval() {
        let event = goal_event(
            EventType::GoalDeliberationAwaitingApproval,
            "awaiting_approval",
            "",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Awaiting);
        assert!(body.contains("approval"), "body={body}");
    }

    #[test]
    fn classify_deliberation_auto_executing() {
        let event = goal_event(
            EventType::GoalDeliberationAutoExecuting,
            "running",
            "description=test goal",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Working);
        assert!(body.contains("Auto-executing"), "body={body}");
    }

    #[test]
    fn classify_goal_created() {
        let mut event = goal_event(EventType::GoalCreated, "running", "");
        event.title = Some("Build a rocket".to_string());
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Working);
        assert!(body.contains("Build a rocket"), "body={body}");
    }

    #[test]
    fn classify_goal_answer() {
        let event = goal_event(EventType::GoalAnswer, "completed", "Blue");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Accepted);
        assert_eq!(body, "Blue");
    }

    #[test]
    fn classify_inquisition_started() {
        let event = goal_event(EventType::GoalInquisitionStarted, "running", "");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Working);
        assert!(body.contains("Analyzing"), "body={body}");
    }

    #[test]
    fn classify_deliberation_executed_passed() {
        let event = goal_event(
            EventType::GoalDeliberationExecuted,
            "completed",
            "plan=quick_search passed=true",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Success);
        assert!(body.contains("quick_search"), "body={body}");
    }

    #[test]
    fn classify_deliberation_executed_failed() {
        let event = goal_event(
            EventType::GoalDeliberationExecuted,
            "failed",
            "plan=research passed=false",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Fail);
        assert!(body.contains("research"), "body={body}");
    }

    #[test]
    fn classify_deliberation_council() {
        let event = goal_event(
            EventType::GoalDeliberationCouncil,
            "deliberating",
            "council_session=abc123",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Working);
        assert!(body.contains("council"), "body={body}");
    }

    #[test]
    fn classify_deliberation_rejected() {
        let event = goal_event(
            EventType::GoalDeliberationRejected,
            "rejected",
            "too ambiguous",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Fail);
        assert!(body.contains("too ambiguous"), "body={body}");
    }

    #[test]
    fn classify_deliberation_failed() {
        let event = goal_event(
            EventType::GoalDeliberationFailed,
            "failed",
            "LLM provider error",
        );
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Fail);
        assert!(body.contains("LLM provider error"), "body={body}");
    }

    #[test]
    fn classify_chat_reply() {
        let event = goal_event(EventType::ChatReply, "completed", "The capital is Paris");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Success);
        assert_eq!(body, "The capital is Paris");
    }

    #[test]
    fn classify_task_result() {
        let event = goal_event(EventType::TaskResult, "completed", "Summary: ...");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Success);
        assert_eq!(body, "Summary: ...");
    }

    #[test]
    fn classify_routing_events_are_state() {
        for event_type in &[
            EventType::RoutingCreated,
            EventType::RoutingArchived,
            EventType::RoutingSplit,
            EventType::RoutingMoved,
        ] {
            let event = goal_event(*event_type, "completed", "thread=t1");
            let (kind, status, _body) = event.classify();
            assert_eq!(
                kind,
                Kind::State,
                "routing event {event_type} should be State"
            );
            assert_eq!(status, Status::Success);
        }
    }

    #[test]
    fn classify_archive_review_events_are_state() {
        for event_type in &[EventType::ArchiveReviewEnqueue, EventType::ArchiveReview] {
            let event = goal_event(*event_type, "completed", "record=r1");
            let (kind, status, _body) = event.classify();
            assert_eq!(
                kind,
                Kind::State,
                "archive event {event_type} should be State"
            );
            assert_eq!(status, Status::Success);
        }
    }

    #[test]
    fn classify_catchall_completed() {
        // GoalProgress falls through to the catch-all branch
        let event = goal_event(EventType::GoalProgress, "completed", "");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Success);
        assert!(body.contains("completed"), "body={body}");
    }

    #[test]
    fn classify_catchall_failed() {
        // IngestFetch with failed status falls through to the catch-all branch
        let event = goal_event(EventType::IngestFetch, "failed", "");
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Message);
        assert_eq!(status, Status::Fail);
        assert!(body.contains("failed"), "body={body}");
    }

    #[test]
    fn to_envelope_basic() {
        let event = goal_event(EventType::GoalResult, "completed", "The answer is 42");
        let envelope = event.to_envelope(1000);
        assert_eq!(envelope.msgtype, "sym.e");
        assert_eq!(envelope.body, "The answer is 42");
        assert_eq!(envelope.sym.k, Kind::Message);
        assert_eq!(envelope.sym.s, Some(Status::Success));
        assert_eq!(envelope.sym.ts, 1000);
        // Without an explicit thread_id, goal_id still acts as a legacy fallback.
        assert_eq!(envelope.sym.t.as_deref(), Some("goal-123"));
        // Should have goal_id and template in detail
        let d = envelope.sym.d.unwrap();
        assert_eq!(d.get("goal_id").unwrap().as_str(), Some("goal-123"));
        assert_eq!(d.get("template").unwrap().as_str(), Some("deliberation"));
    }

    #[test]
    fn to_envelope_question_with_choices() {
        let mut event = goal_event(EventType::GoalQuestion, "awaiting_input", "Pick a color");
        event.quick_replies = Some(r#"["Red","Blue","Green"]"#.to_string());
        let envelope = event.to_envelope(1000);
        assert_eq!(envelope.sym.k, Kind::Question);
        assert_eq!(envelope.sym.s, Some(Status::Awaiting));
        assert_eq!(
            envelope.sym.ch.as_deref(),
            Some(&["Red".to_string(), "Blue".to_string(), "Green".to_string()][..])
        );
    }

    #[test]
    fn to_envelope_with_sensitivity() {
        let mut event = goal_event(EventType::GoalResult, "completed", "secret stuff");
        event.sensitivity = Some("private".to_string());
        let envelope = event.to_envelope(1000);
        assert_eq!(envelope.sensitivity(), Some("private"));
    }

    #[test]
    fn to_envelope_step_has_structured_detail() {
        let event = goal_event(
            EventType::GoalStepStarted,
            "running",
            "step=classify type=agent index=1 total=3",
        );
        let envelope = event.to_envelope(1000);
        let d = envelope.sym.d.unwrap();
        assert_eq!(d.get("step").unwrap().as_str(), Some("classify"));
        assert_eq!(d.get("i").unwrap().as_u64(), Some(1));
        assert_eq!(d.get("n").unwrap().as_u64(), Some(3));
    }

    #[test]
    fn to_envelope_plan_proposed_has_plan_in_detail() {
        let json = r#"{"summary":"Do the thing","confidence":0.9,"steps":[{"id":"s1"}]}"#;
        let event = goal_event(EventType::GoalPlanProposed, "awaiting_approval", json);
        let envelope = event.to_envelope(1000);
        let d = envelope.sym.d.unwrap();
        assert!(d.get("plan").is_some(), "plan should be in detail");
        assert_eq!(
            d.get("plan").unwrap().get("summary").unwrap().as_str(),
            Some("Do the thing")
        );
    }

    #[test]
    fn to_envelope_validates() {
        let event = goal_event(EventType::GoalResult, "completed", "done");
        let envelope = event.to_envelope(1000);
        assert!(envelope.validate().is_ok());
    }

    #[test]
    fn to_envelope_uses_thread_id_fallback() {
        let mut event = goal_event(EventType::GoalResult, "completed", "done");
        event.goal_id = None;
        event.thread_id = Some("thread-456".to_string());
        let envelope = event.to_envelope(1000);
        assert_eq!(envelope.sym.t.as_deref(), Some("thread-456"));
    }

    #[test]
    fn to_envelope_prefers_explicit_thread_id_over_goal_id() {
        let mut event = goal_event(EventType::GoalResult, "completed", "done");
        event.thread_id = Some("thread-456".to_string());
        let envelope = event.to_envelope(1000);
        assert_eq!(envelope.sym.t.as_deref(), Some("thread-456"));
    }

    #[test]
    fn to_envelope_no_thread_when_neither_set() {
        let mut event = goal_event(EventType::GoalResult, "completed", "done");
        event.goal_id = None;
        event.thread_id = None;
        let envelope = event.to_envelope(1000);
        assert!(envelope.sym.t.is_none());
    }

    #[test]
    fn classify_memory_staleness() {
        let mut event = goal_event(
            EventType::MemoryStaleness,
            "completed",
            "3 stale/suspect fact(s) detected in thread thread-test",
        );
        event.goal_id = None;
        event.thread_id = Some("thread-test".to_string());
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Notification);
        assert_eq!(status, Status::Success);
        assert!(body.contains("stale/suspect"), "body={body}");
    }

    #[test]
    fn classify_promotion_proposed() {
        let mut event = goal_event(
            EventType::ThreadPromotionProposed,
            "awaiting",
            "Goal-worthy thread detected",
        );
        event.goal_id = None;
        event.thread_id = Some("thread-promo".to_string());
        event.title = Some("Build a website".to_string());
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Question);
        assert_eq!(status, Status::Awaiting);
        assert!(body.contains("Build a website"), "body={body}");
    }

    #[test]
    fn classify_promotion_dismissed() {
        let mut event = goal_event(
            EventType::ThreadPromotionDismissed,
            "completed",
            "Promotion dismissed by user",
        );
        event.goal_id = None;
        event.thread_id = Some("thread-promo".to_string());
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::State);
        assert_eq!(status, Status::Success);
        assert!(body.contains("promotion dismissed"), "body={body}");
    }

    #[test]
    fn classify_promotion_accepted() {
        let mut event = goal_event(
            EventType::ThreadPromotionAccepted,
            "completed",
            "Promoted to goal: Build a website",
        );
        event.goal_id = None;
        event.thread_id = Some("thread-promo".to_string());
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::State);
        assert_eq!(status, Status::Success);
        assert!(body.contains("promoted to goal"), "body={body}");
    }

    #[test]
    fn event_type_as_str_roundtrip() {
        assert_eq!(EventType::GoalQuestion.as_str(), "goal.question");
        assert_eq!(EventType::RoutingCreated.as_str(), "routing.created");
        assert_eq!(EventType::ChatReply.as_str(), "chat.reply");
        assert_eq!(EventType::TaskResult.as_str(), "task.result");
    }

    #[test]
    fn event_type_display() {
        assert_eq!(format!("{}", EventType::IngestFetch), "ingest.fetch");
        assert_eq!(
            format!("{}", EventType::GoalPlanProposed),
            "goal.plan.proposed"
        );
    }

    #[test]
    fn event_type_is_step() {
        assert!(EventType::GoalStepStarted.is_step());
        assert!(EventType::GoalStepCompleted.is_step());
        assert!(EventType::GoalStepFailed.is_step());
        assert!(!EventType::GoalResult.is_step());
    }

    #[test]
    fn event_type_is_install() {
        assert!(EventType::InstallNucleus.is_install());
        assert!(EventType::InstallAlive.is_install());
        assert!(!EventType::GoalResult.is_install());
    }

    #[test]
    fn structural_proposal_serializes_correctly() {
        let et = EventType::StructuralProposal;
        assert_eq!(et.as_str(), "structural.proposal");
        assert_eq!(format!("{et}"), "structural.proposal");
        assert_eq!(
            "structural.proposal".parse::<EventType>().unwrap(),
            EventType::StructuralProposal
        );
    }

    #[test]
    fn grouped_inquisition_event_variants_roundtrip() {
        // Every T130 §07 event variant must round-trip via as_str <-> FromStr
        // + render identically via Display.
        let cases = [
            (EventType::GoalQuestionGroup, "goal.question_group"),
            (
                EventType::GoalQuestionGroupRejected,
                "goal.question_group.rejected",
            ),
            (
                EventType::GoalQuestionGroupExpired,
                "goal.question_group.expired",
            ),
            (EventType::GoalUnblocked, "goal.unblocked"),
            (EventType::GoalSubgoalSpawned, "goal.subgoal.spawned"),
            (EventType::SubgoalProgress, "subgoal.progress"),
            (EventType::GoalSubgoalCompleted, "goal.subgoal.completed"),
            (EventType::GoalSubgoalFailed, "goal.subgoal.failed"),
        ];
        for (variant, wire) in cases {
            assert_eq!(variant.as_str(), wire);
            assert_eq!(format!("{variant}"), wire);
            assert_eq!(wire.parse::<EventType>().unwrap(), variant);
        }
    }

    #[test]
    fn classify_structural_proposal() {
        let event = DaemonEvent {
            event_type: EventType::StructuralProposal,
            status: "completed".to_string(),
            job_id: None,
            detail: "Thread spans 5 distinct topics.\n\nSuggestion: split it.".to_string(),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: Some("thread-mega".to_string()),
        };
        let (kind, status, body) = event.classify();
        assert_eq!(kind, Kind::Notification);
        assert_eq!(status, Status::Success);
        assert!(body.contains("split it"));
    }
}

/// Request to create a Matrix room for a thread. Produced by sync command
/// handlers (e.g. `UxClass::Goal`), consumed by the async pump loop.
#[derive(Debug, Clone)]
pub struct RoomCreationRequest {
    pub thread_slug: String,
    pub thread_title: String,
    pub goal_id: String,
}

/// A point-in-time snapshot of the daemon's queue state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatusSnapshot {
    pub queued: usize,
    pub running: usize,
    pub failed: usize,
    pub done: usize,
    pub dlq: usize,
    pub timestamp: u64,
}

/// A `MatrixEventEnvelope` bound to a specific room.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedMatrixEnvelope {
    pub room_id: String,
    pub envelope: MatrixEventEnvelope,
}

/// FNV-1a-style hash for deterministic ID generation.
pub(crate) fn simple_hash(input: &str) -> u64 {
    let mut acc = 1469598103934665603u64;
    for byte in input.bytes() {
        acc ^= byte as u64;
        acc = acc.wrapping_mul(1099511628211u64);
    }
    acc
}

/// Extract the first meaningful line from markdown content as a title.
/// Strips leading heading markers (`#`) and caps at 120 characters.
pub(crate) fn extract_title_from_markdown(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let title = trimmed.trim_start_matches('#').trim();
        if !title.is_empty() {
            return Some(title.chars().take(120).collect());
        }
    }
    None
}
