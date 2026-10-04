use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use symbiotic_agent_runner::{ExecutionCheckpointArtifact, ExecutionContextPacket};
use symbiotic_agents::builtin_tools::{PendingQuestion, ProposedPlan};

use crate::auth_jobs::PendingAuthRequest;
use crate::{harden_dir_permissions, harden_file_permissions};

#[derive(Debug, Clone, Default)]
pub struct BridgeSessionArtifacts {
    pub context_packet: Option<ExecutionContextPacket>,
    pub pending_question: Option<PendingQuestion>,
    pub pending_plan: Option<ProposedPlan>,
    pub pending_auth_request: Option<PendingAuthRequest>,
    pub checkpoint_artifact: Option<ExecutionCheckpointArtifact>,
}

#[derive(Debug, Default)]
pub struct BridgeSessionStore {
    sessions: HashMap<String, BridgeSessionArtifacts>,
}

impl BridgeSessionStore {
    pub(crate) fn record_context_packet(
        &mut self,
        token_id: &str,
        context_packet: ExecutionContextPacket,
    ) {
        self.sessions
            .entry(token_id.to_string())
            .or_default()
            .context_packet = Some(context_packet);
    }

    pub(crate) fn record_pending_question(&mut self, token_id: &str, question: PendingQuestion) {
        self.sessions
            .entry(token_id.to_string())
            .or_default()
            .pending_question = Some(question);
    }

    pub(crate) fn record_pending_plan(&mut self, token_id: &str, plan: ProposedPlan) {
        self.sessions
            .entry(token_id.to_string())
            .or_default()
            .pending_plan = Some(plan);
    }

    pub(crate) fn record_pending_auth_request(
        &mut self,
        token_id: &str,
        auth_request: PendingAuthRequest,
    ) {
        self.sessions
            .entry(token_id.to_string())
            .or_default()
            .pending_auth_request = Some(auth_request);
    }

    pub(crate) fn record_checkpoint_artifact(
        &mut self,
        token_id: &str,
        artifact: ExecutionCheckpointArtifact,
    ) {
        self.sessions
            .entry(token_id.to_string())
            .or_default()
            .checkpoint_artifact = Some(artifact);
    }

    pub fn take_artifacts(&mut self, token_id: &str) -> BridgeSessionArtifacts {
        self.sessions.remove(token_id).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeInteractionKind {
    ContextPacketLoaded,
    PendingQuestion,
    ProposedPlan,
    PendingAuthRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRuntimeLogEntryType {
    Tool,
    Result,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeInteractionRecord {
    pub event_id: String,
    pub token_id: String,
    pub agent_id: String,
    pub goal_scope: Option<String>,
    pub thread_id: Option<String>,
    pub kind: BridgeInteractionKind,
    pub summary: String,
    pub detail: String,
    pub created_at: u64,
    pub raw_payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedCheckpointArtifact {
    pub token_id: String,
    pub agent_id: String,
    pub goal_scope: Option<String>,
    pub created_at: u64,
    pub artifact: ExecutionCheckpointArtifact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRuntimeLogRecord {
    pub event_id: String,
    pub token_id: String,
    pub agent_id: String,
    pub goal_scope: Option<String>,
    pub thread_id: Option<String>,
    pub entry_type: AgentRuntimeLogEntryType,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_params: Option<String>,
    pub created_at: u64,
    pub raw_payload: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct BridgeInteractionLogStore {
    path: PathBuf,
    records: Vec<BridgeInteractionRecord>,
}

impl BridgeInteractionLogStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("bridge").join("raw-events.jsonl"),
            records: Vec::new(),
        }
    }

    pub fn load(&mut self) -> Result<()> {
        self.records.clear();
        if !self.path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        for (index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record =
                serde_json::from_str::<BridgeInteractionRecord>(line).with_context(|| {
                    format!(
                        "failed to decode bridge interaction at {} line {}",
                        self.path.display(),
                        index + 1
                    )
                })?;
            self.records.push(record);
        }
        Ok(())
    }

    pub fn append(&mut self, record: BridgeInteractionRecord) -> Result<()> {
        append_jsonl_record(&self.path, &record)?;
        self.records.push(record);
        Ok(())
    }

    pub fn recent_for_thread(
        &self,
        thread_id: &str,
        goal_scopes: &[String],
        limit: usize,
    ) -> Vec<BridgeInteractionRecord> {
        let mut records: Vec<BridgeInteractionRecord> = self
            .records
            .iter()
            .filter(|record| {
                record.thread_id.as_deref() == Some(thread_id)
                    || record
                        .goal_scope
                        .as_ref()
                        .is_some_and(|scope| goal_scopes.iter().any(|item| item == scope))
            })
            .cloned()
            .collect();
        records.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        records.truncate(limit);
        records
    }
}

#[derive(Debug, Clone)]
pub struct BridgeCheckpointStore {
    path: PathBuf,
    checkpoints: Vec<PersistedCheckpointArtifact>,
}

impl BridgeCheckpointStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("bridge").join("checkpoints.jsonl"),
            checkpoints: Vec::new(),
        }
    }

    pub fn load(&mut self) -> Result<()> {
        self.checkpoints.clear();
        if !self.path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        for (index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let checkpoint = serde_json::from_str::<PersistedCheckpointArtifact>(line)
                .with_context(|| {
                    format!(
                        "failed to decode bridge checkpoint at {} line {}",
                        self.path.display(),
                        index + 1
                    )
                })?;
            self.checkpoints.push(checkpoint);
        }
        Ok(())
    }

    pub fn append(&mut self, checkpoint: PersistedCheckpointArtifact) -> Result<()> {
        append_jsonl_record(&self.path, &checkpoint)?;
        self.checkpoints.push(checkpoint);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AgentRuntimeLogStore {
    path: PathBuf,
    records: Vec<AgentRuntimeLogRecord>,
}

impl AgentRuntimeLogStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("bridge").join("agent-logs.jsonl"),
            records: Vec::new(),
        }
    }

    pub fn load(&mut self) -> Result<()> {
        self.records.clear();
        if !self.path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        for (index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record =
                serde_json::from_str::<AgentRuntimeLogRecord>(line).with_context(|| {
                    format!(
                        "failed to decode agent runtime log at {} line {}",
                        self.path.display(),
                        index + 1
                    )
                })?;
            self.records.push(record);
        }
        Ok(())
    }

    pub fn append(&mut self, record: AgentRuntimeLogRecord) -> Result<()> {
        append_jsonl_record(&self.path, &record)?;
        self.records.push(record);
        Ok(())
    }

    pub fn recent_for_thread(
        &self,
        thread_id: &str,
        goal_scopes: &[String],
        limit: usize,
    ) -> Vec<AgentRuntimeLogRecord> {
        let mut records: Vec<AgentRuntimeLogRecord> = self
            .records
            .iter()
            .filter(|record| {
                record.thread_id.as_deref() == Some(thread_id)
                    || record
                        .goal_scope
                        .as_ref()
                        .is_some_and(|scope| goal_scopes.iter().any(|item| item == scope))
            })
            .cloned()
            .collect();
        records.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        records.truncate(limit);
        records
    }
}

fn append_jsonl_record<T: Serialize>(path: &Path, record: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let _ = harden_dir_permissions(parent, 0o700);
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("failed to encode {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to append newline to {}", path.display()))?;
    harden_file_permissions(path, 0o600)
        .with_context(|| format!("failed to harden {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_log_store_round_trips_and_filters_by_thread_or_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = BridgeInteractionLogStore::new(dir.path());
        store
            .append(BridgeInteractionRecord {
                event_id: "evt-1".to_string(),
                token_id: "token-1".to_string(),
                agent_id: "agent-1".to_string(),
                goal_scope: Some("run-1".to_string()),
                thread_id: None,
                kind: BridgeInteractionKind::PendingQuestion,
                summary: "Needs budget".to_string(),
                detail: "What budget?".to_string(),
                created_at: 10,
                raw_payload: serde_json::json!({"question":"What budget?"}),
            })
            .expect("append first");
        store
            .append(BridgeInteractionRecord {
                event_id: "evt-2".to_string(),
                token_id: "token-2".to_string(),
                agent_id: "agent-2".to_string(),
                goal_scope: None,
                thread_id: Some("thread-9".to_string()),
                kind: BridgeInteractionKind::PendingAuthRequest,
                summary: "Needs GitHub login".to_string(),
                detail: "Authenticate with github.com".to_string(),
                created_at: 20,
                raw_payload: serde_json::json!({"target":"github.com"}),
            })
            .expect("append second");

        let mut reopened = BridgeInteractionLogStore::new(dir.path());
        reopened.load().expect("load");

        let by_scope = reopened.recent_for_thread("other-thread", &["run-1".to_string()], 8);
        assert_eq!(by_scope.len(), 1);
        assert_eq!(by_scope[0].event_id, "evt-1");

        let by_thread = reopened.recent_for_thread("thread-9", &[], 8);
        assert_eq!(by_thread.len(), 1);
        assert_eq!(by_thread[0].event_id, "evt-2");
    }

    #[test]
    fn checkpoint_store_round_trips_persisted_artifacts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = BridgeCheckpointStore::new(dir.path());
        store
            .append(PersistedCheckpointArtifact {
                token_id: "token-1".to_string(),
                agent_id: "agent-1".to_string(),
                goal_scope: Some("run-1".to_string()),
                created_at: 10,
                artifact: ExecutionCheckpointArtifact {
                    protocol_version: "v1".to_string(),
                    agent_id: "agent-1".to_string(),
                    goal_scope: Some("run-1".to_string()),
                    iterations: 3,
                    context_packet_loaded: true,
                    context_sources_read: vec!["CONTEXT.md".to_string()],
                    checkpoint_summary: "Landed the slice.".to_string(),
                },
            })
            .expect("append checkpoint");

        let mut reopened = BridgeCheckpointStore::new(dir.path());
        reopened.load().expect("load");
        assert_eq!(reopened.checkpoints.len(), 1);
        assert_eq!(reopened.checkpoints[0].artifact.iterations, 3);
    }

    #[test]
    fn agent_runtime_log_store_round_trips_and_filters_by_thread_or_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AgentRuntimeLogStore::new(dir.path());
        store
            .append(AgentRuntimeLogRecord {
                event_id: "log-1".to_string(),
                token_id: "token-1".to_string(),
                agent_id: "agent-1".to_string(),
                goal_scope: Some("run-1".to_string()),
                thread_id: None,
                entry_type: AgentRuntimeLogEntryType::Tool,
                content: "Calling tool: read_file".to_string(),
                tool_name: Some("read_file".to_string()),
                tool_params: Some("{\"path\":\"README.md\"}".to_string()),
                created_at: 10,
                raw_payload: serde_json::json!({"tool":"read_file"}),
            })
            .expect("append first");
        store
            .append(AgentRuntimeLogRecord {
                event_id: "log-2".to_string(),
                token_id: "token-2".to_string(),
                agent_id: "agent-2".to_string(),
                goal_scope: None,
                thread_id: Some("thread-9".to_string()),
                entry_type: AgentRuntimeLogEntryType::Blocked,
                content: "Permission denied".to_string(),
                tool_name: Some("shell".to_string()),
                tool_params: None,
                created_at: 20,
                raw_payload: serde_json::json!({"error":"Permission denied"}),
            })
            .expect("append second");

        let mut reopened = AgentRuntimeLogStore::new(dir.path());
        reopened.load().expect("load");

        let by_scope = reopened.recent_for_thread("other-thread", &["run-1".to_string()], 8);
        assert_eq!(by_scope.len(), 1);
        assert_eq!(by_scope[0].event_id, "log-1");

        let by_thread = reopened.recent_for_thread("thread-9", &[], 8);
        assert_eq!(by_thread.len(), 1);
        assert_eq!(by_thread[0].event_id, "log-2");
    }
}
