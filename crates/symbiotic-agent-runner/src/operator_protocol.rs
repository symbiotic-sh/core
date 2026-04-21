use serde::{Deserialize, Serialize};

const CHECKPOINT_SUMMARY_LIMIT: usize = 1200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionContextPacket {
    pub protocol_version: String,
    pub agent_id: String,
    pub goal_scope: Option<String>,
    pub thread_id: Option<String>,
    pub goal: String,
    pub task_context: Option<String>,
    pub context_sources: Vec<String>,
    pub canonical_truth: String,
    pub derived_surfaces: String,
    pub verification_order: Vec<String>,
    pub checkpoint_rule: String,
}

impl ExecutionContextPacket {
    pub fn prompt_prelude(&self) -> String {
        let context_sources = self
            .context_sources
            .iter()
            .map(|source| format!("- {source}"))
            .collect::<Vec<_>>()
            .join("\n");
        let verification_order = self
            .verification_order
            .iter()
            .enumerate()
            .map(|(idx, item)| format!("{}. {item}", idx + 1))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            "Operator Protocol Context Packet\n\
Protocol version: {}\n\
Agent: {}\n\
Goal scope: {}\n\
Thread attachment: {}\n\
\n\
Context sources:\n\
{}\n\
\n\
Canonical truth:\n\
{}\n\
\n\
Derived surfaces:\n\
{}\n\
\n\
Verification order:\n\
{}\n\
\n\
Checkpoint rule:\n\
{}\n",
            self.protocol_version,
            self.agent_id,
            self.goal_scope.as_deref().unwrap_or("global"),
            self.thread_id.as_deref().unwrap_or("none"),
            context_sources,
            self.canonical_truth,
            self.derived_surfaces,
            verification_order,
            self.checkpoint_rule,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionCheckpointArtifact {
    pub protocol_version: String,
    pub agent_id: String,
    pub goal_scope: Option<String>,
    pub iterations: usize,
    pub context_packet_loaded: bool,
    pub context_sources_read: Vec<String>,
    pub checkpoint_summary: String,
}

impl ExecutionCheckpointArtifact {
    pub fn from_execution(
        packet: &ExecutionContextPacket,
        iterations: usize,
        output: &str,
    ) -> Self {
        Self {
            protocol_version: packet.protocol_version.clone(),
            agent_id: packet.agent_id.clone(),
            goal_scope: packet.goal_scope.clone(),
            iterations,
            context_packet_loaded: true,
            context_sources_read: packet.context_sources.clone(),
            checkpoint_summary: summarize_checkpoint_output(output),
        }
    }
}

fn summarize_checkpoint_output(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.len() <= CHECKPOINT_SUMMARY_LIMIT {
        return trimmed.to_string();
    }

    let mut summary = trimmed
        .chars()
        .take(CHECKPOINT_SUMMARY_LIMIT)
        .collect::<String>();
    summary.push_str("...");
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_prelude_includes_boundary_and_verification() {
        let packet = ExecutionContextPacket {
            protocol_version: "v1".to_string(),
            agent_id: "agent-1".to_string(),
            goal_scope: Some("wf-1".to_string()),
            thread_id: Some("thread-1".to_string()),
            goal: "Ship the slice".to_string(),
            task_context: Some("Current task state".to_string()),
            context_sources: vec!["CONTEXT.md".to_string(), "tasks/NEXT.md".to_string()],
            canonical_truth: "ledger/*.md records".to_string(),
            derived_surfaces: "briefs and thread docs".to_string(),
            verification_order: vec![
                "source-of-truth mutation".to_string(),
                "orchestrator follow-through".to_string(),
            ],
            checkpoint_rule: "Checkpoint after each coherent slice.".to_string(),
        };

        let prompt = packet.prompt_prelude();
        assert!(prompt.contains("Canonical truth"));
        assert!(prompt.contains("Verification order"));
        assert!(prompt.contains("CONTEXT.md"));
        assert!(prompt.contains("wf-1"));
        assert!(prompt.contains("thread-1"));
    }

    #[test]
    fn checkpoint_artifact_truncates_large_output() {
        let packet = ExecutionContextPacket {
            protocol_version: "v1".to_string(),
            agent_id: "agent-1".to_string(),
            goal_scope: None,
            thread_id: None,
            goal: "Goal".to_string(),
            task_context: None,
            context_sources: vec!["CONTEXT.md".to_string()],
            canonical_truth: "truth".to_string(),
            derived_surfaces: "derived".to_string(),
            verification_order: vec!["verify".to_string()],
            checkpoint_rule: "checkpoint".to_string(),
        };
        let long_output = "x".repeat(CHECKPOINT_SUMMARY_LIMIT + 50);

        let artifact = ExecutionCheckpointArtifact::from_execution(&packet, 3, &long_output);

        assert!(artifact.context_packet_loaded);
        assert_eq!(
            artifact.context_sources_read,
            vec!["CONTEXT.md".to_string()]
        );
        assert!(artifact.checkpoint_summary.ends_with("..."));
    }
}
