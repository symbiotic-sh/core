//! Integration tests for symbiotic-git-swarm.
//!
//! These tests exercise the public API across module boundaries, covering
//! end-to-end PR lifecycle flows, merge rule evaluation, branch protection,
//! and edge cases. All tests are pure in-memory (no Docker, no network).

use symbiotic_git_swarm::{
    BranchRule, CheckStatus, MergeRuleSet, PRManager, PRStatus, ReviewComment, ReviewVerdict,
    SwarmRepo, SwarmRepoStatus,
};

use chrono::Utc;

// ===========================================================================
// Helpers
// ===========================================================================

/// Build default merge rules (1 approval, no required checks).
fn default_rules() -> MergeRuleSet {
    MergeRuleSet::default()
}

/// Build merge rules requiring specific checks and a given number of approvals.
fn rules_with_checks(approvals: u32, checks: Vec<&str>) -> MergeRuleSet {
    MergeRuleSet {
        required_approvals: approvals,
        required_checks: checks.into_iter().map(String::from).collect(),
        dismiss_stale_reviews: true,
        allowed_merge_agents: vec!["*".to_string()],
    }
}

/// Build a test SwarmRepo with the given branch rules.
fn make_repo(id: &str, branch_rules: Vec<BranchRule>) -> SwarmRepo {
    SwarmRepo {
        id: id.to_string(),
        container_path: format!("/repos/{}.git", id),
        created_at: Utc::now(),
        status: SwarmRepoStatus::Active,
        branch_rules,
        default_merge_rules: MergeRuleSet::default(),
    }
}

// ===========================================================================
// PR Lifecycle Tests
// ===========================================================================

#[test]
fn test_full_pr_lifecycle() {
    // create PR → approve → checks pass → evaluate_merge → mark_merged
    let mut mgr = PRManager::new();
    let rules = rules_with_checks(1, vec!["cargo-test"]);

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/full-lifecycle",
            "main",
            "Full lifecycle test",
            "Test the complete happy path",
            "agent-author",
            rules,
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // PR starts Open
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Open);

    // Add approving review (not enough yet — check still pending)
    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    // Still Open because required check hasn't passed
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Open);
    assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);

    // Report check success
    mgr.update_check(&pr_id, "cargo-test", "ci-agent", CheckStatus::Success, None)
        .unwrap();

    // Now auto-approved
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Approved);
    assert!(mgr.evaluate_merge(&pr_id).unwrap().can_merge);

    // Mark merged
    mgr.mark_merged(&pr_id).unwrap();
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Merged);
}

#[test]
fn test_pr_rejected_flow() {
    // create → request_changes → can't merge → new approve → can merge
    let mut mgr = PRManager::new();

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/rejected",
            "main",
            "Rejected then approved",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // Reviewer requests changes
    mgr.add_review(
        &pr_id,
        "reviewer-1",
        ReviewVerdict::ChangesRequested,
        vec![ReviewComment {
            file: "src/lib.rs".to_string(),
            line: Some(10),
            body: "Needs better error handling".to_string(),
        }],
    )
    .unwrap();

    assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Open);

    // Same reviewer now approves (supersedes previous request)
    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();

    // The later approval supersedes the earlier ChangesRequested from same reviewer
    let eval = mgr.evaluate_merge(&pr_id).unwrap();
    assert!(eval.can_merge);
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Approved);
}

#[test]
fn test_pr_with_required_checks() {
    // create PR with 2 required checks → one passes → not ready → second passes → ready
    let mut mgr = PRManager::new();
    let rules = MergeRuleSet {
        required_approvals: 0,
        required_checks: vec!["cargo-test".to_string(), "vault-linter".to_string()],
        dismiss_stale_reviews: false,
        allowed_merge_agents: vec!["*".to_string()],
    };

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/two-checks",
            "main",
            "Two checks required",
            "",
            "agent-1",
            rules,
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // No checks yet — can't merge
    assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);

    // First check passes
    mgr.update_check(&pr_id, "cargo-test", "ci-1", CheckStatus::Success, None)
        .unwrap();
    assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);

    // Second check passes
    mgr.update_check(
        &pr_id,
        "vault-linter",
        "ci-2",
        CheckStatus::Success,
        Some("All files pass".to_string()),
    )
    .unwrap();
    assert!(mgr.evaluate_merge(&pr_id).unwrap().can_merge);
}

#[test]
fn test_pr_needs_both_approval_and_checks() {
    // create with min_approvals=1 + required_checks → approve but no checks → can't
    // → checks pass → can merge
    let mut mgr = PRManager::new();
    let rules = rules_with_checks(1, vec!["lint"]);

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/both",
            "main",
            "Needs approval + check",
            "",
            "agent-1",
            rules,
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // Approve without check — still blocked
    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Open);

    // Check passes — now both conditions met
    mgr.update_check(&pr_id, "lint", "ci-1", CheckStatus::Success, None)
        .unwrap();
    assert!(mgr.evaluate_merge(&pr_id).unwrap().can_merge);
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Approved);
}

#[test]
fn test_multiple_prs_same_repo() {
    // create 3 PRs in same repo, verify list, merge one, verify others still open
    let mut mgr = PRManager::new();

    let pr1 = mgr
        .create_pr(
            "repo-1",
            "feature/a",
            "main",
            "PR A",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
    let pr2 = mgr
        .create_pr(
            "repo-1",
            "feature/b",
            "main",
            "PR B",
            "",
            "agent-2",
            default_rules(),
        )
        .unwrap();
    let pr3 = mgr
        .create_pr(
            "repo-1",
            "feature/c",
            "main",
            "PR C",
            "",
            "agent-3",
            default_rules(),
        )
        .unwrap();

    // All 3 visible
    let all = mgr.list_for_repo("repo-1", None);
    assert_eq!(all.len(), 3);

    // Approve and merge PR1
    mgr.add_review(&pr1.id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    mgr.mark_merged(&pr1.id).unwrap();

    // open_count decreased
    assert_eq!(mgr.open_count(), 2);

    // PR2 and PR3 still open
    assert_eq!(mgr.get(&pr2.id).unwrap().status, PRStatus::Open);
    assert_eq!(mgr.get(&pr3.id).unwrap().status, PRStatus::Open);

    // Filter by status
    let open = mgr.list_for_repo("repo-1", Some(PRStatus::Open));
    assert_eq!(open.len(), 2);
    let merged = mgr.list_for_repo("repo-1", Some(PRStatus::Merged));
    assert_eq!(merged.len(), 1);
}

#[test]
fn test_pr_close_without_merge() {
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/abandoned",
            "main",
            "Will be closed",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();

    mgr.close(&pr.id).unwrap();
    assert_eq!(mgr.get(&pr.id).unwrap().status, PRStatus::Closed);

    // After closing, the branch name is freed for a new PR
    let pr2 = mgr.create_pr(
        "repo-1",
        "feature/abandoned",
        "main",
        "Reopened after close",
        "",
        "agent-1",
        default_rules(),
    );
    // find_open_pr only matches Open status, so a closed PR doesn't block new ones
    // Actually, looking at the code — find_open_pr only checks PRStatus::Open.
    // A Closed PR won't block creating a new one with the same branch.
    assert!(pr2.is_ok());
}

// ===========================================================================
// Merge Rule Tests
// ===========================================================================

#[test]
fn test_merge_rules_min_approvals() {
    let mut mgr = PRManager::new();

    // 0 approvals required — auto-mergeable (no reviews needed)
    let rules_zero = MergeRuleSet {
        required_approvals: 0,
        required_checks: vec![],
        dismiss_stale_reviews: false,
        allowed_merge_agents: vec!["*".to_string()],
    };
    let pr0 = mgr
        .create_pr(
            "repo-1",
            "feature/zero-approvals",
            "main",
            "No approval needed",
            "",
            "agent-1",
            rules_zero,
        )
        .unwrap();
    assert!(mgr.evaluate_merge(&pr0.id).unwrap().can_merge);

    // 2 approvals required — 1 isn't enough
    let rules_two = MergeRuleSet {
        required_approvals: 2,
        required_checks: vec![],
        dismiss_stale_reviews: false,
        allowed_merge_agents: vec!["*".to_string()],
    };
    let pr2 = mgr
        .create_pr(
            "repo-1",
            "feature/two-approvals",
            "main",
            "Needs two",
            "",
            "agent-1",
            rules_two,
        )
        .unwrap();

    // 0 approvals
    assert!(!mgr.evaluate_merge(&pr2.id).unwrap().can_merge);

    // 1 approval — still not enough
    mgr.add_review(&pr2.id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    assert!(!mgr.evaluate_merge(&pr2.id).unwrap().can_merge);

    // 2 approvals — now enough
    mgr.add_review(&pr2.id, "reviewer-2", ReviewVerdict::Approved, vec![])
        .unwrap();
    assert!(mgr.evaluate_merge(&pr2.id).unwrap().can_merge);
}

#[test]
fn test_merge_rules_required_checks_subset() {
    // Only configured checks block merge, not arbitrary check names
    let mut mgr = PRManager::new();
    let rules = MergeRuleSet {
        required_approvals: 0,
        required_checks: vec!["cargo-test".to_string()],
        dismiss_stale_reviews: false,
        allowed_merge_agents: vec!["*".to_string()],
    };

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/subset",
            "main",
            "Subset checks",
            "",
            "agent-1",
            rules,
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // Report a non-required check failing — should NOT block merge of the required check
    mgr.update_check(&pr_id, "optional-lint", "ci-1", CheckStatus::Failure, None)
        .unwrap();

    // Required check still missing — can't merge
    assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);

    // Required check passes — can merge (despite optional check failure)
    mgr.update_check(&pr_id, "cargo-test", "ci-2", CheckStatus::Success, None)
        .unwrap();
    assert!(mgr.evaluate_merge(&pr_id).unwrap().can_merge);
}

#[test]
fn test_merge_rules_empty_allows_merge() {
    // No rules = auto-mergeable with 0 approvals and no checks
    let mut mgr = PRManager::new();
    let rules = MergeRuleSet {
        required_approvals: 0,
        required_checks: vec![],
        dismiss_stale_reviews: false,
        allowed_merge_agents: vec!["*".to_string()],
    };

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/empty-rules",
            "main",
            "No rules",
            "",
            "agent-1",
            rules,
        )
        .unwrap();
    assert!(mgr.evaluate_merge(&pr.id).unwrap().can_merge);
}

// ===========================================================================
// Branch Rule Tests
// ===========================================================================

#[test]
fn test_branch_rules_protected_branch() {
    let repo = make_repo(
        "repo-1",
        vec![BranchRule::protected_main(), BranchRule::standard_push("*")],
    );

    // main is protected
    let main_rule = repo.branch_rule_for("main").unwrap();
    assert!(main_rule.require_pr);
    assert_eq!(
        main_rule.allowed_push_scopes,
        vec!["git.push:protected".to_string()]
    );

    // feature branches are not protected
    let feat_rule = repo.branch_rule_for("feature/my-thing").unwrap();
    assert!(!feat_rule.require_pr);
    assert_eq!(feat_rule.allowed_push_scopes, vec!["git.push".to_string()]);
}

#[test]
fn test_branch_rules_wildcard_pattern() {
    let repo = make_repo(
        "repo-1",
        vec![
            BranchRule::protected_main(),
            BranchRule::protected_release(),
            BranchRule {
                pattern: "feature/*".to_string(),
                allowed_push_scopes: vec!["git.push".to_string()],
                require_pr: false,
            },
            BranchRule::standard_push("*"),
        ],
    );

    // main -> protected
    assert!(repo.branch_rule_for("main").unwrap().require_pr);

    // release/* -> protected
    assert!(repo.branch_rule_for("release/v1.0").unwrap().require_pr);
    assert!(repo.branch_rule_for("release/hotfix").unwrap().require_pr);

    // feature/* -> standard push (matched by feature/* rule, not wildcard)
    let feat = repo.branch_rule_for("feature/agent-42").unwrap();
    assert!(!feat.require_pr);
    assert_eq!(feat.pattern, "feature/*");

    // bugfix/xxx -> caught by wildcard
    let bugfix = repo.branch_rule_for("bugfix/fix-123").unwrap();
    assert!(!bugfix.require_pr);
    assert_eq!(bugfix.pattern, "*");
}

// ===========================================================================
// Review Tests
// ===========================================================================

#[test]
fn test_multiple_reviewers() {
    // Two reviewers — one approves, one requests changes.
    // Use 2 required approvals so a single approval doesn't auto-approve the PR.
    let mut mgr = PRManager::new();
    let rules = MergeRuleSet {
        required_approvals: 2,
        required_checks: vec![],
        dismiss_stale_reviews: true,
        allowed_merge_agents: vec!["*".to_string()],
    };

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/multi-review",
            "main",
            "Multiple reviewers",
            "",
            "agent-author",
            rules,
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // Reviewer A approves (1/2 — PR stays Open)
    mgr.add_review(&pr_id, "reviewer-a", ReviewVerdict::Approved, vec![])
        .unwrap();
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Open);

    // Reviewer B requests changes — blocks merge
    mgr.add_review(
        &pr_id,
        "reviewer-b",
        ReviewVerdict::ChangesRequested,
        vec![],
    )
    .unwrap();

    assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);

    // Reviewer B now approves — unblocks (2 approvals, no outstanding requests)
    mgr.add_review(&pr_id, "reviewer-b", ReviewVerdict::Approved, vec![])
        .unwrap();

    assert!(mgr.evaluate_merge(&pr_id).unwrap().can_merge);
}

#[test]
fn test_review_with_comments() {
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/comments",
            "main",
            "Review comments test",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
    let pr_id = pr.id.clone();

    let comments = vec![
        ReviewComment {
            file: "src/lib.rs".to_string(),
            line: Some(42),
            body: "Consider using `?` instead of unwrap".to_string(),
        },
        ReviewComment {
            file: "src/server.rs".to_string(),
            line: None,
            body: "This file needs documentation".to_string(),
        },
    ];

    mgr.add_review(
        &pr_id,
        "reviewer-1",
        ReviewVerdict::ChangesRequested,
        comments,
    )
    .unwrap();

    let stored_pr = mgr.get(&pr_id).unwrap();
    assert_eq!(stored_pr.reviews.len(), 1);

    let review = &stored_pr.reviews[0];
    assert_eq!(review.comments.len(), 2);
    assert_eq!(review.comments[0].file, "src/lib.rs");
    assert_eq!(review.comments[0].line, Some(42));
    assert!(review.comments[0].body.contains("Consider using `?`"));
    assert_eq!(review.comments[1].line, None); // file-level comment
}

// ===========================================================================
// Edge Cases
// ===========================================================================

#[test]
fn test_create_pr_nonexistent_repo_is_allowed() {
    // PRManager doesn't track repos — it trusts the caller. Creating a PR for
    // a "nonexistent" repo_id succeeds at the PRManager level; the repo check
    // happens at a higher layer (SwarmServer). Verify basic creation works.
    let mut mgr = PRManager::new();
    let result = mgr.create_pr(
        "nonexistent-repo",
        "feature/x",
        "main",
        "PR for missing repo",
        "",
        "agent-1",
        default_rules(),
    );
    // PRManager doesn't validate repo existence — that's the server's job
    assert!(result.is_ok());
}

#[test]
fn test_double_merge_fails() {
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/double-merge",
            "main",
            "Double merge attempt",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // Approve and merge
    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    mgr.mark_merged(&pr_id).unwrap();

    // Try to merge again — should fail
    let result = mgr.mark_merged(&pr_id);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("must be Approved"));
}

#[test]
fn test_review_merged_pr_fails() {
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/review-after-merge",
            "main",
            "Review after merge",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // Approve and merge
    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    mgr.mark_merged(&pr_id).unwrap();

    // Try to review a merged PR — should fail
    let result = mgr.add_review(&pr_id, "reviewer-2", ReviewVerdict::Approved, vec![]);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cannot review PR"));
}

#[test]
fn test_review_closed_pr_fails() {
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/review-closed",
            "main",
            "Review closed PR",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
    let pr_id = pr.id.clone();

    mgr.close(&pr_id).unwrap();

    let result = mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![]);
    assert!(result.is_err());
}

#[test]
fn test_close_merged_pr_fails() {
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/close-merged",
            "main",
            "Close merged PR",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
    let pr_id = pr.id.clone();

    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    mgr.mark_merged(&pr_id).unwrap();

    let result = mgr.close(&pr_id);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already-merged"));
}

#[test]
fn test_merge_unapproved_pr_fails() {
    // Trying to mark_merged on an Open PR should fail
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/unapproved",
            "main",
            "Unapproved merge attempt",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();

    let result = mgr.mark_merged(&pr.id);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("must be Approved"));
}

// ===========================================================================
// Stale Review Dismissal Tests
// ===========================================================================

#[test]
fn test_dismiss_stale_reviews_resets_status() {
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/stale-dismiss",
            "main",
            "Stale dismiss test",
            "",
            "agent-1",
            default_rules(), // dismiss_stale_reviews = true
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // Approve → PR becomes Approved
    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Approved);

    // Simulate new push → dismiss stale reviews
    let dismissed = mgr.dismiss_stale_reviews(&pr_id).unwrap();
    assert_eq!(dismissed, 1);
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Open);

    // Re-evaluate — should NOT be mergeable now
    assert!(!mgr.evaluate_merge(&pr_id).unwrap().can_merge);

    // Re-approve → mergeable again
    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    assert!(mgr.evaluate_merge(&pr_id).unwrap().can_merge);
}

#[test]
fn test_dismiss_stale_reviews_disabled() {
    let mut mgr = PRManager::new();
    let rules = MergeRuleSet {
        required_approvals: 1,
        required_checks: vec![],
        dismiss_stale_reviews: false, // Disabled
        allowed_merge_agents: vec!["*".to_string()],
    };

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/no-dismiss",
            "main",
            "No dismiss test",
            "",
            "agent-1",
            rules,
        )
        .unwrap();
    let pr_id = pr.id.clone();

    mgr.add_review(&pr_id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();

    // Dismiss does nothing when disabled
    let dismissed = mgr.dismiss_stale_reviews(&pr_id).unwrap();
    assert_eq!(dismissed, 0);
    assert_eq!(mgr.get(&pr_id).unwrap().status, PRStatus::Approved);
}

// ===========================================================================
// Check Lifecycle Tests
// ===========================================================================

#[test]
fn test_check_update_replaces_existing() {
    // Updating a check by name should replace the existing entry, not add a new one
    let mut mgr = PRManager::new();
    let rules = rules_with_checks(0, vec!["cargo-test"]);

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/check-update",
            "main",
            "Check update test",
            "",
            "agent-1",
            rules,
        )
        .unwrap();
    let pr_id = pr.id.clone();

    // Initial: pending (via Running)
    mgr.update_check(&pr_id, "cargo-test", "ci-1", CheckStatus::Running, None)
        .unwrap();
    assert_eq!(mgr.get(&pr_id).unwrap().checks.len(), 1);

    // Update to failure
    mgr.update_check(
        &pr_id,
        "cargo-test",
        "ci-1",
        CheckStatus::Failure,
        Some("3 tests failed".to_string()),
    )
    .unwrap();
    assert_eq!(mgr.get(&pr_id).unwrap().checks.len(), 1); // Still 1, not 2
    assert_eq!(
        mgr.get(&pr_id).unwrap().checks[0].status,
        CheckStatus::Failure
    );
    assert!(mgr.get(&pr_id).unwrap().checks[0].completed_at.is_some());

    // Update to success
    mgr.update_check(
        &pr_id,
        "cargo-test",
        "ci-1",
        CheckStatus::Success,
        Some("All tests pass".to_string()),
    )
    .unwrap();
    assert_eq!(mgr.get(&pr_id).unwrap().checks.len(), 1);
    assert_eq!(
        mgr.get(&pr_id).unwrap().checks[0].status,
        CheckStatus::Success
    );
}

// ===========================================================================
// Merge Evaluation Detail Tests
// ===========================================================================

#[test]
fn test_merge_evaluation_reports_individual_checks() {
    // Verify that MergeEvaluation contains granular RuleCheck entries
    let mut mgr = PRManager::new();
    let rules = rules_with_checks(1, vec!["cargo-test", "vault-linter"]);

    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/eval-detail",
            "main",
            "Evaluation detail",
            "",
            "agent-1",
            rules,
        )
        .unwrap();
    let pr_id = pr.id.clone();

    let eval = mgr.evaluate_merge(&pr_id).unwrap();
    assert!(!eval.can_merge);

    // Should have 4 checks: approvals, no change requests, cargo-test, vault-linter
    assert_eq!(eval.checks.len(), 4);

    // Approvals check
    assert!(eval.checks[0].description.contains("Required approvals"));
    assert!(!eval.checks[0].satisfied); // 0/1

    // No outstanding change requests — satisfied (no reviews yet)
    assert!(eval.checks[1]
        .description
        .contains("No outstanding change requests"));
    assert!(eval.checks[1].satisfied);

    // CI checks — both unsatisfied
    assert!(eval.checks[2].description.contains("cargo-test"));
    assert!(!eval.checks[2].satisfied);
    assert!(eval.checks[3].description.contains("vault-linter"));
    assert!(!eval.checks[3].satisfied);
}

// ===========================================================================
// Cross-Repo Isolation Tests
// ===========================================================================

#[test]
fn test_prs_across_repos_are_isolated() {
    let mut mgr = PRManager::new();

    let pr_r1 = mgr
        .create_pr(
            "repo-a",
            "feature/x",
            "main",
            "PR in repo A",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();
    let pr_r2 = mgr
        .create_pr(
            "repo-b",
            "feature/x",
            "main",
            "PR in repo B (same branch name!)",
            "",
            "agent-1",
            default_rules(),
        )
        .unwrap();

    // Same branch name, different repos — both should exist
    assert_ne!(pr_r1.id, pr_r2.id);

    // list_for_repo returns only that repo's PRs
    assert_eq!(mgr.list_for_repo("repo-a", None).len(), 1);
    assert_eq!(mgr.list_for_repo("repo-b", None).len(), 1);

    // Merging in repo-a doesn't affect repo-b
    mgr.add_review(&pr_r1.id, "reviewer-1", ReviewVerdict::Approved, vec![])
        .unwrap();
    mgr.mark_merged(&pr_r1.id).unwrap();

    assert_eq!(mgr.get(&pr_r2.id).unwrap().status, PRStatus::Open);
}

// ===========================================================================
// Self-Review Prevention
// ===========================================================================

#[test]
fn test_self_review_prevented() {
    let mut mgr = PRManager::new();
    let pr = mgr
        .create_pr(
            "repo-1",
            "feature/self-review",
            "main",
            "Self review test",
            "",
            "agent-author",
            default_rules(),
        )
        .unwrap();

    // Author can't review their own PR
    let result = mgr.add_review(&pr.id, "agent-author", ReviewVerdict::Approved, vec![]);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("cannot review their own"));
}

// ===========================================================================
// Duplicate PR Prevention
// ===========================================================================

#[test]
fn test_duplicate_pr_for_same_branch_rejected() {
    let mut mgr = PRManager::new();

    mgr.create_pr(
        "repo-1",
        "feature/dup",
        "main",
        "First PR",
        "",
        "agent-1",
        default_rules(),
    )
    .unwrap();

    // Second PR for same branch in same repo — should fail
    let result = mgr.create_pr(
        "repo-1",
        "feature/dup",
        "main",
        "Duplicate PR",
        "",
        "agent-2",
        default_rules(),
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already open"));
}
