use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRuntimeStatusKind {
    Starting,
    Running,
    Waiting,
    Blocked,
    Completed,
    Failed,
}

impl AgentRuntimeStatusKind {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            AgentRuntimeStatusKind::Starting
                | AgentRuntimeStatusKind::Running
                | AgentRuntimeStatusKind::Waiting
                | AgentRuntimeStatusKind::Blocked
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRuntimeProfile {
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub max_iterations: Option<u32>,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRuntimeStatus {
    pub agent_id: String,
    pub token_id: String,
    pub goal_scope: Option<String>,
    pub thread_id: Option<String>,
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub status: AgentRuntimeStatusKind,
    pub detail: Option<String>,
    pub current_iteration: Option<u32>,
    pub max_iterations: Option<u32>,
    pub active_tool_name: Option<String>,
    pub updated_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedAgentRuntimeStatuses {
    #[serde(default)]
    statuses: Vec<AgentRuntimeStatus>,
}

#[derive(Debug, Clone)]
pub struct AgentRuntimeStatusStore {
    data_dir: PathBuf,
    statuses: HashMap<String, AgentRuntimeStatus>,
}

impl AgentRuntimeStatusStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            statuses: HashMap::new(),
        }
    }

    fn store_path(&self) -> PathBuf {
        self.data_dir.join("agents").join("runtime-status.json")
    }

    pub fn load(&mut self) -> Result<()> {
        let path = self.store_path();
        if !path.exists() {
            return Ok(());
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let persisted = serde_json::from_str::<PersistedAgentRuntimeStatuses>(&data)
            .with_context(|| format!("failed to decode {}", path.display()))?;
        self.statuses.clear();
        for status in persisted.statuses {
            self.statuses.insert(status.agent_id.clone(), status);
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let path = self.store_path();
        let dir = path
            .parent()
            .expect("agent runtime status path should have parent");
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let mut statuses: Vec<&AgentRuntimeStatus> = self.statuses.values().collect();
        statuses.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        let json = serde_json::to_string_pretty(&PersistedAgentRuntimeStatuses {
            statuses: statuses.into_iter().cloned().collect(),
        })
        .with_context(|| format!("failed to encode {}", path.display()))?;
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, json.as_bytes())
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }

    pub fn get(&self, agent_id: &str) -> Option<&AgentRuntimeStatus> {
        self.statuses.get(agent_id)
    }

    pub fn upsert(&mut self, status: AgentRuntimeStatus) -> Result<()> {
        self.statuses.insert(status.agent_id.clone(), status);
        self.save()
    }

    pub fn record_handshake(
        &mut self,
        agent_id: String,
        token_id: String,
        goal_scope: Option<String>,
        profile: AgentRuntimeProfile,
        updated_at: u64,
    ) -> Result<()> {
        self.upsert(AgentRuntimeStatus {
            agent_id,
            token_id,
            goal_scope,
            thread_id: profile.thread_id,
            role: profile.role,
            sandbox_type: profile.sandbox_type,
            model_label: profile.model_label,
            status: AgentRuntimeStatusKind::Starting,
            detail: Some("Bridge session connected".to_string()),
            current_iteration: None,
            max_iterations: profile.max_iterations,
            active_tool_name: None,
            updated_at,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_event(
        &mut self,
        agent_id: &str,
        token_id: &str,
        goal_scope: Option<String>,
        thread_id: Option<String>,
        status: AgentRuntimeStatusKind,
        detail: Option<String>,
        current_iteration: Option<u32>,
        max_iterations: Option<u32>,
        active_tool_name: Option<String>,
        updated_at: u64,
    ) -> Result<()> {
        let mut current = self
            .statuses
            .get(agent_id)
            .cloned()
            .unwrap_or(AgentRuntimeStatus {
                agent_id: agent_id.to_string(),
                token_id: token_id.to_string(),
                goal_scope: goal_scope.clone(),
                thread_id: thread_id.clone(),
                role: None,
                sandbox_type: "unknown".to_string(),
                model_label: None,
                status: AgentRuntimeStatusKind::Starting,
                detail: None,
                current_iteration: None,
                max_iterations: None,
                active_tool_name: None,
                updated_at,
            });
        current.token_id = token_id.to_string();
        current.goal_scope = goal_scope.or(current.goal_scope);
        current.thread_id = thread_id.or(current.thread_id);
        current.status = status;
        current.detail = detail;
        current.current_iteration = current_iteration.or(current.current_iteration);
        current.max_iterations = max_iterations.or(current.max_iterations);
        current.active_tool_name = active_tool_name;
        current.updated_at = updated_at;
        self.upsert(current)
    }

    pub fn statuses_for_thread(
        &self,
        thread_id: &str,
        goal_scopes: &[String],
        limit: usize,
    ) -> Vec<AgentRuntimeStatus> {
        let mut statuses: Vec<AgentRuntimeStatus> = self
            .statuses
            .values()
            .filter(|status| {
                status.thread_id.as_deref() == Some(thread_id)
                    || status
                        .goal_scope
                        .as_ref()
                        .is_some_and(|scope| goal_scopes.iter().any(|item| item == scope))
            })
            .cloned()
            .collect();
        statuses.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        statuses.truncate(limit);
        statuses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips_runtime_statuses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AgentRuntimeStatusStore::new(dir.path());
        store
            .record_handshake(
                "agent-1".to_string(),
                "token-1".to_string(),
                Some("run-1".to_string()),
                AgentRuntimeProfile {
                    role: Some("coder".to_string()),
                    sandbox_type: "vm_sandbox".to_string(),
                    model_label: Some("hybrid:cloud:local".to_string()),
                    max_iterations: Some(15),
                    thread_id: Some("thread-1".to_string()),
                },
                10,
            )
            .expect("record handshake");
        store
            .record_event(
                "agent-1",
                "token-1",
                Some("run-1".to_string()),
                None,
                AgentRuntimeStatusKind::Running,
                Some("Iteration 3/15".to_string()),
                Some(3),
                Some(15),
                Some("read_file".to_string()),
                20,
            )
            .expect("record event");

        let mut reopened = AgentRuntimeStatusStore::new(dir.path());
        reopened.load().expect("load");
        let status = reopened.get("agent-1").expect("status");
        assert_eq!(status.role.as_deref(), Some("coder"));
        assert_eq!(status.sandbox_type, "vm_sandbox");
        assert_eq!(status.current_iteration, Some(3));
        assert_eq!(status.active_tool_name.as_deref(), Some("read_file"));
    }

    #[test]
    fn statuses_for_thread_match_thread_or_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AgentRuntimeStatusStore::new(dir.path());
        store
            .upsert(AgentRuntimeStatus {
                agent_id: "agent-1".to_string(),
                token_id: "token-1".to_string(),
                goal_scope: Some("run-1".to_string()),
                thread_id: None,
                role: Some("coder".to_string()),
                sandbox_type: "local_process".to_string(),
                model_label: None,
                status: AgentRuntimeStatusKind::Running,
                detail: None,
                current_iteration: Some(1),
                max_iterations: Some(15),
                active_tool_name: None,
                updated_at: 10,
            })
            .expect("upsert");
        store
            .upsert(AgentRuntimeStatus {
                agent_id: "agent-2".to_string(),
                token_id: "token-2".to_string(),
                goal_scope: None,
                thread_id: Some("thread-9".to_string()),
                role: None,
                sandbox_type: "vm_sandbox".to_string(),
                model_label: None,
                status: AgentRuntimeStatusKind::Waiting,
                detail: None,
                current_iteration: None,
                max_iterations: None,
                active_tool_name: None,
                updated_at: 20,
            })
            .expect("upsert");

        let by_scope = store.statuses_for_thread("other-thread", &["run-1".to_string()], 8);
        assert_eq!(by_scope.len(), 1);
        assert_eq!(by_scope[0].agent_id, "agent-1");

        let by_thread = store.statuses_for_thread("thread-9", &[], 8);
        assert_eq!(by_thread.len(), 1);
        assert_eq!(by_thread[0].agent_id, "agent-2");
    }
}
