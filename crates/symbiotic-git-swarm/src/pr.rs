//! PR lifecycle management.
//!
//! The PRManager owns all pull request state. Agents interact with it via
//! JSON-RPC tools routed through the daemon. This is analogous to GitHub's
//! REST API — the daemon is the server, agents are API clients.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::types::{
    CheckRun, CheckStatus, MergeEvaluation, MergeRuleSet, PRStatus, PullRequestId, Review,
    ReviewComment, ReviewVerdict, SwarmPR, SwarmRepoId,
};

/// Manages all pull request state for the swarm system.
pub struct PRManager {
    /// All PRs indexed by ID.
    prs: HashMap<PullRequestId, SwarmPR>,
    /// Index: repo_id -> list of PR IDs.
    by_repo: HashMap<SwarmRepoId, Vec<PullRequestId>>,
}

impl PRManager {
    pub fn new() -> Self {
        Self {
            prs: HashMap::new(),
            by_repo: HashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // PR lifecycle
    // -----------------------------------------------------------------------

    /// Create a new pull request.
    #[allow(clippy::too_many_arguments)]
    pub fn create_pr(
        &mut self,
        repo_id: &str,
        branch: &str,
        base: &str,
        title: &str,
        description: &str,
        author_agent: &str,
        merge_rules: MergeRuleSet,
    ) -> Result<SwarmPR> {
        // Prevent duplicate PRs for the same branch
        if let Some(existing) = self.find_open_pr(repo_id, branch) {
            return Err(anyhow!(
                "PR already open for branch '{}' in repo '{}': {}",
                branch,
                repo_id,
                existing.id
            ));
        }

        let id = format!("pr-{}", Uuid::new_v4());
        let now = Utc::now();

        let pr = SwarmPR {
            id: id.clone(),
            repo_id: repo_id.to_string(),
            branch: branch.to_string(),
            base: base.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            author_agent: author_agent.to_string(),
            status: PRStatus::Open,
            reviews: Vec::new(),
            checks: Vec::new(),
            merge_rules,
            created_at: now,
            updated_at: now,
        };

        self.prs.insert(id.clone(), pr.clone());
        self.by_repo
            .entry(repo_id.to_string())
            .or_default()
            .push(id.clone());

        info!(
            pr_id = %id,
            repo_id = repo_id,
            branch = branch,
            base = base,
            author = author_agent,
            "PR created"
        );

        Ok(pr)
    }

    /// Add a review to a PR.
    pub fn add_review(
        &mut self,
        pr_id: &str,
        reviewer_agent: &str,
        verdict: ReviewVerdict,
        comments: Vec<ReviewComment>,
    ) -> Result<&SwarmPR> {
        let pr = self
            .prs
            .get_mut(pr_id)
            .ok_or_else(|| anyhow!("PR not found: {}", pr_id))?;

        if pr.status != PRStatus::Open {
            return Err(anyhow!("cannot review PR in status {:?}", pr.status));
        }

        if reviewer_agent == pr.author_agent {
            return Err(anyhow!("authors cannot review their own PRs"));
        }

        let review = Review {
            reviewer_agent: reviewer_agent.to_string(),
            verdict,
            comments,
            submitted_at: Utc::now(),
        };

        pr.reviews.push(review);
        pr.updated_at = Utc::now();

        // Auto-update status if all rules now satisfied
        let eval = pr.merge_rules.evaluate(pr);
        if eval.can_merge && pr.status == PRStatus::Open {
            pr.status = PRStatus::Approved;
            info!(pr_id = pr_id, "PR auto-approved (all rules satisfied)");
        }

        info!(
            pr_id = pr_id,
            reviewer = reviewer_agent,
            verdict = ?verdict,
            "review added"
        );

        Ok(pr)
    }

    /// Add or update a CI check run.
    pub fn update_check(
        &mut self,
        pr_id: &str,
        check_name: &str,
        agent_id: &str,
        status: CheckStatus,
        output: Option<String>,
    ) -> Result<&SwarmPR> {
        let pr = self
            .prs
            .get_mut(pr_id)
            .ok_or_else(|| anyhow!("PR not found: {}", pr_id))?;

        if let Some(existing) = pr.checks.iter_mut().find(|c| c.name == check_name) {
            existing.status = status;
            existing.output = output;
            if matches!(status, CheckStatus::Success | CheckStatus::Failure) {
                existing.completed_at = Some(Utc::now());
            }
        } else {
            pr.checks.push(CheckRun {
                name: check_name.to_string(),
                agent_id: agent_id.to_string(),
                status,
                output,
                completed_at: if matches!(status, CheckStatus::Success | CheckStatus::Failure) {
                    Some(Utc::now())
                } else {
                    None
                },
            });
        }

        pr.updated_at = Utc::now();

        // Re-evaluate merge rules
        let eval = pr.merge_rules.evaluate(pr);
        if eval.can_merge && pr.status == PRStatus::Open {
            pr.status = PRStatus::Approved;
            info!(pr_id = pr_id, "PR auto-approved after check update");
        }

        Ok(pr)
    }

    /// Mark a PR as merged. Call this AFTER the actual git merge succeeds.
    pub fn mark_merged(&mut self, pr_id: &str) -> Result<()> {
        let pr = self
            .prs
            .get_mut(pr_id)
            .ok_or_else(|| anyhow!("PR not found: {}", pr_id))?;

        if pr.status != PRStatus::Approved {
            return Err(anyhow!(
                "cannot merge PR in status {:?} — must be Approved",
                pr.status
            ));
        }

        pr.status = PRStatus::Merged;
        pr.updated_at = Utc::now();
        info!(pr_id = pr_id, "PR merged");
        Ok(())
    }

    /// Close a PR without merging.
    pub fn close(&mut self, pr_id: &str) -> Result<()> {
        let pr = self
            .prs
            .get_mut(pr_id)
            .ok_or_else(|| anyhow!("PR not found: {}", pr_id))?;

        if pr.status == PRStatus::Merged {
            return Err(anyhow!("cannot close an already-merged PR"));
        }

        pr.status = PRStatus::Closed;
        pr.updated_at = Utc::now();
        info!(pr_id = pr_id, "PR closed");
        Ok(())
    }

    /// Dismiss stale reviews (after a new push to the PR branch).
    ///
    /// When `dismiss_stale_reviews` is true in merge rules, all existing
    /// approvals are replaced with "Commented" to require re-review.
    pub fn dismiss_stale_reviews(&mut self, pr_id: &str) -> Result<u32> {
        let pr = self
            .prs
            .get_mut(pr_id)
            .ok_or_else(|| anyhow!("PR not found: {}", pr_id))?;

        if !pr.merge_rules.dismiss_stale_reviews {
            return Ok(0);
        }

        let mut dismissed = 0u32;
        for review in &mut pr.reviews {
            if review.verdict == ReviewVerdict::Approved {
                review.verdict = ReviewVerdict::Commented;
                dismissed += 1;
            }
        }

        if dismissed > 0 {
            pr.status = PRStatus::Open;
            pr.updated_at = Utc::now();
            warn!(
                pr_id = pr_id,
                dismissed = dismissed,
                "stale reviews dismissed after new push"
            );
        }

        Ok(dismissed)
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Get a PR by ID.
    pub fn get(&self, pr_id: &str) -> Option<&SwarmPR> {
        self.prs.get(pr_id)
    }

    /// Evaluate merge rules for a PR.
    pub fn evaluate_merge(&self, pr_id: &str) -> Result<MergeEvaluation> {
        let pr = self
            .prs
            .get(pr_id)
            .ok_or_else(|| anyhow!("PR not found: {}", pr_id))?;
        Ok(pr.merge_rules.evaluate(pr))
    }

    /// Find an open PR for a specific branch in a repo.
    pub fn find_open_pr(&self, repo_id: &str, branch: &str) -> Option<&SwarmPR> {
        self.by_repo.get(repo_id)?.iter().find_map(|id| {
            let pr = self.prs.get(id)?;
            if pr.branch == branch && pr.status == PRStatus::Open {
                Some(pr)
            } else {
                None
            }
        })
    }

    /// List all PRs for a repo, optionally filtered by status.
    pub fn list_for_repo(&self, repo_id: &str, status_filter: Option<PRStatus>) -> Vec<&SwarmPR> {
        self.by_repo
            .get(repo_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| {
                        let pr = self.prs.get(id)?;
                        if let Some(filter) = status_filter {
                            if pr.status == filter {
                                Some(pr)
                            } else {
                                None
                            }
                        } else {
                            Some(pr)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Count open PRs across all repos.
    pub fn open_count(&self) -> usize {
        self.prs
            .values()
            .filter(|pr| pr.status == PRStatus::Open)
            .count()
    }
}

impl Default for PRManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MergeRuleSet;

    fn default_rules() -> MergeRuleSet {
        MergeRuleSet::default()
    }

    #[test]
    fn create_and_get_pr() {
        let mut mgr = PRManager::new();
        let pr = mgr
            .create_pr(
                "repo-1",
                "feature/x",
                "main",
                "Add feature X",
                "Description",
                "agent-1",
                default_rules(),
            )
            .unwrap();

        assert_eq!(pr.status, PRStatus::Open);
        assert!(mgr.get(&pr.id).is_some());
    }

    #[test]
    fn duplicate_pr_rejected() {
        let mut mgr = PRManager::new();
        mgr.create_pr(
            "repo-1",
            "feature/x",
            "main",
            "First",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();

        let result = mgr.create_pr(
            "repo-1",
            "feature/x",
            "main",
            "Second",
            "",
            "agent-2",
            default_rules(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn self_review_rejected() {
        let mut mgr = PRManager::new();
        let pr = mgr
            .create_pr(
                "repo-1",
                "feature/x",
                "main",
                "Test",
                "",
                "agent-1",
                default_rules(),
            )
            .unwrap();

        let result = mgr.add_review(&pr.id, "agent-1", ReviewVerdict::Approved, vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn review_approve_flow() {
        let mut mgr = PRManager::new();
        let pr = mgr
            .create_pr(
                "repo-1",
                "feature/x",
                "main",
                "Test",
                "",
                "agent-1",
                default_rules(),
            )
            .unwrap();
        let pr_id = pr.id.clone();

        // Approve triggers auto-status-update
        let pr = mgr
            .add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
            .unwrap();
        assert_eq!(pr.status, PRStatus::Approved);

        // Can merge
        mgr.mark_merged(&pr_id).unwrap();
        assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Merged);
    }

    #[test]
    fn changes_requested_blocks_merge() {
        let mut mgr = PRManager::new();
        let pr = mgr
            .create_pr(
                "repo-1",
                "feature/x",
                "main",
                "Test",
                "",
                "agent-1",
                default_rules(),
            )
            .unwrap();
        let pr_id = pr.id.clone();

        mgr.add_review(
            &pr_id,
            "reviewer-1",
            ReviewVerdict::ChangesRequested,
            vec![ReviewComment {
                file: "src/main.rs".to_string(),
                line: Some(42),
                body: "Fix this error handling".to_string(),
            }],
        )
        .unwrap();

        let eval = mgr.evaluate_merge(&pr_id).unwrap();
        assert!(!eval.can_merge);
    }

    #[test]
    fn ci_check_required() {
        let mut mgr = PRManager::new();
        let rules = MergeRuleSet {
            required_approvals: 0,
            required_checks: vec!["cargo-test".to_string()],
            dismiss_stale_reviews: false,
            allowed_merge_agents: vec!["*".to_string()],
        };
        let pr = mgr
            .create_pr("repo-1", "feature/x", "main", "Test", "", "agent-1", rules)
            .unwrap();
        let pr_id = pr.id.clone();

        // Missing check
        assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);

        // Failing check
        mgr.update_check(&pr_id, "cargo-test", "ci-1", CheckStatus::Failure, None)
            .unwrap();
        assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);

        // Passing check
        mgr.update_check(&pr_id, "cargo-test", "ci-1", CheckStatus::Success, None)
            .unwrap();
        assert!(mgr.evaluate_merge(&pr_id).unwrap().can_merge);
    }

    #[test]
    fn dismiss_stale_reviews() {
        let mut mgr = PRManager::new();
        let pr = mgr
            .create_pr(
                "repo-1",
                "feature/x",
                "main",
                "Test",
                "",
                "agent-1",
                default_rules(),
            )
            .unwrap();
        let pr_id = pr.id.clone();

        // Approve
        mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
            .unwrap();
        assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Approved);

        // New push dismisses
        let dismissed = mgr.dismiss_stale_reviews(&pr_id).unwrap();
        assert_eq!(dismissed, 1);
        assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Open);
    }

    #[test]
    fn close_pr() {
        let mut mgr = PRManager::new();
        let pr = mgr
            .create_pr(
                "repo-1",
                "feature/x",
                "main",
                "Test",
                "",
                "agent-1",
                default_rules(),
            )
            .unwrap();

        mgr.close(&pr.id).unwrap();
        assert_eq!(mgr.get(&pr.id).unwrap().status, PRStatus::Closed);
    }

    #[test]
    fn list_for_repo() {
        let mut mgr = PRManager::new();

        mgr.create_pr(
            "repo-1",
            "feature/a",
            "main",
            "A",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
        mgr.create_pr(
            "repo-1",
            "feature/b",
            "main",
            "B",
            "",
            "agent-2",
            default_rules(),
        )
        .unwrap();
        mgr.create_pr(
            "repo-2",
            "feature/c",
            "main",
            "C",
            "",
            "agent-3",
            default_rules(),
        )
        .unwrap();

        assert_eq!(mgr.list_for_repo("repo-1", None).len(), 2);
        assert_eq!(mgr.list_for_repo("repo-2", None).len(), 1);
        assert_eq!(mgr.list_for_repo("repo-3", None).len(), 0);
    }

    #[test]
    fn open_count() {
        let mut mgr = PRManager::new();
        mgr.create_pr(
            "repo-1",
            "feature/a",
            "main",
            "A",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
        mgr.create_pr(
            "repo-1",
            "feature/b",
            "main",
            "B",
            "",
            "agent-2",
            default_rules(),
        )
        .unwrap();
        assert_eq!(mgr.open_count(), 2);
    }
}
