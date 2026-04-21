//! Server-side approval-gate state machine for operations like external-push
//! that require `requires_operator_approval_for` gating. Tickets carry rich
//! `ApprovalContext` so the approving channel (matrix/push/app) has everything
//! needed to decide. See `docs/design/repo-manifest.md`
//! §agent_scopes.requires_operator_approval_for and T126 §07b spec.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use symbiotic_agents::FindingSeverity;
use thiserror::Error;
use uuid::Uuid;

/// Typed enum naming the operation an approval ticket is gating. Replaces
/// the prior free-form `String` field (T128 §15 D1 smell-fix). New ticket
/// kinds add a variant here; the matching detail metadata lives in an
/// `archeology_detail`-style optional field on `ApprovalContext`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOperation {
    /// T126 mirror push to an external remote (`mirror_push_with_approval`).
    PushExternal,
    /// T128 §15 archeology finding push (`archeology_dispatch::dispatch_finding`).
    ArcheologyPush,
}

/// Per-archeology-push detail bundle — populated when
/// `ApprovalContext.operation == ArcheologyPush`. Future operation kinds add
/// their own additive `Option<...>` fields rather than expanding this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcheologyApprovalDetail {
    pub finding_id: String,
    pub evidence_path: String,
    pub severity: FindingSeverity,
    pub category: String,
    pub rationale: String,
}

/// Rich context carried by every approval ticket. Enough to decide without
/// leaving the notification; inspect affordance fetches the diff on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalContext {
    // Decision frame
    /// Typed operation kind (T128 §15 D1: was `String`).
    pub operation: ApprovalOperation,
    /// Human-readable "Agent {id} wants to push {n} commits to {remote}:{branch}
    /// from goal {goal_id}". 1-2 sentences, suitable for a matrix message body
    /// or a notification body.
    pub explanation: String,

    // Targets
    /// e.g. "repo:flux"
    pub repo_id: String,
    /// e.g. "git@github.com:kakajansh/flux.git"
    pub remote_url: String,
    /// e.g. "data/git-server/repos/flux.git"
    pub local_bare_path: String,
    /// e.g. "agent/fix-auth" (the ref being pushed)
    pub branch: String,
    /// True if branch matches source.protected_branches
    pub is_protected_branch: bool,

    // Who / Why
    /// The requesting agent
    pub agent_id: String,
    /// Originating goal if any
    pub goal_id: Option<String>,
    /// e.g. "project:flux"
    pub project_id: String,

    // What (diff summary for the decision; full diff fetched via inspect)
    pub commit_range: CommitRange,
    pub diff_stats: DiffStats,
    /// First line of HEAD commit
    pub top_commit_message: String,

    /// Kind-specific metadata. Currently only populated when
    /// `operation == ArcheologyPush`; future kinds extend with their own
    /// additive `Option<...>` fields rather than expanding existing types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archeology_detail: Option<ArcheologyApprovalDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitRange {
    /// Current remote HEAD of target branch (or "0000...")
    pub from_sha: String,
    /// Local bare HEAD being pushed
    pub to_sha: String,
    pub commit_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffStats {
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum ApprovalState {
    Pending,
    Approved {
        approved_by: String,
        at: u64,
    },
    Denied {
        denied_by: String,
        at: u64,
        reason: Option<String>,
    },
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalTicket {
    pub ticket_id: String,
    pub context: ApprovalContext,
    pub requested_at: u64,
    pub state: ApprovalState,
    pub ttl_secs: u64,
}

#[derive(Debug, Default)]
pub struct ApprovalGate {
    by_ticket_id: HashMap<String, ApprovalTicket>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ApprovalGateError {
    #[error("ticket not found: {0}")]
    TicketNotFound(String),
    #[error("ticket not pending (current state: {0})")]
    NotPending(&'static str),
}

impl ApprovalGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a new ticket with a rich context. Returns the ticket so the caller
    /// can read ticket_id + embed it in matrix/push messages.
    pub fn open_ticket(
        &mut self,
        context: ApprovalContext,
        now: u64,
        ttl_secs: u64,
    ) -> ApprovalTicket {
        let ticket_id = Uuid::new_v4().to_string();
        let ticket = ApprovalTicket {
            ticket_id: ticket_id.clone(),
            context,
            requested_at: now,
            state: ApprovalState::Pending,
            ttl_secs,
        };
        self.by_ticket_id.insert(ticket_id, ticket.clone());
        ticket
    }

    pub fn approve(
        &mut self,
        ticket_id: &str,
        approved_by: &str,
        now: u64,
    ) -> Result<(), ApprovalGateError> {
        let ticket = self
            .by_ticket_id
            .get_mut(ticket_id)
            .ok_or_else(|| ApprovalGateError::TicketNotFound(ticket_id.to_string()))?;
        match &ticket.state {
            ApprovalState::Pending => {
                ticket.state = ApprovalState::Approved {
                    approved_by: approved_by.to_string(),
                    at: now,
                };
                Ok(())
            }
            ApprovalState::Approved { .. } => Err(ApprovalGateError::NotPending("approved")),
            ApprovalState::Denied { .. } => Err(ApprovalGateError::NotPending("denied")),
            ApprovalState::Expired => Err(ApprovalGateError::NotPending("expired")),
        }
    }

    pub fn deny(
        &mut self,
        ticket_id: &str,
        denied_by: &str,
        now: u64,
        reason: Option<String>,
    ) -> Result<(), ApprovalGateError> {
        let ticket = self
            .by_ticket_id
            .get_mut(ticket_id)
            .ok_or_else(|| ApprovalGateError::TicketNotFound(ticket_id.to_string()))?;
        match &ticket.state {
            ApprovalState::Pending => {
                ticket.state = ApprovalState::Denied {
                    denied_by: denied_by.to_string(),
                    at: now,
                    reason,
                };
                Ok(())
            }
            ApprovalState::Approved { .. } => Err(ApprovalGateError::NotPending("approved")),
            ApprovalState::Denied { .. } => Err(ApprovalGateError::NotPending("denied")),
            ApprovalState::Expired => Err(ApprovalGateError::NotPending("expired")),
        }
    }

    pub fn expire_stale(&mut self, now: u64) {
        for ticket in self.by_ticket_id.values_mut() {
            if matches!(ticket.state, ApprovalState::Pending)
                && ticket.requested_at + ticket.ttl_secs <= now
            {
                ticket.state = ApprovalState::Expired;
            }
        }
    }

    pub fn get(&self, ticket_id: &str) -> Option<&ApprovalTicket> {
        self.by_ticket_id.get(ticket_id)
    }

    #[allow(dead_code)] // wired in when operator pending-ticket UI lands
    pub fn list_pending(&self) -> Vec<&ApprovalTicket> {
        self.by_ticket_id
            .values()
            .filter(|t| matches!(t.state, ApprovalState::Pending))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_context() -> ApprovalContext {
        ApprovalContext {
            operation: ApprovalOperation::PushExternal,
            explanation: "Agent a1 wants to push 2 commits to origin:main from goal g1".to_string(),
            repo_id: "repo:flux".to_string(),
            remote_url: "git@github.com:kakajansh/flux.git".to_string(),
            local_bare_path: "data/git-server/repos/flux.git".to_string(),
            branch: "agent/fix-auth".to_string(),
            is_protected_branch: false,
            agent_id: "agent-1".to_string(),
            goal_id: Some("goal-1".to_string()),
            project_id: "project:flux".to_string(),
            commit_range: CommitRange {
                from_sha: "0000000000000000000000000000000000000000".to_string(),
                to_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
                commit_count: 2,
            },
            diff_stats: DiffStats {
                files_changed: 3,
                insertions: 42,
                deletions: 7,
            },
            top_commit_message: "fix: tighten auth flow".to_string(),
            archeology_detail: None,
        }
    }

    #[test]
    fn open_ticket_creates_pending_with_context() {
        let mut gate = ApprovalGate::new();
        let ctx = build_context();
        let ticket = gate.open_ticket(ctx.clone(), 1000, 3600);

        assert_eq!(ticket.state, ApprovalState::Pending);
        assert_eq!(ticket.requested_at, 1000);
        assert_eq!(ticket.ttl_secs, 3600);
        assert_eq!(ticket.context.operation, ctx.operation);
        assert_eq!(ticket.context.agent_id, ctx.agent_id);
        assert!(!ticket.ticket_id.is_empty());

        let stored = gate.get(&ticket.ticket_id).expect("ticket stored");
        assert_eq!(stored.ticket_id, ticket.ticket_id);
        assert_eq!(stored.state, ApprovalState::Pending);
    }

    #[test]
    fn approve_transitions_to_approved() {
        let mut gate = ApprovalGate::new();
        let ticket = gate.open_ticket(build_context(), 1000, 3600);

        gate.approve(&ticket.ticket_id, "operator", 1200)
            .expect("approve ok");

        let stored = gate.get(&ticket.ticket_id).expect("ticket stored");
        assert_eq!(
            stored.state,
            ApprovalState::Approved {
                approved_by: "operator".to_string(),
                at: 1200,
            }
        );
    }

    #[test]
    fn deny_transitions_to_denied_with_reason() {
        let mut gate = ApprovalGate::new();
        let ticket = gate.open_ticket(build_context(), 1000, 3600);

        gate.deny(
            &ticket.ticket_id,
            "operator",
            1300,
            Some("too risky".to_string()),
        )
        .expect("deny ok");

        let stored = gate.get(&ticket.ticket_id).expect("ticket stored");
        assert_eq!(
            stored.state,
            ApprovalState::Denied {
                denied_by: "operator".to_string(),
                at: 1300,
                reason: Some("too risky".to_string()),
            }
        );
    }

    #[test]
    fn approve_already_decided_returns_not_pending() {
        let mut gate = ApprovalGate::new();
        let ticket = gate.open_ticket(build_context(), 1000, 3600);

        gate.approve(&ticket.ticket_id, "operator", 1200)
            .expect("first approve ok");

        let err = gate
            .approve(&ticket.ticket_id, "operator", 1400)
            .expect_err("second approve should fail");
        assert_eq!(err, ApprovalGateError::NotPending("approved"));

        // Also verify denied path rejects approve.
        let t2 = gate.open_ticket(build_context(), 2000, 3600);
        gate.deny(&t2.ticket_id, "operator", 2100, None)
            .expect("deny ok");
        let err2 = gate
            .approve(&t2.ticket_id, "operator", 2200)
            .expect_err("approve after deny should fail");
        assert_eq!(err2, ApprovalGateError::NotPending("denied"));

        // And expired path.
        let t3 = gate.open_ticket(build_context(), 3000, 10);
        gate.expire_stale(3100);
        let err3 = gate
            .approve(&t3.ticket_id, "operator", 3200)
            .expect_err("approve after expire should fail");
        assert_eq!(err3, ApprovalGateError::NotPending("expired"));
    }

    #[test]
    fn expire_stale_marks_expired_when_ttl_passed() {
        let mut gate = ApprovalGate::new();
        let now: u64 = 10_000;
        let ticket = gate.open_ticket(build_context(), now, 3600);
        // Move requested_at back so requested_at + ttl_secs <= now.
        {
            let stored = gate
                .by_ticket_id
                .get_mut(&ticket.ticket_id)
                .expect("stored");
            stored.requested_at = now - 100;
            stored.ttl_secs = 10;
        }

        // A fresh pending ticket that hasn't timed out should stay pending.
        let fresh = gate.open_ticket(build_context(), now, 3600);

        gate.expire_stale(now);

        let stored = gate.get(&ticket.ticket_id).expect("ticket stored");
        assert_eq!(stored.state, ApprovalState::Expired);

        let fresh_stored = gate.get(&fresh.ticket_id).expect("fresh stored");
        assert_eq!(fresh_stored.state, ApprovalState::Pending);

        // Running expire_stale again should not flip non-pending tickets.
        gate.expire_stale(now + 10_000);
        let stored_after = gate.get(&ticket.ticket_id).expect("ticket still stored");
        assert_eq!(stored_after.state, ApprovalState::Expired);
    }

    #[test]
    fn get_unknown_returns_none() {
        let gate = ApprovalGate::new();
        assert!(gate.get("no-such-ticket").is_none());
    }

    #[test]
    fn list_pending_filters_correctly() {
        let mut gate = ApprovalGate::new();
        let t_approved = gate.open_ticket(build_context(), 1000, 3600);
        let t_denied = gate.open_ticket(build_context(), 1001, 3600);
        let t_pending = gate.open_ticket(build_context(), 1002, 3600);

        gate.approve(&t_approved.ticket_id, "operator", 1100)
            .expect("approve ok");
        gate.deny(&t_denied.ticket_id, "operator", 1101, None)
            .expect("deny ok");

        let pending = gate.list_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].ticket_id, t_pending.ticket_id);
    }
}
