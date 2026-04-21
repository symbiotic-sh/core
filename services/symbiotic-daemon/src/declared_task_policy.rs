use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use symbiotic_control_plane::manifest::ManifestParser;
use symbiotic_control_plane::types::{
    AvailabilityRuleManifest, GoalEventManifest, GoalManifest, GoalState as GoalManifestState,
    GoalTaskDeliveryWindowConfig, GoalTaskDeliveryWindowMode, GoalTaskDriver,
    GoalTaskEscalationDefaults, GoalTaskEscalationPolicy, GoalTaskEscalationSeverity, GoalTaskKind,
    GoalTaskLatenessBasis, GoalTaskPlanState, GoalTaskPolicyDefaults,
    GoalTaskPolicyEvaluatorDefaults, GoalTaskQuietHoursWindow, GoalTaskStatus, GoalTaskWeekday,
    GoalTaskWorkingHoursWindow, PreferencesManifest,
};
use symbiotic_core::protocol::{Kind, Status};
use symbiotic_matrix::events::MatrixEventEnvelope;

use crate::availability::{load_availability_rule_by_subject, resolve_delivery_subject};
use crate::events::RoutedMatrixEnvelope;
use crate::goal_management::{
    append_goal_event_archive, declared_task_escalation_detail, default_escalation_severity,
    escalation_audience_label, escalation_policy_label, escalation_severity_label,
    escalation_targets_alerts, infer_declared_task_resume_status,
    load_active_goal_tasks_from_archive, load_goal_events_from_archive,
    sync_goal_task_declared_status, GoalEventMetadata, GoalHierarchyContext, PlannedTaskRecord,
};
use crate::policy_scopes::{load_goal_policy_scopes, load_policy_scope_by_id};
use crate::routing::RoomRole;
use crate::SymbioticDaemon;

const DEFAULT_EVALUATOR_INTERVAL_SECS: u64 = 30;
const DEFAULT_MAX_ACTIONS_PER_TICK: usize = 32;
const POLICY_ACTOR: &str = "nucleus.policy";
const TRIGGER_AFTER_SECS: &str = "after_secs";

#[derive(Debug, Clone)]
pub(crate) struct EffectiveEscalationPolicy {
    pub(crate) mode: GoalTaskEscalationPolicy,
    pub(crate) audience: String,
    pub(crate) severity: GoalTaskEscalationSeverity,
    pub(crate) on_enter_blocked: bool,
    pub(crate) after_secs: Option<u64>,
    pub(crate) max_count: Option<u32>,
    pub(crate) cooldown_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct EffectiveTimingPolicy {
    pub(crate) timezone: String,
    pub(crate) lateness_basis: GoalTaskLatenessBasis,
    pub(crate) delivery_window: GoalTaskDeliveryWindowConfig,
    pub(crate) delivery_subject: String,
}

#[derive(Debug, Clone, Copy)]
struct EvaluatorConfig {
    interval_secs: u64,
    max_actions_per_tick: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct TaskEscalationHistory {
    escalation_count: u32,
    last_escalated_at: Option<i64>,
    last_deferred_at: Option<i64>,
    last_window_opened_at: Option<i64>,
    last_suppressed_at: Option<i64>,
    last_dependencies_satisfied_at: Option<i64>,
    last_replan_requested_at: Option<i64>,
    last_replan_enqueued_at: Option<i64>,
    latest_plan_reconciled_at: Option<i64>,
    blocked_since_from_events: Option<i64>,
}

enum EvaluationDecision {
    Escalate,
    Suppress {
        note: String,
        cooldown_until: Option<i64>,
    },
}

enum DeliveryDecision {
    DeliverNow,
    Deferred { detail: String },
}

impl SymbioticDaemon {
    pub fn process_declared_task_policy_tick(&self, now: u64) -> Result<Vec<RoutedMatrixEnvelope>> {
        let Some(archive_root) = self.config.archive_path.clone() else {
            return Ok(Vec::new());
        };

        let parser = ManifestParser::new();
        let preferences = load_preferences(&parser, &archive_root);
        let config = evaluator_config(preferences.as_ref());

        {
            let mut last_run = self.declared_task_policy_last_run.lock().map_err(|error| {
                anyhow::anyhow!("declared task policy state lock poisoned: {error}")
            })?;
            if let Some(last_run_at) = *last_run {
                if now < last_run_at.saturating_add(config.interval_secs) {
                    return Ok(Vec::new());
                }
            }
            *last_run = Some(now);
        }

        // Discover active goals via the hierarchical project-owned layout
        // (`operations/projects/*/goals/*`). `ManifestParser::parse_all`
        // already walks that tree; the legacy flat `operations/goals/` path
        // is no longer the source of truth for runtime goal discovery.
        let desired_state = parser.parse_all(&archive_root)?;

        let mut routed = Vec::new();
        let mut actions = 0usize;
        let now_i64 = now as i64;

        for goal in desired_state.goals.iter() {
            if !matches!(goal.state, GoalManifestState::Active) {
                continue;
            }
            let goal_id = goal.slug.clone();
            let context = GoalHierarchyContext {
                project_id: &goal.project_id,
                goal_id: &goal_id,
                title: &goal.title,
                summary: first_goal_summary_line(&goal.plan_markdown),
                owner: None,
                thread_id: goal.thread_id.as_deref(),
                observed_at: now_i64,
            };
            let tasks = load_active_goal_tasks_from_archive(&archive_root, &goal_id);
            if tasks.is_empty() {
                continue;
            }
            let events = load_goal_events_from_archive(&archive_root, &goal_id)?;
            let task_statuses = tasks
                .iter()
                .map(|task| (task.task_id.clone(), task.execution_status))
                .collect::<std::collections::HashMap<_, _>>();

            for task in tasks {
                if actions >= config.max_actions_per_tick {
                    return Ok(routed);
                }
                let history = task_escalation_history(&events, &task.task_id);
                if dependencies_satisfied(&task, &task_statuses)
                    && should_emit_dependencies_satisfied(&task, history)
                {
                    append_goal_event_archive(
                        &archive_root,
                        &context,
                        "task_dependencies_satisfied",
                        task.plan_version.max(goal.plan_version),
                        &format!("task={} dependencies satisfied", task.task_id),
                        GoalEventMetadata {
                            task_id: Some(&task.task_id),
                            actor: Some(POLICY_ACTOR),
                            note: Some("All declared dependencies are now complete."),
                            ..GoalEventMetadata::default()
                        },
                    )?;
                    routed.push(self.route_dependencies_satisfied(
                        &goal_id,
                        &task,
                        goal.thread_id.as_deref(),
                        now,
                    ));
                    if should_auto_resume_after_internal_dependencies(&task) {
                        let resume_status =
                            infer_declared_task_resume_status(&events, &task.task_id);
                        let note = "Declared dependencies cleared; task resumed to its last runnable status.";
                        let _ = sync_goal_task_declared_status(
                            &self.management_store,
                            Some(&archive_root),
                            &context,
                            &task,
                            resume_status,
                            Some(POLICY_ACTOR),
                            Some(note),
                        );
                    }
                    actions = actions.saturating_add(1);
                    continue;
                }
                let Some(policy) = effective_escalation_policy(
                    preferences.as_ref(),
                    Some(&archive_root),
                    goal,
                    &task,
                ) else {
                    continue;
                };
                let timing = effective_timing_policy(
                    preferences.as_ref(),
                    Some(&archive_root),
                    goal,
                    &task,
                    Some(policy.audience.as_str()),
                );
                let Some(after_secs) = policy.after_secs else {
                    continue;
                };
                if !matches!(task.task_driver, GoalTaskDriver::Declared)
                    || !matches!(task.state, GoalTaskPlanState::Active)
                    || !matches!(task.execution_status, GoalTaskStatus::Blocked)
                {
                    continue;
                }

                let Some(blocked_since) = task
                    .last_status_change_at
                    .or(history.blocked_since_from_events)
                else {
                    continue;
                };
                let blocked_for = blocked_duration_for_policy(&timing, blocked_since, now_i64);
                if blocked_for < after_secs as i64 {
                    continue;
                }

                match evaluate_escalation_decision(&policy, history, blocked_since, now_i64) {
                    EvaluationDecision::Escalate => {
                        let detail = background_escalation_detail(
                            &task,
                            after_secs,
                            blocked_since,
                            blocked_for,
                        );
                        match evaluate_delivery_decision(&timing, history, blocked_since, now_i64) {
                            DeliveryDecision::DeliverNow => {
                                if deferred_delivery_pending(history, blocked_since) {
                                    append_goal_event_archive(
                                        &archive_root,
                                        &context,
                                        "task_escalation_window_opened",
                                        task.plan_version.max(goal.plan_version),
                                        &format!(
                                            "task={} delivery window opened for deferred escalation",
                                            task.task_id
                                        ),
                                        GoalEventMetadata {
                                            task_id: Some(&task.task_id),
                                            previous_status: Some("blocked"),
                                            next_status: Some("blocked"),
                                            escalation_policy: Some(escalation_policy_label(policy.mode)),
                                            escalation_trigger: Some(TRIGGER_AFTER_SECS),
                                            escalation_audience: Some(policy.audience.as_str()),
                                            escalation_severity: Some(escalation_severity_label(policy.severity)),
                                            actor: Some(POLICY_ACTOR),
                                            note: Some("Deferred escalation became deliverable because the delivery window is now open."),
                                            ..GoalEventMetadata::default()
                                        },
                                    )?;
                                    actions = actions.saturating_add(1);
                                }
                                append_goal_event_archive(
                                    &archive_root,
                                    &context,
                                    "task_escalated",
                                    task.plan_version.max(goal.plan_version),
                                    &format!(
                                        "task={} policy={} detail={}",
                                        task.task_id,
                                        escalation_policy_label(policy.mode),
                                        detail
                                    ),
                                    GoalEventMetadata {
                                        task_id: Some(&task.task_id),
                                        previous_status: Some("blocked"),
                                        next_status: Some("blocked"),
                                        escalation_policy: Some(escalation_policy_label(
                                            policy.mode,
                                        )),
                                        escalation_trigger: Some(TRIGGER_AFTER_SECS),
                                        escalation_audience: Some(policy.audience.as_str()),
                                        escalation_severity: Some(escalation_severity_label(
                                            policy.severity,
                                        )),
                                        escalation_count: Some(
                                            history.escalation_count.saturating_add(1),
                                        ),
                                        cooldown_until: policy
                                            .cooldown_secs
                                            .map(|secs| now_i64.saturating_add(secs as i64)),
                                        actor: Some(POLICY_ACTOR),
                                        note: Some(&detail),
                                        ..GoalEventMetadata::default()
                                    },
                                )?;
                                if matches!(policy.mode, GoalTaskEscalationPolicy::AutoReplan) {
                                    append_replan_event_for_policy(
                                        &archive_root,
                                        &context,
                                        &task,
                                        &policy,
                                        &history,
                                        TRIGGER_AFTER_SECS,
                                        &detail,
                                    )?;
                                }
                                routed.extend(self.routes_for_declared_task_escalation(
                                    &goal_id,
                                    &task,
                                    goal.thread_id.as_deref(),
                                    &policy,
                                    &detail,
                                    TRIGGER_AFTER_SECS,
                                    now,
                                ));
                                actions = actions.saturating_add(1);
                            }
                            DeliveryDecision::Deferred { detail } => {
                                if should_append_deferred_event(history, blocked_since) {
                                    append_goal_event_archive(
                                        &archive_root,
                                        &context,
                                        "task_escalation_deferred",
                                        task.plan_version.max(goal.plan_version),
                                        &format!(
                                            "task={} policy={} deferred",
                                            task.task_id,
                                            escalation_policy_label(policy.mode)
                                        ),
                                        GoalEventMetadata {
                                            task_id: Some(&task.task_id),
                                            previous_status: Some("blocked"),
                                            next_status: Some("blocked"),
                                            escalation_policy: Some(escalation_policy_label(
                                                policy.mode,
                                            )),
                                            escalation_trigger: Some(TRIGGER_AFTER_SECS),
                                            escalation_audience: Some(policy.audience.as_str()),
                                            escalation_severity: Some(escalation_severity_label(
                                                policy.severity,
                                            )),
                                            actor: Some(POLICY_ACTOR),
                                            note: Some(&detail),
                                            ..GoalEventMetadata::default()
                                        },
                                    )?;
                                    if matches!(policy.mode, GoalTaskEscalationPolicy::AutoReplan) {
                                        append_replan_event_for_policy(
                                            &archive_root,
                                            &context,
                                            &task,
                                            &policy,
                                            &history,
                                            TRIGGER_AFTER_SECS,
                                            &detail,
                                        )?;
                                    }
                                    actions = actions.saturating_add(1);
                                }
                            }
                        }
                    }
                    EvaluationDecision::Suppress {
                        note,
                        cooldown_until,
                    } => {
                        if should_append_suppressed_event(history, blocked_since) {
                            append_goal_event_archive(
                                &archive_root,
                                &context,
                                "task_escalation_suppressed",
                                task.plan_version.max(goal.plan_version),
                                &format!(
                                    "task={} policy={} suppressed",
                                    task.task_id,
                                    escalation_policy_label(policy.mode)
                                ),
                                GoalEventMetadata {
                                    task_id: Some(&task.task_id),
                                    previous_status: Some("blocked"),
                                    next_status: Some("blocked"),
                                    escalation_policy: Some(escalation_policy_label(policy.mode)),
                                    escalation_trigger: Some(TRIGGER_AFTER_SECS),
                                    escalation_audience: Some(policy.audience.as_str()),
                                    escalation_severity: Some(escalation_severity_label(
                                        policy.severity,
                                    )),
                                    escalation_count: Some(history.escalation_count),
                                    cooldown_until,
                                    actor: Some(POLICY_ACTOR),
                                    note: Some(&note),
                                    ..GoalEventMetadata::default()
                                },
                            )?;
                            actions = actions.saturating_add(1);
                        }
                    }
                }
            }
        }

        Ok(routed)
    }

    fn routes_for_declared_task_escalation(
        &self,
        goal_id: &str,
        task: &PlannedTaskRecord,
        goal_thread_id: Option<&str>,
        policy: &EffectiveEscalationPolicy,
        detail: &str,
        trigger: &str,
        now: u64,
    ) -> Vec<RoutedMatrixEnvelope> {
        let thread_room = goal_thread_id
            .and_then(|thread_id| self.resolve_thread_room(thread_id))
            .unwrap_or_else(|| self.resolve_room(RoomRole::Goals).to_string());
        let operator_notice = MatrixEventEnvelope::new(
            Kind::Question,
            Status::Awaiting,
            now,
            &format!("Task blocked: {}. {}", task.title, detail),
        )
        .with_detail_field("goal_id", goal_id)
        .with_detail_field("task_id", task.task_id.as_str())
        .with_detail_field("escalation_policy", escalation_policy_label(policy.mode))
        .with_detail_field("escalation_trigger", trigger)
        .with_detail_field("escalation_audience", policy.audience.as_str())
        .with_detail_field(
            "escalation_severity",
            escalation_severity_label(policy.severity),
        )
        .with_detail_field("detail", detail);
        let operator_notice = if let Some(thread_id) = goal_thread_id {
            operator_notice.with_thread(thread_id)
        } else {
            operator_notice
        };
        let mut routed = vec![RoutedMatrixEnvelope {
            room_id: thread_room,
            envelope: operator_notice,
        }];

        if escalation_targets_alerts(policy.mode, &policy.audience, policy.severity) {
            routed.push(RoutedMatrixEnvelope {
                room_id: self.resolve_room(RoomRole::Alerts).to_string(),
                envelope: MatrixEventEnvelope::state(
                    "goal.task.escalated",
                    now,
                    "Declared task escalated to alerts",
                )
                .with_detail_field("goal_id", goal_id)
                .with_detail_field("task_id", task.task_id.as_str())
                .with_detail_field("detail", detail)
                .with_detail_field("escalation_policy", escalation_policy_label(policy.mode))
                .with_detail_field("escalation_trigger", trigger)
                .with_detail_field("escalation_audience", policy.audience.as_str())
                .with_detail_field(
                    "escalation_severity",
                    escalation_severity_label(policy.severity),
                ),
            });
        }

        if matches!(policy.mode, GoalTaskEscalationPolicy::AutoReplan) {
            routed.push(RoutedMatrixEnvelope {
                room_id: self.resolve_room(RoomRole::Goals).to_string(),
                envelope: MatrixEventEnvelope::state(
                    "goal.task.replan.requested",
                    now,
                    "Blocked task requested replanning",
                )
                .with_detail_field("goal_id", goal_id)
                .with_detail_field("task_id", task.task_id.as_str())
                .with_detail_field("detail", detail)
                .with_detail_field("escalation_policy", "auto_replan")
                .with_detail_field("escalation_trigger", trigger)
                .with_detail_field("escalation_audience", policy.audience.as_str())
                .with_detail_field(
                    "escalation_severity",
                    escalation_severity_label(policy.severity),
                ),
            });
        }

        routed
    }

    fn route_dependencies_satisfied(
        &self,
        goal_id: &str,
        task: &PlannedTaskRecord,
        goal_thread_id: Option<&str>,
        now: u64,
    ) -> RoutedMatrixEnvelope {
        let room_id = goal_thread_id
            .and_then(|thread_id| self.resolve_thread_room(thread_id))
            .unwrap_or_else(|| self.resolve_room(RoomRole::Goals).to_string());
        let envelope = MatrixEventEnvelope::state(
            "goal.task.dependencies.satisfied",
            now,
            "Declared task dependencies satisfied",
        )
        .with_detail_field("goal_id", goal_id)
        .with_detail_field("task_id", task.task_id.as_str())
        .with_detail_field("detail", "All declared dependencies are now complete.");
        let envelope = if let Some(thread_id) = goal_thread_id {
            envelope.with_thread(thread_id)
        } else {
            envelope
        };
        RoutedMatrixEnvelope { room_id, envelope }
    }
}

fn load_preferences(
    parser: &ManifestParser,
    archive_root: &std::path::Path,
) -> Option<PreferencesManifest> {
    let path = parser.resolve_preferences_path(archive_root)?;
    parser.parse_preferences(&path).ok()
}

fn evaluator_config(preferences: Option<&PreferencesManifest>) -> EvaluatorConfig {
    let defaults = preferences
        .map(|prefs| &prefs.task_policy_defaults.evaluator)
        .cloned()
        .unwrap_or_default();
    EvaluatorConfig {
        interval_secs: defaults
            .interval_secs
            .unwrap_or(DEFAULT_EVALUATOR_INTERVAL_SECS)
            .max(5),
        max_actions_per_tick: defaults
            .max_actions_per_tick
            .unwrap_or(DEFAULT_MAX_ACTIONS_PER_TICK)
            .max(1),
    }
}

pub(crate) fn effective_escalation_policy(
    preferences: Option<&PreferencesManifest>,
    archive_root: Option<&std::path::Path>,
    goal: &GoalManifest,
    task: &PlannedTaskRecord,
) -> Option<EffectiveEscalationPolicy> {
    if !matches!(task.task_driver, GoalTaskDriver::Declared) {
        return None;
    }

    let mut effective = built_in_defaults(task.task_kind)?;
    if let Some(operator_defaults) = preferences
        .map(|prefs| &prefs.task_policy_defaults.declared_task_defaults)
        .and_then(|defaults| defaults_for_kind(defaults, task.task_kind))
    {
        apply_escalation_defaults(&mut effective, operator_defaults);
    }
    if let Some(archive_root) = archive_root {
        let parser = ManifestParser::new();
        if let Ok(policy_scopes) = load_goal_policy_scopes(&parser, archive_root, goal) {
            for scope in policy_scopes {
                if let Some(scope_defaults) =
                    defaults_for_kind(&scope.task_policy_defaults, task.task_kind)
                {
                    apply_escalation_defaults(&mut effective, scope_defaults);
                }
            }
        }
    }
    if let Some(goal_defaults) = defaults_for_kind(&goal.task_policy_defaults, task.task_kind) {
        apply_escalation_defaults(&mut effective, goal_defaults);
    }
    if let Some(task_policy) = task.policy.escalation.as_ref() {
        effective = EffectiveEscalationPolicy {
            mode: task_policy.mode,
            audience: task_policy
                .audience
                .clone()
                .unwrap_or_else(|| escalation_audience_label(None, task_policy.mode).to_string()),
            severity: task_policy
                .severity
                .unwrap_or_else(|| default_escalation_severity(task_policy.mode)),
            on_enter_blocked: task_policy.on_enter_blocked,
            after_secs: task_policy.after_secs,
            max_count: task_policy.max_count,
            cooldown_secs: task_policy.cooldown_secs,
        };
    } else {
        effective.audience =
            escalation_audience_label(Some(&effective.audience), effective.mode).to_string();
        effective.severity =
            normalize_escalation_severity(Some(effective.severity), effective.mode);
    }
    Some(effective)
}

pub(crate) fn effective_timing_policy(
    preferences: Option<&PreferencesManifest>,
    archive_root: Option<&std::path::Path>,
    goal: &GoalManifest,
    task: &PlannedTaskRecord,
    audience: Option<&str>,
) -> EffectiveTimingPolicy {
    let mut effective = built_in_timing_defaults();
    let parser = ManifestParser::new();
    if let Some(defaults) = preferences.map(|prefs| &prefs.task_policy_defaults.evaluator) {
        apply_timing_defaults(&mut effective, defaults);
    }
    let audience_scope = if let (Some(archive_root), Some(audience)) = (archive_root, audience) {
        load_policy_scope_by_id(&parser, archive_root, audience)
            .ok()
            .flatten()
    } else {
        None
    };
    if let (Some(_archive_root), Some(_audience)) = (archive_root, audience) {
        if let Some(scope) = audience_scope.as_ref() {
            if let Some(defaults) = scope.task_policy_defaults.evaluator.as_ref() {
                apply_timing_defaults(&mut effective, defaults);
            }
        }
    }
    if let Some(archive_root) = archive_root {
        if let Ok(policy_scopes) = load_goal_policy_scopes(&parser, archive_root, goal) {
            for scope in policy_scopes {
                if let Some(defaults) = scope.task_policy_defaults.evaluator.as_ref() {
                    apply_timing_defaults(&mut effective, defaults);
                }
            }
        }
    }
    if let Some(defaults) = goal.task_policy_defaults.evaluator.as_ref() {
        apply_timing_defaults(&mut effective, defaults);
    }
    if let Some(archive_root) = archive_root {
        if let Ok(delivery_subject) =
            resolve_delivery_subject(&parser, archive_root, audience, audience_scope.as_ref())
        {
            effective.delivery_subject = delivery_subject.clone();
            if let Ok(Some(rule)) =
                load_availability_rule_by_subject(&parser, archive_root, &delivery_subject)
            {
                apply_availability_rule(&mut effective, &rule);
            }
        }
    }
    if let Some(task_timing) = task.policy.timing.as_ref() {
        if let Some(timezone) = task_timing.timezone.as_deref() {
            effective.timezone = timezone.to_string();
        }
        if let Some(lateness_basis) = task_timing.lateness_basis {
            effective.lateness_basis = lateness_basis;
        }
        if let Some(delivery_window) = task_timing.delivery_window.clone() {
            effective.delivery_window = delivery_window;
        }
    }
    effective
}

fn built_in_defaults(task_kind: GoalTaskKind) -> Option<EffectiveEscalationPolicy> {
    match task_kind {
        GoalTaskKind::Waiting => Some(EffectiveEscalationPolicy {
            mode: GoalTaskEscalationPolicy::NotifyOperator,
            audience: "operator".to_string(),
            severity: GoalTaskEscalationSeverity::Normal,
            on_enter_blocked: true,
            after_secs: Some(21_600),
            max_count: Some(3),
            cooldown_secs: Some(21_600),
        }),
        GoalTaskKind::Approval => Some(EffectiveEscalationPolicy {
            mode: GoalTaskEscalationPolicy::NotifyOperator,
            audience: "operator".to_string(),
            severity: GoalTaskEscalationSeverity::Normal,
            on_enter_blocked: true,
            after_secs: Some(86_400),
            max_count: Some(3),
            cooldown_secs: Some(86_400),
        }),
        GoalTaskKind::Coordination | GoalTaskKind::Review => Some(EffectiveEscalationPolicy {
            mode: GoalTaskEscalationPolicy::NotifyOperator,
            audience: "operator".to_string(),
            severity: GoalTaskEscalationSeverity::Normal,
            on_enter_blocked: true,
            after_secs: Some(14_400),
            max_count: Some(3),
            cooldown_secs: Some(14_400),
        }),
        GoalTaskKind::Execution | GoalTaskKind::Distillation => None,
    }
}

fn defaults_for_kind(
    defaults: &GoalTaskPolicyDefaults,
    task_kind: GoalTaskKind,
) -> Option<&GoalTaskEscalationDefaults> {
    match task_kind {
        GoalTaskKind::Waiting => defaults.waiting.as_ref(),
        GoalTaskKind::Approval => defaults.approval.as_ref(),
        GoalTaskKind::Coordination => defaults.coordination.as_ref(),
        GoalTaskKind::Review => defaults.review.as_ref(),
        GoalTaskKind::Execution | GoalTaskKind::Distillation => None,
    }
}

fn built_in_timing_defaults() -> EffectiveTimingPolicy {
    EffectiveTimingPolicy {
        timezone: "UTC".to_string(),
        lateness_basis: GoalTaskLatenessBasis::WallClock,
        delivery_window: GoalTaskDeliveryWindowConfig {
            mode: GoalTaskDeliveryWindowMode::Anytime,
            quiet_hours: None,
            working_hours: None,
        },
        delivery_subject: "operator".to_string(),
    }
}

fn apply_timing_defaults(
    effective: &mut EffectiveTimingPolicy,
    defaults: &GoalTaskPolicyEvaluatorDefaults,
) {
    if let Some(timezone) = defaults.timezone.as_deref() {
        effective.timezone = timezone.to_string();
    }
    if let Some(lateness_basis) = defaults.lateness_basis {
        effective.lateness_basis = lateness_basis;
    }
    if let Some(delivery_window) = defaults.delivery_window.clone() {
        effective.delivery_window = delivery_window;
    }
}

fn apply_availability_rule(effective: &mut EffectiveTimingPolicy, rule: &AvailabilityRuleManifest) {
    if let Some(timezone) = rule.timezone.as_deref() {
        effective.timezone = timezone.to_string();
    }
    match effective.delivery_window.mode {
        GoalTaskDeliveryWindowMode::OutsideQuietHours => {
            if let Some(quiet_hours) = rule.quiet_hours.as_ref() {
                effective.delivery_window.quiet_hours = Some(quiet_hours.clone());
            }
        }
        GoalTaskDeliveryWindowMode::WorkingHours | GoalTaskDeliveryWindowMode::Custom => {
            if let Some(working_hours) = rule.working_hours.as_ref() {
                effective.delivery_window.working_hours = Some(working_hours.clone());
            }
        }
        GoalTaskDeliveryWindowMode::Anytime => {}
    }
}

fn apply_escalation_defaults(
    effective: &mut EffectiveEscalationPolicy,
    defaults: &GoalTaskEscalationDefaults,
) {
    if let Some(mode) = defaults.mode {
        effective.mode = mode;
        if defaults.audience.is_none() {
            effective.audience = escalation_audience_label(None, mode).to_string();
        }
        if defaults.severity.is_none() {
            effective.severity = default_escalation_severity(mode);
        }
    }
    if let Some(audience) = defaults.audience.as_deref() {
        effective.audience = audience.to_string();
    }
    if let Some(severity) = defaults.severity {
        effective.severity = severity;
    }
    if let Some(on_enter_blocked) = defaults.on_enter_blocked {
        effective.on_enter_blocked = on_enter_blocked;
    }
    if let Some(after_secs) = defaults.after_secs {
        effective.after_secs = Some(after_secs);
    }
    if let Some(max_count) = defaults.max_count {
        effective.max_count = Some(max_count);
    }
    if let Some(cooldown_secs) = defaults.cooldown_secs {
        effective.cooldown_secs = Some(cooldown_secs);
    }
}

fn normalize_escalation_severity(
    severity: Option<GoalTaskEscalationSeverity>,
    policy: GoalTaskEscalationPolicy,
) -> GoalTaskEscalationSeverity {
    severity.unwrap_or_else(|| default_escalation_severity(policy))
}

fn task_escalation_history(events: &[GoalEventManifest], task_id: &str) -> TaskEscalationHistory {
    let mut history = TaskEscalationHistory::default();
    for event in events
        .iter()
        .filter(|event| event.task_id.as_deref() == Some(task_id))
    {
        match event.event_type.as_str() {
            "task_status_changed" if event.next_status.as_deref() == Some("blocked") => {
                history.blocked_since_from_events = Some(
                    history
                        .blocked_since_from_events
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            "task_escalated" => {
                history.escalation_count = history.escalation_count.saturating_add(1);
                history.last_escalated_at = Some(
                    history
                        .last_escalated_at
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            "task_escalation_deferred" => {
                history.last_deferred_at = Some(
                    history
                        .last_deferred_at
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            "task_escalation_window_opened" => {
                history.last_window_opened_at = Some(
                    history
                        .last_window_opened_at
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            "task_escalation_suppressed" => {
                history.last_suppressed_at = Some(
                    history
                        .last_suppressed_at
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            "task_dependencies_satisfied" => {
                history.last_dependencies_satisfied_at = Some(
                    history
                        .last_dependencies_satisfied_at
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            "task_replan_requested" => {
                history.last_replan_requested_at = Some(
                    history
                        .last_replan_requested_at
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            "task_replan_enqueued" => {
                history.last_replan_enqueued_at = Some(
                    history
                        .last_replan_enqueued_at
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            "plan_reconciled" => {
                history.latest_plan_reconciled_at = Some(
                    history
                        .latest_plan_reconciled_at
                        .map(|existing| existing.max(event.observed_at))
                        .unwrap_or(event.observed_at),
                );
            }
            _ => {}
        }
    }
    history
}

fn dependencies_satisfied(
    task: &PlannedTaskRecord,
    task_statuses: &std::collections::HashMap<String, GoalTaskStatus>,
) -> bool {
    if task.depends_on.is_empty() {
        return false;
    }
    if matches!(
        task.execution_status,
        GoalTaskStatus::Done | GoalTaskStatus::Cancelled | GoalTaskStatus::InProgress
    ) {
        return false;
    }
    task.depends_on
        .iter()
        .all(|dependency| matches!(task_statuses.get(dependency), Some(GoalTaskStatus::Done)))
}

fn evaluate_escalation_decision(
    policy: &EffectiveEscalationPolicy,
    history: TaskEscalationHistory,
    blocked_since: i64,
    now: i64,
) -> EvaluationDecision {
    if let Some(max_count) = policy.max_count {
        if history.escalation_count >= max_count {
            return EvaluationDecision::Suppress {
                note: format!(
                    "Escalation suppressed: max_count={} reached for current blocked period.",
                    max_count
                ),
                cooldown_until: None,
            };
        }
    }
    if let (Some(cooldown_secs), Some(last_escalated_at)) =
        (policy.cooldown_secs, history.last_escalated_at)
    {
        let cooldown_until = last_escalated_at.saturating_add(cooldown_secs as i64);
        if now < cooldown_until {
            return EvaluationDecision::Suppress {
                note: format!("Escalation suppressed: cooldown active until {cooldown_until}."),
                cooldown_until: Some(cooldown_until),
            };
        }
    }
    if history.last_escalated_at.unwrap_or(blocked_since) < blocked_since {
        return EvaluationDecision::Escalate;
    }
    EvaluationDecision::Escalate
}

fn evaluate_delivery_decision(
    timing: &EffectiveTimingPolicy,
    history: TaskEscalationHistory,
    blocked_since: i64,
    now: i64,
) -> DeliveryDecision {
    if delivery_window_is_open(timing, now) {
        DeliveryDecision::DeliverNow
    } else {
        let detail = if deferred_delivery_pending(history, blocked_since) {
            format!(
                "Task is overdue but human delivery remains deferred until the policy window opens in timezone {}.",
                timing.timezone
            )
        } else {
            format!(
                "Task became overdue outside the policy delivery window in timezone {}; delivery is deferred until the window opens.",
                timing.timezone
            )
        };
        DeliveryDecision::Deferred { detail }
    }
}

fn should_append_suppressed_event(history: TaskEscalationHistory, blocked_since: i64) -> bool {
    let reference = history.last_escalated_at.unwrap_or(blocked_since);
    history
        .last_suppressed_at
        .map(|last| last < reference)
        .unwrap_or(true)
}

fn should_append_deferred_event(history: TaskEscalationHistory, blocked_since: i64) -> bool {
    let blocked_reference = history.last_escalated_at.unwrap_or(blocked_since);
    history
        .last_deferred_at
        .map(|last| last < blocked_reference || last < blocked_since)
        .unwrap_or(true)
}

fn should_emit_dependencies_satisfied(
    task: &PlannedTaskRecord,
    history: TaskEscalationHistory,
) -> bool {
    let reference = task
        .last_status_change_at
        .into_iter()
        .chain(history.latest_plan_reconciled_at)
        .max()
        .unwrap_or(i64::MIN);
    history
        .last_dependencies_satisfied_at
        .map(|last| last < reference)
        .unwrap_or(true)
}

fn should_auto_resume_after_internal_dependencies(task: &PlannedTaskRecord) -> bool {
    matches!(task.task_driver, GoalTaskDriver::Declared)
        && matches!(task.execution_status, GoalTaskStatus::Blocked)
        && task.declared_context.waiting_for.is_none()
        && task.declared_context.review_target.is_none()
        && task.declared_context.coordination_target.is_none()
        && task.declared_context.external_dependency.is_none()
}

fn replan_request_pending(history: TaskEscalationHistory) -> bool {
    let consumed_at = history
        .last_replan_enqueued_at
        .into_iter()
        .chain(history.latest_plan_reconciled_at)
        .max()
        .unwrap_or(i64::MIN);
    history
        .last_replan_requested_at
        .map(|requested_at| requested_at > consumed_at)
        .unwrap_or(false)
}

fn deferred_delivery_pending(history: TaskEscalationHistory, blocked_since: i64) -> bool {
    let last_deferred_at = history.last_deferred_at.unwrap_or(i64::MIN);
    if last_deferred_at < blocked_since {
        return false;
    }
    last_deferred_at
        > history
            .last_window_opened_at
            .into_iter()
            .chain(history.last_escalated_at)
            .max()
            .unwrap_or(i64::MIN)
}

fn background_escalation_detail(
    task: &PlannedTaskRecord,
    after_secs: u64,
    blocked_since: i64,
    blocked_for: i64,
) -> String {
    let base = declared_task_escalation_detail(task, None);
    format!(
        "{base}. Task remained blocked since {blocked_since} and accumulated {blocked_for}s toward policy after_secs={after_secs}."
    )
}

pub(crate) fn blocked_duration_for_policy(
    timing: &EffectiveTimingPolicy,
    blocked_since: i64,
    now: i64,
) -> i64 {
    if now <= blocked_since {
        return 0;
    }
    match timing.lateness_basis {
        GoalTaskLatenessBasis::WallClock => now.saturating_sub(blocked_since),
        GoalTaskLatenessBasis::DeliveryWindowElapsed => {
            seconds_in_delivery_window(timing, blocked_since, now)
        }
    }
}

pub(crate) fn delivery_window_is_open(timing: &EffectiveTimingPolicy, now: i64) -> bool {
    if matches!(
        timing.delivery_window.mode,
        GoalTaskDeliveryWindowMode::Anytime
    ) {
        return true;
    }
    seconds_in_delivery_window(timing, now.saturating_sub(60), now.saturating_add(60)) > 0
}

fn seconds_in_delivery_window(timing: &EffectiveTimingPolicy, start_ts: i64, end_ts: i64) -> i64 {
    if end_ts <= start_ts {
        return 0;
    }
    let tz = parse_timezone(&timing.timezone);
    let Some(start_utc) = utc_datetime(start_ts) else {
        return 0;
    };
    let Some(end_utc) = utc_datetime(end_ts) else {
        return 0;
    };
    let start_local = start_utc.with_timezone(&tz);
    let end_local = end_utc.with_timezone(&tz);
    let Some(mut current_date) = start_local
        .date_naive()
        .checked_sub_days(chrono::Days::new(1))
    else {
        return 0;
    };
    let Some(last_date) = end_local
        .date_naive()
        .checked_add_days(chrono::Days::new(1))
    else {
        return 0;
    };

    let mut total = 0i64;
    while current_date <= last_date {
        for (window_start, window_end) in
            delivery_window_segments_for_date(&tz, current_date, &timing.delivery_window)
        {
            let overlap_start = std::cmp::max(window_start, start_utc);
            let overlap_end = std::cmp::min(window_end, end_utc);
            if overlap_end > overlap_start {
                total = total.saturating_add((overlap_end - overlap_start).num_seconds());
            }
        }
        let Some(next_date) = current_date.checked_add_days(chrono::Days::new(1)) else {
            break;
        };
        current_date = next_date;
    }
    total.max(0)
}

fn delivery_window_segments_for_date(
    tz: &Tz,
    date: chrono::NaiveDate,
    window: &GoalTaskDeliveryWindowConfig,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    match window.mode {
        GoalTaskDeliveryWindowMode::Anytime => {
            whole_local_day_segment(tz, date).into_iter().collect()
        }
        GoalTaskDeliveryWindowMode::OutsideQuietHours => {
            outside_quiet_hours_segments(tz, date, window.quiet_hours.as_ref())
        }
        GoalTaskDeliveryWindowMode::WorkingHours | GoalTaskDeliveryWindowMode::Custom => {
            working_hours_segments(tz, date, window.working_hours.as_ref())
        }
    }
}

fn whole_local_day_segment(
    tz: &Tz,
    date: chrono::NaiveDate,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start = local_datetime_to_utc(tz, date, parse_local_time("00:00"), false)?;
    let next_date = date.checked_add_days(chrono::Days::new(1))?;
    let end = local_datetime_to_utc(tz, next_date, parse_local_time("00:00"), false)?;
    Some((start, end))
}

fn outside_quiet_hours_segments(
    tz: &Tz,
    date: chrono::NaiveDate,
    quiet_hours: Option<&GoalTaskQuietHoursWindow>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let Some(quiet_hours) = quiet_hours else {
        return whole_local_day_segment(tz, date).into_iter().collect();
    };
    let start = parse_local_time(&quiet_hours.start_local);
    let end = parse_local_time(&quiet_hours.end_local);
    if start == end {
        return whole_local_day_segment(tz, date).into_iter().collect();
    }
    if start > end {
        return make_segment(tz, date, end, start).into_iter().collect();
    }
    let mut segments = Vec::new();
    if let Some(segment) = make_segment(tz, date, parse_local_time("00:00"), start) {
        segments.push(segment);
    }
    let next_date = date.checked_add_days(chrono::Days::new(1));
    if let Some(next_date) = next_date {
        if let Some(segment) = make_segment(tz, date, end, parse_local_time("23:59")) {
            segments.push(segment);
        }
        if let Some((_, end_of_day)) = whole_local_day_segment(tz, date) {
            if let Some(start_utc) = local_datetime_to_utc(tz, date, end, false) {
                if end_of_day > start_utc {
                    segments.push((start_utc, end_of_day));
                }
            }
        }
        let _ = next_date;
    }
    segments
}

fn working_hours_segments(
    tz: &Tz,
    date: chrono::NaiveDate,
    working_hours: Option<&GoalTaskWorkingHoursWindow>,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let Some(working_hours) = working_hours else {
        return whole_local_day_segment(tz, date).into_iter().collect();
    };
    if !working_hours.weekdays.is_empty()
        && !working_hours
            .weekdays
            .iter()
            .any(|weekday| weekday_matches(*weekday, date.weekday()))
    {
        return Vec::new();
    }
    let start = parse_local_time(&working_hours.start_local);
    let end = parse_local_time(&working_hours.end_local);
    if start == end {
        return whole_local_day_segment(tz, date).into_iter().collect();
    }
    if start < end {
        return make_segment(tz, date, start, end).into_iter().collect();
    }
    let Some(next_date) = date.checked_add_days(chrono::Days::new(1)) else {
        return Vec::new();
    };
    let Some(start_utc) = local_datetime_to_utc(tz, date, start, false) else {
        return Vec::new();
    };
    let Some(end_utc) = local_datetime_to_utc(tz, next_date, end, true) else {
        return Vec::new();
    };
    if end_utc > start_utc {
        vec![(start_utc, end_utc)]
    } else {
        Vec::new()
    }
}

fn make_segment(
    tz: &Tz,
    date: chrono::NaiveDate,
    start: NaiveTime,
    end: NaiveTime,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start_utc = local_datetime_to_utc(tz, date, start, false)?;
    let end_utc = local_datetime_to_utc(tz, date, end, true)?;
    (end_utc > start_utc).then_some((start_utc, end_utc))
}

fn utc_datetime(ts: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(ts, 0).single()
}

fn parse_timezone(value: &str) -> Tz {
    value.parse::<Tz>().unwrap_or(chrono_tz::UTC)
}

fn parse_local_time(value: &str) -> NaiveTime {
    NaiveTime::parse_from_str(value, "%H:%M")
        .unwrap_or_else(|_| NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is valid"))
}

fn local_datetime_to_utc(
    tz: &Tz,
    date: chrono::NaiveDate,
    time: NaiveTime,
    prefer_latest: bool,
) -> Option<DateTime<Utc>> {
    let local = date.and_time(time);
    match tz.from_local_datetime(&local) {
        chrono::LocalResult::Single(value) => Some(value.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(earliest, latest) => {
            Some(if prefer_latest { latest } else { earliest }.with_timezone(&Utc))
        }
        chrono::LocalResult::None => {
            for minutes in 1..=180 {
                let adjusted = local + Duration::minutes(minutes);
                match tz.from_local_datetime(&adjusted) {
                    chrono::LocalResult::Single(value) => return Some(value.with_timezone(&Utc)),
                    chrono::LocalResult::Ambiguous(earliest, latest) => {
                        return Some(
                            if prefer_latest { latest } else { earliest }.with_timezone(&Utc),
                        )
                    }
                    chrono::LocalResult::None => continue,
                }
            }
            None
        }
    }
}

fn weekday_matches(expected: GoalTaskWeekday, actual: chrono::Weekday) -> bool {
    matches!(
        (expected, actual),
        (GoalTaskWeekday::Mon, chrono::Weekday::Mon)
            | (GoalTaskWeekday::Tue, chrono::Weekday::Tue)
            | (GoalTaskWeekday::Wed, chrono::Weekday::Wed)
            | (GoalTaskWeekday::Thu, chrono::Weekday::Thu)
            | (GoalTaskWeekday::Fri, chrono::Weekday::Fri)
            | (GoalTaskWeekday::Sat, chrono::Weekday::Sat)
            | (GoalTaskWeekday::Sun, chrono::Weekday::Sun)
    )
}

fn append_replan_event_for_policy(
    archive_root: &std::path::Path,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    policy: &EffectiveEscalationPolicy,
    history: &TaskEscalationHistory,
    trigger: &str,
    detail: &str,
) -> Result<()> {
    if replan_request_pending(*history) {
        append_goal_event_archive(
            archive_root,
            context,
            "task_replan_skipped",
            task.plan_version,
            &format!("task={} replan request already pending", task.task_id),
            GoalEventMetadata {
                task_id: Some(&task.task_id),
                previous_status: Some("blocked"),
                next_status: Some("blocked"),
                escalation_policy: Some("auto_replan"),
                escalation_trigger: Some(trigger),
                escalation_audience: Some(policy.audience.as_str()),
                escalation_severity: Some(escalation_severity_label(policy.severity)),
                escalation_count: Some(history.escalation_count.saturating_add(1)),
                actor: Some(POLICY_ACTOR),
                note: Some(
                    "Replanning request skipped because an equivalent request is still pending.",
                ),
                ..GoalEventMetadata::default()
            },
        )?;
    } else {
        append_goal_event_archive(
            archive_root,
            context,
            "task_replan_requested",
            task.plan_version,
            &format!("task={} blocked -> replan requested", task.task_id),
            GoalEventMetadata {
                task_id: Some(&task.task_id),
                previous_status: Some("blocked"),
                next_status: Some("blocked"),
                escalation_policy: Some("auto_replan"),
                escalation_trigger: Some(trigger),
                escalation_audience: Some(policy.audience.as_str()),
                escalation_severity: Some(escalation_severity_label(policy.severity)),
                escalation_count: Some(history.escalation_count.saturating_add(1)),
                actor: Some(POLICY_ACTOR),
                note: Some(detail),
                ..GoalEventMetadata::default()
            },
        )?;
    }
    Ok(())
}

fn first_goal_summary_line(markdown: &str) -> &str {
    markdown
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("Archive-declared goal plan.")
}
