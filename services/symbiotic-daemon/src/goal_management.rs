use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use symbiotic_control_plane::types::{
    GoalTaskDeclaredContext, GoalTaskDeliveryWindowConfig, GoalTaskDeliveryWindowMode,
    GoalTaskDriver, GoalTaskEscalationDefaults, GoalTaskEscalationPolicy,
    GoalTaskEscalationSeverity, GoalTaskKind, GoalTaskLatenessBasis, GoalTaskPlanState,
    GoalTaskPolicy, GoalTaskPolicyDefaults, GoalTaskQuietHoursWindow, GoalTaskStatus,
    GoalTaskTimingConfig, GoalTaskWeekday, GoalTaskWorkingHoursWindow,
};
use symbiotic_control_plane::{
    manifest::ManifestParser, AgentAssignment, AssignmentMode, ManagementStore, ReviewMode,
    WorkItem, WorkItemKind, WorkItemStatus, WorkPriority, WorkUrgency,
};

use crate::declared_task_policy::{
    delivery_window_is_open, effective_escalation_policy, effective_timing_policy,
};

pub(crate) fn goal_work_item_id(slug: &str) -> String {
    format!("goal:{slug}")
}

pub(crate) fn goal_task_work_item_id(slug: &str, task_id: &str) -> String {
    format!("goal:{slug}:task:{task_id}")
}

pub(crate) fn goal_execution_work_item_id(slug: &str, task_id: &str) -> String {
    format!("goal:{slug}:task:{task_id}:execution")
}

pub(crate) fn stable_goal_scope_id(template: &str, fallback_goal_id: &str) -> String {
    template
        .strip_prefix("inquisition:")
        .or_else(|| template.strip_prefix("agent-execute:"))
        .unwrap_or(fallback_goal_id)
        .to_string()
}

pub(crate) struct GoalWorkItemUpdate<'a> {
    pub slug: &'a str,
    pub title: &'a str,
    pub project_id: &'a str,
    pub phase: Option<&'a str>,
    pub owner: &'a str,
    pub thread_id: Option<&'a str>,
    pub priority: WorkPriority,
    pub status: WorkItemStatus,
    pub observed_at: i64,
}

pub(crate) fn sync_goal_work_item(
    management_store: &Arc<Mutex<ManagementStore>>,
    update: GoalWorkItemUpdate<'_>,
) {
    let work_item_id = goal_work_item_id(update.slug);
    let mut store = match management_store.lock() {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(
                goal_slug = %update.slug,
                %error,
                "goal_management: failed to lock management store"
            );
            return;
        }
    };

    let mut work_item = store
        .get_work_item(&work_item_id)
        .cloned()
        .unwrap_or_else(|| WorkItem {
            id: work_item_id,
            project_id: update.project_id.to_string(),
            initiative_id: Some(update.slug.to_string()),
            parent_work_item_id: None,
            kind: WorkItemKind::Goal,
            thread_id: update.thread_id.map(str::to_string),
            title: update.title.to_string(),
            summary: String::new(),
            status: update.status,
            priority: update.priority,
            urgency: WorkUrgency::Normal,
            assignment_mode: AssignmentMode::SingleOwner,
            requested_scopes: Vec::new(),
            accepted_claim_ids: Vec::new(),
            assignee: None,
            blocked_by: Vec::new(),
            depends_on: Vec::new(),
            review_mode: ReviewMode::NoReview,
            cancellation: None,
            created_at: update.observed_at,
            updated_at: update.observed_at,
        });

    work_item.title = update.title.to_string();
    work_item.project_id = update.project_id.to_string();
    work_item.thread_id = update.thread_id.map(str::to_string);
    work_item.summary = match update.phase {
        Some(phase) if !phase.is_empty() => {
            format!(
                "Goal lifecycle ownership tracked via {} (phase: {phase})",
                update.owner
            )
        }
        _ => format!("Goal lifecycle ownership tracked via {}", update.owner),
    };
    work_item.priority = update.priority;
    work_item.assignee = Some(AgentAssignment {
        agent_id: update.owner.to_string(),
        runner_id: None,
        assigned_at: update.observed_at,
    });
    work_item.set_status(update.status, update.observed_at);

    if let Err(error) = store.upsert_work_item(work_item) {
        tracing::warn!(
            goal_slug = %update.slug,
            %error,
            "goal_management: failed to persist goal work item"
        );
    }
}

pub(crate) fn priority_from_goal_priority(priority: u8) -> WorkPriority {
    match priority {
        0..=24 => WorkPriority::P0,
        25..=49 => WorkPriority::P1,
        50..=74 => WorkPriority::P2,
        _ => WorkPriority::P3,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct GoalHierarchyContext<'a> {
    pub project_id: &'a str,
    pub goal_id: &'a str,
    pub title: &'a str,
    pub summary: &'a str,
    pub owner: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub observed_at: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedTaskRecord {
    pub task_id: String,
    pub task_slug: String,
    pub task_kind: GoalTaskKind,
    pub task_driver: GoalTaskDriver,
    pub title: String,
    pub summary: String,
    pub state: GoalTaskPlanState,
    pub execution_status: GoalTaskStatus,
    pub role: Option<String>,
    pub depends_on: Vec<String>,
    pub questionnaire_context: Vec<String>,
    pub owner_hint: Option<String>,
    pub declared_context: GoalTaskDeclaredContext,
    pub policy: GoalTaskPolicy,
    pub retry_count: u32,
    pub reopen_count: u32,
    pub last_status_change_at: Option<i64>,
    pub plan_version: u32,
    pub superseded_by: Vec<String>,
    pub derived_from: Vec<String>,
    pub replaces: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclaredTaskEscalation {
    pub policy: GoalTaskEscalationPolicy,
    pub audience: Option<String>,
    pub severity: Option<GoalTaskEscalationSeverity>,
    pub task_id: String,
    pub task_title: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingGoalReplanRequest {
    pub goal_id: String,
    pub task_id: String,
    pub thread_id: Option<String>,
    pub detail: Option<String>,
    pub observed_at: i64,
}

fn sanitize_archive_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_dash = false;
    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else if matches!(ch, '-' | '_') {
            Some('-')
        } else {
            None
        };
        match normalized {
            Some('-') => {
                if !last_dash && !out.is_empty() {
                    out.push('-');
                    last_dash = true;
                }
            }
            Some(ch) => {
                out.push(ch);
                last_dash = false;
            }
            None => {
                if !last_dash && !out.is_empty() {
                    out.push('-');
                    last_dash = true;
                }
            }
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "item".to_string()
    } else {
        out.to_string()
    }
}

fn write_markdown_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing parent directory for {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid file name for {}", path.display()))?;
    let tmp_path = parent.join(format!("{file_name}.tmp"));
    fs::write(&tmp_path, content)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn project_archive_component(project_id: &str) -> String {
    let raw = project_id.strip_prefix("project:").unwrap_or(project_id);
    sanitize_archive_component(raw)
}

pub(crate) fn goal_archive_dir(archive_root: &Path, project_id: &str, goal_slug: &str) -> PathBuf {
    archive_root
        .join("operations")
        .join("projects")
        .join(project_archive_component(project_id))
        .join("goals")
        .join(sanitize_archive_component(goal_slug))
}

fn project_archive_dir(archive_root: &Path, project_id: &str) -> PathBuf {
    archive_root
        .join("operations")
        .join("projects")
        .join(project_archive_component(project_id))
}

fn render_project_markdown(project_id: &str) -> String {
    let slug = project_archive_component(project_id);
    let title = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    let mut word = first.to_uppercase().collect::<String>();
                    word.push_str(chars.as_str());
                    word
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "---\nid: \"{project_id}\"\nslug: {slug}\ntitle: \"{title}\"\nstate: active\n---\n\n# {title}\n"
    )
}

fn ensure_project_archive_manifest(archive_root: &Path, project_id: &str) -> anyhow::Result<()> {
    let project_dir = project_archive_dir(archive_root, project_id);
    let project_path = project_dir.join("project.md");
    if project_path.exists() {
        return Ok(());
    }
    write_markdown_atomic(&project_path, &render_project_markdown(project_id))
}

fn render_goal_plan_markdown(
    context: &GoalHierarchyContext<'_>,
    phase: &str,
    tasks: &[PlannedTaskRecord],
    plan_version: u32,
    policy_scopes: &[String],
    task_policy_defaults: &GoalTaskPolicyDefaults,
) -> String {
    let goal_slug = sanitize_archive_component(context.goal_id);
    let policy_scopes_yaml = render_goal_policy_scopes_yaml(policy_scopes);
    let task_policy_defaults_yaml = render_goal_task_policy_defaults_yaml(task_policy_defaults);
    let policy_scopes_block = if policy_scopes_yaml.is_empty() {
        String::new()
    } else {
        format!("policy_scopes:\n{policy_scopes_yaml}")
    };
    let task_policy_defaults_block = if task_policy_defaults_yaml.is_empty() {
        String::new()
    } else {
        format!("task_policy_defaults:\n{task_policy_defaults_yaml}")
    };
    let task_links = if tasks.is_empty() {
        "- [ ] Await clarification, deliberation, or approval before execution.\n".to_string()
    } else {
        tasks
            .iter()
            .map(|task| {
                let task_doc = sanitize_archive_component(&task.task_id);
                format!(
                    "- [ ] [[tasks/{task_doc}|{}]] — {}\n",
                    task.title, task.summary
                )
            })
            .collect::<String>()
    };
    format!(
        "---\nid: \"{goal_id}\"\nproject_id: \"{project_id}\"\nslug: {goal_slug}\ntitle: \"{title}\"\nstate: active\npriority: 50\nautonomy_level: semi\nprocess:\n  type: on_demand\n  check_frequency: daily\n  max_parallel_agents: {parallelism}\nphase: {phase}\nplan_version: {plan_version}\ndomains: [projects]\nvault_namespace: goal-{goal_slug}\nthread_id: {thread_id}\n{policy_scopes_block}{task_policy_defaults_block}---\n\n# {title}\n\n## Objective\n\n{summary}\n\n## Planned Tasks\n\n{task_links}",
        goal_id = context.goal_id,
        project_id = context.project_id,
        goal_slug = goal_slug,
        title = context.title.replace('"', "\\\""),
        parallelism = tasks.len().max(1),
        phase = phase,
        plan_version = plan_version,
        thread_id = context
            .thread_id
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        policy_scopes_block = policy_scopes_block,
        task_policy_defaults_block = task_policy_defaults_block,
        summary = context.summary,
        task_links = task_links,
    )
}

fn render_goal_policy_scopes_yaml(policy_scopes: &[String]) -> String {
    if policy_scopes.is_empty() {
        return String::new();
    }
    policy_scopes
        .iter()
        .map(|scope_id| format!("  - \"{}\"\n", scope_id.replace('"', "\\\"")))
        .collect::<String>()
}

fn render_goal_task_policy_defaults_yaml(defaults: &GoalTaskPolicyDefaults) -> String {
    let mut out = String::new();
    if let Some(evaluator) = defaults.evaluator.as_ref() {
        out.push_str("  evaluator:\n");
        if let Some(interval_secs) = evaluator.interval_secs {
            out.push_str(&format!("    interval_secs: {interval_secs}\n"));
        }
        if let Some(max_actions_per_tick) = evaluator.max_actions_per_tick {
            out.push_str(&format!(
                "    max_actions_per_tick: {max_actions_per_tick}\n"
            ));
        }
        if let Some(timezone) = evaluator.timezone.as_deref() {
            out.push_str(&format!(
                "    timezone: \"{}\"\n",
                timezone.replace('"', "\\\"")
            ));
        }
        if let Some(lateness_basis) = evaluator.lateness_basis {
            out.push_str(&format!(
                "    lateness_basis: {}\n",
                lateness_basis_label(lateness_basis)
            ));
        }
        if let Some(delivery_window) = evaluator.delivery_window.as_ref() {
            out.push_str("    delivery_window:\n");
            out.push_str(&render_delivery_window_yaml(delivery_window, 6));
        }
    }

    fn render_defaults(label: &str, value: &Option<GoalTaskEscalationDefaults>) -> Option<String> {
        let value = value.as_ref()?;
        let mut out = String::new();
        out.push_str(&format!("  {label}:\n"));
        if let Some(mode) = value.mode {
            out.push_str(&format!(
                "    mode: {}\n",
                match mode {
                    GoalTaskEscalationPolicy::NotifyOperator => "notify_operator",
                    GoalTaskEscalationPolicy::RaiseAlert => "raise_alert",
                    GoalTaskEscalationPolicy::AutoReplan => "auto_replan",
                }
            ));
        }
        if let Some(audience) = value.audience.as_deref() {
            out.push_str(&format!(
                "    audience: \"{}\"\n",
                audience.replace('"', "\\\"")
            ));
        }
        if let Some(severity) = value.severity {
            out.push_str(&format!(
                "    severity: {}\n",
                escalation_severity_label(severity)
            ));
        }
        if let Some(on_enter_blocked) = value.on_enter_blocked {
            out.push_str(&format!("    on_enter_blocked: {on_enter_blocked}\n"));
        }
        if let Some(after_secs) = value.after_secs {
            out.push_str(&format!("    after_secs: {after_secs}\n"));
        }
        if let Some(max_count) = value.max_count {
            out.push_str(&format!("    max_count: {max_count}\n"));
        }
        if let Some(cooldown_secs) = value.cooldown_secs {
            out.push_str(&format!("    cooldown_secs: {cooldown_secs}\n"));
        }
        Some(out)
    }

    out.push_str(
        &[
            render_defaults("waiting", &defaults.waiting),
            render_defaults("approval", &defaults.approval),
            render_defaults("coordination", &defaults.coordination),
            render_defaults("review", &defaults.review),
        ]
        .into_iter()
        .flatten()
        .collect::<String>(),
    );
    out
}

fn render_goal_task_markdown(
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
) -> String {
    let depends_on = if task.depends_on.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            task.depends_on
                .iter()
                .map(|value| format!("\"{value}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let questionnaire_context = if task.questionnaire_context.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            task.questionnaire_context
                .iter()
                .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let lineage_array = |values: &[String]| {
        if values.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                values
                    .iter()
                    .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    };
    let declared_context_yaml = render_declared_context_yaml(&task.declared_context);
    let policy_yaml = render_task_policy_yaml(&task.policy);
    let declared_context_body = render_declared_context_markdown(&task.declared_context);
    let policy_body = render_task_policy_markdown(&task.policy);
    format!(
        "---\nid: \"goal:{goal_id}:task:{task_id}\"\ngoal_id: \"{goal_id}\"\ntask_id: \"{task_id}\"\ntask_slug: \"{task_slug}\"\ntask_kind: {task_kind}\ntask_driver: {task_driver}\ntitle: \"{title}\"\nstate: {state}\nexecution_status: {execution_status}\nrole: {role}\ndepends_on: {depends_on}\nquestionnaire_context: {questionnaire_context}\nowner_hint: {owner_hint}\ndeclared_context:\n{declared_context_yaml}policy:\n{policy_yaml}\nretry_count: {retry_count}\nreopen_count: {reopen_count}\nlast_status_change_at: {last_status_change_at}\nplan_version: {plan_version}\nsuperseded_by: {superseded_by}\nderived_from: {derived_from}\nreplaces: {replaces}\nthread_id: {thread_id}\nsource_step_id: \"{task_id}\"\n---\n\n# {title}\n\n## Summary\n\n{summary}\n\n## Questionnaire Context\n{questionnaire_body}\n\n## Declared Context\n{declared_context_body}\n\n## Policy\n{policy_body}\n\n## Checklist\n\n- [ ] {checklist_label}\n",
        goal_id = context.goal_id,
        task_id = task.task_id,
        task_slug = task.task_slug,
        task_kind = match task.task_kind {
            GoalTaskKind::Execution => "execution",
            GoalTaskKind::Review => "review",
            GoalTaskKind::Distillation => "distillation",
            GoalTaskKind::Coordination => "coordination",
            GoalTaskKind::Waiting => "waiting",
            GoalTaskKind::Approval => "approval",
        },
        task_driver = match task.task_driver {
            GoalTaskDriver::Agent => "agent",
            GoalTaskDriver::Declared => "declared",
        },
        title = task.title.replace('"', "\\\""),
        state = match task.state {
            GoalTaskPlanState::Active => "active",
            GoalTaskPlanState::Cancelled => "cancelled",
            GoalTaskPlanState::Superseded => "superseded",
        },
        execution_status = match task.execution_status {
            GoalTaskStatus::Planned => "planned",
            GoalTaskStatus::InProgress => "in_progress",
            GoalTaskStatus::Blocked => "blocked",
            GoalTaskStatus::Done => "done",
            GoalTaskStatus::Cancelled => "cancelled",
        },
        role = task
            .role
            .as_deref()
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        depends_on = depends_on,
        questionnaire_context = questionnaire_context,
        owner_hint = task
            .owner_hint
            .as_deref()
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        declared_context_yaml = declared_context_yaml,
        policy_yaml = policy_yaml,
        retry_count = task.retry_count,
        reopen_count = task.reopen_count,
        last_status_change_at = task
            .last_status_change_at
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string()),
        plan_version = task.plan_version,
        superseded_by = lineage_array(&task.superseded_by),
        derived_from = lineage_array(&task.derived_from),
        replaces = lineage_array(&task.replaces),
        thread_id = context
            .thread_id
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        checklist_label = match task.task_kind {
            GoalTaskKind::Execution => "Execute this task",
            GoalTaskKind::Review => "Review this task",
            GoalTaskKind::Distillation => "Distill this task",
            GoalTaskKind::Coordination => "Coordinate this task",
            GoalTaskKind::Waiting => "Await the required external signal",
            GoalTaskKind::Approval => "Obtain or record approval",
        },
        questionnaire_body = if task.questionnaire_context.is_empty() {
            "\n- None yet\n".to_string()
        } else {
            task.questionnaire_context
                .iter()
                .map(|item| format!("\n- {item}"))
                .collect::<String>()
                + "\n"
        },
        declared_context_body = declared_context_body,
        policy_body = policy_body,
        summary = task.summary,
    )
}

fn render_declared_context_yaml(context: &GoalTaskDeclaredContext) -> String {
    let line = |label: &str, value: Option<&str>| {
        value
            .map(|value| format!("  {label}: \"{}\"\n", value.replace('"', "\\\"")))
            .unwrap_or_else(|| format!("  {label}: null\n"))
    };
    format!(
        "{}{}{}{}",
        line("review_target", context.review_target.as_deref()),
        line("waiting_for", context.waiting_for.as_deref()),
        line(
            "coordination_target",
            context.coordination_target.as_deref()
        ),
        line(
            "external_dependency",
            context.external_dependency.as_deref()
        )
    )
}

fn render_declared_context_markdown(context: &GoalTaskDeclaredContext) -> String {
    let mut items = Vec::new();
    if let Some(review_target) = context.review_target.as_deref() {
        items.push(format!("- Review target: {review_target}"));
    }
    if let Some(waiting_for) = context.waiting_for.as_deref() {
        items.push(format!("- Waiting for: {waiting_for}"));
    }
    if let Some(coordination_target) = context.coordination_target.as_deref() {
        items.push(format!("- Coordination target: {coordination_target}"));
    }
    if let Some(external_dependency) = context.external_dependency.as_deref() {
        items.push(format!("- External dependency: {external_dependency}"));
    }
    if items.is_empty() {
        "\n- None declared\n".to_string()
    } else {
        format!("\n{}\n", items.join("\n"))
    }
}

fn render_task_policy_yaml(policy: &GoalTaskPolicy) -> String {
    let mut out = String::new();
    if let Some(escalation) = policy.escalation.as_ref() {
        out.push_str(&format!(
            "  escalation:\n    mode: {}\n    audience: {}\n    severity: {}\n    on_enter_blocked: {}\n    after_secs: {}\n    max_count: {}\n    cooldown_secs: {}\n",
            escalation_policy_label(escalation.mode),
            escalation
                .audience
                .as_deref()
                .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
                .unwrap_or_else(|| "null".to_string()),
            escalation
                .severity
                .map(escalation_severity_label)
                .unwrap_or("null"),
            if escalation.on_enter_blocked { "true" } else { "false" },
            escalation
                .after_secs
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
            escalation
                .max_count
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
            escalation
                .cooldown_secs
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
        ));
    } else {
        out.push_str("  escalation: null\n");
    }
    if let Some(timing) = policy.timing.as_ref() {
        out.push_str("  timing:\n");
        out.push_str(&render_timing_yaml(timing, 4));
    } else {
        out.push_str("  timing: null\n");
    }
    out
}

fn render_task_policy_markdown(policy: &GoalTaskPolicy) -> String {
    let mut items = Vec::new();
    if let Some(escalation) = policy.escalation.as_ref() {
        items.push(format!(
            "- Escalation: {}",
            escalation_policy_label(escalation.mode)
        ));
        if let Some(audience) = escalation.audience.as_deref() {
            items.push(format!("- Audience: {audience}"));
        }
        if let Some(severity) = escalation.severity {
            items.push(format!(
                "- Severity: {}",
                escalation_severity_label(severity)
            ));
        }
        items.push(format!(
            "- On enter blocked: {}",
            if escalation.on_enter_blocked {
                "yes"
            } else {
                "no"
            }
        ));
        if let Some(after_secs) = escalation.after_secs {
            items.push(format!("- After seconds: {after_secs}"));
        }
        if let Some(max_count) = escalation.max_count {
            items.push(format!("- Max count: {max_count}"));
        }
        if let Some(cooldown_secs) = escalation.cooldown_secs {
            items.push(format!("- Cooldown seconds: {cooldown_secs}"));
        }
    }
    if let Some(timing) = policy.timing.as_ref() {
        if let Some(timezone) = timing.timezone.as_deref() {
            items.push(format!("- Timezone: {timezone}"));
        }
        if let Some(lateness_basis) = timing.lateness_basis {
            items.push(format!(
                "- Lateness basis: {}",
                lateness_basis_label(lateness_basis)
            ));
        }
        if let Some(delivery_window) = timing.delivery_window.as_ref() {
            items.push(format!(
                "- Delivery window: {}",
                delivery_window_summary(delivery_window)
            ));
        }
    }
    if items.is_empty() {
        return "\n- None declared\n".to_string();
    }
    format!("\n{}\n", items.join("\n"))
}

fn declared_context_summary_suffix(
    context: &GoalTaskDeclaredContext,
    policy: &GoalTaskPolicy,
) -> Option<String> {
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
    if let Some(escalation) = policy.escalation.as_ref() {
        parts.push(format!(
            "escalation: {}",
            escalation_policy_label(escalation.mode)
        ));
        if let Some(audience) = escalation.audience.as_deref() {
            parts.push(format!("audience: {audience}"));
        }
        if let Some(severity) = escalation.severity {
            parts.push(format!("severity: {}", escalation_severity_label(severity)));
        }
    }
    if let Some(timing) = policy.timing.as_ref() {
        if let Some(timezone) = timing.timezone.as_deref() {
            parts.push(format!("timezone: {timezone}"));
        }
        if let Some(lateness_basis) = timing.lateness_basis {
            parts.push(format!(
                "lateness: {}",
                lateness_basis_label(lateness_basis)
            ));
        }
        if let Some(delivery_window) = timing.delivery_window.as_ref() {
            parts.push(format!(
                "window: {}",
                delivery_window_summary(delivery_window)
            ));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn management_summary_for_task(task: &PlannedTaskRecord) -> String {
    match declared_context_summary_suffix(&task.declared_context, &task.policy) {
        Some(suffix) => format!("{} ({suffix})", task.summary),
        None => task.summary.clone(),
    }
}

pub(crate) fn persist_goal_plan_archive(
    archive_root: &Path,
    context: &GoalHierarchyContext<'_>,
    phase: &str,
    tasks: &[PlannedTaskRecord],
) -> anyhow::Result<()> {
    ensure_project_archive_manifest(archive_root, context.project_id)?;
    let goal_dir = goal_archive_dir(archive_root, context.project_id, context.goal_id);
    let next_plan_version = current_goal_plan_version(archive_root, context.goal_id)
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    let plan_path = goal_dir.join("plan.md");
    let existing_task_policy_defaults = ManifestParser::new()
        .parse_goal(&plan_path)
        .ok()
        .map(|goal| goal.task_policy_defaults)
        .unwrap_or_default();
    let existing_policy_scopes = ManifestParser::new()
        .parse_goal(&plan_path)
        .ok()
        .map(|goal| goal.policy_scopes)
        .unwrap_or_default();
    write_markdown_atomic(
        &plan_path,
        &render_goal_plan_markdown(
            context,
            phase,
            tasks,
            next_plan_version,
            &existing_policy_scopes,
            &existing_task_policy_defaults,
        ),
    )?;

    let tasks_dir = goal_dir.join("tasks");
    fs::create_dir_all(&tasks_dir)?;
    let existing_tasks = load_goal_tasks_from_archive(archive_root, context.goal_id)
        .into_iter()
        .map(|task| (task.task_id.clone(), task))
        .collect::<HashMap<_, _>>();
    let active_task_ids = tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect::<HashSet<_>>();
    let replacement_map = replacement_map(tasks);
    let supersession_edges = supersession_edges(&replacement_map);
    let mut owner_change_edges = Vec::new();
    let mut added = Vec::new();
    let mut preserved = Vec::new();
    let mut deactivated = Vec::new();
    for task in tasks {
        let task_doc = sanitize_archive_component(&task.task_id);
        let task_path = tasks_dir.join(format!("{task_doc}.md"));
        let merged_task = match existing_tasks.get(&task.task_id) {
            Some(existing) => {
                preserved.push(task.task_id.clone());
                let merged = merge_planned_task_record(existing, task, next_plan_version);
                if effective_task_owner(existing) != effective_task_owner(&merged) {
                    owner_change_edges.push(format!(
                        "{}:{}->{}",
                        task.task_id,
                        effective_task_owner(existing).unwrap_or_else(|| "unassigned".to_string()),
                        effective_task_owner(&merged).unwrap_or_else(|| "unassigned".to_string())
                    ));
                }
                merged
            }
            None => {
                added.push(task.task_id.clone());
                with_plan_version(task.clone(), next_plan_version)
            }
        };
        write_markdown_atomic(
            &task_path,
            &render_goal_task_markdown(context, &merged_task),
        )?;
    }
    for existing in existing_tasks.values() {
        if active_task_ids.contains(existing.task_id.as_str()) {
            continue;
        }
        let superseded_by = replacement_map
            .get(existing.task_id.as_str())
            .cloned()
            .unwrap_or_default();
        let deactivated_record =
            deactivate_planned_task_record(existing, next_plan_version, &superseded_by);
        let task_doc = sanitize_archive_component(&existing.task_id);
        let task_path = tasks_dir.join(format!("{task_doc}.md"));
        write_markdown_atomic(
            &task_path,
            &render_goal_task_markdown(context, &deactivated_record),
        )?;
        deactivated.push(existing.task_id.clone());
    }
    owner_change_edges.sort();
    append_goal_event_archive(
        archive_root,
        context,
        "plan_reconciled",
        next_plan_version,
        &format!(
            "added={} preserved={} deactivated={}",
            added.join(", "),
            preserved.join(", "),
            deactivated.join(", ")
        ),
        GoalEventMetadata {
            added_task_ids: &added,
            preserved_task_ids: &preserved,
            deactivated_task_ids: &deactivated,
            supersession_edges: &supersession_edges,
            owner_change_edges: &owner_change_edges,
            ..GoalEventMetadata::default()
        },
    )?;
    Ok(())
}

fn with_plan_version(mut task: PlannedTaskRecord, plan_version: u32) -> PlannedTaskRecord {
    task.plan_version = plan_version;
    task
}

fn replacement_map(tasks: &[PlannedTaskRecord]) -> HashMap<&str, Vec<String>> {
    let mut replacements = HashMap::<&str, Vec<String>>::new();
    for task in tasks {
        for replaced_task_id in &task.replaces {
            replacements
                .entry(replaced_task_id.as_str())
                .or_default()
                .push(task.task_id.clone());
        }
    }
    for task_ids in replacements.values_mut() {
        task_ids.sort();
        task_ids.dedup();
    }
    replacements
}

fn supersession_edges(replacements: &HashMap<&str, Vec<String>>) -> Vec<String> {
    let mut edges = Vec::new();
    for (replaced_task_id, replacement_task_ids) in replacements {
        for replacement_task_id in replacement_task_ids {
            edges.push(format!("{replaced_task_id}->{replacement_task_id}"));
        }
    }
    edges.sort();
    edges
}

fn effective_task_owner(task: &PlannedTaskRecord) -> Option<String> {
    task.owner_hint
        .clone()
        .or_else(|| task.role.as_deref().map(|role| format!("role:{role}")))
}

fn merge_declared_context(
    existing: &GoalTaskDeclaredContext,
    incoming: &GoalTaskDeclaredContext,
) -> GoalTaskDeclaredContext {
    GoalTaskDeclaredContext {
        review_target: incoming
            .review_target
            .clone()
            .or_else(|| existing.review_target.clone()),
        waiting_for: incoming
            .waiting_for
            .clone()
            .or_else(|| existing.waiting_for.clone()),
        coordination_target: incoming
            .coordination_target
            .clone()
            .or_else(|| existing.coordination_target.clone()),
        external_dependency: incoming
            .external_dependency
            .clone()
            .or_else(|| existing.external_dependency.clone()),
    }
}

fn merge_task_policy(existing: &GoalTaskPolicy, incoming: &GoalTaskPolicy) -> GoalTaskPolicy {
    GoalTaskPolicy {
        escalation: incoming
            .escalation
            .clone()
            .or_else(|| existing.escalation.clone()),
        timing: incoming.timing.clone().or_else(|| existing.timing.clone()),
    }
}

fn lateness_basis_label(value: GoalTaskLatenessBasis) -> &'static str {
    match value {
        GoalTaskLatenessBasis::WallClock => "wall_clock",
        GoalTaskLatenessBasis::DeliveryWindowElapsed => "delivery_window_elapsed",
    }
}

fn weekday_label(value: GoalTaskWeekday) -> &'static str {
    match value {
        GoalTaskWeekday::Mon => "mon",
        GoalTaskWeekday::Tue => "tue",
        GoalTaskWeekday::Wed => "wed",
        GoalTaskWeekday::Thu => "thu",
        GoalTaskWeekday::Fri => "fri",
        GoalTaskWeekday::Sat => "sat",
        GoalTaskWeekday::Sun => "sun",
    }
}

fn render_timing_yaml(timing: &GoalTaskTimingConfig, indent: usize) -> String {
    let prefix = " ".repeat(indent);
    let nested = " ".repeat(indent + 2);
    let mut out = String::new();
    out.push_str(&format!(
        "{prefix}timezone: {}\n",
        timing
            .timezone
            .as_deref()
            .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
            .unwrap_or_else(|| "null".to_string())
    ));
    out.push_str(&format!(
        "{prefix}lateness_basis: {}\n",
        timing
            .lateness_basis
            .map(lateness_basis_label)
            .unwrap_or("null")
    ));
    if let Some(delivery_window) = timing.delivery_window.as_ref() {
        out.push_str(&format!("{prefix}delivery_window:\n"));
        out.push_str(&render_delivery_window_yaml(delivery_window, indent + 2));
    } else {
        out.push_str(&format!("{prefix}delivery_window: null\n"));
    }
    if out.ends_with(&nested) {
        out.truncate(out.trim_end_matches(&nested).len());
    }
    out
}

fn render_delivery_window_yaml(
    delivery_window: &GoalTaskDeliveryWindowConfig,
    indent: usize,
) -> String {
    let prefix = " ".repeat(indent);
    let mut out = String::new();
    out.push_str(&format!(
        "{prefix}mode: {}\n",
        match delivery_window.mode {
            GoalTaskDeliveryWindowMode::Anytime => "anytime",
            GoalTaskDeliveryWindowMode::OutsideQuietHours => "outside_quiet_hours",
            GoalTaskDeliveryWindowMode::WorkingHours => "working_hours",
            GoalTaskDeliveryWindowMode::Custom => "custom",
        }
    ));
    match delivery_window.quiet_hours.as_ref() {
        Some(quiet_hours) => {
            out.push_str(&format!("{prefix}quiet_hours:\n"));
            out.push_str(&render_quiet_hours_yaml(quiet_hours, indent + 2));
        }
        None => out.push_str(&format!("{prefix}quiet_hours: null\n")),
    }
    match delivery_window.working_hours.as_ref() {
        Some(working_hours) => {
            out.push_str(&format!("{prefix}working_hours:\n"));
            out.push_str(&render_working_hours_yaml(working_hours, indent + 2));
        }
        None => out.push_str(&format!("{prefix}working_hours: null\n")),
    }
    out
}

fn render_quiet_hours_yaml(quiet_hours: &GoalTaskQuietHoursWindow, indent: usize) -> String {
    let prefix = " ".repeat(indent);
    format!(
        "{prefix}start_local: \"{}\"\n{prefix}end_local: \"{}\"\n",
        quiet_hours.start_local, quiet_hours.end_local
    )
}

fn render_working_hours_yaml(working_hours: &GoalTaskWorkingHoursWindow, indent: usize) -> String {
    let prefix = " ".repeat(indent);
    let weekdays = if working_hours.weekdays.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            working_hours
                .weekdays
                .iter()
                .map(|value| weekday_label(*value))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "{prefix}weekdays: {weekdays}\n{prefix}start_local: \"{}\"\n{prefix}end_local: \"{}\"\n",
        working_hours.start_local, working_hours.end_local
    )
}

fn delivery_window_summary(delivery_window: &GoalTaskDeliveryWindowConfig) -> String {
    match delivery_window.mode {
        GoalTaskDeliveryWindowMode::Anytime => "anytime".to_string(),
        GoalTaskDeliveryWindowMode::OutsideQuietHours => match delivery_window.quiet_hours.as_ref()
        {
            Some(quiet_hours) => format!(
                "outside quiet hours {}-{}",
                quiet_hours.start_local, quiet_hours.end_local
            ),
            None => "outside quiet hours".to_string(),
        },
        GoalTaskDeliveryWindowMode::WorkingHours | GoalTaskDeliveryWindowMode::Custom => {
            match delivery_window.working_hours.as_ref() {
                Some(working_hours) => {
                    let weekdays = if working_hours.weekdays.is_empty() {
                        "all days".to_string()
                    } else {
                        working_hours
                            .weekdays
                            .iter()
                            .map(|value| weekday_label(*value))
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    format!(
                        "{} {} {}-{}",
                        match delivery_window.mode {
                            GoalTaskDeliveryWindowMode::WorkingHours => "working hours",
                            GoalTaskDeliveryWindowMode::Custom => "custom window",
                            _ => unreachable!(),
                        },
                        weekdays,
                        working_hours.start_local,
                        working_hours.end_local
                    )
                }
                None => match delivery_window.mode {
                    GoalTaskDeliveryWindowMode::WorkingHours => "working hours".to_string(),
                    GoalTaskDeliveryWindowMode::Custom => "custom window".to_string(),
                    _ => unreachable!(),
                },
            }
        }
    }
}

fn merge_planned_task_record(
    existing: &PlannedTaskRecord,
    incoming: &PlannedTaskRecord,
    plan_version: u32,
) -> PlannedTaskRecord {
    PlannedTaskRecord {
        task_id: incoming.task_id.clone(),
        task_slug: incoming.task_slug.clone(),
        task_kind: incoming.task_kind,
        task_driver: incoming.task_driver,
        title: incoming.title.clone(),
        summary: incoming.summary.clone(),
        state: GoalTaskPlanState::Active,
        execution_status: existing.execution_status,
        role: incoming.role.clone().or_else(|| existing.role.clone()),
        depends_on: incoming.depends_on.clone(),
        questionnaire_context: if incoming.questionnaire_context.is_empty() {
            existing.questionnaire_context.clone()
        } else {
            incoming.questionnaire_context.clone()
        },
        owner_hint: incoming
            .owner_hint
            .clone()
            .or_else(|| existing.owner_hint.clone()),
        declared_context: merge_declared_context(
            &existing.declared_context,
            &incoming.declared_context,
        ),
        policy: merge_task_policy(&existing.policy, &incoming.policy),
        retry_count: existing.retry_count,
        reopen_count: existing.reopen_count,
        last_status_change_at: existing.last_status_change_at,
        plan_version,
        superseded_by: Vec::new(),
        derived_from: if incoming.derived_from.is_empty() {
            existing.derived_from.clone()
        } else {
            incoming.derived_from.clone()
        },
        replaces: if incoming.replaces.is_empty() {
            existing.replaces.clone()
        } else {
            incoming.replaces.clone()
        },
    }
}

fn deactivate_planned_task_record(
    existing: &PlannedTaskRecord,
    plan_version: u32,
    superseded_by: &[String],
) -> PlannedTaskRecord {
    let mut updated = existing.clone();
    updated.plan_version = plan_version;
    updated.superseded_by = superseded_by.to_vec();
    if superseded_by.is_empty() {
        match existing.execution_status {
            GoalTaskStatus::Planned => {
                updated.state = GoalTaskPlanState::Cancelled;
                updated.execution_status = GoalTaskStatus::Cancelled;
            }
            GoalTaskStatus::Cancelled => {
                updated.state = GoalTaskPlanState::Cancelled;
            }
            GoalTaskStatus::Done => {
                updated.state = GoalTaskPlanState::Superseded;
            }
            GoalTaskStatus::InProgress | GoalTaskStatus::Blocked => {
                updated.state = GoalTaskPlanState::Superseded;
                updated.execution_status = GoalTaskStatus::Cancelled;
            }
        }
    } else {
        updated.state = GoalTaskPlanState::Superseded;
        if !matches!(existing.execution_status, GoalTaskStatus::Done) {
            updated.execution_status = GoalTaskStatus::Cancelled;
        }
    }
    updated
}

fn current_goal_plan_version(archive_root: &Path, goal_id: &str) -> Option<u32> {
    let plan_path = resolve_goal_archive_dir(archive_root, goal_id)?.join("plan.md");
    if !plan_path.exists() {
        return None;
    }
    ManifestParser::new()
        .parse_goal(&plan_path)
        .ok()
        .map(|goal| goal.plan_version)
}

fn resolve_goal_archive_dir(archive_root: &Path, goal_id: &str) -> Option<PathBuf> {
    let parser = ManifestParser::new();
    let state = parser.parse_all(archive_root).ok()?;
    let goal = state
        .goals
        .into_iter()
        .find(|goal| goal.id == goal_id || goal.slug == goal_id)?;
    Some(goal_archive_dir(archive_root, &goal.project_id, &goal.slug))
}

#[derive(Default)]
pub(crate) struct GoalEventMetadata<'a> {
    pub(crate) task_id: Option<&'a str>,
    pub(crate) previous_status: Option<&'a str>,
    pub(crate) next_status: Option<&'a str>,
    pub(crate) previous_owner: Option<&'a str>,
    pub(crate) next_owner: Option<&'a str>,
    pub(crate) escalation_policy: Option<&'a str>,
    pub(crate) escalation_trigger: Option<&'a str>,
    pub(crate) escalation_audience: Option<&'a str>,
    pub(crate) escalation_severity: Option<&'a str>,
    pub(crate) escalation_count: Option<u32>,
    pub(crate) cooldown_until: Option<i64>,
    pub(crate) condition_kind: Option<&'a str>,
    pub(crate) condition_value: Option<&'a str>,
    pub(crate) actor: Option<&'a str>,
    pub(crate) note: Option<&'a str>,
    pub(crate) added_task_ids: &'a [String],
    pub(crate) preserved_task_ids: &'a [String],
    pub(crate) deactivated_task_ids: &'a [String],
    pub(crate) supersession_edges: &'a [String],
    pub(crate) owner_change_edges: &'a [String],
}

fn yaml_string_array(values: &[String]) -> String {
    if values.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            values
                .iter()
                .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

pub(crate) fn append_goal_event_archive(
    archive_root: &Path,
    context: &GoalHierarchyContext<'_>,
    event_type: &str,
    plan_version: u32,
    detail: &str,
    metadata: GoalEventMetadata<'_>,
) -> anyhow::Result<()> {
    ensure_project_archive_manifest(archive_root, context.project_id)?;
    let goal_dir = goal_archive_dir(archive_root, context.project_id, context.goal_id);
    let events_dir = goal_dir.join("events");
    fs::create_dir_all(&events_dir)?;
    let event_path = events_dir.join(format!(
        "{}-{}-{}.md",
        context.observed_at,
        sanitize_archive_component(event_type),
        sanitize_archive_component(detail)
    ));
    let content = format!(
        "---\ngoal_id: \"{}\"\nevent_type: \"{}\"\nobserved_at: {}\nplan_version: {}\nthread_id: {}\ntask_id: {}\nprevious_status: {}\nnext_status: {}\nprevious_owner: {}\nnext_owner: {}\nescalation_policy: {}\nescalation_trigger: {}\nescalation_audience: {}\nescalation_severity: {}\nescalation_count: {}\ncooldown_until: {}\ncondition_kind: {}\ncondition_value: {}\nactor: {}\nnote: {}\nadded_task_ids: {}\npreserved_task_ids: {}\ndeactivated_task_ids: {}\nsupersession_edges: {}\nowner_change_edges: {}\n---\n\n# {}\n\n{}\n",
        context.goal_id,
        event_type,
        context.observed_at,
        plan_version,
        context
            .thread_id
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .task_id
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .previous_status
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .next_status
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .previous_owner
            .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .next_owner
            .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .escalation_policy
            .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .escalation_trigger
            .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .escalation_audience
            .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .escalation_severity
            .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .escalation_count
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .cooldown_until
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .condition_kind
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .condition_value
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .actor
            .map(|value| format!("\"{}\"", value))
            .unwrap_or_else(|| "null".to_string()),
        metadata
            .note
            .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
            .unwrap_or_else(|| "null".to_string()),
        yaml_string_array(metadata.added_task_ids),
        yaml_string_array(metadata.preserved_task_ids),
        yaml_string_array(metadata.deactivated_task_ids),
        yaml_string_array(metadata.supersession_edges),
        yaml_string_array(metadata.owner_change_edges),
        event_type.replace('_', " "),
        detail
    );
    write_markdown_atomic(&event_path, &content)
}

pub(crate) fn escalation_policy_label(policy: GoalTaskEscalationPolicy) -> &'static str {
    match policy {
        GoalTaskEscalationPolicy::NotifyOperator => "notify_operator",
        GoalTaskEscalationPolicy::RaiseAlert => "raise_alert",
        GoalTaskEscalationPolicy::AutoReplan => "auto_replan",
    }
}

pub(crate) fn escalation_severity_label(severity: GoalTaskEscalationSeverity) -> &'static str {
    match severity {
        GoalTaskEscalationSeverity::Normal => "normal",
        GoalTaskEscalationSeverity::High => "high",
        GoalTaskEscalationSeverity::Urgent => "urgent",
        GoalTaskEscalationSeverity::Critical => "critical",
    }
}

pub(crate) fn default_escalation_severity(
    policy: GoalTaskEscalationPolicy,
) -> GoalTaskEscalationSeverity {
    match policy {
        GoalTaskEscalationPolicy::NotifyOperator | GoalTaskEscalationPolicy::AutoReplan => {
            GoalTaskEscalationSeverity::Normal
        }
        GoalTaskEscalationPolicy::RaiseAlert => GoalTaskEscalationSeverity::High,
    }
}

pub(crate) fn escalation_audience_label(
    audience: Option<&str>,
    policy: GoalTaskEscalationPolicy,
) -> &str {
    audience.unwrap_or(match policy {
        GoalTaskEscalationPolicy::NotifyOperator
        | GoalTaskEscalationPolicy::RaiseAlert
        | GoalTaskEscalationPolicy::AutoReplan => "operator",
    })
}

pub(crate) fn escalation_targets_alerts(
    policy: GoalTaskEscalationPolicy,
    audience: &str,
    severity: GoalTaskEscalationSeverity,
) -> bool {
    matches!(policy, GoalTaskEscalationPolicy::RaiseAlert)
        || !audience.eq_ignore_ascii_case("operator")
        || matches!(
            severity,
            GoalTaskEscalationSeverity::Urgent | GoalTaskEscalationSeverity::Critical
        )
}

pub(crate) fn declared_task_escalation_detail(
    task: &PlannedTaskRecord,
    note: Option<&str>,
) -> String {
    if let Some(note) = note.filter(|value| !value.trim().is_empty()) {
        return note.to_string();
    }
    if let Some(waiting_for) = task.declared_context.waiting_for.as_deref() {
        return format!("Blocked waiting for {waiting_for}");
    }
    if let Some(review_target) = task.declared_context.review_target.as_deref() {
        return format!("Blocked pending review of {review_target}");
    }
    if let Some(coordination_target) = task.declared_context.coordination_target.as_deref() {
        return format!("Blocked pending coordination with {coordination_target}");
    }
    if let Some(external_dependency) = task.declared_context.external_dependency.as_deref() {
        return format!("Blocked on external dependency {external_dependency}");
    }
    format!("Blocked task: {}", task.title)
}

fn declared_task_escalation(
    preferences: Option<&symbiotic_control_plane::types::PreferencesManifest>,
    archive_root: Option<&Path>,
    goal_manifest: Option<&symbiotic_control_plane::types::GoalManifest>,
    task: &PlannedTaskRecord,
    previous_status: GoalTaskStatus,
    next_status: GoalTaskStatus,
    note: Option<&str>,
) -> Option<DeclaredTaskEscalation> {
    if !matches!(task.task_driver, GoalTaskDriver::Declared) {
        return None;
    }
    if matches!(previous_status, GoalTaskStatus::Blocked)
        || !matches!(next_status, GoalTaskStatus::Blocked)
    {
        return None;
    }
    let goal_manifest = goal_manifest?;
    let escalation = effective_escalation_policy(preferences, archive_root, goal_manifest, task)?;
    if !escalation.on_enter_blocked {
        return None;
    }
    Some(DeclaredTaskEscalation {
        policy: escalation.mode,
        audience: Some(escalation.audience),
        severity: Some(escalation.severity),
        task_id: task.task_id.clone(),
        task_title: task.title.clone(),
        detail: declared_task_escalation_detail(task, note),
    })
}

pub(crate) fn infer_declared_task_resume_status(
    events: &[symbiotic_control_plane::types::GoalEventManifest],
    task_id: &str,
) -> GoalTaskStatus {
    let latest_blocked_transition = events.iter().rev().find(|event| {
        event.task_id.as_deref() == Some(task_id)
            && event.event_type == "task_status_changed"
            && event.next_status.as_deref() == Some("blocked")
    });

    match latest_blocked_transition.and_then(|event| event.previous_status.as_deref()) {
        Some("in_progress") => GoalTaskStatus::InProgress,
        Some("planned") => GoalTaskStatus::Planned,
        Some("blocked") | Some("done") | Some("cancelled") | Some(_) | None => {
            GoalTaskStatus::Planned
        }
    }
}

fn extract_task_summary(markdown: &str) -> String {
    let mut in_summary = false;
    let mut lines = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("## Summary") {
            in_summary = true;
            continue;
        }
        if in_summary && trimmed.starts_with("## ") {
            break;
        }
        if in_summary && !trimmed.is_empty() {
            lines.push(trimmed.trim_start_matches("- ").to_string());
        }
    }
    if !lines.is_empty() {
        lines.join(" ")
    } else {
        markdown
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .unwrap_or_default()
            .to_string()
    }
}

pub(crate) fn load_goal_tasks_from_archive(
    archive_root: &Path,
    goal_id: &str,
) -> Vec<PlannedTaskRecord> {
    let parser = ManifestParser::new();
    let Some(goal_dir) = resolve_goal_archive_dir(archive_root, goal_id) else {
        return Vec::new();
    };
    parser
        .parse_goal_tasks(&goal_dir)
        .into_iter()
        .map(|task| PlannedTaskRecord {
            task_id: task.task_id,
            task_slug: task.task_slug,
            task_kind: task.task_kind,
            task_driver: task.task_driver,
            title: task.title,
            summary: extract_task_summary(&task.task_markdown),
            state: task.state,
            execution_status: task.execution_status,
            role: task.role,
            depends_on: task.depends_on,
            questionnaire_context: task.questionnaire_context,
            owner_hint: task.owner_hint,
            declared_context: task.declared_context,
            policy: task.policy,
            retry_count: task.retry_count,
            reopen_count: task.reopen_count,
            last_status_change_at: task.last_status_change_at,
            plan_version: task.plan_version,
            superseded_by: task.superseded_by,
            derived_from: task.derived_from,
            replaces: task.replaces,
        })
        .collect()
}

pub(crate) fn load_goal_task_from_archive(
    archive_root: &Path,
    goal_id: &str,
    task_id: &str,
) -> anyhow::Result<Option<PlannedTaskRecord>> {
    let parser = ManifestParser::new();
    let Some(goal_dir) = resolve_goal_archive_dir(archive_root, goal_id) else {
        return Ok(None);
    };
    let task_path = goal_dir
        .join("tasks")
        .join(format!("{}.md", sanitize_archive_component(task_id)));
    if !task_path.exists() {
        return Ok(None);
    }
    let task = parser.parse_goal_task(&task_path)?;
    Ok(Some(PlannedTaskRecord {
        task_id: task.task_id,
        task_slug: task.task_slug,
        task_kind: task.task_kind,
        task_driver: task.task_driver,
        title: task.title,
        summary: extract_task_summary(&task.task_markdown),
        state: task.state,
        execution_status: task.execution_status,
        role: task.role,
        depends_on: task.depends_on,
        questionnaire_context: task.questionnaire_context,
        owner_hint: task.owner_hint,
        declared_context: task.declared_context,
        policy: task.policy,
        retry_count: task.retry_count,
        reopen_count: task.reopen_count,
        last_status_change_at: task.last_status_change_at,
        plan_version: task.plan_version,
        superseded_by: task.superseded_by,
        derived_from: task.derived_from,
        replaces: task.replaces,
    }))
}

fn task_needs_execution_child(task_driver: GoalTaskDriver) -> bool {
    matches!(task_driver, GoalTaskDriver::Agent)
}

fn review_mode_for_task(task: &PlannedTaskRecord) -> ReviewMode {
    match (task.task_kind, task.task_driver) {
        (GoalTaskKind::Review, GoalTaskDriver::Agent) => ReviewMode::AutoReviewThenHumanIfNeeded,
        (GoalTaskKind::Review, GoalTaskDriver::Declared) => ReviewMode::HumanRequired,
        (GoalTaskKind::Approval, _) => ReviewMode::HumanRequired,
        _ => ReviewMode::NoReview,
    }
}

fn work_item_status_for_task(task: &PlannedTaskRecord) -> WorkItemStatus {
    if matches!(task.task_driver, GoalTaskDriver::Declared) {
        return match task.task_kind {
            GoalTaskKind::Waiting => match task.execution_status {
                GoalTaskStatus::Planned => WorkItemStatus::Todo,
                GoalTaskStatus::InProgress => WorkItemStatus::Blocked,
                GoalTaskStatus::Blocked => WorkItemStatus::Blocked,
                GoalTaskStatus::Done => WorkItemStatus::Done,
                GoalTaskStatus::Cancelled => WorkItemStatus::Cancelled,
            },
            GoalTaskKind::Approval | GoalTaskKind::Review => match task.execution_status {
                GoalTaskStatus::Planned => WorkItemStatus::Todo,
                GoalTaskStatus::InProgress => WorkItemStatus::PendingReview,
                GoalTaskStatus::Blocked => WorkItemStatus::Blocked,
                GoalTaskStatus::Done => WorkItemStatus::Done,
                GoalTaskStatus::Cancelled => WorkItemStatus::Cancelled,
            },
            _ => work_item_status_from_task_status(task.execution_status),
        };
    }

    work_item_status_from_task_status(task.execution_status)
}

fn execution_title_for_task(task: &PlannedTaskRecord) -> String {
    let prefix = match task.task_kind {
        GoalTaskKind::Execution => "Execute",
        GoalTaskKind::Review => "Review",
        GoalTaskKind::Distillation => "Distill",
        GoalTaskKind::Coordination => "Coordinate",
        GoalTaskKind::Waiting => "Await",
        GoalTaskKind::Approval => "Approve",
    };
    format!("{prefix}: {}", task.title)
}

fn task_assignee_for_task(
    task: &PlannedTaskRecord,
    fallback_owner: Option<&str>,
    observed_at: i64,
) -> Option<AgentAssignment> {
    let agent_id = if let Some(owner_hint) = task.owner_hint.as_deref() {
        owner_hint.to_string()
    } else if let Some(role) = task.role.as_deref() {
        format!("role:{role}")
    } else {
        fallback_owner?.to_string()
    };
    Some(AgentAssignment {
        agent_id,
        runner_id: None,
        assigned_at: observed_at,
    })
}

pub(crate) fn load_active_goal_tasks_from_archive(
    archive_root: &Path,
    goal_id: &str,
) -> Vec<PlannedTaskRecord> {
    load_goal_tasks_from_archive(archive_root, goal_id)
        .into_iter()
        .filter(|task| matches!(task.state, GoalTaskPlanState::Active))
        .collect()
}

#[allow(clippy::type_complexity)] // (project_id, thread_id, title, detail) tuple is intentionally positional
pub(crate) fn load_goal_identity_from_archive(
    archive_root: &Path,
    goal_id: &str,
) -> anyhow::Result<Option<(String, String, String, Option<String>)>> {
    let parser = ManifestParser::new();
    let Some(goal_dir) = resolve_goal_archive_dir(archive_root, goal_id) else {
        return Ok(None);
    };
    let plan_path = goal_dir.join("plan.md");
    if !plan_path.exists() {
        return Ok(None);
    }
    let goal = parser.parse_goal(&plan_path)?;
    Ok(Some((
        goal.project_id,
        goal.title,
        extract_goal_summary(&goal.plan_markdown),
        goal.thread_id,
    )))
}

pub(crate) fn load_goal_events_from_archive(
    archive_root: &Path,
    goal_id: &str,
) -> anyhow::Result<Vec<symbiotic_control_plane::types::GoalEventManifest>> {
    let parser = ManifestParser::new();
    let Some(goal_dir) = resolve_goal_archive_dir(archive_root, goal_id) else {
        return Ok(Vec::new());
    };
    let events_dir = goal_dir.join("events");
    let Ok(entries) = fs::read_dir(events_dir) else {
        return Ok(Vec::new());
    };

    let mut events = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        match parser.parse_goal_event(&path) {
            Ok(event) => events.push(event),
            Err(error) => tracing::warn!(
                goal_id = %goal_id,
                path = %path.display(),
                %error,
                "goal_management: failed to parse archive goal event"
            ),
        }
    }
    events.sort_by_key(|event| event.observed_at);
    Ok(events)
}

pub(crate) fn load_pending_goal_replan_requests(
    archive_root: &Path,
) -> anyhow::Result<Vec<PendingGoalReplanRequest>> {
    let parser = ManifestParser::new();
    let state = match parser.parse_all(archive_root) {
        Ok(state) => state,
        Err(_) => return Ok(Vec::new()),
    };

    let mut pending = Vec::new();
    for goal in state.goals {
        let events_dir =
            goal_archive_dir(archive_root, &goal.project_id, &goal.slug).join("events");
        let Ok(event_entries) = fs::read_dir(&events_dir) else {
            continue;
        };

        let mut latest_requests: HashMap<
            String,
            symbiotic_control_plane::types::GoalEventManifest,
        > = HashMap::new();
        let mut latest_enqueued_at: HashMap<String, i64> = HashMap::new();
        let mut latest_plan_reconciled_at: Option<i64> = None;

        for event_entry in event_entries.flatten() {
            let path = event_entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Ok(event) = parser.parse_goal_event(&path) else {
                continue;
            };
            match event.event_type.as_str() {
                "task_replan_requested" => {
                    if let Some(task_id) = event.task_id.clone() {
                        let should_replace = latest_requests
                            .get(&task_id)
                            .map(|existing| existing.observed_at < event.observed_at)
                            .unwrap_or(true);
                        if should_replace {
                            latest_requests.insert(task_id, event);
                        }
                    }
                }
                "task_replan_enqueued" => {
                    if let Some(task_id) = event.task_id {
                        latest_enqueued_at
                            .entry(task_id)
                            .and_modify(|existing| *existing = (*existing).max(event.observed_at))
                            .or_insert(event.observed_at);
                    }
                }
                "plan_reconciled" => {
                    latest_plan_reconciled_at = Some(
                        latest_plan_reconciled_at
                            .map(|existing| existing.max(event.observed_at))
                            .unwrap_or(event.observed_at),
                    );
                }
                _ => {}
            }
        }

        for (task_id, event) in latest_requests {
            let consumed_at = latest_enqueued_at
                .get(&task_id)
                .copied()
                .into_iter()
                .chain(latest_plan_reconciled_at)
                .max()
                .unwrap_or(i64::MIN);
            if event.observed_at <= consumed_at {
                continue;
            }
            pending.push(PendingGoalReplanRequest {
                goal_id: event.goal_id,
                task_id,
                thread_id: event.thread_id,
                detail: event.note,
                observed_at: event.observed_at,
            });
        }
    }

    pending.sort_by_key(|request| request.observed_at);
    Ok(pending)
}

pub(crate) fn append_goal_replan_enqueued_archive(
    archive_root: &Path,
    context: &GoalHierarchyContext<'_>,
    task_id: &str,
    detail: &str,
    actor: Option<&str>,
) -> anyhow::Result<()> {
    let plan_version = current_goal_plan_version(archive_root, context.goal_id).unwrap_or(1);
    append_goal_event_archive(
        archive_root,
        context,
        "task_replan_enqueued",
        plan_version,
        &format!("task={task_id} replan enqueued"),
        GoalEventMetadata {
            task_id: Some(task_id),
            actor,
            note: Some(detail),
            ..GoalEventMetadata::default()
        },
    )
}

pub(crate) fn ensure_goal_work_item(
    store: &mut ManagementStore,
    context: &GoalHierarchyContext<'_>,
    status: WorkItemStatus,
) -> anyhow::Result<String> {
    let work_item_id = goal_work_item_id(context.goal_id);
    let mut work_item = store
        .get_work_item(&work_item_id)
        .cloned()
        .unwrap_or_else(|| WorkItem {
            id: work_item_id.clone(),
            project_id: "symbiotic".to_string(),
            initiative_id: Some(context.goal_id.to_string()),
            parent_work_item_id: None,
            kind: WorkItemKind::Goal,
            thread_id: context.thread_id.map(str::to_string),
            title: context.title.to_string(),
            summary: context.summary.to_string(),
            status,
            priority: WorkPriority::P1,
            urgency: WorkUrgency::Normal,
            assignment_mode: AssignmentMode::ParallelChildren,
            requested_scopes: Vec::new(),
            accepted_claim_ids: Vec::new(),
            assignee: None,
            blocked_by: Vec::new(),
            depends_on: Vec::new(),
            review_mode: ReviewMode::NoReview,
            cancellation: None,
            created_at: context.observed_at,
            updated_at: context.observed_at,
        });

    work_item.thread_id = context
        .thread_id
        .map(str::to_string)
        .or(work_item.thread_id);
    work_item.title = context.title.to_string();
    work_item.summary = context.summary.to_string();
    work_item.initiative_id = Some(context.goal_id.to_string());
    work_item.assignee = context.owner.map(|owner| AgentAssignment {
        agent_id: owner.to_string(),
        runner_id: None,
        assigned_at: context.observed_at,
    });
    work_item.set_status(status, context.observed_at);
    store.upsert_work_item(work_item)?;
    Ok(work_item_id)
}

pub(crate) fn ensure_goal_task_work_item(
    store: &mut ManagementStore,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
) -> anyhow::Result<String> {
    let task_id = goal_task_work_item_id(context.goal_id, &task.task_id);
    let mut work_item = store
        .get_work_item(&task_id)
        .cloned()
        .unwrap_or_else(|| WorkItem {
            id: task_id.clone(),
            project_id: "symbiotic".to_string(),
            initiative_id: Some(context.goal_id.to_string()),
            parent_work_item_id: Some(goal_work_item_id(context.goal_id)),
            kind: WorkItemKind::Task,
            thread_id: context.thread_id.map(str::to_string),
            title: task.title.to_string(),
            summary: management_summary_for_task(task),
            status: work_item_status_for_task(task),
            priority: WorkPriority::P2,
            urgency: WorkUrgency::Normal,
            assignment_mode: if task_needs_execution_child(task.task_driver) {
                AssignmentMode::ParallelChildren
            } else {
                AssignmentMode::SingleOwner
            },
            requested_scopes: Vec::new(),
            accepted_claim_ids: Vec::new(),
            assignee: task_assignee_for_task(task, context.owner, context.observed_at),
            blocked_by: Vec::new(),
            depends_on: Vec::new(),
            review_mode: review_mode_for_task(task),
            cancellation: None,
            created_at: context.observed_at,
            updated_at: context.observed_at,
        });

    work_item.thread_id = context
        .thread_id
        .map(str::to_string)
        .or(work_item.thread_id);
    work_item.title = task.title.to_string();
    work_item.summary = management_summary_for_task(task);
    work_item.parent_work_item_id = Some(goal_work_item_id(context.goal_id));
    work_item.initiative_id = Some(context.goal_id.to_string());
    work_item.depends_on = task
        .depends_on
        .iter()
        .map(|slug| goal_task_work_item_id(context.goal_id, slug))
        .collect();
    work_item.assignment_mode = if task_needs_execution_child(task.task_driver) {
        AssignmentMode::ParallelChildren
    } else {
        AssignmentMode::SingleOwner
    };
    work_item.review_mode = review_mode_for_task(task);
    work_item.assignee = task_assignee_for_task(task, context.owner, context.observed_at);
    work_item.set_status(work_item_status_for_task(task), context.observed_at);
    store.upsert_work_item(work_item)?;
    Ok(task_id)
}

pub(crate) fn ensure_goal_execution_work_item(
    store: &mut ManagementStore,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    role: Option<&str>,
    status: WorkItemStatus,
) -> anyhow::Result<String> {
    let execution_id = goal_execution_work_item_id(context.goal_id, &task.task_id);
    let mut work_item = store
        .get_work_item(&execution_id)
        .cloned()
        .unwrap_or_else(|| WorkItem {
            id: execution_id.clone(),
            project_id: "symbiotic".to_string(),
            initiative_id: Some(context.goal_id.to_string()),
            parent_work_item_id: Some(goal_task_work_item_id(context.goal_id, &task.task_id)),
            kind: WorkItemKind::Execution,
            thread_id: context.thread_id.map(str::to_string),
            title: execution_title_for_task(task),
            summary: management_summary_for_task(task),
            status,
            priority: WorkPriority::P2,
            urgency: WorkUrgency::Normal,
            assignment_mode: AssignmentMode::SingleOwner,
            requested_scopes: Vec::new(),
            accepted_claim_ids: Vec::new(),
            assignee: role.map(|role| AgentAssignment {
                agent_id: format!("role:{role}"),
                runner_id: None,
                assigned_at: context.observed_at,
            }),
            blocked_by: Vec::new(),
            depends_on: Vec::new(),
            review_mode: review_mode_for_task(task),
            cancellation: None,
            created_at: context.observed_at,
            updated_at: context.observed_at,
        });

    work_item.thread_id = context
        .thread_id
        .map(str::to_string)
        .or(work_item.thread_id);
    work_item.title = execution_title_for_task(task);
    work_item.summary = match role {
        Some(role) if !role.is_empty() => {
            format!(
                "Execution lane for role '{role}'. {}",
                management_summary_for_task(task)
            )
        }
        _ => management_summary_for_task(task),
    };
    work_item.parent_work_item_id = Some(goal_task_work_item_id(context.goal_id, &task.task_id));
    work_item.initiative_id = Some(context.goal_id.to_string());
    work_item.depends_on = task
        .depends_on
        .iter()
        .map(|slug| goal_execution_work_item_id(context.goal_id, slug))
        .collect();
    work_item.review_mode = review_mode_for_task(task);
    work_item.assignee = role.map(|role| AgentAssignment {
        agent_id: format!("role:{role}"),
        runner_id: None,
        assigned_at: context.observed_at,
    });
    work_item.set_status(status, context.observed_at);
    store.upsert_work_item(work_item)?;
    Ok(execution_id)
}

pub(crate) fn refresh_parent_work_item_status(
    store: &mut ManagementStore,
    parent_work_item_id: &str,
    observed_at: i64,
) -> anyhow::Result<()> {
    let Some(mut parent) = store.get_work_item(parent_work_item_id).cloned() else {
        return Ok(());
    };
    let children = store.work_items_for_parent(parent_work_item_id);
    if children.is_empty() {
        return Ok(());
    }

    let next_status = if children
        .iter()
        .any(|item| matches!(item.status, WorkItemStatus::Blocked))
    {
        WorkItemStatus::Blocked
    } else if children
        .iter()
        .any(|item| matches!(item.status, WorkItemStatus::PendingReview))
    {
        WorkItemStatus::PendingReview
    } else if children.iter().any(|item| {
        matches!(
            item.status,
            WorkItemStatus::Running | WorkItemStatus::Claimed | WorkItemStatus::ClaimPending
        )
    }) {
        WorkItemStatus::Running
    } else if children
        .iter()
        .all(|item| matches!(item.status, WorkItemStatus::Done))
    {
        WorkItemStatus::Done
    } else if children
        .iter()
        .all(|item| matches!(item.status, WorkItemStatus::Cancelled))
    {
        WorkItemStatus::Cancelled
    } else if children.iter().any(|item| {
        matches!(
            item.status,
            WorkItemStatus::Expired | WorkItemStatus::Failed
        )
    }) {
        WorkItemStatus::Failed
    } else {
        WorkItemStatus::Todo
    };

    parent.set_status(next_status, observed_at);
    store.upsert_work_item(parent)
}

pub(crate) fn sync_goal_plan_hierarchy(
    management_store: &Arc<Mutex<ManagementStore>>,
    context: &GoalHierarchyContext<'_>,
    tasks: &[PlannedTaskRecord],
    goal_status: WorkItemStatus,
) {
    let mut store = match management_store.lock() {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(
                goal_id = %context.goal_id,
                %error,
                "goal_management: failed to lock management store for plan hierarchy"
            );
            return;
        }
    };

    if let Err(error) = ensure_goal_work_item(&mut store, context, goal_status) {
        tracing::warn!(
            goal_id = %context.goal_id,
            %error,
            "goal_management: failed to persist goal work item from approved plan"
        );
        return;
    }

    for task in tasks {
        if let Err(error) = ensure_goal_task_work_item(&mut store, context, task) {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to persist planned task work item"
            );
            continue;
        }
        if task_needs_execution_child(task.task_driver) {
            if let Err(error) = ensure_goal_execution_work_item(
                &mut store,
                context,
                task,
                task.role.as_deref(),
                work_item_status_from_task_status(task.execution_status),
            ) {
                tracing::warn!(
                    goal_id = %context.goal_id,
                    task_slug = %task.task_id,
                    %error,
                    "goal_management: failed to persist planned execution work item"
                );
                continue;
            }
            let task_id = goal_task_work_item_id(context.goal_id, &task.task_id);
            if let Err(error) =
                refresh_parent_work_item_status(&mut store, &task_id, context.observed_at)
            {
                tracing::warn!(
                    goal_id = %context.goal_id,
                    task_slug = %task.task_id,
                    %error,
                    "goal_management: failed to refresh planned task status"
                );
            }
        }
    }

    let goal_id = goal_work_item_id(context.goal_id);
    if let Err(error) = refresh_parent_work_item_status(&mut store, &goal_id, context.observed_at) {
        tracing::warn!(
            goal_id = %context.goal_id,
            %error,
            "goal_management: failed to refresh goal status from planned tasks"
        );
    }
}

pub(crate) fn sync_goal_task_execution_status(
    management_store: &Arc<Mutex<ManagementStore>>,
    archive_root: Option<&Path>,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    status: WorkItemStatus,
) {
    let mut store = match management_store.lock() {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(
                goal_id = %context.goal_id,
                %error,
                "goal_management: failed to lock management store for execution sync"
            );
            return;
        }
    };

    if let Err(error) = ensure_goal_work_item(&mut store, context, WorkItemStatus::Running) {
        tracing::warn!(
            goal_id = %context.goal_id,
            %error,
            "goal_management: failed to ensure goal work item for execution sync"
        );
        return;
    }

    if let Err(error) = ensure_goal_task_work_item(&mut store, context, task) {
        tracing::warn!(
            goal_id = %context.goal_id,
            task_slug = %task.task_id,
            %error,
            "goal_management: failed to ensure task work item for execution sync"
        );
        return;
    }

    if task_needs_execution_child(task.task_driver) {
        if let Err(error) =
            ensure_goal_execution_work_item(&mut store, context, task, task.role.as_deref(), status)
        {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to sync execution work item status"
            );
            return;
        }

        let task_id = goal_task_work_item_id(context.goal_id, &task.task_id);
        if let Err(error) =
            refresh_parent_work_item_status(&mut store, &task_id, context.observed_at)
        {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to refresh parent task after execution sync"
            );
            return;
        }
    }

    let goal_id = goal_work_item_id(context.goal_id);
    if let Err(error) = refresh_parent_work_item_status(&mut store, &goal_id, context.observed_at) {
        tracing::warn!(
            goal_id = %context.goal_id,
            %error,
            "goal_management: failed to refresh goal after execution sync"
        );
    }

    if let Some(archive_root) = archive_root {
        if let Err(error) = persist_goal_task_status_archive(archive_root, context, task, status) {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to persist task status to archive"
            );
        }
    }
}

pub(crate) fn sync_goal_task_declared_status(
    management_store: &Arc<Mutex<ManagementStore>>,
    archive_root: Option<&Path>,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    next_status: GoalTaskStatus,
    actor: Option<&str>,
    note: Option<&str>,
) -> Option<DeclaredTaskEscalation> {
    let mut updated_task = task.clone();
    updated_task.execution_status = next_status;
    updated_task.last_status_change_at = Some(context.observed_at);

    let mut store = match management_store.lock() {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to lock management store for declared task sync"
            );
            return None;
        }
    };

    if let Err(error) = ensure_goal_work_item(&mut store, context, WorkItemStatus::Running) {
        tracing::warn!(
            goal_id = %context.goal_id,
            task_slug = %task.task_id,
            %error,
            "goal_management: failed to ensure goal work item for declared task sync"
        );
        return None;
    }

    if let Err(error) = ensure_goal_task_work_item(&mut store, context, &updated_task) {
        tracing::warn!(
            goal_id = %context.goal_id,
            task_slug = %task.task_id,
            %error,
            "goal_management: failed to ensure task work item for declared task sync"
        );
        return None;
    }

    if task_needs_execution_child(updated_task.task_driver) {
        if let Err(error) = ensure_goal_execution_work_item(
            &mut store,
            context,
            &updated_task,
            updated_task.role.as_deref(),
            work_item_status_from_task_status(next_status),
        ) {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to ensure execution work item for declared task sync"
            );
            return None;
        }
        let task_id = goal_task_work_item_id(context.goal_id, &updated_task.task_id);
        if let Err(error) =
            refresh_parent_work_item_status(&mut store, &task_id, context.observed_at)
        {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to refresh task parent after declared task sync"
            );
            return None;
        }
    }

    let goal_id = goal_work_item_id(context.goal_id);
    if let Err(error) = refresh_parent_work_item_status(&mut store, &goal_id, context.observed_at) {
        tracing::warn!(
            goal_id = %context.goal_id,
            task_slug = %task.task_id,
            %error,
            "goal_management: failed to refresh goal after declared task sync"
        );
    }

    drop(store);

    let mut escalation = None;
    if let Some(archive_root) = archive_root {
        match persist_goal_task_declared_status_archive(
            archive_root,
            context,
            task,
            next_status,
            actor,
            note,
        ) {
            Ok(result) => escalation = result,
            Err(error) => {
                tracing::warn!(
                    goal_id = %context.goal_id,
                    task_slug = %task.task_id,
                    %error,
                    "goal_management: failed to persist declared task status to archive"
                );
            }
        }
    }
    escalation
}

pub(crate) fn sync_goal_task_owner(
    management_store: &Arc<Mutex<ManagementStore>>,
    archive_root: Option<&Path>,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    next_owner_hint: &str,
    actor: Option<&str>,
    note: Option<&str>,
) {
    let mut updated_task = task.clone();
    updated_task.owner_hint = Some(next_owner_hint.to_string());

    let mut store = match management_store.lock() {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to lock management store for owner sync"
            );
            return;
        }
    };

    let goal_status = store
        .get_work_item(&goal_work_item_id(context.goal_id))
        .map(|item| item.status)
        .unwrap_or_else(|| work_item_status_for_task(&updated_task));
    if let Err(error) = ensure_goal_work_item(&mut store, context, goal_status) {
        tracing::warn!(
            goal_id = %context.goal_id,
            task_slug = %task.task_id,
            %error,
            "goal_management: failed to ensure goal work item for owner sync"
        );
        return;
    }

    if let Err(error) = ensure_goal_task_work_item(&mut store, context, &updated_task) {
        tracing::warn!(
            goal_id = %context.goal_id,
            task_slug = %task.task_id,
            %error,
            "goal_management: failed to ensure task work item for owner sync"
        );
        return;
    }

    let goal_id = goal_work_item_id(context.goal_id);
    if let Err(error) = refresh_parent_work_item_status(&mut store, &goal_id, context.observed_at) {
        tracing::warn!(
            goal_id = %context.goal_id,
            task_slug = %task.task_id,
            %error,
            "goal_management: failed to refresh goal after owner sync"
        );
    }

    drop(store);

    if let Some(archive_root) = archive_root {
        if let Err(error) = persist_goal_task_owner_archive(
            archive_root,
            context,
            task,
            next_owner_hint,
            actor,
            note,
        ) {
            tracing::warn!(
                goal_id = %context.goal_id,
                task_slug = %task.task_id,
                %error,
                "goal_management: failed to persist task owner to archive"
            );
        }
    }
}

fn work_item_status_from_task_status(status: GoalTaskStatus) -> WorkItemStatus {
    match status {
        GoalTaskStatus::Planned => WorkItemStatus::Todo,
        GoalTaskStatus::InProgress => WorkItemStatus::Running,
        GoalTaskStatus::Blocked => WorkItemStatus::Blocked,
        GoalTaskStatus::Done => WorkItemStatus::Done,
        GoalTaskStatus::Cancelled => WorkItemStatus::Cancelled,
    }
}

fn extract_goal_summary(markdown: &str) -> String {
    markdown
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("Archive-declared goal plan.")
        .to_string()
}

pub(crate) fn archive_task_status_from_work_item_status(status: WorkItemStatus) -> GoalTaskStatus {
    match status {
        WorkItemStatus::Todo | WorkItemStatus::ClaimPending | WorkItemStatus::Claimed => {
            GoalTaskStatus::Planned
        }
        WorkItemStatus::Running | WorkItemStatus::PendingReview => GoalTaskStatus::InProgress,
        WorkItemStatus::Blocked => GoalTaskStatus::Blocked,
        WorkItemStatus::Done => GoalTaskStatus::Done,
        WorkItemStatus::Cancelled => GoalTaskStatus::Cancelled,
        WorkItemStatus::Expired | WorkItemStatus::Failed => GoalTaskStatus::Blocked,
    }
}

pub(crate) fn persist_goal_task_status_archive(
    archive_root: &Path,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    status: WorkItemStatus,
) -> anyhow::Result<()> {
    persist_goal_task_declared_status_archive(
        archive_root,
        context,
        task,
        archive_task_status_from_work_item_status(status),
        None,
        None,
    )
    .map(|_| ())
}

pub(crate) fn persist_goal_task_declared_status_archive(
    archive_root: &Path,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    next_status: GoalTaskStatus,
    actor: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<Option<DeclaredTaskEscalation>> {
    let parser = ManifestParser::new();
    let preferences = parser
        .resolve_preferences_path(archive_root)
        .and_then(|path| parser.parse_preferences(&path).ok());
    let goal_path =
        goal_archive_dir(archive_root, context.project_id, context.goal_id).join("plan.md");
    let goal_manifest = goal_path
        .exists()
        .then(|| parser.parse_goal(&goal_path))
        .transpose()?;
    let task_path = goal_archive_dir(archive_root, context.project_id, context.goal_id)
        .join("tasks")
        .join(format!("{}.md", sanitize_archive_component(&task.task_id)));
    if !task_path.exists() {
        return Ok(None);
    }
    let manifest = parser.parse_goal_task(&task_path)?;
    let escalation = declared_task_escalation(
        preferences.as_ref(),
        Some(archive_root),
        goal_manifest.as_ref(),
        task,
        manifest.execution_status,
        next_status,
        note,
    );
    let mut updated = task.clone();
    updated.execution_status = next_status;
    updated.state = manifest.state;
    updated.retry_count = manifest.retry_count;
    updated.reopen_count = manifest.reopen_count;
    updated.last_status_change_at = Some(context.observed_at);
    updated.plan_version = manifest.plan_version;
    updated.superseded_by = manifest.superseded_by.clone();
    updated.derived_from = manifest.derived_from.clone();
    updated.replaces = manifest.replaces.clone();
    updated.owner_hint = task
        .owner_hint
        .clone()
        .or(manifest.owner_hint)
        .or_else(|| task.role.as_deref().map(|role| format!("role:{role}")));

    if matches!(manifest.execution_status, GoalTaskStatus::Blocked)
        && matches!(next_status, GoalTaskStatus::InProgress)
    {
        updated.retry_count = updated.retry_count.saturating_add(1);
    }
    if matches!(
        manifest.execution_status,
        GoalTaskStatus::Done | GoalTaskStatus::Cancelled
    ) && matches!(
        next_status,
        GoalTaskStatus::Planned | GoalTaskStatus::InProgress
    ) {
        updated.reopen_count = updated.reopen_count.saturating_add(1);
    }

    write_markdown_atomic(&task_path, &render_goal_task_markdown(context, &updated))?;
    append_goal_event_archive(
        archive_root,
        context,
        "task_status_changed",
        updated.plan_version,
        &format!(
            "task={} previous={} next={}",
            task.task_id,
            match manifest.execution_status {
                GoalTaskStatus::Planned => "planned",
                GoalTaskStatus::InProgress => "in_progress",
                GoalTaskStatus::Blocked => "blocked",
                GoalTaskStatus::Done => "done",
                GoalTaskStatus::Cancelled => "cancelled",
            },
            match next_status {
                GoalTaskStatus::Planned => "planned",
                GoalTaskStatus::InProgress => "in_progress",
                GoalTaskStatus::Blocked => "blocked",
                GoalTaskStatus::Done => "done",
                GoalTaskStatus::Cancelled => "cancelled",
            }
        ),
        GoalEventMetadata {
            task_id: Some(&task.task_id),
            previous_status: Some(match manifest.execution_status {
                GoalTaskStatus::Planned => "planned",
                GoalTaskStatus::InProgress => "in_progress",
                GoalTaskStatus::Blocked => "blocked",
                GoalTaskStatus::Done => "done",
                GoalTaskStatus::Cancelled => "cancelled",
            }),
            next_status: Some(match next_status {
                GoalTaskStatus::Planned => "planned",
                GoalTaskStatus::InProgress => "in_progress",
                GoalTaskStatus::Blocked => "blocked",
                GoalTaskStatus::Done => "done",
                GoalTaskStatus::Cancelled => "cancelled",
            }),
            actor,
            note,
            ..GoalEventMetadata::default()
        },
    )?;
    if let Some(escalation) = escalation.as_ref() {
        let timing = goal_manifest.as_ref().map(|goal| {
            effective_timing_policy(
                preferences.as_ref(),
                Some(archive_root),
                goal,
                &updated,
                escalation.audience.as_deref(),
            )
        });
        let should_defer_delivery = timing
            .as_ref()
            .map(|timing| !delivery_window_is_open(timing, context.observed_at))
            .unwrap_or(false);
        let deferred_note = timing.as_ref().map(|timing| {
            format!(
                "Task entered blocked outside the active delivery window in timezone {}; human delivery is deferred until the window opens.",
                timing.timezone
            )
        });
        if should_defer_delivery {
            append_goal_event_archive(
                archive_root,
                context,
                "task_escalation_deferred",
                updated.plan_version,
                &format!(
                    "task={} policy={} deferred",
                    task.task_id,
                    escalation_policy_label(escalation.policy),
                ),
                GoalEventMetadata {
                    task_id: Some(&task.task_id),
                    previous_status: Some(match manifest.execution_status {
                        GoalTaskStatus::Planned => "planned",
                        GoalTaskStatus::InProgress => "in_progress",
                        GoalTaskStatus::Blocked => "blocked",
                        GoalTaskStatus::Done => "done",
                        GoalTaskStatus::Cancelled => "cancelled",
                    }),
                    next_status: Some("blocked"),
                    escalation_policy: Some(escalation_policy_label(escalation.policy)),
                    escalation_trigger: Some("on_enter_blocked"),
                    escalation_audience: Some(escalation_audience_label(
                        escalation.audience.as_deref(),
                        escalation.policy,
                    )),
                    escalation_severity: escalation.severity.map(escalation_severity_label),
                    actor,
                    note: deferred_note.as_deref(),
                    ..GoalEventMetadata::default()
                },
            )?;
        } else {
            append_goal_event_archive(
                archive_root,
                context,
                "task_escalated",
                updated.plan_version,
                &format!(
                    "task={} policy={} detail={}",
                    task.task_id,
                    escalation_policy_label(escalation.policy),
                    escalation.detail
                ),
                GoalEventMetadata {
                    task_id: Some(&task.task_id),
                    previous_status: Some(match manifest.execution_status {
                        GoalTaskStatus::Planned => "planned",
                        GoalTaskStatus::InProgress => "in_progress",
                        GoalTaskStatus::Blocked => "blocked",
                        GoalTaskStatus::Done => "done",
                        GoalTaskStatus::Cancelled => "cancelled",
                    }),
                    next_status: Some("blocked"),
                    escalation_policy: Some(escalation_policy_label(escalation.policy)),
                    escalation_trigger: Some("on_enter_blocked"),
                    escalation_audience: Some(escalation_audience_label(
                        escalation.audience.as_deref(),
                        escalation.policy,
                    )),
                    escalation_severity: escalation.severity.map(escalation_severity_label),
                    actor,
                    note: Some(escalation.detail.as_str()),
                    ..GoalEventMetadata::default()
                },
            )?;
        }
        if matches!(escalation.policy, GoalTaskEscalationPolicy::AutoReplan) {
            append_goal_event_archive(
                archive_root,
                context,
                "task_replan_requested",
                updated.plan_version,
                &format!("task={} blocked -> replan requested", task.task_id),
                GoalEventMetadata {
                    task_id: Some(&task.task_id),
                    previous_status: Some(match manifest.execution_status {
                        GoalTaskStatus::Planned => "planned",
                        GoalTaskStatus::InProgress => "in_progress",
                        GoalTaskStatus::Blocked => "blocked",
                        GoalTaskStatus::Done => "done",
                        GoalTaskStatus::Cancelled => "cancelled",
                    }),
                    next_status: Some("blocked"),
                    escalation_policy: Some(escalation_policy_label(escalation.policy)),
                    escalation_trigger: Some("on_enter_blocked"),
                    escalation_audience: Some(escalation_audience_label(
                        escalation.audience.as_deref(),
                        escalation.policy,
                    )),
                    escalation_severity: escalation.severity.map(escalation_severity_label),
                    actor,
                    note: Some(escalation.detail.as_str()),
                    ..GoalEventMetadata::default()
                },
            )?;
        }
        if should_defer_delivery {
            return Ok(None);
        }
    }
    Ok(escalation)
}

pub(crate) fn persist_goal_task_owner_archive(
    archive_root: &Path,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    next_owner_hint: &str,
    actor: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let parser = ManifestParser::new();
    let task_path = goal_archive_dir(archive_root, context.project_id, context.goal_id)
        .join("tasks")
        .join(format!("{}.md", sanitize_archive_component(&task.task_id)));
    if !task_path.exists() {
        return Ok(());
    }
    let manifest = parser.parse_goal_task(&task_path)?;
    let previous_owner = manifest
        .owner_hint
        .clone()
        .or_else(|| manifest.role.as_deref().map(|role| format!("role:{role}")));
    let next_owner = next_owner_hint.to_string();
    if previous_owner.as_deref() == Some(next_owner.as_str()) {
        return Ok(());
    }

    let mut updated = task.clone();
    updated.state = manifest.state;
    updated.execution_status = manifest.execution_status;
    updated.role = task.role.clone().or(manifest.role);
    updated.depends_on = task.depends_on.clone();
    updated.questionnaire_context = if task.questionnaire_context.is_empty() {
        manifest.questionnaire_context
    } else {
        task.questionnaire_context.clone()
    };
    updated.owner_hint = Some(next_owner.clone());
    updated.retry_count = manifest.retry_count;
    updated.reopen_count = manifest.reopen_count;
    updated.last_status_change_at = manifest.last_status_change_at;
    updated.plan_version = manifest.plan_version;
    updated.superseded_by = manifest.superseded_by;
    updated.derived_from = manifest.derived_from;
    updated.replaces = manifest.replaces;

    write_markdown_atomic(&task_path, &render_goal_task_markdown(context, &updated))?;
    append_goal_event_archive(
        archive_root,
        context,
        "task_owner_changed",
        updated.plan_version,
        &format!(
            "task={} previous_owner={} next_owner={}",
            task.task_id,
            previous_owner.as_deref().unwrap_or("unassigned"),
            next_owner
        ),
        GoalEventMetadata {
            task_id: Some(&task.task_id),
            previous_owner: previous_owner.as_deref(),
            next_owner: Some(next_owner.as_str()),
            actor,
            note,
            ..GoalEventMetadata::default()
        },
    )?;
    Ok(())
}

pub(crate) fn persist_goal_task_condition_set_archive(
    archive_root: &Path,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    condition_kind: &str,
    condition_value: &str,
    actor: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let parser = ManifestParser::new();
    let task_path = goal_archive_dir(archive_root, context.project_id, context.goal_id)
        .join("tasks")
        .join(format!("{}.md", sanitize_archive_component(&task.task_id)));
    if !task_path.exists() {
        return Ok(());
    }

    let manifest = parser.parse_goal_task(&task_path)?;
    let mut updated = task.clone();
    updated.execution_status = manifest.execution_status;
    updated.state = manifest.state;
    updated.retry_count = manifest.retry_count;
    updated.reopen_count = manifest.reopen_count;
    updated.last_status_change_at = manifest.last_status_change_at;
    updated.plan_version = manifest.plan_version;
    updated.superseded_by = manifest.superseded_by.clone();
    updated.derived_from = manifest.derived_from.clone();
    updated.replaces = manifest.replaces.clone();
    updated.owner_hint = manifest.owner_hint.or_else(|| updated.owner_hint.clone());

    match condition_kind {
        "waiting_for" => updated.declared_context.waiting_for = Some(condition_value.to_string()),
        "review_target" => {
            updated.declared_context.review_target = Some(condition_value.to_string())
        }
        "coordination_target" => {
            updated.declared_context.coordination_target = Some(condition_value.to_string())
        }
        "external_dependency" => {
            updated.declared_context.external_dependency = Some(condition_value.to_string())
        }
        _ => return Ok(()),
    }

    write_markdown_atomic(&task_path, &render_goal_task_markdown(context, &updated))?;
    append_goal_event_archive(
        archive_root,
        context,
        "task_condition_set",
        updated.plan_version,
        &format!("task={} condition={} set", task.task_id, condition_kind),
        GoalEventMetadata {
            task_id: Some(&task.task_id),
            condition_kind: Some(condition_kind),
            condition_value: Some(condition_value),
            actor,
            note,
            ..GoalEventMetadata::default()
        },
    )?;
    Ok(())
}

pub(crate) fn persist_goal_task_condition_cleared_archive(
    archive_root: &Path,
    context: &GoalHierarchyContext<'_>,
    task: &PlannedTaskRecord,
    condition_kind: &str,
    actor: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let parser = ManifestParser::new();
    let task_path = goal_archive_dir(archive_root, context.project_id, context.goal_id)
        .join("tasks")
        .join(format!("{}.md", sanitize_archive_component(&task.task_id)));
    if !task_path.exists() {
        return Ok(None);
    }

    let manifest = parser.parse_goal_task(&task_path)?;
    let mut updated = task.clone();
    updated.execution_status = manifest.execution_status;
    updated.state = manifest.state;
    updated.retry_count = manifest.retry_count;
    updated.reopen_count = manifest.reopen_count;
    updated.last_status_change_at = manifest.last_status_change_at;
    updated.plan_version = manifest.plan_version;
    updated.superseded_by = manifest.superseded_by.clone();
    updated.derived_from = manifest.derived_from.clone();
    updated.replaces = manifest.replaces.clone();
    updated.owner_hint = manifest.owner_hint.or_else(|| updated.owner_hint.clone());

    let cleared_value = match condition_kind {
        "waiting_for" => updated.declared_context.waiting_for.take(),
        "review_target" => updated.declared_context.review_target.take(),
        "coordination_target" => updated.declared_context.coordination_target.take(),
        "external_dependency" => updated.declared_context.external_dependency.take(),
        _ => None,
    };
    let Some(condition_value) = cleared_value else {
        return Ok(None);
    };

    write_markdown_atomic(&task_path, &render_goal_task_markdown(context, &updated))?;
    append_goal_event_archive(
        archive_root,
        context,
        "task_condition_satisfied",
        updated.plan_version,
        &format!(
            "task={} condition={} satisfied",
            task.task_id, condition_kind
        ),
        GoalEventMetadata {
            task_id: Some(&task.task_id),
            condition_kind: Some(condition_kind),
            condition_value: Some(&condition_value),
            actor,
            note,
            ..GoalEventMetadata::default()
        },
    )?;
    Ok(Some(condition_value))
}

pub(crate) fn rehydrate_management_from_archive(
    store: &mut ManagementStore,
    archive_root: &Path,
    observed_at: i64,
) -> anyhow::Result<usize> {
    let parser = ManifestParser::new();
    let desired = parser.parse_all(archive_root)?;
    let mut hydrated = 0usize;

    for goal in desired.goals {
        if !matches!(goal.state, symbiotic_control_plane::GoalState::Active) {
            continue;
        }
        let context = GoalHierarchyContext {
            project_id: &goal.project_id,
            goal_id: &goal.id,
            title: &goal.title,
            summary: &extract_goal_summary(&goal.plan_markdown),
            owner: None,
            thread_id: goal.thread_id.as_deref(),
            observed_at,
        };
        ensure_goal_work_item(store, &context, WorkItemStatus::Running)?;
        hydrated += 1;

        for task in goal.tasks {
            let summary = extract_task_summary(&task.task_markdown);
            let record = PlannedTaskRecord {
                task_id: task.task_id,
                task_slug: task.task_slug,
                task_kind: task.task_kind,
                task_driver: task.task_driver,
                title: task.title,
                summary,
                state: task.state,
                execution_status: task.execution_status,
                role: task.role,
                depends_on: task.depends_on,
                questionnaire_context: task.questionnaire_context,
                owner_hint: task.owner_hint,
                declared_context: task.declared_context,
                policy: task.policy,
                retry_count: task.retry_count,
                reopen_count: task.reopen_count,
                last_status_change_at: task.last_status_change_at,
                plan_version: task.plan_version,
                superseded_by: task.superseded_by,
                derived_from: task.derived_from,
                replaces: task.replaces,
            };
            if !matches!(record.state, GoalTaskPlanState::Active) {
                continue;
            }
            ensure_goal_task_work_item(store, &context, &record)?;
            if task_needs_execution_child(record.task_driver) {
                ensure_goal_execution_work_item(
                    store,
                    &context,
                    &record,
                    record.role.as_deref(),
                    work_item_status_from_task_status(record.execution_status),
                )?;
                let task_id = goal_task_work_item_id(context.goal_id, &record.task_id);
                refresh_parent_work_item_status(store, &task_id, observed_at)?;
            }
        }

        let goal_id = goal_work_item_id(context.goal_id);
        refresh_parent_work_item_status(store, &goal_id, observed_at)?;
    }

    Ok(hydrated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn goal_archive_test_dir(root: &Path, project_id: &str, goal_id: &str) -> PathBuf {
        root.join("operations")
            .join("projects")
            .join(project_archive_component(project_id))
            .join("goals")
            .join(goal_id)
    }

    #[test]
    fn archive_task_status_updates_retry_and_reopen_counters() {
        let tmp = TempDir::new().expect("tempdir");
        let context = GoalHierarchyContext {
            project_id: "project:test",
            goal_id: "goal-test",
            title: "Test Goal",
            summary: "Test summary",
            owner: Some("@user:test"),
            thread_id: Some("thread-test"),
            observed_at: 100,
        };
        let base_task = PlannedTaskRecord {
            task_id: "step_1".to_string(),
            task_slug: "step_1".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Step 1".to_string(),
            summary: "Do step 1".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: Some("researcher".to_string()),
            depends_on: Vec::new(),
            questionnaire_context: vec!["Budget -> Under $500".to_string()],
            owner_hint: Some("role:researcher".to_string()),
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };

        persist_goal_plan_archive(
            tmp.path(),
            &context,
            "implementation",
            std::slice::from_ref(&base_task),
        )
        .expect("persist initial task");

        let mut blocked_context = context;
        blocked_context.observed_at = 120;
        persist_goal_task_status_archive(
            tmp.path(),
            &blocked_context,
            &base_task,
            WorkItemStatus::Failed,
        )
        .expect("persist blocked state");

        let mut retry_context = context;
        retry_context.observed_at = 140;
        persist_goal_task_status_archive(
            tmp.path(),
            &retry_context,
            &base_task,
            WorkItemStatus::Running,
        )
        .expect("persist retry state");

        let mut done_context = context;
        done_context.observed_at = 160;
        persist_goal_task_status_archive(
            tmp.path(),
            &done_context,
            &base_task,
            WorkItemStatus::Done,
        )
        .expect("persist done state");

        let mut reopen_context = context;
        reopen_context.observed_at = 180;
        persist_goal_task_status_archive(
            tmp.path(),
            &reopen_context,
            &base_task,
            WorkItemStatus::Running,
        )
        .expect("persist reopened state");

        let parser = ManifestParser::new();
        let manifest = parser
            .parse_goal_task(
                &goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id)
                    .join("tasks/step-1.md"),
            )
            .expect("parse task manifest");

        assert_eq!(manifest.state, GoalTaskPlanState::Active);
        assert_eq!(manifest.execution_status, GoalTaskStatus::InProgress);
        assert_eq!(manifest.retry_count, 1);
        assert_eq!(manifest.reopen_count, 1);
        assert_eq!(manifest.last_status_change_at, Some(180));
        let status_event_path =
            goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id).join(
                "events/180-task-status-changed-task-step-1-previous-done-next-in-progress.md",
            );
        let status_event =
            std::fs::read_to_string(status_event_path).expect("read status event doc");
        assert!(status_event.contains("task_id: \"step_1\""));
        assert!(status_event.contains("previous_status: \"done\""));
        assert!(status_event.contains("next_status: \"in_progress\""));
    }

    #[test]
    fn persist_goal_plan_archive_preserves_existing_task_lifecycle_fields() {
        let tmp = TempDir::new().expect("tempdir");
        let context = GoalHierarchyContext {
            project_id: "project:test",
            goal_id: "goal-test",
            title: "Test Goal",
            summary: "Test summary",
            owner: Some("@user:test"),
            thread_id: Some("thread-test"),
            observed_at: 100,
        };
        let existing = PlannedTaskRecord {
            task_id: "step_1".to_string(),
            task_slug: "step_1".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Existing Step".to_string(),
            summary: "Existing summary".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Blocked,
            role: Some("researcher".to_string()),
            depends_on: Vec::new(),
            questionnaire_context: vec!["Budget -> Under $500".to_string()],
            owner_hint: Some("role:researcher".to_string()),
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 2,
            reopen_count: 1,
            last_status_change_at: Some(123),
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        persist_goal_plan_archive(tmp.path(), &context, "implementation", &[existing])
            .expect("persist existing task");

        let updated = PlannedTaskRecord {
            task_id: "step_1".to_string(),
            task_slug: "step_1".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Replanned Step".to_string(),
            summary: "Updated summary".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: Some("implementer".to_string()),
            depends_on: vec!["step_0".to_string()],
            questionnaire_context: vec!["Preferred airline -> Any".to_string()],
            owner_hint: Some("role:implementer".to_string()),
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        persist_goal_plan_archive(tmp.path(), &context, "implementation", &[updated])
            .expect("persist replanned task");

        let manifest = ManifestParser::new()
            .parse_goal_task(
                &goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id)
                    .join("tasks/step-1.md"),
            )
            .expect("parse merged manifest");

        assert_eq!(manifest.title, "Replanned Step");
        assert_eq!(manifest.state, GoalTaskPlanState::Active);
        assert_eq!(manifest.execution_status, GoalTaskStatus::Blocked);
        assert_eq!(manifest.role.as_deref(), Some("implementer"));
        assert_eq!(manifest.depends_on, vec!["step_0".to_string()]);
        assert_eq!(
            manifest.questionnaire_context,
            vec!["Preferred airline -> Any".to_string()]
        );
        assert_eq!(manifest.owner_hint.as_deref(), Some("role:implementer"));
        assert_eq!(manifest.retry_count, 2);
        assert_eq!(manifest.reopen_count, 1);
        assert_eq!(manifest.last_status_change_at, Some(123));
        assert_eq!(manifest.plan_version, 2);
    }

    #[test]
    fn persist_goal_plan_archive_deactivates_removed_tasks_and_writes_event() {
        let tmp = TempDir::new().expect("tempdir");
        let context = GoalHierarchyContext {
            project_id: "project:test",
            goal_id: "goal-test",
            title: "Test Goal",
            summary: "Test summary",
            owner: Some("@user:test"),
            thread_id: Some("thread-test"),
            observed_at: 100,
        };
        let retained = PlannedTaskRecord {
            task_id: "step_1".to_string(),
            task_slug: "step_1".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Step 1".to_string(),
            summary: "Do step 1".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Done,
            role: Some("researcher".to_string()),
            depends_on: Vec::new(),
            questionnaire_context: Vec::new(),
            owner_hint: Some("role:researcher".to_string()),
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: Some(100),
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        let removed = PlannedTaskRecord {
            task_id: "step_2".to_string(),
            task_slug: "step_2".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Step 2".to_string(),
            summary: "Do step 2".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::InProgress,
            role: Some("implementer".to_string()),
            depends_on: vec!["step_1".to_string()],
            questionnaire_context: Vec::new(),
            owner_hint: Some("role:implementer".to_string()),
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 1,
            reopen_count: 0,
            last_status_change_at: Some(120),
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        persist_goal_plan_archive(
            tmp.path(),
            &context,
            "implementation",
            &[retained.clone(), removed],
        )
        .expect("persist initial plan");

        let mut replan_context = context;
        replan_context.observed_at = 200;
        persist_goal_plan_archive(tmp.path(), &replan_context, "implementation", &[retained])
            .expect("persist replanned goal");

        let parser = ManifestParser::new();
        let removed_manifest = parser
            .parse_goal_task(
                &goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id)
                    .join("tasks/step-2.md"),
            )
            .expect("parse removed task");
        assert_eq!(removed_manifest.state, GoalTaskPlanState::Superseded);
        assert_eq!(removed_manifest.execution_status, GoalTaskStatus::Cancelled);
        assert_eq!(removed_manifest.plan_version, 2);
        assert!(removed_manifest.superseded_by.is_empty());

        let events_dir =
            goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id).join("events");
        let entries = std::fs::read_dir(events_dir)
            .expect("read events dir")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(
            entries.iter().any(|name| name.contains("plan-reconciled")),
            "expected plan reconciliation event"
        );
        let plan_event_path =
            goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id)
                .join("events/200-plan-reconciled-added-preserved-step-1-deactivated-step-2.md");
        let plan_event = std::fs::read_to_string(plan_event_path).expect("read plan event doc");
        assert!(plan_event.contains("preserved_task_ids: [\"step_1\"]"));
        assert!(plan_event.contains("deactivated_task_ids: [\"step_2\"]"));
        assert!(plan_event.contains("supersession_edges: []"));
    }

    #[test]
    fn persist_goal_plan_archive_marks_replaced_tasks_as_superseded() {
        let tmp = TempDir::new().expect("tempdir");
        let context = GoalHierarchyContext {
            project_id: "project:test",
            goal_id: "goal-test",
            title: "Test Goal",
            summary: "Test summary",
            owner: Some("@user:test"),
            thread_id: Some("thread-test"),
            observed_at: 100,
        };
        let legacy_task = PlannedTaskRecord {
            task_id: "research".to_string(),
            task_slug: "research".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Research options".to_string(),
            summary: "Initial research task".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Done,
            role: Some("researcher".to_string()),
            depends_on: Vec::new(),
            questionnaire_context: Vec::new(),
            owner_hint: Some("role:researcher".to_string()),
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: Some(100),
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        persist_goal_plan_archive(tmp.path(), &context, "implementation", &[legacy_task])
            .expect("persist initial plan");

        let mut replan_context = context;
        replan_context.observed_at = 200;
        let split_task_one = PlannedTaskRecord {
            task_id: "research-airfare".to_string(),
            task_slug: "research-airfare".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Research airfare options".to_string(),
            summary: "Split airfare work".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: Some("researcher".to_string()),
            depends_on: Vec::new(),
            questionnaire_context: Vec::new(),
            owner_hint: Some("role:researcher".to_string()),
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: vec!["research".to_string()],
            replaces: vec!["research".to_string()],
        };
        let split_task_two = PlannedTaskRecord {
            task_id: "research-hotels".to_string(),
            task_slug: "research-hotels".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Research hotel options".to_string(),
            summary: "Split hotel work".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: Some("researcher".to_string()),
            depends_on: vec!["research-airfare".to_string()],
            questionnaire_context: Vec::new(),
            owner_hint: Some("role:researcher".to_string()),
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: vec!["research".to_string()],
            replaces: vec!["research".to_string()],
        };
        persist_goal_plan_archive(
            tmp.path(),
            &replan_context,
            "implementation",
            &[split_task_one, split_task_two],
        )
        .expect("persist replanned split tasks");

        let parser = ManifestParser::new();
        let legacy_manifest = parser
            .parse_goal_task(
                &goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id)
                    .join("tasks/research.md"),
            )
            .expect("parse replaced task");
        assert_eq!(legacy_manifest.state, GoalTaskPlanState::Superseded);
        assert_eq!(
            legacy_manifest.superseded_by,
            vec![
                "research-airfare".to_string(),
                "research-hotels".to_string()
            ]
        );
        let split_manifest = parser
            .parse_goal_task(
                &goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id)
                    .join("tasks/research-airfare.md"),
            )
            .expect("parse split task");
        assert_eq!(split_manifest.derived_from, vec!["research".to_string()]);
        assert_eq!(split_manifest.replaces, vec!["research".to_string()]);
        let plan_event_path = goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id)
            .join("events/200-plan-reconciled-added-research-airfare-research-hotels-preserved-deactivated-research.md");
        let plan_event = std::fs::read_to_string(plan_event_path).expect("read replan event doc");
        assert!(plan_event.contains(
            "supersession_edges: [\"research->research-airfare\", \"research->research-hotels\"]"
        ));
    }

    #[test]
    fn persist_goal_plan_archive_records_owner_changes() {
        let tmp = TempDir::new().expect("tempdir");
        let context = GoalHierarchyContext {
            project_id: "project:test",
            goal_id: "goal-test",
            title: "Test Goal",
            summary: "Test summary",
            owner: Some("@manager:test"),
            thread_id: Some("thread-test"),
            observed_at: 100,
        };
        let original = PlannedTaskRecord {
            task_id: "step_1".to_string(),
            task_slug: "step_1".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Step 1".to_string(),
            summary: "Do step 1".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: Some("researcher".to_string()),
            depends_on: Vec::new(),
            questionnaire_context: Vec::new(),
            owner_hint: None,
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        persist_goal_plan_archive(tmp.path(), &context, "implementation", &[original])
            .expect("persist original plan");

        let mut replan_context = context;
        replan_context.observed_at = 200;
        let reassigned = PlannedTaskRecord {
            owner_hint: Some("@reviewer:test".to_string()),
            ..PlannedTaskRecord {
                task_id: "step_1".to_string(),
                task_slug: "step_1".to_string(),
                task_kind: GoalTaskKind::Execution,
                task_driver: GoalTaskDriver::Agent,
                title: "Step 1".to_string(),
                summary: "Do step 1".to_string(),
                state: GoalTaskPlanState::Active,
                execution_status: GoalTaskStatus::Planned,
                role: Some("researcher".to_string()),
                depends_on: Vec::new(),
                questionnaire_context: Vec::new(),
                owner_hint: None,
                declared_context: GoalTaskDeclaredContext::default(),
                policy: GoalTaskPolicy::default(),
                retry_count: 0,
                reopen_count: 0,
                last_status_change_at: None,
                plan_version: 1,
                superseded_by: Vec::new(),
                derived_from: Vec::new(),
                replaces: Vec::new(),
            }
        };
        persist_goal_plan_archive(tmp.path(), &replan_context, "implementation", &[reassigned])
            .expect("persist reassigned plan");

        let events_dir =
            goal_archive_test_dir(tmp.path(), context.project_id, context.goal_id).join("events");
        let plan_event_path = std::fs::read_dir(events_dir)
            .expect("read events dir")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("200-plan-reconciled-"))
                    .unwrap_or(false)
            })
            .expect("find replan event doc");
        let plan_event = std::fs::read_to_string(plan_event_path).expect("read plan event doc");
        assert!(
            plan_event.contains("owner_change_edges: [\"step_1:role:researcher->@reviewer:test\"]")
        );
    }

    #[test]
    fn sync_goal_plan_hierarchy_keeps_waiting_and_approval_tasks_without_execution_children() {
        let tmp = TempDir::new().expect("tempdir");
        let management_store = Arc::new(Mutex::new(ManagementStore::new(
            tmp.path().join("control-plane"),
        )));
        let context = GoalHierarchyContext {
            project_id: "project:test",
            goal_id: "goal-waiting",
            title: "Waiting Goal",
            summary: "Test waiting and approval tasks",
            owner: Some("@user:test"),
            thread_id: Some("thread-waiting"),
            observed_at: 100,
        };
        let waiting = PlannedTaskRecord {
            task_id: "await-budget".to_string(),
            task_slug: "await-budget".to_string(),
            task_kind: GoalTaskKind::Waiting,
            task_driver: GoalTaskDriver::Declared,
            title: "Wait for budget confirmation".to_string(),
            summary: "Hold until the user confirms budget.".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: None,
            depends_on: Vec::new(),
            questionnaire_context: Vec::new(),
            owner_hint: Some("@user:test".to_string()),
            declared_context: GoalTaskDeclaredContext {
                waiting_for: Some("budget confirmation from operator".to_string()),
                ..GoalTaskDeclaredContext::default()
            },
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        let approval = PlannedTaskRecord {
            task_id: "approve-booking".to_string(),
            task_slug: "approve-booking".to_string(),
            task_kind: GoalTaskKind::Approval,
            task_driver: GoalTaskDriver::Declared,
            title: "Approve the booking".to_string(),
            summary: "Wait for explicit approval before purchase.".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: None,
            depends_on: vec!["await-budget".to_string()],
            questionnaire_context: Vec::new(),
            owner_hint: Some("@user:test".to_string()),
            declared_context: GoalTaskDeclaredContext {
                review_target: Some("booking proposal".to_string()),
                ..GoalTaskDeclaredContext::default()
            },
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        {
            let mut store = management_store.lock().expect("management store lock");
            ensure_goal_work_item(&mut store, &context, WorkItemStatus::Running)
                .expect("goal item");
            ensure_goal_task_work_item(&mut store, &context, &waiting).expect("waiting task");
            ensure_goal_task_work_item(&mut store, &context, &approval).expect("approval task");
            refresh_parent_work_item_status(
                &mut store,
                &goal_work_item_id(context.goal_id),
                context.observed_at,
            )
            .expect("refresh goal");

            let waiting_task = store
                .get_work_item(&goal_task_work_item_id(context.goal_id, "await-budget"))
                .expect("waiting task exists");
            assert_eq!(waiting_task.status, WorkItemStatus::Todo);
            assert_eq!(waiting_task.assignment_mode, AssignmentMode::SingleOwner);
            assert_eq!(waiting_task.review_mode, ReviewMode::NoReview);
            assert!(waiting_task
                .summary
                .contains("waiting for: budget confirmation from operator"));
            assert_eq!(
                waiting_task
                    .assignee
                    .as_ref()
                    .map(|value| value.agent_id.as_str()),
                Some("@user:test")
            );
            assert!(store
                .get_work_item(&goal_execution_work_item_id(
                    context.goal_id,
                    "await-budget"
                ))
                .is_none());

            let approval_task = store
                .get_work_item(&goal_task_work_item_id(context.goal_id, "approve-booking"))
                .expect("approval task exists");
            assert_eq!(approval_task.status, WorkItemStatus::Todo);
            assert_eq!(approval_task.assignment_mode, AssignmentMode::SingleOwner);
            assert_eq!(approval_task.review_mode, ReviewMode::HumanRequired);
            assert!(approval_task
                .summary
                .contains("review target: booking proposal"));
            assert_eq!(
                approval_task
                    .assignee
                    .as_ref()
                    .map(|value| value.agent_id.as_str()),
                Some("@user:test")
            );
            assert!(store
                .get_work_item(&goal_execution_work_item_id(
                    context.goal_id,
                    "approve-booking"
                ))
                .is_none());

            let goal = store
                .get_work_item(&goal_work_item_id(context.goal_id))
                .expect("goal work item exists");
            assert_eq!(goal.status, WorkItemStatus::Todo);
        }

        let mut progressed_context = context;
        progressed_context.observed_at = 200;
        sync_goal_task_declared_status(
            &management_store,
            None,
            &progressed_context,
            &waiting,
            GoalTaskStatus::InProgress,
            Some("@user:test"),
            Some("Waiting on explicit budget signal"),
        );
        sync_goal_task_declared_status(
            &management_store,
            None,
            &progressed_context,
            &approval,
            GoalTaskStatus::InProgress,
            Some("@user:test"),
            Some("Approval requested from operator"),
        );

        let store = management_store.lock().expect("management store lock");
        let waiting_task = store
            .get_work_item(&goal_task_work_item_id(context.goal_id, "await-budget"))
            .expect("waiting task exists after progression");
        assert_eq!(waiting_task.status, WorkItemStatus::Blocked);
        assert!(store
            .get_work_item(&goal_execution_work_item_id(
                context.goal_id,
                "await-budget"
            ))
            .is_none());

        let approval_task = store
            .get_work_item(&goal_task_work_item_id(context.goal_id, "approve-booking"))
            .expect("approval task exists after progression");
        assert_eq!(approval_task.status, WorkItemStatus::PendingReview);
        assert!(store
            .get_work_item(&goal_execution_work_item_id(
                context.goal_id,
                "approve-booking"
            ))
            .is_none());

        let goal = store
            .get_work_item(&goal_work_item_id(context.goal_id))
            .expect("goal work item exists after progression");
        assert_eq!(goal.status, WorkItemStatus::Blocked);
    }

    #[test]
    fn execution_task_work_item_prefers_owner_hint_then_role_for_assignee() {
        let tmp = TempDir::new().expect("tempdir");
        let mut store = ManagementStore::new(tmp.path().join("control-plane"));
        let context = GoalHierarchyContext {
            project_id: "project:test",
            goal_id: "goal-execution",
            title: "Execution Goal",
            summary: "Test execution ownership",
            owner: Some("@manager:test"),
            thread_id: Some("thread-execution"),
            observed_at: 100,
        };
        let role_owned = PlannedTaskRecord {
            task_id: "implement".to_string(),
            task_slug: "implement".to_string(),
            task_kind: GoalTaskKind::Execution,
            task_driver: GoalTaskDriver::Agent,
            title: "Implement the feature".to_string(),
            summary: "Runtime execution task".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: Some("coder".to_string()),
            depends_on: Vec::new(),
            questionnaire_context: Vec::new(),
            owner_hint: None,
            declared_context: GoalTaskDeclaredContext::default(),
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        ensure_goal_task_work_item(&mut store, &context, &role_owned).expect("task");
        let task = store
            .get_work_item(&goal_task_work_item_id(context.goal_id, "implement"))
            .expect("task exists");
        assert_eq!(
            task.assignee.as_ref().map(|value| value.agent_id.as_str()),
            Some("role:coder")
        );

        let owner_hint = PlannedTaskRecord {
            owner_hint: Some("@reviewer:test".to_string()),
            ..role_owned
        };
        ensure_goal_task_work_item(&mut store, &context, &owner_hint).expect("task update");
        let task = store
            .get_work_item(&goal_task_work_item_id(context.goal_id, "implement"))
            .expect("task exists");
        assert_eq!(
            task.assignee.as_ref().map(|value| value.agent_id.as_str()),
            Some("@reviewer:test")
        );
    }

    #[test]
    fn sync_goal_task_owner_updates_archive_and_task_assignee() {
        let tmp = TempDir::new().expect("tempdir");
        let archive_root = tmp.path().join("knowledge-base");
        let context = GoalHierarchyContext {
            project_id: "project:test",
            goal_id: "goal-owner",
            title: "Owner Goal",
            summary: "Track owner changes canonically",
            owner: Some("@manager:test"),
            thread_id: Some("thread-owner"),
            observed_at: 100,
        };
        let task = PlannedTaskRecord {
            task_id: "coordinate".to_string(),
            task_slug: "coordinate".to_string(),
            task_kind: GoalTaskKind::Coordination,
            task_driver: GoalTaskDriver::Declared,
            title: "Coordinate the rollout".to_string(),
            summary: "Keep the rollout aligned".to_string(),
            state: GoalTaskPlanState::Active,
            execution_status: GoalTaskStatus::Planned,
            role: None,
            depends_on: Vec::new(),
            questionnaire_context: Vec::new(),
            owner_hint: Some("@manager:test".to_string()),
            declared_context: GoalTaskDeclaredContext {
                coordination_target: Some("rollout stakeholders".to_string()),
                ..GoalTaskDeclaredContext::default()
            },
            policy: GoalTaskPolicy::default(),
            retry_count: 0,
            reopen_count: 0,
            last_status_change_at: None,
            plan_version: 1,
            superseded_by: Vec::new(),
            derived_from: Vec::new(),
            replaces: Vec::new(),
        };
        persist_goal_plan_archive(
            &archive_root,
            &context,
            "implementation",
            std::slice::from_ref(&task),
        )
        .expect("persist initial plan");

        let management_store = Arc::new(Mutex::new(ManagementStore::new(
            tmp.path().join("control-plane"),
        )));
        sync_goal_plan_hierarchy(
            &management_store,
            &context,
            std::slice::from_ref(&task),
            WorkItemStatus::Running,
        );

        let mut updated_context = context;
        updated_context.observed_at = 200;
        sync_goal_task_owner(
            &management_store,
            Some(&archive_root),
            &updated_context,
            &task,
            "@operator:test",
            Some("@lead:test"),
            Some("Handing off operator ownership"),
        );

        let store = management_store.lock().expect("management store lock");
        let task_work_item = store
            .get_work_item(&goal_task_work_item_id(context.goal_id, "coordinate"))
            .expect("task work item exists");
        assert_eq!(
            task_work_item
                .assignee
                .as_ref()
                .map(|value| value.agent_id.as_str()),
            Some("@operator:test")
        );

        let task_doc = std::fs::read_to_string(
            goal_archive_test_dir(&archive_root, context.project_id, context.goal_id)
                .join("tasks/coordinate.md"),
        )
        .expect("task doc should exist");
        assert!(task_doc.contains("owner_hint: \"@operator:test\""));

        let latest_event_path = std::fs::read_dir(
            goal_archive_test_dir(&archive_root, context.project_id, context.goal_id)
                .join("events"),
        )
        .expect("read events dir")
        .flatten()
        .map(|entry| entry.path())
        .max()
        .expect("event doc should exist");
        let latest_event = std::fs::read_to_string(latest_event_path).expect("read latest event");
        assert!(latest_event.contains("event_type: \"task_owner_changed\""));
        assert!(latest_event.contains("previous_owner: \"@manager:test\""));
        assert!(latest_event.contains("next_owner: \"@operator:test\""));
        assert!(latest_event.contains("actor: \"@lead:test\""));
        assert!(latest_event.contains("note: \"Handing off operator ownership\""));
    }
}
