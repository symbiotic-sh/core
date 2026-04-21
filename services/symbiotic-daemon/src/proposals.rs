//! Self-improvement bridge: converts system proposals into goals.
//!
//! Proposals from friction detection or metrics analysis are stored as
//! `PendingProposal`s. When approved (by user command or auto-approve policy),
//! they convert into goals that enter the standard deliberation pipeline.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// A system-originated proposal awaiting approval to become a goal.
#[derive(Debug, Clone)]
pub(crate) struct PendingProposal {
    pub id: String,
    pub source: ProposalSource,
    pub description: String,
    pub suggestion: String,
    /// Thread where the proposal was detected (friction) or None (metrics).
    pub thread_id: Option<String>,
}

/// Where the proposal originated.
#[derive(Debug, Clone)]
pub(crate) enum ProposalSource {
    Friction,
    RecallProbe,
}

impl ProposalSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Friction => "friction",
            Self::RecallProbe => "recall_probe",
        }
    }
}

/// Thread-safe in-memory store for pending proposals.
///
/// Proposals are ephemeral — they don't survive daemon restarts. This is
/// intentional: friction detection re-runs periodically and will re-detect
/// the same issues. No need for persistence.
pub(crate) struct ProposalStore {
    proposals: Mutex<HashMap<String, PendingProposal>>,
}

impl ProposalStore {
    pub fn new() -> Self {
        Self {
            proposals: Mutex::new(HashMap::new()),
        }
    }

    /// Store a new proposal and return its ID.
    pub fn insert(&self, proposal: PendingProposal) -> String {
        let id = proposal.id.clone();
        if let Ok(mut map) = self.proposals.lock() {
            map.insert(id.clone(), proposal);
        }
        id
    }

    /// Remove and return a proposal by ID (consume on approve/dismiss).
    pub fn take(&self, id: &str) -> Option<PendingProposal> {
        self.proposals.lock().ok()?.remove(id)
    }

    /// Number of pending proposals.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.proposals.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// Generate a unique proposal ID (timestamp-based, same pattern as goal IDs).
pub(crate) fn generate_proposal_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("prop-{:x}", ts)
}

pub(crate) fn generate_recall_probe_proposal_id(target_kind: &str, target_id: &str) -> String {
    let normalized = target_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    format!("prop-recall-{target_kind}-{normalized}")
}

/// Format a friction proposal as a goal description for the deliberation pipeline.
pub(crate) fn format_proposal_as_goal(proposal: &PendingProposal) -> String {
    format!(
        "[Self-improvement: {}] {}\n\nContext: {}\nSuggested action: {}",
        proposal.source.as_str(),
        proposal.suggestion,
        proposal.description,
        proposal.suggestion,
    )
}
