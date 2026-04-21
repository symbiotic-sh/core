use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::{
    escape_field, harden_dir_permissions, harden_file_permissions, room_alias, unescape_field,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalState {
    pub goal_room: String,
    pub thread_id: Option<String>,
    pub project_id: String,
    pub template: String,
    pub status: String,
    pub last_job_id: String,
    pub last_run_id: Option<String>,
    pub owner: Option<String>,
    pub updated_at: u64,
    /// Classified complexity level (Simple, Moderate, Complex, Critical).
    pub complexity: Option<String>,
    /// Current pipeline stage (e.g. "classifying", "planning", "awaiting_approval", "executing").
    pub pipeline_stage: Option<String>,
    /// Link to the audit trail.
    pub audit_id: Option<String>,
    /// Current execution plan ID.
    pub plan_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentState {
    pub agent_id: String,
    pub parent: String,
    pub llm_type: String,
    pub trust_level: String,
    pub last_status: String,
    pub last_scope: Option<String>,
    pub updated_at: u64,
}

pub(crate) struct GoalLogEntry<'a> {
    pub(crate) ts: u64,
    pub(crate) event: &'a str,
    pub(crate) workflow_job_id: &'a str,
    pub(crate) goal_room: Option<&'a str>,
    pub(crate) goal_sender: Option<&'a str>,
    pub(crate) template: &'a str,
    pub(crate) detail: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GoalLogRecord {
    pub ts: u64,
    pub event: String,
    pub workflow_job_id: String,
    pub goal_room: Option<String>,
    pub goal_sender: Option<String>,
    pub template: String,
    pub detail: String,
}

pub(crate) struct AgentLogEntry<'a> {
    pub(crate) ts: u64,
    pub(crate) event: &'a str,
    pub(crate) agent_id: &'a str,
    pub(crate) status: &'a str,
    pub(crate) scope: Option<&'a str>,
    pub(crate) detail: &'a str,
}

pub(crate) fn append_goal_log(path: &Path, entry: GoalLogEntry<'_>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create goal log directory {}", parent.display()))?;
        // Best-effort: parent may be a system directory we cannot chmod.
        let _ = harden_dir_permissions(parent, 0o700);
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open goal log {}", path.display()))?;
    writeln!(
        file,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        entry.ts,
        escape_field(entry.event),
        escape_field(entry.workflow_job_id),
        entry.goal_room.map(escape_field).unwrap_or_default(),
        entry.goal_sender.map(escape_field).unwrap_or_default(),
        escape_field(entry.template),
        escape_field(entry.detail)
    )
    .with_context(|| format!("failed to append goal log {}", path.display()))?;
    harden_file_permissions(path, 0o600)
        .with_context(|| format!("failed to harden goal log permissions {}", path.display()))?;
    Ok(())
}

pub(crate) fn append_agent_lifecycle_log(path: &Path, entry: AgentLogEntry<'_>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create agent lifecycle directory {}",
                parent.display()
            )
        })?;
        // Best-effort: parent may be a system directory we cannot chmod.
        let _ = harden_dir_permissions(parent, 0o700);
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open agent lifecycle log {}", path.display()))?;
    writeln!(
        file,
        "{}\t{}\t{}\t{}\t{}\t{}",
        entry.ts,
        escape_field(entry.event),
        escape_field(entry.agent_id),
        escape_field(entry.status),
        entry.scope.map(escape_field).unwrap_or_default(),
        escape_field(entry.detail)
    )
    .with_context(|| format!("failed to append agent lifecycle log {}", path.display()))?;
    harden_file_permissions(path, 0o600).with_context(|| {
        format!(
            "failed to harden agent lifecycle log permissions {}",
            path.display()
        )
    })?;
    Ok(())
}

pub(crate) fn upsert_goal_state(path: &Path, state: GoalState) -> Result<()> {
    let mut states = load_goal_states(path)?;
    if let Some(existing) = states.iter_mut().find(|item| {
        room_alias(&item.goal_room).eq_ignore_ascii_case(room_alias(&state.goal_room))
            && item.template.eq_ignore_ascii_case(&state.template)
    }) {
        *existing = state;
    } else {
        states.push(state);
    }
    persist_goal_states(path, &states)
}

pub(crate) fn load_goal_log_records(path: &Path) -> Result<Vec<GoalLogRecord>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create goal log directory {}", parent.display()))?;
        let _ = harden_dir_permissions(parent, 0o700);
    }
    if !path.exists() {
        fs::File::create(path)
            .with_context(|| format!("failed to create goal log file {}", path.display()))?;
        harden_file_permissions(path, 0o600)
            .with_context(|| format!("failed to harden goal log file {}", path.display()))?;
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read goal log {}", path.display()))?;
    let mut records = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() != 7 {
            return Err(anyhow!(
                "invalid goal log at {} line {}: expected 7 fields, got {}",
                path.display(),
                index + 1,
                parts.len()
            ));
        }
        let ts = parts[0]
            .parse::<u64>()
            .with_context(|| format!("invalid goal log timestamp at line {}", index + 1))?;
        let goal_room = {
            let value = unescape_field(parts[3]);
            if value.trim().is_empty() {
                None
            } else {
                Some(value)
            }
        };
        let goal_sender = {
            let value = unescape_field(parts[4]);
            if value.trim().is_empty() {
                None
            } else {
                Some(value)
            }
        };
        records.push(GoalLogRecord {
            ts,
            event: unescape_field(parts[1]),
            workflow_job_id: unescape_field(parts[2]),
            goal_room,
            goal_sender,
            template: unescape_field(parts[5]),
            detail: unescape_field(parts[6]),
        });
    }
    Ok(records)
}

pub(crate) fn load_goal_states(path: &Path) -> Result<Vec<GoalState>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create goal state directory {}", parent.display())
        })?;
        // Best-effort: parent may be a system directory we cannot chmod.
        let _ = harden_dir_permissions(parent, 0o700);
    }
    if !path.exists() {
        fs::File::create(path)
            .with_context(|| format!("failed to create goal state file {}", path.display()))?;
        harden_file_permissions(path, 0o600)
            .with_context(|| format!("failed to harden goal state file {}", path.display()))?;
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read goal state file {}", path.display()))?;
    let mut states = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() != 13 {
            return Err(anyhow!(
                "invalid goal state at {} line {}: expected 13 fields, got {}",
                path.display(),
                index + 1,
                parts.len()
            ));
        }
        let thread_id = parts.get(1).and_then(|v| {
            let s = unescape_field(v);
            if s.trim().is_empty() {
                None
            } else {
                Some(s)
            }
        });
        let updated_at = parts[8]
            .parse::<u64>()
            .with_context(|| format!("invalid goal state timestamp at line {}", index + 1))?;
        let last_run_id = {
            let value = unescape_field(parts[6]);
            if value.trim().is_empty() {
                None
            } else {
                Some(value)
            }
        };
        let owner = {
            let value = unescape_field(parts[7]);
            if value.trim().is_empty() {
                None
            } else {
                Some(value)
            }
        };
        let complexity = parts.get(9).and_then(|v| {
            let s = unescape_field(v);
            if s.trim().is_empty() {
                None
            } else {
                Some(s)
            }
        });
        let pipeline_stage = parts.get(10).and_then(|v| {
            let s = unescape_field(v);
            if s.trim().is_empty() {
                None
            } else {
                Some(s)
            }
        });
        let audit_id = parts.get(11).and_then(|v| {
            let s = unescape_field(v);
            if s.trim().is_empty() {
                None
            } else {
                Some(s)
            }
        });
        let plan_id = parts.get(12).and_then(|v| {
            let s = unescape_field(v);
            if s.trim().is_empty() {
                None
            } else {
                Some(s)
            }
        });
        states.push(GoalState {
            goal_room: unescape_field(parts[0]),
            thread_id,
            project_id: unescape_field(parts[2]),
            template: unescape_field(parts[3]),
            status: unescape_field(parts[4]),
            last_job_id: unescape_field(parts[5]),
            last_run_id,
            owner,
            updated_at,
            complexity,
            pipeline_stage,
            audit_id,
            plan_id,
        });
    }
    states.sort_by(|a, b| {
        a.goal_room
            .cmp(&b.goal_room)
            .then_with(|| a.template.cmp(&b.template))
    });
    Ok(states)
}

pub(crate) fn upsert_agent_state(path: &Path, state: AgentState) -> Result<()> {
    let mut states = load_agent_states(path)?;
    if let Some(existing) = states
        .iter_mut()
        .find(|item| item.agent_id.eq_ignore_ascii_case(&state.agent_id))
    {
        *existing = state;
    } else {
        states.push(state);
    }
    persist_agent_states(path, &states)
}

pub(crate) fn load_agent_states(path: &Path) -> Result<Vec<AgentState>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create agent state directory {}",
                parent.display()
            )
        })?;
        // Best-effort: parent may be a system directory we cannot chmod.
        let _ = harden_dir_permissions(parent, 0o700);
    }
    if !path.exists() {
        fs::File::create(path)
            .with_context(|| format!("failed to create agent state file {}", path.display()))?;
        harden_file_permissions(path, 0o600)
            .with_context(|| format!("failed to harden agent state file {}", path.display()))?;
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read agent state file {}", path.display()))?;
    let mut states = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() != 7 {
            return Err(anyhow!(
                "invalid agent state at {} line {}: expected 7 fields",
                path.display(),
                index + 1
            ));
        }
        let updated_at = parts[6]
            .parse::<u64>()
            .with_context(|| format!("invalid agent state timestamp at line {}", index + 1))?;
        let last_scope = {
            let value = unescape_field(parts[5]);
            if value.trim().is_empty() {
                None
            } else {
                Some(value)
            }
        };
        states.push(AgentState {
            agent_id: unescape_field(parts[0]),
            parent: unescape_field(parts[1]),
            llm_type: unescape_field(parts[2]),
            trust_level: unescape_field(parts[3]),
            last_status: unescape_field(parts[4]),
            last_scope,
            updated_at,
        });
    }
    states.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    Ok(states)
}

fn persist_goal_states(path: &Path, states: &[GoalState]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create goal state directory {}", parent.display())
        })?;
        // Best-effort: parent may be a system directory we cannot chmod.
        let _ = harden_dir_permissions(parent, 0o700);
    }
    let tmp_path = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp_path)
        .with_context(|| format!("failed to write temp goal state {}", tmp_path.display()))?;

    let mut sorted = states.to_vec();
    sorted.sort_by(|a, b| a.goal_room.cmp(&b.goal_room));
    for state in sorted {
        writeln!(
            file,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            escape_field(&state.goal_room),
            escape_field(state.thread_id.as_deref().unwrap_or("")),
            escape_field(&state.project_id),
            escape_field(&state.template),
            escape_field(&state.status),
            escape_field(&state.last_job_id),
            escape_field(state.last_run_id.as_deref().unwrap_or("")),
            escape_field(state.owner.as_deref().unwrap_or("")),
            state.updated_at,
            escape_field(state.complexity.as_deref().unwrap_or("")),
            escape_field(state.pipeline_stage.as_deref().unwrap_or("")),
            escape_field(state.audit_id.as_deref().unwrap_or("")),
            escape_field(state.plan_id.as_deref().unwrap_or(""))
        )
        .with_context(|| format!("failed to write goal state {}", tmp_path.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed to flush goal state {}", tmp_path.display()))?;
    harden_file_permissions(&tmp_path, 0o600)
        .with_context(|| format!("failed to harden temp goal state {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to replace goal state file {} from {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    harden_file_permissions(path, 0o600)
        .with_context(|| format!("failed to harden goal state file {}", path.display()))?;
    Ok(())
}

fn persist_agent_states(path: &Path, states: &[AgentState]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create agent state directory {}",
                parent.display()
            )
        })?;
        // Best-effort: parent may be a system directory we cannot chmod.
        let _ = harden_dir_permissions(parent, 0o700);
    }
    let tmp_path = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp_path)
        .with_context(|| format!("failed to write temp agent state {}", tmp_path.display()))?;

    let mut sorted = states.to_vec();
    sorted.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    for state in sorted {
        writeln!(
            file,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            escape_field(&state.agent_id),
            escape_field(&state.parent),
            escape_field(&state.llm_type),
            escape_field(&state.trust_level),
            escape_field(&state.last_status),
            escape_field(state.last_scope.as_deref().unwrap_or("")),
            state.updated_at
        )
        .with_context(|| format!("failed to write agent state {}", tmp_path.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed to flush agent state {}", tmp_path.display()))?;
    harden_file_permissions(&tmp_path, 0o600)
        .with_context(|| format!("failed to harden temp agent state {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to replace agent state file {} from {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    harden_file_permissions(path, 0o600)
        .with_context(|| format!("failed to harden agent state file {}", path.display()))?;
    Ok(())
}
