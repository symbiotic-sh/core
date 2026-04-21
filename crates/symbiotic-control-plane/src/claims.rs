use serde::{Deserialize, Serialize};

use crate::leases::Lease;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeRequirement {
    pub scope: CollaborationScope,
    pub mode: ScopeMode,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeClaim {
    pub id: String,
    pub work_item_id: String,
    pub holder_agent_id: String,
    pub scope: CollaborationScope,
    pub mode: ScopeMode,
    pub status: ScopeClaimStatus,
    pub lease: Lease,
    pub granted_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScopeClaimStatus {
    Active,
    Releasing,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CollaborationScope {
    RepoPath { repo_id: String, path: String },
    RepoDoc { repo_id: String, path: String },
    RepoBranchNamespace { repo_id: String, pattern: String },
    KbSubtree { path: String },
    KbAppendChannel { path: String },
    ReviewQueue { queue: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    SharedRead,
    ExclusiveWrite,
    AppendOnly,
}

impl ScopeClaim {
    pub fn is_active(&self) -> bool {
        self.status == ScopeClaimStatus::Active
    }

    pub fn conflicts_with(&self, other: &Self) -> bool {
        claims_conflict(self, other)
    }
}

pub fn claims_conflict(left: &ScopeClaim, right: &ScopeClaim) -> bool {
    if !left.is_active() || !right.is_active() {
        return false;
    }

    if !scopes_overlap(&left.scope, &right.scope) {
        return false;
    }

    !matches!(
        (left.mode, right.mode),
        (ScopeMode::SharedRead, ScopeMode::SharedRead)
            | (ScopeMode::AppendOnly, ScopeMode::AppendOnly)
            | (ScopeMode::SharedRead, ScopeMode::AppendOnly)
            | (ScopeMode::AppendOnly, ScopeMode::SharedRead)
    )
}

pub fn scopes_overlap(left: &CollaborationScope, right: &CollaborationScope) -> bool {
    match (left, right) {
        (
            CollaborationScope::RepoPath {
                repo_id: left_repo,
                path: left_path,
            },
            CollaborationScope::RepoPath {
                repo_id: right_repo,
                path: right_path,
            },
        ) => left_repo == right_repo && path_overlap(left_path, right_path),
        (
            CollaborationScope::RepoDoc {
                repo_id: left_repo,
                path: left_path,
            },
            CollaborationScope::RepoDoc {
                repo_id: right_repo,
                path: right_path,
            },
        ) => left_repo == right_repo && left_path == right_path,
        (
            CollaborationScope::RepoBranchNamespace {
                repo_id: left_repo,
                pattern: left_pattern,
            },
            CollaborationScope::RepoBranchNamespace {
                repo_id: right_repo,
                pattern: right_pattern,
            },
        ) => left_repo == right_repo && branch_namespace_overlap(left_pattern, right_pattern),
        (
            CollaborationScope::KbSubtree { path: left_path },
            CollaborationScope::KbSubtree { path: right_path },
        ) => path_overlap(left_path, right_path),
        (
            CollaborationScope::KbAppendChannel { path: left_path },
            CollaborationScope::KbAppendChannel { path: right_path },
        ) => left_path == right_path,
        (
            CollaborationScope::ReviewQueue { queue: left_queue },
            CollaborationScope::ReviewQueue { queue: right_queue },
        ) => left_queue == right_queue,
        (
            CollaborationScope::KbSubtree { path: left_path },
            CollaborationScope::KbAppendChannel { path: right_path },
        )
        | (
            CollaborationScope::KbAppendChannel { path: right_path },
            CollaborationScope::KbSubtree { path: left_path },
        ) => path_overlap(left_path, right_path),
        _ => false,
    }
}

fn path_overlap(left: &str, right: &str) -> bool {
    let left = normalize_scope_path(left);
    let right = normalize_scope_path(right);

    left == right
        || left.strip_prefix(&(right.clone() + "/")).is_some()
        || right.strip_prefix(&(left + "/")).is_some()
}

fn branch_namespace_overlap(left: &str, right: &str) -> bool {
    let left = normalize_scope_path(left).trim_end_matches('*').to_string();
    let right = normalize_scope_path(right)
        .trim_end_matches('*')
        .to_string();
    left == right
        || left.strip_prefix(&(right.clone() + "/")).is_some()
        || right.strip_prefix(&(left + "/")).is_some()
}

fn normalize_scope_path(path: &str) -> String {
    path.trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leases::Lease;

    fn claim(id: &str, scope: CollaborationScope, mode: ScopeMode) -> ScopeClaim {
        ScopeClaim {
            id: id.to_string(),
            work_item_id: "w1".to_string(),
            holder_agent_id: "agent-1".to_string(),
            scope,
            mode,
            status: ScopeClaimStatus::Active,
            lease: Lease::new("agent-1".to_string(), 100, 30, 2),
            granted_at: 100,
            updated_at: 100,
        }
    }

    #[test]
    fn repo_path_parent_child_conflicts_for_exclusive_write() {
        let left = claim(
            "c1",
            CollaborationScope::RepoPath {
                repo_id: "runtime".to_string(),
                path: "src".to_string(),
            },
            ScopeMode::ExclusiveWrite,
        );
        let right = claim(
            "c2",
            CollaborationScope::RepoPath {
                repo_id: "runtime".to_string(),
                path: "src/daemon".to_string(),
            },
            ScopeMode::ExclusiveWrite,
        );
        assert!(claims_conflict(&left, &right));
    }

    #[test]
    fn append_only_can_coexist_with_shared_read() {
        let left = claim(
            "c1",
            CollaborationScope::KbAppendChannel {
                path: "operations/projects/symbiotic/handoffs".to_string(),
            },
            ScopeMode::AppendOnly,
        );
        let right = claim(
            "c2",
            CollaborationScope::KbAppendChannel {
                path: "operations/projects/symbiotic/handoffs".to_string(),
            },
            ScopeMode::SharedRead,
        );
        assert!(!claims_conflict(&left, &right));
    }

    #[test]
    fn append_only_conflicts_with_exclusive_write_on_same_surface() {
        let left = claim(
            "c1",
            CollaborationScope::KbSubtree {
                path: "operations/projects/symbiotic/handoffs".to_string(),
            },
            ScopeMode::ExclusiveWrite,
        );
        let right = claim(
            "c2",
            CollaborationScope::KbAppendChannel {
                path: "operations/projects/symbiotic/handoffs".to_string(),
            },
            ScopeMode::AppendOnly,
        );
        assert!(claims_conflict(&left, &right));
    }
}
