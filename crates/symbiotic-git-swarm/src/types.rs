//! Core types for the internal Git swarm system.
//!
//! Models the swarm as a set of bare git repositories with a GitHub-style PR
//! workflow: agents push branches, create PRs, review each other's work, and
//! merge only when configured rules are satisfied.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Repository
// ---------------------------------------------------------------------------

/// Unique identifier for a swarm git repository.
pub type SwarmRepoId = String;

/// A bare git repo managed by the swarm system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmRepo {
    /// Unique repo identifier (e.g. "task-116").
    pub id: SwarmRepoId,
    /// Path inside the git server container (e.g. "/repos/task-116.git").
    pub container_path: String,
    /// When this repo was created.
    pub created_at: DateTime<Utc>,
    /// Current lifecycle status.
    pub status: SwarmRepoStatus,
    /// Branch protection rules for this repo.
    pub branch_rules: Vec<BranchRule>,
    /// Default merge rules applied to new PRs.
    pub default_merge_rules: MergeRuleSet,
}

/// Lifecycle status of a swarm repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmRepoStatus {
    /// Agents are actively working.
    Active,
    /// All work completed and merged.
    Completed,
    /// Swarm failed and was abandoned.
    Failed,
}

// ---------------------------------------------------------------------------
// Pull Request
// ---------------------------------------------------------------------------

/// Unique identifier for a pull request.
pub type PullRequestId = String;

/// A pull request — modeled after GitHub PRs.
///
/// The daemon owns PR state. Agents interact via JSON-RPC tools
/// (pr.create, pr.comment, pr.approve, pr.merge, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmPR {
    /// Unique PR identifier.
    pub id: PullRequestId,
    /// Which repo this PR belongs to.
    pub repo_id: SwarmRepoId,
    /// Source branch (e.g. "feature/agent-42").
    pub branch: String,
    /// Target branch (e.g. "main").
    pub base: String,
    /// Human-readable title.
    pub title: String,
    /// Description of changes.
    pub description: String,
    /// Agent that created this PR.
    pub author_agent: String,
    /// Current PR status.
    pub status: PRStatus,
    /// Reviews submitted by reviewer agents.
    pub reviews: Vec<Review>,
    /// CI check results.
    pub checks: Vec<CheckRun>,
    /// Merge rules for this specific PR (copied from repo defaults, can be overridden).
    pub merge_rules: MergeRuleSet,
    /// When the PR was created.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// PR lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PRStatus {
    /// Open and awaiting review/checks.
    Open,
    /// All rules satisfied, ready to merge.
    Approved,
    /// Successfully merged into base branch.
    Merged,
    /// Closed without merging.
    Closed,
}

// ---------------------------------------------------------------------------
// Reviews
// ---------------------------------------------------------------------------

/// A review submitted by a reviewer agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    /// Agent that submitted this review.
    pub reviewer_agent: String,
    /// Overall verdict.
    pub verdict: ReviewVerdict,
    /// Line-level or file-level comments.
    pub comments: Vec<ReviewComment>,
    /// When the review was submitted.
    pub submitted_at: DateTime<Utc>,
}

/// Review verdict — matches GitHub's review states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// Changes look good, approve merge.
    Approved,
    /// Changes need work before merging.
    ChangesRequested,
    /// General feedback, no approval/rejection.
    Commented,
}

/// A comment attached to a specific location in a PR diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    /// File path relative to repo root.
    pub file: String,
    /// Line number in the diff (None for file-level comments).
    pub line: Option<u32>,
    /// Comment body (Markdown).
    pub body: String,
}

// ---------------------------------------------------------------------------
// CI Checks
// ---------------------------------------------------------------------------

/// A CI check run — a test suite, linter, or other automated verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRun {
    /// Check name (e.g. "cargo-test", "vault-linter").
    pub name: String,
    /// Agent that ran this check.
    pub agent_id: String,
    /// Current status.
    pub status: CheckStatus,
    /// Output/logs from the check (truncated if large).
    pub output: Option<String>,
    /// When the check completed (None if still running).
    pub completed_at: Option<DateTime<Utc>>,
}

/// CI check lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Queued, not yet started.
    Pending,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Success,
    /// Completed with failures.
    Failure,
}

// ---------------------------------------------------------------------------
// Merge Rules & Branch Protection
// ---------------------------------------------------------------------------

/// Merge requirements for a PR. All conditions must be met before merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRuleSet {
    /// Minimum number of approving reviews required.
    pub required_approvals: u32,
    /// Check names that must report `Success`.
    pub required_checks: Vec<String>,
    /// If true, new pushes to the branch invalidate existing reviews.
    pub dismiss_stale_reviews: bool,
    /// Agent IDs allowed to trigger merge ("*" = any with pr.merge scope).
    pub allowed_merge_agents: Vec<String>,
}

impl Default for MergeRuleSet {
    fn default() -> Self {
        Self {
            required_approvals: 1,
            required_checks: Vec::new(),
            dismiss_stale_reviews: true,
            allowed_merge_agents: vec!["*".to_string()],
        }
    }
}

/// Per-branch push protection rule (enforced by git pre-receive hook).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRule {
    /// Glob pattern matching branch names (e.g. "main", "release/*").
    pub pattern: String,
    /// Capability scopes required to push to matching branches.
    pub allowed_push_scopes: Vec<String>,
    /// If true, changes must go through a PR — direct push is blocked.
    pub require_pr: bool,
}

impl BranchRule {
    /// Create a standard protected-branch rule (main).
    pub fn protected_main() -> Self {
        Self {
            pattern: "main".to_string(),
            allowed_push_scopes: vec!["git.push:protected".to_string()],
            require_pr: true,
        }
    }

    /// Create a protected release-branch rule.
    pub fn protected_release() -> Self {
        Self {
            pattern: "release/*".to_string(),
            allowed_push_scopes: vec!["git.push:protected".to_string()],
            require_pr: true,
        }
    }

    /// Create a standard push rule for non-protected branches.
    pub fn standard_push(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_string(),
            allowed_push_scopes: vec!["git.push".to_string()],
            require_pr: false,
        }
    }

    /// Check if a branch name matches this rule's pattern.
    pub fn matches(&self, branch: &str) -> bool {
        if self.pattern == "*" {
            return true;
        }
        if let Some(prefix) = self.pattern.strip_suffix("/*") {
            branch.starts_with(prefix) && branch.len() > prefix.len() + 1
        } else {
            self.pattern == branch
        }
    }
}

impl SwarmRepo {
    /// Return the first matching branch rule for a branch name.
    pub fn branch_rule_for(&self, branch: &str) -> Option<&BranchRule> {
        self.branch_rules.iter().find(|rule| rule.matches(branch))
    }
}

// ---------------------------------------------------------------------------
// Merge Rule Evaluation
// ---------------------------------------------------------------------------

/// Result of evaluating merge rules against a PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeEvaluation {
    /// Whether all rules are satisfied.
    pub can_merge: bool,
    /// Individual rule results.
    pub checks: Vec<RuleCheck>,
}

/// Status of a single merge rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleCheck {
    /// Human-readable description.
    pub description: String,
    /// Whether this rule is satisfied.
    pub satisfied: bool,
}

impl MergeRuleSet {
    /// Evaluate whether a PR satisfies all merge rules.
    pub fn evaluate(&self, pr: &SwarmPR) -> MergeEvaluation {
        let mut checks = Vec::new();

        // Check required approvals
        let approval_count = pr
            .reviews
            .iter()
            .filter(|r| r.verdict == ReviewVerdict::Approved)
            .count() as u32;
        checks.push(RuleCheck {
            description: format!(
                "Required approvals: {}/{}",
                approval_count, self.required_approvals
            ),
            satisfied: approval_count >= self.required_approvals,
        });

        // Check no outstanding change requests
        let has_changes_requested = pr.reviews.iter().any(|r| {
            r.verdict == ReviewVerdict::ChangesRequested
                && !pr.reviews.iter().any(|later| {
                    later.reviewer_agent == r.reviewer_agent
                        && later.submitted_at > r.submitted_at
                        && later.verdict == ReviewVerdict::Approved
                })
        });
        checks.push(RuleCheck {
            description: "No outstanding change requests".to_string(),
            satisfied: !has_changes_requested,
        });

        // Check required CI checks
        for check_name in &self.required_checks {
            let check_passed = pr
                .checks
                .iter()
                .any(|c| c.name == *check_name && c.status == CheckStatus::Success);
            checks.push(RuleCheck {
                description: format!("CI check '{}' passed", check_name),
                satisfied: check_passed,
            });
        }

        let can_merge = checks.iter().all(|c| c.satisfied);
        MergeEvaluation { can_merge, checks }
    }
}

// ---------------------------------------------------------------------------
// Authorization (pre-receive hook requests)
// ---------------------------------------------------------------------------

/// Request from the git server's pre-receive hook to authorize a push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushAuthRequest {
    /// Repository being pushed to.
    pub repo_id: SwarmRepoId,
    /// Branch being updated.
    pub branch: String,
    /// Old ref SHA (0000... for new branches).
    pub old_sha: String,
    /// New ref SHA.
    pub new_sha: String,
    /// Short-lived push session secret supplied by the client transport.
    pub push_session: String,
}

/// Response to a push authorization request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushAuthResponse {
    /// Whether the push is allowed.
    pub allowed: bool,
    /// Reason for denial (if not allowed).
    pub reason: Option<String>,
}

/// Request from an agent runner to mint a short-lived authenticated push session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSessionRequest {
    /// Agent requesting the push session.
    pub agent_id: String,
    /// Repository the agent will push to.
    pub repo_id: SwarmRepoId,
    /// Branch the agent intends to push.
    pub branch: String,
    /// Optional goal/workflow scope owning this branch work.
    pub goal_scope: Option<String>,
    /// Optional thread attachment for truthful observability projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Response containing the short-lived push session secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSessionResponse {
    /// Secret presented back to the git server during push.
    pub push_session: String,
    /// Expiry timestamp for the session.
    pub expires_at: u64,
}

// ---------------------------------------------------------------------------
// Swarm Configuration
// ---------------------------------------------------------------------------

/// Configuration for the git swarm subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSwarmConfig {
    /// Docker image for the git server container.
    pub git_server_image: String,
    /// Port the git server binds to inside the container.
    pub git_server_port: u16,
    /// Host bind address for the git server (Docker bridge).
    pub git_server_bind: String,
    /// Base path for bare repos inside the git server container.
    pub repos_base_path: String,
    /// Default merge rules for new repos.
    pub default_merge_rules: MergeRuleSet,
    /// Default branch rules for new repos.
    pub default_branch_rules: Vec<BranchRule>,
    /// Maximum repo size in bytes (100MB default).
    pub max_repo_size_bytes: u64,
}

impl Default for GitSwarmConfig {
    fn default() -> Self {
        Self {
            git_server_image: "symbiotic-git-server:latest".to_string(),
            git_server_port: 80,
            git_server_bind: "172.17.0.1".to_string(),
            repos_base_path: "/repos".to_string(),
            default_merge_rules: MergeRuleSet::default(),
            default_branch_rules: vec![
                BranchRule::protected_main(),
                BranchRule::protected_release(),
                BranchRule::standard_push("*"),
            ],
            max_repo_size_bytes: 100 * 1024 * 1024,
        }
    }
}

/// State tracked for each active swarm (in-memory, persisted as JSON).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwarmState {
    /// All repos in this swarm.
    pub repos: HashMap<SwarmRepoId, SwarmRepo>,
    /// All open/merged PRs.
    pub pull_requests: HashMap<PullRequestId, SwarmPR>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_rule_matches_exact() {
        let rule = BranchRule::protected_main();
        assert!(rule.matches("main"));
        assert!(!rule.matches("main-backup"));
        assert!(!rule.matches("feature/main"));
    }

    #[test]
    fn branch_rule_matches_glob() {
        let rule = BranchRule {
            pattern: "release/*".to_string(),
            allowed_push_scopes: vec!["git.push:protected".to_string()],
            require_pr: true,
        };
        assert!(rule.matches("release/v1.0"));
        assert!(rule.matches("release/hotfix-42"));
        assert!(!rule.matches("release"));
        assert!(!rule.matches("feature/release/foo"));
    }

    #[test]
    fn branch_rule_wildcard() {
        let rule = BranchRule {
            pattern: "*".to_string(),
            allowed_push_scopes: vec![],
            require_pr: false,
        };
        assert!(rule.matches("anything"));
        assert!(rule.matches("main"));
    }

    #[test]
    fn swarm_repo_returns_first_matching_branch_rule() {
        let repo = SwarmRepo {
            id: "repo-1".to_string(),
            container_path: "/repos/repo-1.git".to_string(),
            created_at: Utc::now(),
            status: SwarmRepoStatus::Active,
            branch_rules: vec![BranchRule::protected_main(), BranchRule::standard_push("*")],
            default_merge_rules: MergeRuleSet::default(),
        };

        let main_rule = repo
            .branch_rule_for("main")
            .expect("main rule should match");
        assert_eq!(main_rule.allowed_push_scopes, vec!["git.push:protected"]);

        let feature_rule = repo
            .branch_rule_for("feature/agent-42")
            .expect("feature rule should match");
        assert_eq!(feature_rule.allowed_push_scopes, vec!["git.push"]);
        assert!(!feature_rule.require_pr);
    }

    #[test]
    fn default_config_includes_standard_push_rule() {
        let config = GitSwarmConfig::default();
        let wildcard = config
            .default_branch_rules
            .iter()
            .find(|rule| rule.pattern == "*")
            .expect("wildcard push rule");
        assert_eq!(wildcard.allowed_push_scopes, vec!["git.push"]);
        assert!(!wildcard.require_pr);
    }

    #[test]
    fn merge_rules_default_requires_one_approval() {
        let rules = MergeRuleSet::default();
        assert_eq!(rules.required_approvals, 1);
        assert!(rules.dismiss_stale_reviews);
    }

    #[test]
    fn merge_evaluation_no_reviews() {
        let rules = MergeRuleSet::default();
        let pr = make_test_pr(vec![], vec![]);
        let eval = rules.evaluate(&pr);
        assert!(!eval.can_merge);
    }

    #[test]
    fn merge_evaluation_approved() {
        let rules = MergeRuleSet::default();
        let pr = make_test_pr(
            vec![Review {
                reviewer_agent: "reviewer-1".to_string(),
                verdict: ReviewVerdict::Approved,
                comments: vec![],
                submitted_at: Utc::now(),
            }],
            vec![],
        );
        let eval = rules.evaluate(&pr);
        assert!(eval.can_merge);
    }

    #[test]
    fn merge_evaluation_changes_requested_blocks() {
        let rules = MergeRuleSet::default();
        let pr = make_test_pr(
            vec![
                Review {
                    reviewer_agent: "reviewer-1".to_string(),
                    verdict: ReviewVerdict::Approved,
                    comments: vec![],
                    submitted_at: Utc::now(),
                },
                Review {
                    reviewer_agent: "reviewer-2".to_string(),
                    verdict: ReviewVerdict::ChangesRequested,
                    comments: vec![],
                    submitted_at: Utc::now(),
                },
            ],
            vec![],
        );
        let eval = rules.evaluate(&pr);
        assert!(!eval.can_merge);
    }

    #[test]
    fn merge_evaluation_required_checks() {
        let rules = MergeRuleSet {
            required_approvals: 0,
            required_checks: vec!["cargo-test".to_string()],
            dismiss_stale_reviews: false,
            allowed_merge_agents: vec!["*".to_string()],
        };

        // Missing check
        let pr = make_test_pr(vec![], vec![]);
        assert!(!rules.evaluate(&pr).can_merge);

        // Failing check
        let pr = make_test_pr(
            vec![],
            vec![CheckRun {
                name: "cargo-test".to_string(),
                agent_id: "ci-1".to_string(),
                status: CheckStatus::Failure,
                output: None,
                completed_at: Some(Utc::now()),
            }],
        );
        assert!(!rules.evaluate(&pr).can_merge);

        // Passing check
        let pr = make_test_pr(
            vec![],
            vec![CheckRun {
                name: "cargo-test".to_string(),
                agent_id: "ci-1".to_string(),
                status: CheckStatus::Success,
                output: None,
                completed_at: Some(Utc::now()),
            }],
        );
        assert!(rules.evaluate(&pr).can_merge);
    }

    fn make_test_pr(reviews: Vec<Review>, checks: Vec<CheckRun>) -> SwarmPR {
        SwarmPR {
            id: "pr-1".to_string(),
            repo_id: "repo-1".to_string(),
            branch: "feature/test".to_string(),
            base: "main".to_string(),
            title: "Test PR".to_string(),
            description: "A test".to_string(),
            author_agent: "agent-1".to_string(),
            status: PRStatus::Open,
            reviews,
            checks,
            merge_rules: MergeRuleSet::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}
