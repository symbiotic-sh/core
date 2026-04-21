//! Exploratory sub-goal backend (T130 §06).
//!
//! Wires `UnblockKey::Exploratory { topic }` into the T116 internal git swarm
//! per design §4.1:
//!
//! ```text
//! Exploratory { topic } →
//!     repo = daemon.create_swarm_repo("subgoal-{sub_goal_id}")
//!     sandbox = runner.spawn(
//!         prompt = seed_from_parent + group_answers,
//!         tools = [file_edit, git_push_swarm, ask_user_group, request_review],
//!         repo_endpoint = repo.internal_url,
//!     )
//! ```
//!
//! On `request_review` + reviewer/CI approval the daemon merges to main, which
//! triggers the existing T116 distillery pipeline (post-merge linter +
//! knowledge extraction). When distillery completes the backend reports
//! `Verdict::Success` with the extracted artifacts as Archive note refs.
//!
//! # T116 readiness
//!
//! Audited at chunk-write time:
//!
//! | Primitive | Available? |
//! | --- | --- |
//! | `GitServerManager::create_repo` (bare-repo init) | yes (`symbiotic-git-swarm::server::GitServerManager::create_repo`) |
//! | `SwarmServer::rpc_create_repo` RPC entrypoint | yes |
//! | Sandboxed runner spawn (`build_*_job` + `VmManager`) | yes — but only for the **CI / Reviewer / Distillery** roles, which are keyed off an *existing* PR |
//! | Sub-goal authoring runner role (`tools = [file_edit, git_push_swarm, ask_user_group, request_review]`) | **not yet wired** — `git_push_swarm`, `ask_user_group`, `request_review` are not registered as runner tools, and there is no `build_subgoal_authoring_job` that seeds a fresh repo with a sub-goal prompt |
//! | `SwarmServer` accessible from `SymbioticDaemon` | **not yet wired** — `SwarmServer` is owned by `main.rs` and threaded through to `LlmGateway`; `SymbioticDaemon` has no `swarm_server()` accessor for the dispatcher to call |
//!
//! Two of the three bullets the chunk spec calls out (`create_swarm_repo` is
//! ready; the post-merge distillery pipeline is ready) are present, but the
//! sub-goal **authoring** spawn — the agent that writes the initial code,
//! pushes the feature branch, and emits `request_review` — is not yet
//! registered. Until both gaps close, the production backend cannot be
//! constructed.
//!
//! Per the §06 chunk spec's escape hatch:
//!
//! > If T116 primitives aren't ready yet, your `Exploratory` branch should
//! > fall back to a temporary stub that emits
//! > `goal.subgoal.failed { outcome: BackendNotReady, reason: "T116 primitives pending" }`
//!
//! This module ships:
//!
//! 1. The [`ExploratoryBackend`] trait — the seam the production wiring will
//!    plug into once the missing T116 primitives land. The trait shape is
//!    deliberately narrow (one async `execute` call returning a typed
//!    [`ExploratoryOutcome`]) so the dispatcher contract stays stable.
//! 2. [`NotReadyExploratoryBackend`] — the default backend used until the
//!    T116 sub-goal authoring role is wired. It performs no I/O and returns
//!    [`ExploratoryError::BackendNotReady`] naming the missing primitive in
//!    the reason string, which the dispatcher surfaces as a
//!    `goal.subgoal.failed` event with `outcome=BackendNotReady`.
//!
//! When the missing primitives land, the production wiring path is:
//!
//! 1. Add a `build_subgoal_authoring_job` helper next to the existing
//!    `build_ci_job` / `build_reviewer_job` in `swarm_server.rs`.
//! 2. Register the missing runner tools (`git_push_swarm`, `ask_user_group`,
//!    `request_review`) in `symbiotic-agent-runner`.
//! 3. Expose a `daemon.swarm_server()` accessor on `SymbioticDaemon`.
//! 4. Implement a `SwarmExploratoryBackend` struct in this module that
//!    composes those pieces; swap the `Arc<dyn ExploratoryBackend>` the
//!    dispatcher holds at construction time.
//!
//! Steps 1–3 are out of scope for this chunk — they touch the high-risk T116
//! crate per [CONTEXT.md §High-Risk Agent & Matrix Paths] and require their
//! own design review. Step 4 is a one-line wiring change once they land.
//!
//! # Three-channel merge-back
//!
//! Stubbed identically to §05's ResearchOnly path: this module describes
//! the *event-channel* outcome only. The Archive note write and thread
//! pill (design §5.1) are §08's job; the dispatcher emits the event channel
//! and §08 will add the rest when the production backend lands.

use async_trait::async_trait;
use thiserror::Error;

/// What a successful Exploratory run produces, in the shape the dispatcher
/// needs to emit `goal.subgoal.completed`.
///
/// Mirrors the design §5.2 event payload — `summary` is the one-line
/// human summary that ends up in both the event detail and the (deferred)
/// thread pill, `artifact_refs` is the list of Archive note paths the
/// distillery extracted post-merge (design §5.3 / §6 of the T116 README).
#[derive(Debug, Clone, PartialEq)]
pub struct ExploratoryOutcome {
    pub sub_goal_id: String,
    pub summary: String,
    /// Archive references (e.g. `archive://episodic/subgoals/<id>/result.md`).
    /// Empty when the production backend hasn't landed yet.
    pub artifact_refs: Vec<String>,
}

/// Errors returned by [`ExploratoryBackend::execute`].
///
/// `BackendNotReady` is the canonical fallback signal when the T116 sub-goal
/// authoring runner role hasn't been wired. The dispatcher renders it as a
/// `goal.subgoal.failed { outcome: BackendNotReady, reason: <missing
/// primitive> }` event so observers (UI, parent goal, audit log) can tell
/// "the backend hasn't shipped" apart from "the backend tried and failed".
#[derive(Debug, Error)]
pub enum ExploratoryError {
    #[error("exploratory backend not ready: {reason}")]
    BackendNotReady { reason: String },
    #[error("swarm repo creation failed: {0}")]
    SwarmRepoCreation(#[source] anyhow::Error),
    #[error("sandbox spawn failed: {0}")]
    SandboxSpawn(#[source] anyhow::Error),
    #[error("sub-goal authoring agent reported failure: {0}")]
    AuthoringFailed(String),
    #[error("post-merge distillery rejected the bundle: {0}")]
    DistilleryRejected(String),
}

/// Inputs handed from the dispatcher to a backend on each
/// `goal.unblocked { unblock_key: Exploratory }` event.
///
/// The shape stays narrow — the dispatcher already owns budget reservation,
/// child goal creation, and event emission. The backend's only job is to
/// drive the swarm pipeline and report the terminal outcome.
#[derive(Debug, Clone)]
pub struct ExploratoryRequest {
    /// The sub-goal slug the dispatcher minted (matches the persisted child
    /// `GoalProcess`). Used both as the swarm repo id and as the
    /// authoring-agent id seed.
    pub sub_goal_id: String,
    /// The topic carried by `UnblockKey::Exploratory { topic }`. Becomes
    /// part of the sub-goal authoring agent's seed prompt.
    pub topic: String,
    /// Operator answers from the resolved group (question_index → text),
    /// folded into the seed prompt as parent context.
    pub operator_answers: Vec<(usize, String)>,
}

impl ExploratoryRequest {
    pub fn new(sub_goal_id: impl Into<String>, topic: impl Into<String>) -> Self {
        Self {
            sub_goal_id: sub_goal_id.into(),
            topic: topic.into(),
            operator_answers: Vec::new(),
        }
    }

    pub fn with_answers(mut self, answers: Vec<(usize, String)>) -> Self {
        self.operator_answers = answers;
        self
    }
}

/// The seam between the dispatcher and the T116 swarm pipeline.
///
/// One async call: take a request, return a terminal outcome (or a typed
/// error). Implementations:
///
/// - [`NotReadyExploratoryBackend`] — ships in this chunk; surfaces
///   `BackendNotReady` until the T116 authoring role lands.
/// - `SwarmExploratoryBackend` (future) — composes
///   `daemon.create_swarm_repo` + the (not-yet-wired) sub-goal authoring
///   sandbox + the existing T116 PR / distillery pipeline.
#[async_trait]
pub trait ExploratoryBackend: Send + Sync {
    async fn execute(
        &self,
        request: ExploratoryRequest,
    ) -> Result<ExploratoryOutcome, ExploratoryError>;
}

/// Default fallback backend used until the T116 sub-goal authoring runner
/// role is wired (see module docs for the audited gap list).
///
/// All calls return [`ExploratoryError::BackendNotReady`] with a stable
/// reason string. The dispatcher surfaces this as a
/// `goal.subgoal.failed { outcome: BackendNotReady, reason }` event without
/// ever creating a child goal or reserving any swarm-side resources.
#[derive(Debug, Clone, Default)]
pub struct NotReadyExploratoryBackend {
    reason: String,
}

impl NotReadyExploratoryBackend {
    /// Default reason — names the specific T116 primitives that are still
    /// missing. Audited against `swarm_server.rs` + `symbiotic-agent-runner`
    /// at chunk-write time; update when a wiring step closes a gap.
    pub const DEFAULT_REASON: &'static str =
        "T116 primitives pending: sub-goal authoring runner role \
         (git_push_swarm + ask_user_group + request_review tools) not yet wired, \
         and SwarmServer is not yet exposed on SymbioticDaemon";

    pub fn new() -> Self {
        Self {
            reason: Self::DEFAULT_REASON.to_string(),
        }
    }

    /// Override the fallback reason — useful for tests that want to assert
    /// on a specific message, and for future callers that want to narrow
    /// the explanation as wiring lands incrementally.
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
impl ExploratoryBackend for NotReadyExploratoryBackend {
    async fn execute(
        &self,
        _request: ExploratoryRequest,
    ) -> Result<ExploratoryOutcome, ExploratoryError> {
        Err(ExploratoryError::BackendNotReady {
            reason: self.reason.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn not_ready_backend_returns_backend_not_ready_with_default_reason() {
        let backend = NotReadyExploratoryBackend::new();
        let request = ExploratoryRequest::new("sg-x", "frontend-framework");
        let err = backend.execute(request).await.unwrap_err();
        match err {
            ExploratoryError::BackendNotReady { reason } => {
                assert!(
                    reason.contains("T116 primitives pending"),
                    "unexpected reason: {reason}"
                );
                assert!(
                    reason.contains("sub-goal authoring runner role"),
                    "expected reason to name the missing role: {reason}"
                );
            }
            other => panic!("expected BackendNotReady, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_ready_backend_uses_custom_reason_when_constructed_with_one() {
        let backend = NotReadyExploratoryBackend::with_reason("test-only stub");
        assert_eq!(backend.reason(), "test-only stub");
        let err = backend
            .execute(ExploratoryRequest::new("sg-x", "topic"))
            .await
            .unwrap_err();
        match err {
            ExploratoryError::BackendNotReady { reason } => {
                assert_eq!(reason, "test-only stub");
            }
            other => panic!("expected BackendNotReady, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_carries_operator_answers_through_to_backend() {
        // Builder shape sanity — the seam needs to round-trip operator
        // context so the production backend can fold it into the seed
        // prompt.
        let request = ExploratoryRequest::new("sg-x", "frontend")
            .with_answers(vec![(0, "Vue".into()), (1, "Yes".into())]);
        assert_eq!(request.sub_goal_id, "sg-x");
        assert_eq!(request.topic, "frontend");
        assert_eq!(request.operator_answers.len(), 2);
        assert_eq!(request.operator_answers[0], (0, "Vue".to_string()));
    }

    /// A backend that succeeds — used by the dispatcher tests to prove the
    /// production wire-in pattern works without depending on the (not-yet-
    /// shipped) `SwarmExploratoryBackend`.
    pub(super) struct StubSuccessBackend {
        pub summary: String,
        pub artifact_refs: Vec<String>,
    }

    #[async_trait]
    impl ExploratoryBackend for StubSuccessBackend {
        async fn execute(
            &self,
            request: ExploratoryRequest,
        ) -> Result<ExploratoryOutcome, ExploratoryError> {
            Ok(ExploratoryOutcome {
                sub_goal_id: request.sub_goal_id,
                summary: self.summary.clone(),
                artifact_refs: self.artifact_refs.clone(),
            })
        }
    }

    #[tokio::test]
    async fn stub_success_backend_round_trips_request_to_outcome() {
        let backend = StubSuccessBackend {
            summary: "exploratory complete".to_string(),
            artifact_refs: vec!["archive://episodic/subgoals/sg-x/result.md".to_string()],
        };
        let outcome = backend
            .execute(ExploratoryRequest::new("sg-x", "topic"))
            .await
            .expect("stub succeeds");
        assert_eq!(outcome.sub_goal_id, "sg-x");
        assert_eq!(outcome.summary, "exploratory complete");
        assert_eq!(outcome.artifact_refs.len(), 1);
    }
}
