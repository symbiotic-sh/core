//! T128 §15 — External-push gating for Source Archeology findings.
//!
//! Mirrors the T126 inline-blocking `mirror_push_with_approval` pattern at
//! `repo_mirror.rs::mirror_push_with_approval` exactly: open ticket, post
//! matrix message, poll the gate inline until terminal state, then dispatch
//! the push. No background pump.
//!
//! See `tasks/128-source-archeology/15-external-push-gating.md` for the full
//! ratified design (D1 typed `ApprovalOperation` enum refactor, D6
//! inline-blocking pattern, D7 explicit `FindingSeverity` `Ord` impl).

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use credential_gateway::{
    with_git_push_session, GitCredentialKind, GitPushSessionError, GoalScopedVault,
};
use symbiotic_agents::{ArcheologyMode, ArcheologyTarget, Finding, FindingAction};
use symbiotic_control_plane::{ArcheologyPolicy, RepoManifest, RepoRole};

use crate::approval_gate::{
    ApprovalContext, ApprovalGate, ApprovalOperation, ApprovalState, ArcheologyApprovalDetail,
    CommitRange, DiffStats,
};
use crate::matrix_poster::MatrixPoster;

// ── GitPushSession trait + impls ───────────────────────────────────────

/// Mockable wrapper over `credential_gateway::with_git_push_session`. The
/// production impl materializes the credential through the scope-guarded
/// closure; tests substitute a `MockGitPushSession` that captures inputs and
/// returns canned commit SHAs without touching disk or git.
///
/// The trait owns the patch-apply-then-push step end-to-end so callers don't
/// need to reach into `git apply` plumbing. Returns the resulting commit SHA
/// on the target branch.
#[async_trait]
pub trait GitPushSession: Send + Sync {
    /// Apply `patch_diff` to the bare repo at `target_bare_path` on
    /// `target_branch` and push to the remote bound to `credential_id`.
    /// Returns the resulting commit SHA on success.
    async fn apply_and_push(
        &self,
        credential_id: &str,
        target_bare_path: &Path,
        target_branch: &str,
        patch_diff: &str,
    ) -> Result<String>;
}

/// Production `GitPushSession` that wraps `with_git_push_session`. Holds an
/// `Arc<GoalScopedVault>` so the credential resolves under the scope-guarded
/// closure when `apply_and_push` is called.
///
/// Patch application + push mechanics live behind this impl; this chunk
/// stubs the actual git invocation so the tests in `archeology_dispatch`
/// can run without git fixtures. Wiring the real `git apply` + `git push`
/// pipeline lands when the dispatch path is exercised end-to-end (T128 §17).
pub struct CredentialGatewayPushSession {
    vault: Arc<GoalScopedVault>,
    kind: GitCredentialKind,
    goal_scope: Option<String>,
}

impl CredentialGatewayPushSession {
    pub fn new(
        vault: Arc<GoalScopedVault>,
        kind: GitCredentialKind,
        goal_scope: Option<String>,
    ) -> Self {
        Self {
            vault,
            kind,
            goal_scope,
        }
    }
}

#[async_trait]
impl GitPushSession for CredentialGatewayPushSession {
    async fn apply_and_push(
        &self,
        credential_id: &str,
        _target_bare_path: &Path,
        _target_branch: &str,
        _patch_diff: &str,
    ) -> Result<String> {
        // Materialize the credential through the scope-guarded closure. The
        // closure body is the natural seam where the real `git apply` +
        // `git push` invocation will land in the end-to-end wiring chunk
        // (T128 §17). Until then, we surface a typed error so callers can
        // distinguish "credential ok but apply not yet wired" from
        // credential resolution failures.
        let result = with_git_push_session(
            &self.vault,
            credential_id,
            self.kind,
            self.goal_scope.as_deref(),
            |_env| -> std::result::Result<String, GitPushSessionError> {
                // TODO(T128 §17): apply patch + push using `_env.apply(cmd)`.
                Err(GitPushSessionError::Subprocess(
                    "archeology apply_and_push not yet wired (pending T128 §17)".to_string(),
                ))
            },
        );
        match result {
            Ok(sha) => Ok(sha),
            Err(e) => Err(anyhow!("git push session failed: {e}")),
        }
    }
}

// ── DispatchOutcome ────────────────────────────────────────────────────

/// Terminal outcome of `dispatch_finding`. Mirrors T126's
/// `mirror_push_with_approval` shape — caller observes the variant and
/// emits the appropriate archive event / matrix follow-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Push applied + landed via operator approval. Commit SHA on the
    /// target branch.
    Dispatched { commit_sha: String },
    /// Auto-approved via `archeology_policy.auto_approve_max_severity`;
    /// push applied without operator involvement.
    AutoApproved { commit_sha: String },
    /// Rejected before the approval gate (scope / role / patch-invalid /
    /// non-Patch finding action / DryRun-incompatible inputs).
    Rejected { reason: String },
    /// Operator denied the ticket.
    Denied { reason: Option<String> },
    /// `target.mode == ArcheologyMode::DryRun` → no push; preview returned
    /// for the operator to inspect.
    DryRun { preview: String },
    /// TTL elapsed with the ticket still `Pending`.
    TimedOut,
}

// ── dispatch_finding ───────────────────────────────────────────────────

/// Inline-blocking dispatch for a single archeology `Resolve`-disposition
/// finding. Polls the gate until terminal state — mirrors T126
/// `mirror_push_with_approval` (no background pump).
///
/// Evaluation order (each step short-circuits on match):
///   1. `target.mode == DryRun`            → `DryRun { preview }`
///   2. `finding.proposed_action != Patch` → `Rejected` (NewFile/Report
///      route elsewhere)
///   3. `!agent_scopes.push_external`      → `Rejected`
///   4. `repo_role == ReferenceLibrary`    → `Rejected`
///   5. `auto_approve_max_severity` covers severity → push immediately,
///      `AutoApproved`
///   6. otherwise open ticket, post matrix, poll until terminal.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_finding(
    finding: &Finding,
    manifest: &RepoManifest,
    target: &ArcheologyTarget,
    approval_gate: &Arc<Mutex<ApprovalGate>>,
    matrix_poster: &dyn MatrixPoster,
    push_session: &dyn GitPushSession,
    target_bare_path: &Path,
    credential_id: &str,
    operator_room_id: &str,
    archeology_policy: &ArcheologyPolicy,
    ttl_secs: u64,
    poll_interval_ms: u64,
    now_fn: impl Fn() -> u64,
) -> Result<DispatchOutcome> {
    // 1. DryRun mode short-circuits with a preview of the proposed patch.
    if matches!(target.mode, ArcheologyMode::DryRun) {
        let preview = match &finding.proposed_action {
            FindingAction::Patch { diff } => diff.clone(),
            FindingAction::NewFile { path, content } => {
                format!("(new file)\npath: {path}\n---\n{content}")
            }
            FindingAction::Report { text } => format!("(report)\n{text}"),
        };
        return Ok(DispatchOutcome::DryRun { preview });
    }

    // 2. Only Patch findings flow through this dispatcher. NewFile is
    //    handled by T127 Auto-Provision; Report is operator-read-only.
    let patch_diff = match &finding.proposed_action {
        FindingAction::Patch { diff } => diff.clone(),
        FindingAction::NewFile { .. } | FindingAction::Report { .. } => {
            return Ok(DispatchOutcome::Rejected {
                reason: "archeology_dispatch only handles Patch actions".to_string(),
            });
        }
    };

    // 3. Scope check — agent must have push_external on this repo.
    if !manifest.agent_scopes.push_external {
        return Ok(DispatchOutcome::Rejected {
            reason: "push_external not in scope".to_string(),
        });
    }

    // 4. Role check — reference libraries are always read-only.
    if matches!(manifest.repo_role, RepoRole::ReferenceLibrary) {
        return Ok(DispatchOutcome::Rejected {
            reason: "reference_library is always read-only".to_string(),
        });
    }

    // 5. Auto-approve when the finding's severity is at or below the
    //    operator-configured threshold (None = always require approval).
    if let Some(max) = archeology_policy.auto_approve_max_severity {
        if finding.severity <= max {
            let commit_sha = push_session
                .apply_and_push(
                    credential_id,
                    target_bare_path,
                    &target.base_branch,
                    &patch_diff,
                )
                .await?;
            return Ok(DispatchOutcome::AutoApproved { commit_sha });
        }
    }

    // 6. Open approval ticket via the same `ApprovalGate` T126 uses.
    let now = now_fn();
    let explanation = format!(
        "Agent wants to resolve finding {fid} ({severity:?}) at {path}",
        fid = finding.id,
        severity = finding.severity,
        path = finding.evidence_path,
    );
    let context = ApprovalContext {
        operation: ApprovalOperation::ArcheologyPush,
        explanation: explanation.clone(),
        repo_id: manifest.id.clone(),
        remote_url: manifest.source.url.clone(),
        local_bare_path: target_bare_path.display().to_string(),
        branch: target.base_branch.clone(),
        is_protected_branch: manifest
            .source
            .protected_branches
            .iter()
            .any(|b| b == &target.base_branch),
        agent_id: format!("archeology:{}", target.goal_id),
        goal_id: Some(target.goal_id.clone()),
        project_id: manifest.project_id.clone(),
        commit_range: CommitRange {
            from_sha: "0000000000000000000000000000000000000000".to_string(),
            to_sha: "(pending)".to_string(),
            commit_count: 1,
        },
        diff_stats: DiffStats {
            files_changed: 0,
            insertions: 0,
            deletions: 0,
        },
        top_commit_message: explanation.clone(),
        archeology_detail: Some(ArcheologyApprovalDetail {
            finding_id: finding.id.clone(),
            evidence_path: finding.evidence_path.clone(),
            severity: finding.severity,
            category: finding.category.clone(),
            rationale: finding.description.clone(),
        }),
    };

    let ticket = {
        let mut gate = approval_gate
            .lock()
            .map_err(|e| anyhow!("approval_gate mutex poisoned: {e}"))?;
        gate.open_ticket(context, now, ttl_secs)
    };

    // 7. Post the approval body to the operator room.
    //
    // TODO(T128 §16a / Agent B): switch to
    //   `matrix_poster.post_text_with_source(operator_room_id, &body, "archeology", now)`
    // once the typed-source variant lands on the `MatrixPoster` trait. Until
    // then, the existing `post_text` carries the body and the source tag is
    // implicit in the body header.
    let body = format!(
        "[archeology] Approval requested for finding `{fid}` ({severity:?}) at `{path}`.\n\nTicket: `{ticket_id}`\n{explanation}\n\nReply: approve {ticket_id}  |  deny {ticket_id} [reason]",
        fid = finding.id,
        severity = finding.severity,
        path = finding.evidence_path,
        ticket_id = ticket.ticket_id,
    );
    matrix_poster
        .post_text(operator_room_id, &body, now)
        .await
        .map_err(|e| anyhow!("matrix_poster.post_text failed: {e}"))?;

    // 8. Poll until terminal state.
    let deadline = now + ttl_secs;
    let outcome_state = loop {
        tokio::time::sleep(std::time::Duration::from_millis(poll_interval_ms)).await;
        let now_check = now_fn();
        let state = {
            let mut gate = approval_gate
                .lock()
                .map_err(|e| anyhow!("approval_gate mutex poisoned: {e}"))?;
            gate.expire_stale(now_check);
            gate.get(&ticket.ticket_id)
                .map(|t| t.state.clone())
                .ok_or_else(|| anyhow!("approval ticket vanished: {}", ticket.ticket_id))?
        };
        match state {
            ApprovalState::Pending => {
                if now_check > deadline {
                    // Force expire at the next iteration in case `requested_at`
                    // arithmetic and the wallclock disagree by an off-by-one.
                    let mut gate = approval_gate
                        .lock()
                        .map_err(|e| anyhow!("approval_gate mutex poisoned: {e}"))?;
                    gate.expire_stale(now_check.saturating_add(ttl_secs));
                    let s = gate
                        .get(&ticket.ticket_id)
                        .map(|t| t.state.clone())
                        .ok_or_else(|| anyhow!("approval ticket vanished post-expire"))?;
                    if matches!(s, ApprovalState::Pending) {
                        // Still pending after a forced expire pass — bail
                        // with `TimedOut` so we don't loop forever.
                        break ApprovalState::Expired;
                    }
                    break s;
                }
                continue;
            }
            other => break other,
        }
    };

    match outcome_state {
        ApprovalState::Approved { .. } => {
            let commit_sha = push_session
                .apply_and_push(
                    credential_id,
                    target_bare_path,
                    &target.base_branch,
                    &patch_diff,
                )
                .await?;
            Ok(DispatchOutcome::Dispatched { commit_sha })
        }
        ApprovalState::Denied { reason, .. } => Ok(DispatchOutcome::Denied { reason }),
        ApprovalState::Expired => Ok(DispatchOutcome::TimedOut),
        ApprovalState::Pending => {
            // The poll loop only breaks on non-pending states, so this is
            // unreachable — surface as `TimedOut` defensively.
            Ok(DispatchOutcome::TimedOut)
        }
    }
}

// ── Resolve credential kind from URL (helper) ──────────────────────────

#[allow(dead_code)] // wired in once the e2e dispatch path lands (T128 §17)
pub(crate) fn credential_kind_from_url(url: &str) -> GitCredentialKind {
    if url.starts_with("https://") || url.starts_with("http://") {
        GitCredentialKind::HttpsToken
    } else {
        GitCredentialKind::SshPrivateKey
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use symbiotic_agents::{
        ArcheologyMode, ArcheologyTarget, Finding, FindingAction, FindingSeverity,
        FindingSourceStage, GoalAlignment, PathPattern,
    };
    use symbiotic_control_plane::{
        ArcheologyPolicy, CredentialScope, MirrorDirection, RepoAgentScopes, RepoCheckoutPolicy,
        RepoCredentialBinding, RepoHooks, RepoManifest, RepoMetadata, RepoMirrorPolicy,
        RepoProvider, RepoRole, RepoSource, RepoState,
    };
    use symbiotic_trust::AgentTrustLevel;

    // ── Mock matrix poster ────────────────────────────────────────────

    struct MockMatrixPoster {
        captured: StdMutex<Vec<(String, String)>>,
    }

    impl MockMatrixPoster {
        fn new() -> Self {
            Self {
                captured: StdMutex::new(Vec::new()),
            }
        }

        fn captured(&self) -> Vec<(String, String)> {
            self.captured.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MatrixPoster for MockMatrixPoster {
        async fn post_text(&self, room_id: &str, body: &str, _now: u64) -> Result<()> {
            self.captured
                .lock()
                .unwrap()
                .push((room_id.to_string(), body.to_string()));
            Ok(())
        }
    }

    // ── Mock git push session ─────────────────────────────────────────

    #[derive(Debug, Clone)]
    #[allow(dead_code)] // captured for inspection in tests; not all fields asserted
    struct CapturedPush {
        credential_id: String,
        target_bare_path: PathBuf,
        target_branch: String,
        patch_diff: String,
    }

    struct MockGitPushSession {
        commit_sha: String,
        captured: StdMutex<Vec<CapturedPush>>,
    }

    impl MockGitPushSession {
        fn new(commit_sha: &str) -> Self {
            Self {
                commit_sha: commit_sha.to_string(),
                captured: StdMutex::new(Vec::new()),
            }
        }

        fn pushes(&self) -> Vec<CapturedPush> {
            self.captured.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl GitPushSession for MockGitPushSession {
        async fn apply_and_push(
            &self,
            credential_id: &str,
            target_bare_path: &Path,
            target_branch: &str,
            patch_diff: &str,
        ) -> Result<String> {
            self.captured.lock().unwrap().push(CapturedPush {
                credential_id: credential_id.to_string(),
                target_bare_path: target_bare_path.to_path_buf(),
                target_branch: target_branch.to_string(),
                patch_diff: patch_diff.to_string(),
            });
            Ok(self.commit_sha.clone())
        }
    }

    // ── Fixture builders ──────────────────────────────────────────────

    fn build_finding(severity: FindingSeverity, action: FindingAction) -> Finding {
        Finding {
            id: "f-test-1".to_string(),
            source_stage: FindingSourceStage::Reconcile,
            severity,
            category: "drift".to_string(),
            evidence_path: "docs/README.md".to_string(),
            description: "Section X references a deleted script.".to_string(),
            proposed_action: action,
            goal_alignment: GoalAlignment::InScope,
        }
    }

    fn patch_finding(severity: FindingSeverity) -> Finding {
        build_finding(
            severity,
            FindingAction::Patch {
                diff: "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            },
        )
    }

    fn build_manifest(role: RepoRole, push_external: bool) -> RepoManifest {
        RepoManifest {
            id: "repo:flux".to_string(),
            project_id: "project:flux".to_string(),
            slug: "flux".to_string(),
            title: "Flux".to_string(),
            state: RepoState::Active,
            repo_role: role,
            source: RepoSource {
                url: "git@github.com:kakajansh/flux.git".to_string(),
                provider: RepoProvider::Github,
                default_branch: "main".to_string(),
                protected_branches: vec!["main".to_string()],
                pinned_head: None,
            },
            credential: RepoCredentialBinding {
                id: "cred:gh-1".to_string(),
                scope: CredentialScope::Push,
                trust_floor: AgentTrustLevel::ExternalAct,
            },
            mirror: RepoMirrorPolicy {
                internal_bare_path: PathBuf::from("data/git-server/repos/flux.git"),
                direction: MirrorDirection::Bidirectional,
                sync_interval_secs: 600,
                last_pulled_at: None,
                last_pushed_at: None,
            },
            checkout: RepoCheckoutPolicy {
                worktree_root: PathBuf::from("data/worktrees"),
                agent_branch_prefix: "agent/".to_string(),
                max_concurrent_worktrees: 4,
                cleanup_on_goal_close: true,
            },
            agent_scopes: RepoAgentScopes {
                read: vec!["**/*".to_string()],
                write: vec!["docs/**".to_string()],
                push_external,
                requires_operator_approval_for: vec!["push_external".to_string()],
            },
            hooks: RepoHooks {
                on_attach: None,
                on_drift_detected: None,
                on_detach: None,
            },
            indexing: None,
            archeology_policy: None,
            metadata: RepoMetadata {
                attached_at: "2026-04-19T00:00:00Z".to_string(),
                attached_by: "operator".to_string(),
                notes: String::new(),
            },
            body_markdown: String::new(),
        }
    }

    fn build_target(mode: ArcheologyMode) -> ArcheologyTarget {
        ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "agent/archeology".to_string(),
            goal_id: "goal-1".to_string(),
            allowed_paths: vec![PathPattern("docs/**".to_string())],
            mode,
        }
    }

    fn empty_policy() -> ArcheologyPolicy {
        ArcheologyPolicy::default()
    }

    fn auto_approve_policy(max: FindingSeverity) -> ArcheologyPolicy {
        ArcheologyPolicy {
            auto_approve_max_severity: Some(max),
            ..ArcheologyPolicy::default()
        }
    }

    // ── Tests 1-10 ────────────────────────────────────────────────────

    /// Test 1 — `DryRun` mode short-circuits with a preview; gate
    /// untouched, no matrix post.
    #[tokio::test]
    async fn dry_run_mode_returns_preview() {
        let finding = patch_finding(FindingSeverity::High);
        let manifest = build_manifest(RepoRole::Source, true);
        let target = build_target(ArcheologyMode::DryRun);
        let gate = Arc::new(Mutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let session = MockGitPushSession::new("aaaaaaa");
        let policy = empty_policy();

        let outcome = dispatch_finding(
            &finding,
            &manifest,
            &target,
            &gate,
            &poster,
            &session,
            Path::new("/tmp/bare"),
            "cred:gh-1",
            "!ops:matrix",
            &policy,
            3600,
            10,
            || 1000,
        )
        .await
        .expect("dispatch ok");

        match outcome {
            DispatchOutcome::DryRun { preview } => {
                assert!(
                    preview.contains("--- a/x"),
                    "preview missing diff: {preview}"
                );
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
        assert!(poster.captured().is_empty(), "no matrix post in DryRun");
        assert!(session.pushes().is_empty(), "no push in DryRun");
    }

    /// Test 2 — Non-Patch finding (NewFile) → `Rejected`.
    #[tokio::test]
    async fn non_patch_finding_rejected() {
        let finding = build_finding(
            FindingSeverity::Low,
            FindingAction::NewFile {
                path: "docs/new.md".to_string(),
                content: "# New\n".to_string(),
            },
        );
        let manifest = build_manifest(RepoRole::Source, true);
        let target = build_target(ArcheologyMode::Full);
        let gate = Arc::new(Mutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let session = MockGitPushSession::new("aaaaaaa");
        let policy = empty_policy();

        let outcome = dispatch_finding(
            &finding,
            &manifest,
            &target,
            &gate,
            &poster,
            &session,
            Path::new("/tmp/bare"),
            "cred:gh-1",
            "!ops:matrix",
            &policy,
            3600,
            10,
            || 1000,
        )
        .await
        .expect("dispatch ok");

        match outcome {
            DispatchOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("Patch"),
                    "reason should mention Patch: {reason}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert!(session.pushes().is_empty());
    }

    /// Test 3 — `push_external` scope missing → `Rejected`.
    #[tokio::test]
    async fn scope_missing_rejected() {
        let finding = patch_finding(FindingSeverity::Low);
        let manifest = build_manifest(RepoRole::Source, /* push_external */ false);
        let target = build_target(ArcheologyMode::Full);
        let gate = Arc::new(Mutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let session = MockGitPushSession::new("aaaaaaa");
        let policy = empty_policy();

        let outcome = dispatch_finding(
            &finding,
            &manifest,
            &target,
            &gate,
            &poster,
            &session,
            Path::new("/tmp/bare"),
            "cred:gh-1",
            "!ops:matrix",
            &policy,
            3600,
            10,
            || 1000,
        )
        .await
        .expect("dispatch ok");

        match outcome {
            DispatchOutcome::Rejected { reason } => {
                assert!(reason.contains("push_external"), "scope reason: {reason}");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    /// Test 4 — `RepoRole::ReferenceLibrary` → `Rejected` regardless of scope.
    #[tokio::test]
    async fn reference_library_role_rejected() {
        let finding = patch_finding(FindingSeverity::Low);
        let manifest = build_manifest(RepoRole::ReferenceLibrary, /* push_external */ true);
        let target = build_target(ArcheologyMode::Full);
        let gate = Arc::new(Mutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let session = MockGitPushSession::new("aaaaaaa");
        let policy = empty_policy();

        let outcome = dispatch_finding(
            &finding,
            &manifest,
            &target,
            &gate,
            &poster,
            &session,
            Path::new("/tmp/bare"),
            "cred:gh-1",
            "!ops:matrix",
            &policy,
            3600,
            10,
            || 1000,
        )
        .await
        .expect("dispatch ok");

        match outcome {
            DispatchOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("reference_library"),
                    "role reason: {reason}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    /// Test 5 — Auto-approve eligible (severity ≤ threshold) → `AutoApproved`.
    #[tokio::test]
    async fn auto_approve_eligible() {
        let finding = patch_finding(FindingSeverity::Low);
        let manifest = build_manifest(RepoRole::Source, true);
        let target = build_target(ArcheologyMode::Full);
        let gate = Arc::new(Mutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let session = MockGitPushSession::new("autoapprove-sha");
        let policy = auto_approve_policy(FindingSeverity::Medium);

        let outcome = dispatch_finding(
            &finding,
            &manifest,
            &target,
            &gate,
            &poster,
            &session,
            Path::new("/tmp/bare"),
            "cred:gh-1",
            "!ops:matrix",
            &policy,
            3600,
            10,
            || 1000,
        )
        .await
        .expect("dispatch ok");

        match outcome {
            DispatchOutcome::AutoApproved { commit_sha } => {
                assert_eq!(commit_sha, "autoapprove-sha");
            }
            other => panic!("expected AutoApproved, got {other:?}"),
        }
        assert_eq!(session.pushes().len(), 1, "exactly one push");
        assert!(
            poster.captured().is_empty(),
            "auto-approve must not post to matrix"
        );
        let gate_has_tickets = !gate.lock().unwrap().list_pending().is_empty();
        assert!(!gate_has_tickets, "gate untouched on auto-approve");
    }

    /// Test 6 — Pending-then-approve: ticket opens, matrix post captured,
    /// mock gate flip to `Approved` triggers `Dispatched`.
    #[tokio::test]
    async fn pending_then_approve_dispatches() {
        let finding = patch_finding(FindingSeverity::High);
        let manifest = build_manifest(RepoRole::Source, true);
        let target = build_target(ArcheologyMode::Full);
        let gate = Arc::new(Mutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let session = MockGitPushSession::new("approved-sha");
        let policy = empty_policy();

        // Spawn the dispatcher; flip the gate Approved on a separate task.
        let gate_for_flip = gate.clone();
        let flipper = tokio::spawn(async move {
            // Wait for the ticket to appear in the gate, then approve it.
            for _ in 0..200 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                let pending_id = {
                    let g = gate_for_flip.lock().unwrap();
                    g.list_pending().first().map(|t| t.ticket_id.clone())
                };
                if let Some(id) = pending_id {
                    let mut g = gate_for_flip.lock().unwrap();
                    g.approve(&id, "@op:matrix", 1500).expect("approve ok");
                    return;
                }
            }
            panic!("ticket never appeared for approve flip");
        });

        let outcome = dispatch_finding(
            &finding,
            &manifest,
            &target,
            &gate,
            &poster,
            &session,
            Path::new("/tmp/bare"),
            "cred:gh-1",
            "!ops:matrix",
            &policy,
            3600,
            10,
            || 1000,
        )
        .await
        .expect("dispatch ok");

        flipper.await.unwrap();

        match outcome {
            DispatchOutcome::Dispatched { commit_sha } => {
                assert_eq!(commit_sha, "approved-sha");
            }
            other => panic!("expected Dispatched, got {other:?}"),
        }
        let posts = poster.captured();
        assert_eq!(posts.len(), 1, "one matrix post for the ticket");
        assert_eq!(posts[0].0, "!ops:matrix");
        assert!(posts[0].1.contains("[archeology]"));
        assert_eq!(session.pushes().len(), 1);
    }

    /// Test 7 — Pending-then-deny → `Denied { reason }`.
    #[tokio::test]
    async fn pending_then_deny_returns_denied() {
        let finding = patch_finding(FindingSeverity::High);
        let manifest = build_manifest(RepoRole::Source, true);
        let target = build_target(ArcheologyMode::Full);
        let gate = Arc::new(Mutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let session = MockGitPushSession::new("never");
        let policy = empty_policy();

        let gate_for_flip = gate.clone();
        let flipper = tokio::spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                let pending_id = {
                    let g = gate_for_flip.lock().unwrap();
                    g.list_pending().first().map(|t| t.ticket_id.clone())
                };
                if let Some(id) = pending_id {
                    let mut g = gate_for_flip.lock().unwrap();
                    g.deny(&id, "@op:matrix", 1500, Some("too risky".to_string()))
                        .expect("deny ok");
                    return;
                }
            }
            panic!("ticket never appeared for deny flip");
        });

        let outcome = dispatch_finding(
            &finding,
            &manifest,
            &target,
            &gate,
            &poster,
            &session,
            Path::new("/tmp/bare"),
            "cred:gh-1",
            "!ops:matrix",
            &policy,
            3600,
            10,
            || 1000,
        )
        .await
        .expect("dispatch ok");

        flipper.await.unwrap();

        match outcome {
            DispatchOutcome::Denied { reason } => {
                assert_eq!(reason.as_deref(), Some("too risky"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert!(session.pushes().is_empty(), "no push on deny");
    }

    /// Test 8 — TTL elapsed (clock advances past `requested_at + ttl_secs`)
    /// → `TimedOut`; ticket marked `Expired` by the gate.
    #[tokio::test]
    async fn ttl_elapsed_times_out() {
        let finding = patch_finding(FindingSeverity::High);
        let manifest = build_manifest(RepoRole::Source, true);
        let target = build_target(ArcheologyMode::Full);
        let gate = Arc::new(Mutex::new(ApprovalGate::new()));
        let poster = MockMatrixPoster::new();
        let session = MockGitPushSession::new("never");
        let policy = empty_policy();

        // now_fn returns 1000 on first call (open_ticket), 9999 on every
        // subsequent call (each poll iteration). With ttl_secs=10 and
        // `now_check > 1010`, the ticket expires on the first poll.
        let calls = StdMutex::new(0u32);
        let now_fn = || {
            let mut n = calls.lock().unwrap();
            *n += 1;
            if *n == 1 {
                1000
            } else {
                9999
            }
        };

        let outcome = dispatch_finding(
            &finding,
            &manifest,
            &target,
            &gate,
            &poster,
            &session,
            Path::new("/tmp/bare"),
            "cred:gh-1",
            "!ops:matrix",
            &policy,
            10, // ttl_secs
            10, // poll_interval_ms
            now_fn,
        )
        .await
        .expect("dispatch ok");

        match outcome {
            DispatchOutcome::TimedOut => {}
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert!(session.pushes().is_empty());
        // The ticket must be in Expired state per the gate.
        let g = gate.lock().unwrap();
        let any_expired = g.list_pending().is_empty();
        assert!(any_expired, "ticket should no longer be pending");
    }

    /// Test 9 — `FindingSeverity` ordering: `Low < Medium < High < Critical`.
    /// Unit test on the explicit `Ord` impl (no derive).
    #[test]
    fn finding_severity_ordering() {
        use FindingSeverity::*;
        assert!(Low < Medium);
        assert!(Medium < High);
        assert!(High < Critical);
        assert!(Low < Critical);

        // Reflexive: ≤ ought to allow equal severity in
        // `auto_approve_max_severity` checks.
        assert!(Low <= Low);
        assert!(High <= High);

        // Cross-compare a vec sort to lock in the ordering empirically.
        let mut v = vec![Critical, Low, High, Medium];
        v.sort();
        assert_eq!(v, vec![Low, Medium, High, Critical]);
    }

    /// Test 10 — Typed `ApprovalOperation` enum serde: `ArcheologyPush`
    /// round-trips as the snake_case string `"archeology_push"`.
    #[test]
    fn approval_operation_serde_round_trip() {
        let op = ApprovalOperation::ArcheologyPush;
        let s = serde_json::to_string(&op).expect("serialize");
        assert_eq!(s, "\"archeology_push\"");
        let parsed: ApprovalOperation = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(parsed, ApprovalOperation::ArcheologyPush);

        // PushExternal also round-trips for symmetry.
        let push_ext = ApprovalOperation::PushExternal;
        let s2 = serde_json::to_string(&push_ext).expect("serialize");
        assert_eq!(s2, "\"push_external\"");
        let parsed2: ApprovalOperation = serde_json::from_str(&s2).expect("deserialize");
        assert_eq!(parsed2, ApprovalOperation::PushExternal);
    }
}
