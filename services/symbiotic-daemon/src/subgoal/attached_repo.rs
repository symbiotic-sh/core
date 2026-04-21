//! AttachedRepo sub-goal backend (T130 §07).
//!
//! Wires `UnblockKey::AttachedRepo { repo_id, branch_hint, requires_approval }`
//! into T126's `mirror_push_with_approval` + `ApprovalGate` + `GitPushSession`
//! per design §4.1:
//!
//! ```text
//! AttachedRepo { repo_id, branch_hint, requires_approval } →
//!     manifest = registry.load(repo_id)
//!     assert manifest.agent_scopes.push_external
//!     session = GitPushSession::open(repo_id, branch_hint)
//!     sandbox = runner.spawn(
//!         prompt = seed_from_parent + group_answers,
//!         tools = [file_edit, git_push_attached(session), ask_user_group],
//!     )
//!     // On completion: mirror_push_with_approval gates the push via Matrix approval
//! ```
//!
//! Per the chunk spec this module **composes** T126 primitives; it does NOT
//! extend them. `mirror_push_with_approval`, `ApprovalGate`, and
//! `GitPushSession` (under `credential-gateway::with_git_push_session`) are
//! treated as frozen.
//!
//! # Flow
//!
//! 1. Look up the [`RepoManifest`] from the registry by `repo_id`. Missing or
//!    detached manifests immediately fail with [`AttachedRepoError::ManifestNotFound`].
//! 2. Verify `agent_scopes.push_external` is granted; otherwise emit a
//!    scope-denied failure ([`AttachedRepoError::ScopeDenied`]) without ever
//!    opening a session, so no credential or git surface is touched.
//! 3. (Production wire-in) Open a [`GitPushSession`] bound to `branch_hint`;
//!    spawn the sandboxed runner with the `git_push_attached(session)` tool;
//!    when the runner completes, hand off to `mirror_push_with_approval`.
//! 4. The `ApprovalGate` state machine then drives a Matrix approval request
//!    via the daemon's existing `MatrixPoster` wiring (operator approves or
//!    denies via the matrix room or the app).
//! 5. Terminal outcomes:
//!     - **Approved**: push proceeded;
//!       [`AttachedRepoOutcome::Success`] with the pushed branch ref.
//!     - **Denied**: [`AttachedRepoOutcome::Partial`] with the operator's
//!       reason; work is preserved in the per-session isolated ref but never
//!       published.
//!     - **Expired** / scope-denied / missing-manifest: [`AttachedRepoOutcome::Failed`]
//!       with the typed reason.
//! 6. Cleanup: the session goes out of scope on every terminal path; the
//!    per-session credential tempfiles are unlinked via the session Drop.
//!
//! # Three-channel merge-back
//!
//! Stubbed identically to §05/§06: this module ships the **event-channel**
//! payload (the dispatcher renders `goal.subgoal.completed | failed`). The
//! Archive note + thread pill (design §5.1) are §08's job. Denied pushes
//! carry a `denial_note` field on the Partial outcome so §08 can surface the
//! rationale without re-running the gate.

use async_trait::async_trait;
use thiserror::Error;

use symbiotic_control_plane::RepoManifest;

use crate::repo_registry::SharedRepoRegistry;

/// Inputs handed from the dispatcher to a backend on each
/// `goal.unblocked { unblock_key: AttachedRepo }` event.
///
/// The shape stays narrow — the dispatcher already owns budget reservation,
/// child goal creation, and event emission. The backend's only job is to
/// drive the (per-session sandboxed runner + approval-gated push) pipeline
/// and report the terminal outcome.
#[derive(Debug, Clone)]
pub struct AttachedRepoRequest {
    /// The sub-goal slug the dispatcher minted (matches the persisted child
    /// `GoalProcess`). Used as the per-session ref slug and as the agent id
    /// seed.
    pub sub_goal_id: String,
    /// `repo_id` from `UnblockKey::AttachedRepo` — the canonical
    /// `repo:{slug}` id used by the [`RepoRegistry`].
    pub repo_id: String,
    /// The branch hint from `UnblockKey::AttachedRepo`. Becomes the per-session
    /// isolated ref the runner pushes to.
    pub branch_hint: String,
    /// `requires_approval` from `UnblockKey::AttachedRepo`. Note: this is
    /// **advisory** — the actual approval gate is driven by the manifest's
    /// `agent_scopes.requires_operator_approval_for` list. Per design §8.2,
    /// the autonomy dial governs *answers*, not *capability elevation*.
    pub requires_approval_hint: bool,
    /// Operator answers from the resolved group (question_index → text),
    /// folded into the runner's seed prompt.
    pub operator_answers: Vec<(usize, String)>,
    /// The agent role attached to the sub-goal (for the scope check).
    /// Reserved for future per-role gating; today the manifest-level
    /// `push_external` flag is the only gate.
    pub agent_role: String,
}

impl AttachedRepoRequest {
    pub fn new(
        sub_goal_id: impl Into<String>,
        repo_id: impl Into<String>,
        branch_hint: impl Into<String>,
        requires_approval_hint: bool,
    ) -> Self {
        Self {
            sub_goal_id: sub_goal_id.into(),
            repo_id: repo_id.into(),
            branch_hint: branch_hint.into(),
            requires_approval_hint,
            operator_answers: Vec::new(),
            agent_role: "default".to_string(),
        }
    }

    pub fn with_answers(mut self, answers: Vec<(usize, String)>) -> Self {
        self.operator_answers = answers;
        self
    }

    pub fn with_agent_role(mut self, role: impl Into<String>) -> Self {
        self.agent_role = role.into();
        self
    }
}

/// Terminal outcome of a single AttachedRepo backend run.
///
/// Matches the design §5.1 three-outcome shape — the dispatcher uses this to
/// build the event-channel payload (and §08 will use it to write the
/// Archive note + thread pill).
#[derive(Debug, Clone, PartialEq)]
pub enum AttachedRepoOutcome {
    /// The push landed. `branch_ref` names the pushed branch on the source
    /// remote (typically `refs/heads/{branch_hint}`).
    Success {
        sub_goal_id: String,
        summary: String,
        branch_ref: String,
    },
    /// The operator denied the push. Work exists in the per-session isolated
    /// ref but was never published. `denial_note` carries the operator's
    /// reason (from `ApprovalGate::deny`) for §08 to surface in the Archive
    /// note + thread pill (TODO §08).
    Partial {
        sub_goal_id: String,
        summary: String,
        denial_note: String,
    },
}

/// Errors returned by [`AttachedRepoBackend::execute`].
///
/// The dispatcher renders these as `goal.subgoal.failed { outcome, reason }`
/// events. `ScopeDenied` is the canonical short-circuit when the manifest is
/// missing the `push_external` scope — no session is opened, no credential
/// touched.
#[derive(Debug, Error)]
pub enum AttachedRepoError {
    #[error("scope-denied: push_external not in agent_scopes for repo {repo_id}")]
    ScopeDenied { repo_id: String },
    #[error("repo manifest not found in registry: {repo_id}")]
    ManifestNotFound { repo_id: String },
    #[error("repo manifest is not Active: {repo_id} (state {state:?})")]
    ManifestNotActive {
        repo_id: String,
        state: symbiotic_control_plane::RepoState,
    },
    #[error("ApprovalGate timeout for ticket {ticket_id}")]
    ApprovalTimeout { ticket_id: String },
    #[error("session open failed: {0}")]
    SessionOpenFailed(String),
    #[error("runner authoring failed: {0}")]
    AuthoringFailed(String),
    #[error("push failed: {0}")]
    PushFailed(String),
    #[error("attached_repo backend not ready: {reason}")]
    BackendNotReady { reason: String },
}

/// The seam between the dispatcher and T126's mirror_push_with_approval +
/// ApprovalGate + GitPushSession composition.
///
/// One async call: take a request, return a terminal [`AttachedRepoOutcome`]
/// or a typed [`AttachedRepoError`]. Implementations:
///
/// - [`MirrorPushAttachedRepoBackend`] — the production composition over
///   T126 primitives (manifest-aware scope check + GitPushSession runner +
///   `mirror_push_with_approval` + `ApprovalGate`).
/// - [`ManifestScopeAttachedRepoBackend`] — a slim partial backend that only
///   performs the scope/manifest checks; useful when the runner-spawn +
///   approval-gate composition isn't reachable in a given context (e.g. the
///   dispatcher is constructed before `MatrixPoster` / `PushProvider` are
///   threaded through). Returns [`AttachedRepoError::BackendNotReady`] for
///   the spawn step but exercises the full scope-denied / manifest-not-found
///   error matrix.
#[async_trait]
pub trait AttachedRepoBackend: Send + Sync {
    async fn execute(
        &self,
        request: AttachedRepoRequest,
    ) -> Result<AttachedRepoOutcome, AttachedRepoError>;
}

/// Default fallback backend used when the dispatcher is constructed without
/// an explicit AttachedRepo backend. Emits [`AttachedRepoError::BackendNotReady`]
/// with a stable reason string. The dispatcher renders this as a
/// `goal.subgoal.failed { outcome: BackendNotReady, reason }` event.
#[derive(Debug, Clone, Default)]
pub struct NotReadyAttachedRepoBackend {
    reason: String,
}

impl NotReadyAttachedRepoBackend {
    /// Default reason — the production wire-in (T126 + MatrixPoster +
    /// PushProvider) is plumbed via `Dispatcher::new_with_attached_repo`.
    pub const DEFAULT_REASON: &'static str =
        "AttachedRepo backend not wired: pass a MirrorPushAttachedRepoBackend \
         via Dispatcher::new_with_attached_repo";

    pub fn new() -> Self {
        Self {
            reason: Self::DEFAULT_REASON.to_string(),
        }
    }

    pub fn with_reason(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[async_trait]
impl AttachedRepoBackend for NotReadyAttachedRepoBackend {
    async fn execute(
        &self,
        _request: AttachedRepoRequest,
    ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
        Err(AttachedRepoError::BackendNotReady {
            reason: self.reason.clone(),
        })
    }
}

/// Manifest-aware partial backend.
///
/// Performs the chunk's hard scope/manifest checks against a
/// [`SharedRepoRegistry`], then emits [`AttachedRepoError::BackendNotReady`]
/// for the runner-spawn + approval-gate composition. This is the backend the
/// daemon uses today (until `MirrorPushAttachedRepoBackend` is constructible
/// at the wiring site — `MatrixPoster`, `PushProvider`, `GoalScopedVault`,
/// and operator-room id all need to be in scope, which is a wiring concern
/// beyond this chunk).
///
/// Critically, this backend still enforces the security guarantee the chunk
/// spec requires: a sub-goal whose `RepoManifest` does not grant
/// `agent_scopes.push_external` short-circuits to scope-denied **before** any
/// session is opened. It also enforces the manifest-not-found and
/// manifest-not-Active cases.
pub struct ManifestScopeAttachedRepoBackend {
    registry: SharedRepoRegistry,
    /// Reason string for the `BackendNotReady` post-check error, surfaced to
    /// observers so they know "scope passed, but the runner-spawn wiring
    /// isn't here yet".
    backend_reason: String,
}

impl ManifestScopeAttachedRepoBackend {
    pub fn new(registry: SharedRepoRegistry) -> Self {
        Self {
            registry,
            backend_reason: NotReadyAttachedRepoBackend::DEFAULT_REASON.to_string(),
        }
    }

    pub fn with_backend_reason(mut self, reason: impl Into<String>) -> Self {
        self.backend_reason = reason.into();
        self
    }

    /// Look up the manifest + run the scope checks. Public for reuse by the
    /// production [`MirrorPushAttachedRepoBackend`] (so both backends share
    /// the same gate semantics).
    pub async fn check_scope(&self, repo_id: &str) -> Result<RepoManifest, AttachedRepoError> {
        let registry = self.registry.lock().await;
        let manifest = registry
            .get(repo_id)
            .ok_or_else(|| AttachedRepoError::ManifestNotFound {
                repo_id: repo_id.to_string(),
            })?
            .clone();
        if manifest.state != symbiotic_control_plane::RepoState::Active {
            return Err(AttachedRepoError::ManifestNotActive {
                repo_id: repo_id.to_string(),
                state: manifest.state,
            });
        }
        if !manifest.agent_scopes.push_external {
            return Err(AttachedRepoError::ScopeDenied {
                repo_id: repo_id.to_string(),
            });
        }
        Ok(manifest)
    }
}

#[async_trait]
impl AttachedRepoBackend for ManifestScopeAttachedRepoBackend {
    async fn execute(
        &self,
        request: AttachedRepoRequest,
    ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
        // Scope check — fails closed before any session is touched.
        let _manifest = self.check_scope(&request.repo_id).await?;
        // Wiring for the runner-spawn + approval-gate composition isn't in
        // scope for this backend. The dispatcher renders this as a
        // `BackendNotReady` failure — distinct from `ScopeDenied`.
        Err(AttachedRepoError::BackendNotReady {
            reason: self.backend_reason.clone(),
        })
    }
}

/// Production backend: composes T126's
/// [`mirror_push_with_approval`](crate::repo_mirror::mirror_push_with_approval)
/// with the registry-backed scope check.
///
/// This is the backend the daemon plugs in once the wiring site has all of:
///
/// - a [`SharedRepoRegistry`] (to look up the manifest),
/// - a [`credential_gateway::GoalScopedVault`] (to materialize the push
///   credential via [`credential_gateway::with_git_push_session`] under the
///   hood of `mirror_push_once`),
/// - an [`crate::approval_gate::ApprovalGate`] (the state machine T126's
///   wrapper drives),
/// - a [`crate::matrix_poster::MatrixPoster`] (to emit the operator approval
///   request),
/// - a [`crate::push::PushProvider`] (to fan out the push notification),
/// - and an operator room id to address the Matrix approval message to.
///
/// All of those are owned by `main.rs` / `SymbioticDaemon` today; threading
/// them into the dispatcher is a one-line construction change at the wiring
/// site (mirrors `Dispatcher::new_with_exploratory` per §06).
///
/// # Sandboxed runner
///
/// The chunk spec calls for a sandboxed runner stage between scope-check and
/// `mirror_push_with_approval`. The runner shape (a sub-goal authoring agent
/// with the `git_push_attached(session)` tool registered) parallels the §06
/// Exploratory backend's missing T116 sub-goal authoring runner — when that
/// role lands, the AttachedRepo runner spawn becomes a one-line addition
/// here. For this chunk, the backend exposes a hook ([`Self::with_runner`])
/// so a future wiring site can plug it in without changing the trait.
pub struct MirrorPushAttachedRepoBackend {
    scope_gate: ManifestScopeAttachedRepoBackend,
    push_runner: std::sync::Arc<dyn AttachedRepoPushRunner>,
}

impl MirrorPushAttachedRepoBackend {
    pub fn new(
        registry: SharedRepoRegistry,
        push_runner: std::sync::Arc<dyn AttachedRepoPushRunner>,
    ) -> Self {
        Self {
            scope_gate: ManifestScopeAttachedRepoBackend::new(registry),
            push_runner,
        }
    }

    /// Replace the runner — useful for tests that want to drive specific
    /// outcomes through the production composition.
    pub fn with_runner(mut self, push_runner: std::sync::Arc<dyn AttachedRepoPushRunner>) -> Self {
        self.push_runner = push_runner;
        self
    }
}

#[async_trait]
impl AttachedRepoBackend for MirrorPushAttachedRepoBackend {
    async fn execute(
        &self,
        request: AttachedRepoRequest,
    ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
        let manifest = self.scope_gate.check_scope(&request.repo_id).await?;
        self.push_runner.run_push(&manifest, request).await
    }
}

/// The push-runner half of the AttachedRepo composition.
///
/// Splitting this out from [`AttachedRepoBackend`] keeps the registry-backed
/// scope check (which is universal) separate from the
/// `mirror_push_with_approval` invocation (which depends on a long list of
/// wiring-site collaborators). The production runner composes those pieces;
/// tests substitute a mock that returns the desired terminal state directly.
#[async_trait]
pub trait AttachedRepoPushRunner: Send + Sync {
    /// Run the sandboxed-runner + `mirror_push_with_approval` pipeline for a
    /// scope-checked manifest. Implementations must clean up any per-session
    /// resources on every terminal path.
    async fn run_push(
        &self,
        manifest: &RepoManifest,
        request: AttachedRepoRequest,
    ) -> Result<AttachedRepoOutcome, AttachedRepoError>;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use symbiotic_agents::FindingSeverity;
    use symbiotic_control_plane::{
        CredentialScope, MirrorDirection, RepoAgentScopes, RepoCheckoutPolicy,
        RepoCredentialBinding, RepoHooks, RepoManifest, RepoMetadata, RepoMirrorPolicy,
        RepoProvider, RepoSource, RepoState,
    };
    use symbiotic_trust::AgentTrustLevel;
    use tokio::sync::Mutex;

    use crate::repo_registry::RepoRegistry;

    fn _silence_unused() -> FindingSeverity {
        // Keep the symbiotic_agents crate exposed in case future test expansion
        // needs it; the manifest builder doesn't need it directly.
        FindingSeverity::Low
    }

    fn fixture_manifest(repo_id: &str, push_external: bool, state: RepoState) -> RepoManifest {
        RepoManifest {
            id: repo_id.to_string(),
            project_id: "project:test".to_string(),
            slug: "fixture".to_string(),
            title: "Fixture".to_string(),
            state,
            repo_role: symbiotic_control_plane::RepoRole::Source,
            source: RepoSource {
                url: "file:///tmp/source.git".to_string(),
                provider: RepoProvider::Local,
                default_branch: "main".to_string(),
                protected_branches: Vec::new(),
                pinned_head: None,
            },
            credential: RepoCredentialBinding {
                id: "cred:test".to_string(),
                scope: CredentialScope::Push,
                trust_floor: AgentTrustLevel::ReadOnly,
            },
            mirror: RepoMirrorPolicy {
                internal_bare_path: PathBuf::from("/tmp/internal.git"),
                direction: MirrorDirection::Bidirectional,
                sync_interval_secs: 300,
                last_pulled_at: None,
                last_pushed_at: None,
            },
            checkout: RepoCheckoutPolicy {
                worktree_root: PathBuf::from("/tmp/worktrees"),
                agent_branch_prefix: "agent/".to_string(),
                max_concurrent_worktrees: 1,
                cleanup_on_goal_close: true,
            },
            agent_scopes: RepoAgentScopes {
                read: Vec::new(),
                write: Vec::new(),
                push_external,
                requires_operator_approval_for: vec!["push_external".to_string()],
            },
            hooks: RepoHooks {
                on_attach: None,
                on_drift_detected: None,
                on_detach: None,
            },
            indexing: None,
            metadata: RepoMetadata {
                attached_at: "2026-04-18T00:00:00Z".to_string(),
                attached_by: "test".to_string(),
                notes: String::new(),
            },
            archeology_policy: None,
            body_markdown: String::new(),
        }
    }

    fn fixture_registry(manifests: Vec<RepoManifest>) -> SharedRepoRegistry {
        let mut registry = RepoRegistry::new();
        for m in manifests {
            registry.on_repo_attached(m).expect("attach fixture");
        }
        Arc::new(Mutex::new(registry))
    }

    #[tokio::test]
    async fn not_ready_backend_returns_backend_not_ready() {
        let backend = NotReadyAttachedRepoBackend::new();
        let request = AttachedRepoRequest::new("sg-x", "repo:test", "main", true);
        let err = backend.execute(request).await.unwrap_err();
        match err {
            AttachedRepoError::BackendNotReady { reason } => {
                assert!(
                    reason.contains("MirrorPushAttachedRepoBackend"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected BackendNotReady, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manifest_scope_backend_rejects_when_push_external_disabled() {
        // Hard contract: even with a present + Active manifest, push_external=false
        // must short-circuit BEFORE any session-open / runner-spawn step.
        let manifest = fixture_manifest("repo:scope-denied", false, RepoState::Active);
        let registry = fixture_registry(vec![manifest]);
        let backend = ManifestScopeAttachedRepoBackend::new(registry);

        let request = AttachedRepoRequest::new("sg-x", "repo:scope-denied", "main", true);
        let err = backend.execute(request).await.unwrap_err();
        match err {
            AttachedRepoError::ScopeDenied { repo_id } => {
                assert_eq!(repo_id, "repo:scope-denied");
            }
            other => panic!("expected ScopeDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manifest_scope_backend_rejects_unknown_repo() {
        let registry = fixture_registry(vec![]);
        let backend = ManifestScopeAttachedRepoBackend::new(registry);

        let request = AttachedRepoRequest::new("sg-x", "repo:missing", "main", true);
        let err = backend.execute(request).await.unwrap_err();
        match err {
            AttachedRepoError::ManifestNotFound { repo_id } => {
                assert_eq!(repo_id, "repo:missing");
            }
            other => panic!("expected ManifestNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manifest_scope_backend_rejects_detached_repo() {
        let manifest = fixture_manifest("repo:detached", true, RepoState::Detached);
        let registry = fixture_registry(vec![manifest]);
        let backend = ManifestScopeAttachedRepoBackend::new(registry);

        let request = AttachedRepoRequest::new("sg-x", "repo:detached", "main", true);
        let err = backend.execute(request).await.unwrap_err();
        match err {
            AttachedRepoError::ManifestNotActive { repo_id, state } => {
                assert_eq!(repo_id, "repo:detached");
                assert_eq!(state, RepoState::Detached);
            }
            other => panic!("expected ManifestNotActive, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn manifest_scope_backend_passes_scope_then_returns_backend_not_ready() {
        // When push_external IS granted and the manifest IS Active, the slim
        // backend must surface BackendNotReady (not Success) — it doesn't
        // know how to drive the runner + approval-gate composition.
        let manifest = fixture_manifest("repo:ok", true, RepoState::Active);
        let registry = fixture_registry(vec![manifest]);
        let backend = ManifestScopeAttachedRepoBackend::new(registry);

        let request = AttachedRepoRequest::new("sg-x", "repo:ok", "main", true);
        let err = backend.execute(request).await.unwrap_err();
        match err {
            AttachedRepoError::BackendNotReady { reason: _ } => {}
            other => panic!("expected BackendNotReady, got {other:?}"),
        }
    }

    /// Test runner that returns a fixed terminal state. Mirrors what the
    /// production runner does once the operator has approved/denied via
    /// matrix.
    struct StubPushRunner {
        outcome: AttachedRepoOutcome,
    }

    #[async_trait]
    impl AttachedRepoPushRunner for StubPushRunner {
        async fn run_push(
            &self,
            _manifest: &RepoManifest,
            _request: AttachedRepoRequest,
        ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
            Ok(self.outcome.clone())
        }
    }

    /// Test runner that returns a fixed error.
    struct FailingPushRunner {
        err: fn() -> AttachedRepoError,
    }

    #[async_trait]
    impl AttachedRepoPushRunner for FailingPushRunner {
        async fn run_push(
            &self,
            _manifest: &RepoManifest,
            _request: AttachedRepoRequest,
        ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
            Err((self.err)())
        }
    }

    #[tokio::test]
    async fn mirror_push_backend_passes_scope_then_invokes_runner_success() {
        let manifest = fixture_manifest("repo:ok", true, RepoState::Active);
        let registry = fixture_registry(vec![manifest]);
        let runner = Arc::new(StubPushRunner {
            outcome: AttachedRepoOutcome::Success {
                sub_goal_id: "sg-x".to_string(),
                summary: "approved + pushed".to_string(),
                branch_ref: "refs/heads/main".to_string(),
            },
        });
        let backend = MirrorPushAttachedRepoBackend::new(registry, runner);

        let request = AttachedRepoRequest::new("sg-x", "repo:ok", "main", true);
        let outcome = backend.execute(request).await.expect("backend ok");
        match outcome {
            AttachedRepoOutcome::Success {
                sub_goal_id,
                summary,
                branch_ref,
            } => {
                assert_eq!(sub_goal_id, "sg-x");
                assert_eq!(summary, "approved + pushed");
                assert_eq!(branch_ref, "refs/heads/main");
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mirror_push_backend_short_circuits_on_scope_denied_without_invoking_runner() {
        // Hard security contract: scope-denied MUST short-circuit before the
        // runner is reached. Otherwise the runner-spawn could leak per-session
        // git surface even when push_external is denied.
        let manifest = fixture_manifest("repo:denied", false, RepoState::Active);
        let registry = fixture_registry(vec![manifest]);
        let runner_called: Arc<std::sync::Mutex<bool>> = Arc::new(std::sync::Mutex::new(false));

        struct WatchRunner {
            called: Arc<std::sync::Mutex<bool>>,
        }
        #[async_trait]
        impl AttachedRepoPushRunner for WatchRunner {
            async fn run_push(
                &self,
                _manifest: &RepoManifest,
                _request: AttachedRepoRequest,
            ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
                *self.called.lock().unwrap() = true;
                Err(AttachedRepoError::PushFailed("should not be called".into()))
            }
        }

        let runner = Arc::new(WatchRunner {
            called: Arc::clone(&runner_called),
        });
        let backend = MirrorPushAttachedRepoBackend::new(registry, runner);

        let request = AttachedRepoRequest::new("sg-x", "repo:denied", "main", true);
        let err = backend.execute(request).await.unwrap_err();
        assert!(matches!(err, AttachedRepoError::ScopeDenied { .. }));
        assert!(
            !*runner_called.lock().unwrap(),
            "runner must NOT be invoked when scope is denied"
        );
    }

    #[tokio::test]
    async fn mirror_push_backend_partial_outcome_round_trips() {
        let manifest = fixture_manifest("repo:ok", true, RepoState::Active);
        let registry = fixture_registry(vec![manifest]);
        let runner = Arc::new(StubPushRunner {
            outcome: AttachedRepoOutcome::Partial {
                sub_goal_id: "sg-x".to_string(),
                summary: "denied".to_string(),
                denial_note: "operator denied: too risky".to_string(),
            },
        });
        let backend = MirrorPushAttachedRepoBackend::new(registry, runner);

        let request = AttachedRepoRequest::new("sg-x", "repo:ok", "main", true);
        let outcome = backend.execute(request).await.expect("backend ok");
        match outcome {
            AttachedRepoOutcome::Partial { denial_note, .. } => {
                assert!(denial_note.contains("too risky"));
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mirror_push_backend_runner_timeout_propagates() {
        let manifest = fixture_manifest("repo:ok", true, RepoState::Active);
        let registry = fixture_registry(vec![manifest]);
        let runner = Arc::new(FailingPushRunner {
            err: || AttachedRepoError::ApprovalTimeout {
                ticket_id: "t-123".to_string(),
            },
        });
        let backend = MirrorPushAttachedRepoBackend::new(registry, runner);

        let request = AttachedRepoRequest::new("sg-x", "repo:ok", "main", true);
        let err = backend.execute(request).await.unwrap_err();
        match err {
            AttachedRepoError::ApprovalTimeout { ticket_id } => {
                assert_eq!(ticket_id, "t-123");
            }
            other => panic!("expected ApprovalTimeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_carries_operator_answers_through_to_backend() {
        let request = AttachedRepoRequest::new("sg-x", "repo:y", "main", true)
            .with_answers(vec![(0, "yes".into())])
            .with_agent_role("source-archeology");
        assert_eq!(request.sub_goal_id, "sg-x");
        assert_eq!(request.repo_id, "repo:y");
        assert_eq!(request.branch_hint, "main");
        assert!(request.requires_approval_hint);
        assert_eq!(request.operator_answers.len(), 1);
        assert_eq!(request.agent_role, "source-archeology");
    }
}
