//! End-to-end integration test for T130 §07 — Sub-Goal Dispatcher with the
//! `AttachedRepo` (T126 `mirror_push_with_approval` + `ApprovalGate`)
//! backend.
//!
//! This test covers the full flow a real wiring site exercises:
//!
//! 1. A [`QuestionGroup`] with `unblock_key = AttachedRepo { repo_id, branch_hint, requires_approval }`
//!    is registered on both the `QuestionResolver` and the `Dispatcher`.
//! 2. Operator answers come in via `submit_answer`; the resolver produces a
//!    `GroupResolutionOutcome::Unblocked { answers, .. }`.
//! 3. The caller builds an [`UnblockedContext`] from the outcome + the
//!    `room_id` it knows about and calls `Dispatcher::on_unblocked`.
//! 4. The dispatcher routes to the [`AttachedRepoBackend`] trait
//!    implementation it was constructed with via
//!    [`Dispatcher::new_with_attached_repo`].
//!
//! The four scenarios covered (per chunk spec §07):
//!
//! - **approved-push** — backend returns `AttachedRepoOutcome::Success` with a
//!   `branch_ref`; dispatcher emits `goal.subgoal.completed { outcome: Success, branch_ref }`.
//! - **denied-push** — backend returns `AttachedRepoOutcome::Partial { denial_note }`;
//!   dispatcher emits `goal.subgoal.completed { outcome: Partial, denial_note }`
//!   so §08's merge-back can surface the operator's rationale in the Archive
//!   note placeholder + thread pill.
//! - **timeout** — backend returns `AttachedRepoError::ApprovalTimeout`; dispatcher
//!   emits `goal.subgoal.failed` with `reason: "ApprovalGate timeout (ticket ...)"`.
//! - **scope-denied** — backend returns `AttachedRepoError::ScopeDenied`; dispatcher
//!   emits `goal.subgoal.failed { reason: "scope-denied: push_external not in
//!   agent_scopes for <repo_id>" }`. The backend short-circuits before any
//!   session is opened, satisfying the chunk's hard security constraint.
//!
//! In production, the real `MirrorPushAttachedRepoBackend` composes
//! `mirror_push_with_approval` (which drives the `ApprovalGate` state
//! machine, posts the matrix message via `MatrixPoster`, and pushes via
//! `GitPushSession`) — see `subgoal/attached_repo.rs` module docs for the
//! full surface. Substituting test backends here lets us exercise every
//! terminal state without standing up real git remotes / Matrix transports.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use symbiotic_control_plane::goals::GoalProcessManager;
use symbiotic_core::protocol::{ChatMessage, LlmClient};
use symbiotic_core::types::question_group::{
    AnnotatedQuestion, AnswerType, QuestionGroup, QuestionSeverity, ResolutionMode, UnblockKey,
};
use symbiotic_daemon::goal_pipeline::question_resolver::{
    GroupResolutionOutcome, QuestionAnswer, QuestionResolver,
};
use symbiotic_daemon::subgoal::{
    AttachedRepoBackend, AttachedRepoError, AttachedRepoOutcome, AttachedRepoRequest, Dispatcher,
    NotReadyAttachedRepoBackend, RecallSnippet, ResearchRecall, ResearcherAgent, SpawnBudget,
    UnblockedContext, Verdict,
};
use tempfile::TempDir;

const NOW_ISO: &str = "2026-04-18T10:42:00Z";
const NOW_UNIX: u64 = 1_713_437_320;

// ---------------------------------------------------------------------------
// Stubs
// ---------------------------------------------------------------------------

struct EmptyRecall;

#[async_trait]
impl ResearchRecall for EmptyRecall {
    async fn query(&self, _topic: &str, _top_k: usize) -> anyhow::Result<Vec<RecallSnippet>> {
        Ok(Vec::new())
    }
}

struct StubLlm;

#[async_trait]
impl LlmClient for StubLlm {
    async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> anyhow::Result<String> {
        Ok("unreached".to_string())
    }
}

/// Backend that returns a fixed terminal state. Captures the request the
/// dispatcher fed in so each test can assert that the routing carried the
/// repo_id, branch_hint, requires_approval flag, and operator answers
/// through the trait seam.
struct ScriptedAttachedRepoBackend {
    seen: Mutex<Option<AttachedRepoRequest>>,
    outcome: Result<AttachedRepoOutcome, AttachedRepoError>,
}

impl ScriptedAttachedRepoBackend {
    fn success(branch_ref: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            seen: Mutex::new(None),
            outcome: Ok(AttachedRepoOutcome::Success {
                sub_goal_id: String::new(), // overwritten in execute
                summary: summary.into(),
                branch_ref: branch_ref.into(),
            }),
        }
    }

    fn partial(denial_note: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            seen: Mutex::new(None),
            outcome: Ok(AttachedRepoOutcome::Partial {
                sub_goal_id: String::new(),
                summary: summary.into(),
                denial_note: denial_note.into(),
            }),
        }
    }

    fn timeout(ticket_id: impl Into<String>) -> Self {
        Self {
            seen: Mutex::new(None),
            outcome: Err(AttachedRepoError::ApprovalTimeout {
                ticket_id: ticket_id.into(),
            }),
        }
    }

    fn scope_denied(repo_id: impl Into<String>) -> Self {
        Self {
            seen: Mutex::new(None),
            outcome: Err(AttachedRepoError::ScopeDenied {
                repo_id: repo_id.into(),
            }),
        }
    }

    fn last_request(&self) -> Option<AttachedRepoRequest> {
        self.seen.lock().ok().and_then(|g| g.clone())
    }
}

#[async_trait]
impl AttachedRepoBackend for ScriptedAttachedRepoBackend {
    async fn execute(
        &self,
        request: AttachedRepoRequest,
    ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
        if let Ok(mut slot) = self.seen.lock() {
            *slot = Some(request.clone());
        }
        match &self.outcome {
            Ok(AttachedRepoOutcome::Success {
                summary,
                branch_ref,
                ..
            }) => Ok(AttachedRepoOutcome::Success {
                sub_goal_id: request.sub_goal_id,
                summary: summary.clone(),
                branch_ref: branch_ref.clone(),
            }),
            Ok(AttachedRepoOutcome::Partial {
                summary,
                denial_note,
                ..
            }) => Ok(AttachedRepoOutcome::Partial {
                sub_goal_id: request.sub_goal_id,
                summary: summary.clone(),
                denial_note: denial_note.clone(),
            }),
            Err(AttachedRepoError::ApprovalTimeout { ticket_id }) => {
                Err(AttachedRepoError::ApprovalTimeout {
                    ticket_id: ticket_id.clone(),
                })
            }
            Err(AttachedRepoError::ScopeDenied { repo_id }) => {
                Err(AttachedRepoError::ScopeDenied {
                    repo_id: repo_id.clone(),
                })
            }
            Err(other) => Err(AttachedRepoError::PushFailed(format!(
                "test backend reproducing: {other}"
            ))),
        }
    }
}

fn annotated(text: &str) -> AnnotatedQuestion {
    AnnotatedQuestion {
        text: text.to_string(),
        quick_replies: None,
        recommendation: Some("yes".to_string()),
        confidence: 0.85,
        severity: QuestionSeverity::Decision,
        expected_answer_type: AnswerType::FreeText,
        resolution_trail: None,
    }
}

fn fixture_researcher() -> Arc<ResearcherAgent> {
    let recall = Arc::new(EmptyRecall) as Arc<dyn ResearchRecall>;
    let llm = Arc::new(StubLlm) as Arc<dyn LlmClient>;
    Arc::new(ResearcherAgent::new(recall, llm))
}

fn make_resolver_and_unblocked_context(
    dispatcher: &Dispatcher,
    parent_goal_id: &str,
    group_id: &str,
    repo_id: &str,
    branch_hint: &str,
) -> (UnblockKey, std::collections::HashMap<usize, String>) {
    let mut resolver = QuestionResolver::new();
    let unblock_key = UnblockKey::AttachedRepo {
        repo_id: repo_id.to_string(),
        branch_hint: branch_hint.to_string(),
        requires_approval: true,
    };

    let group = QuestionGroup {
        group_id: group_id.to_string(),
        parent_goal_id: parent_goal_id.to_string(),
        unblock_key: unblock_key.clone(),
        questions: vec![
            annotated("Approve push to feature branch?"),
            annotated("Squash commits before push?"),
        ],
        created_at: NOW_ISO.to_string(),
        resolved_at: None,
        resolution_mode: ResolutionMode::AllRequired,
    };
    resolver.register(group);

    dispatcher.remember_group(group_id, unblock_key.clone());

    let _ = resolver
        .submit_answer(
            QuestionAnswer {
                group_id: group_id.into(),
                question_index: 0,
                answer: Some("yes".into()),
            },
            NOW_ISO,
        )
        .expect("first submit ok");
    let resolved = resolver
        .submit_answer(
            QuestionAnswer {
                group_id: group_id.into(),
                question_index: 1,
                answer: Some("yes".into()),
            },
            NOW_ISO,
        )
        .expect("second submit ok")
        .expect("group resolved");

    let answers = match resolved {
        GroupResolutionOutcome::Unblocked { answers, .. } => answers,
        other => panic!("expected Unblocked, got {other:?}"),
    };

    (unblock_key, answers)
}

fn build_dispatcher_with_backend(
    backend: Arc<dyn AttachedRepoBackend>,
) -> (Dispatcher, Arc<Mutex<GoalProcessManager>>, TempDir) {
    let tmp = TempDir::new().unwrap();
    let goal_mgr = Arc::new(Mutex::new(GoalProcessManager::new(
        tmp.path().to_path_buf(),
    )));
    let researcher = fixture_researcher();
    let disp = Dispatcher::new_with_attached_repo(
        SpawnBudget::default(),
        researcher,
        backend,
        Arc::clone(&goal_mgr),
    );
    (disp, goal_mgr, tmp)
}

// ---------------------------------------------------------------------------
// Tests — three terminal states + scope-denied (chunk spec §07)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approved_push_emits_completed_with_branch_ref() {
    // ── Arrange ──────────────────────────────────────────────────────────
    let backend = Arc::new(ScriptedAttachedRepoBackend::success(
        "refs/heads/feature/auth",
        "operator approved + push landed",
    ));
    let (dispatcher, goal_mgr, _tmp) =
        build_dispatcher_with_backend(Arc::clone(&backend) as Arc<dyn AttachedRepoBackend>);

    let parent_goal_id = "goal-build-saas";
    let group_id = "auth-phase";
    let repo_id = "repo:saas-api";
    let branch_hint = "feature/auth";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &dispatcher,
        parent_goal_id,
        group_id,
        repo_id,
        branch_hint,
    );

    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-build-saas".into()),
        room_id: "!thread-build-saas:example".into(),
        now: NOW_UNIX,
        answers,
    };

    // ── Act ──────────────────────────────────────────────────────────────
    let outcome = dispatcher
        .on_unblocked(ctx)
        .await
        .expect("dispatcher route succeeds");

    // ── Assert: verdict + child goal ─────────────────────────────────────
    assert_eq!(outcome.verdict, Verdict::Success);
    let child_slug = outcome.child_goal_slug.expect("child goal minted");
    assert!(child_slug.starts_with("sg-"));

    {
        let mgr = goal_mgr.lock().unwrap();
        let child = mgr.get(&child_slug).expect("child persisted");
        assert_eq!(child.parent_goal_id.as_deref(), Some(parent_goal_id));
        match &child.unblock_key {
            Some(UnblockKey::AttachedRepo {
                repo_id: rid,
                branch_hint: bh,
                ..
            }) => {
                assert_eq!(rid, repo_id);
                assert_eq!(bh, branch_hint);
            }
            other => panic!("expected AttachedRepo, got {other:?}"),
        }
    }

    // ── Assert: backend received the correct request ─────────────────────
    let seen = backend.last_request().expect("backend.execute was called");
    assert_eq!(seen.sub_goal_id, child_slug);
    assert_eq!(seen.repo_id, repo_id);
    assert_eq!(seen.branch_hint, branch_hint);
    assert!(seen.requires_approval_hint);
    assert_eq!(seen.operator_answers.len(), 2);

    // ── Assert: event channel ────────────────────────────────────────────
    assert_eq!(outcome.events.len(), 2);
    let actions: Vec<_> = outcome
        .events
        .iter()
        .map(|e| e.envelope.sym.a.clone().unwrap_or_default())
        .collect();
    assert!(
        actions.contains(&"goal.subgoal.spawned".to_string()),
        "actions: {actions:?}"
    );
    assert!(
        actions.contains(&"goal.subgoal.completed".to_string()),
        "actions: {actions:?}"
    );

    let completed = outcome
        .events
        .iter()
        .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.completed"))
        .expect("completion event");
    let detail = completed.envelope.sym.d.as_ref().unwrap();
    assert_eq!(
        detail.get("parent_goal_id").and_then(|v| v.as_str()),
        Some(parent_goal_id)
    );
    assert_eq!(
        detail.get("sub_goal_id").and_then(|v| v.as_str()),
        Some(child_slug.as_str())
    );
    assert_eq!(
        detail.get("outcome").and_then(|v| v.as_str()),
        Some("Success")
    );
    assert_eq!(
        detail.get("branch_ref").and_then(|v| v.as_str()),
        Some("refs/heads/feature/auth")
    );
    assert_eq!(
        detail.get("summary").and_then(|v| v.as_str()),
        Some("operator approved + push landed")
    );

    // §08 will surface the Archive note + thread pill via the merge-back;
    // for now the dispatcher does NOT yet emit a draft (this would change
    // when §08 lands).
    assert!(
        outcome.archive_note_draft.is_none(),
        "AttachedRepo archive_note_draft is deferred to §08; \
         see TODO in dispatcher.run_attached_repo"
    );
}

#[tokio::test]
async fn denied_push_emits_partial_with_denial_note() {
    // The operator denied the push — backend returns Partial with the
    // operator's rationale captured for §08 to surface in the Archive note
    // placeholder + thread pill.
    let backend = Arc::new(ScriptedAttachedRepoBackend::partial(
        "operator denied: feature flag rollout pending review",
        "push pending — work preserved in per-session ref",
    ));
    let (dispatcher, _goal_mgr, _tmp) =
        build_dispatcher_with_backend(Arc::clone(&backend) as Arc<dyn AttachedRepoBackend>);

    let parent_goal_id = "goal-build-saas";
    let group_id = "auth-phase";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &dispatcher,
        parent_goal_id,
        group_id,
        "repo:saas-api",
        "feature/auth",
    );

    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-build-saas".into()),
        room_id: "!thread-build-saas:example".into(),
        now: NOW_UNIX,
        answers,
    };

    let outcome = dispatcher.on_unblocked(ctx).await.expect("routes");

    // Per design §5.1: Partial is still a "completed" outcome from the
    // event-channel point of view — the parent's planner doesn't hang.
    assert_eq!(outcome.verdict, Verdict::Success);

    let completed = outcome
        .events
        .iter()
        .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.completed"))
        .expect("completion event present");
    let detail = completed.envelope.sym.d.as_ref().unwrap();
    assert_eq!(
        detail.get("outcome").and_then(|v| v.as_str()),
        Some("Partial")
    );
    let denial_note = detail
        .get("denial_note")
        .and_then(|v| v.as_str())
        .expect("denial_note populated");
    assert!(
        denial_note.contains("feature flag rollout pending review"),
        "denial_note must capture operator's rationale: {denial_note}"
    );
    // No branch_ref on Partial — work was never published.
    assert!(detail.get("branch_ref").is_none());
}

#[tokio::test]
async fn approval_timeout_emits_failed_with_typed_reason() {
    // The ApprovalGate ticket TTL elapsed before the operator
    // approved/denied — backend returns ApprovalTimeout, dispatcher emits
    // Failed verdict with the typed reason (so observers can distinguish
    // timeout from scope-denied / push-failed / etc.).
    let backend = Arc::new(ScriptedAttachedRepoBackend::timeout("ticket-abc-123"));
    let (dispatcher, _goal_mgr, _tmp) =
        build_dispatcher_with_backend(Arc::clone(&backend) as Arc<dyn AttachedRepoBackend>);

    let parent_goal_id = "goal-build-saas";
    let group_id = "auth-phase";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &dispatcher,
        parent_goal_id,
        group_id,
        "repo:saas-api",
        "feature/auth",
    );

    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-build-saas".into()),
        room_id: "!thread-build-saas:example".into(),
        now: NOW_UNIX,
        answers,
    };

    let outcome = dispatcher.on_unblocked(ctx).await.expect("routes");

    assert_eq!(outcome.verdict, Verdict::Failed);

    let failed = outcome
        .events
        .iter()
        .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.failed"))
        .expect("failure event present");
    let detail = failed.envelope.sym.d.as_ref().unwrap();
    let reason = detail
        .get("reason")
        .and_then(|v| v.as_str())
        .expect("reason populated");
    assert!(
        reason.contains("ApprovalGate timeout"),
        "expected typed timeout reason, got: {reason}"
    );
    assert!(
        reason.contains("ticket-abc-123"),
        "expected ticket id in reason, got: {reason}"
    );
}

#[tokio::test]
async fn scope_denied_emits_failed_immediately_with_typed_reason() {
    // The chunk spec's hard security contract: when the manifest's
    // agent_scopes.push_external is NOT granted, the backend short-circuits
    // before any session is opened (production: ManifestScopeAttachedRepoBackend
    // / MirrorPushAttachedRepoBackend; here: scripted to surface the same
    // typed error).
    let backend = Arc::new(ScriptedAttachedRepoBackend::scope_denied("repo:locked"));
    let (dispatcher, _goal_mgr, _tmp) =
        build_dispatcher_with_backend(Arc::clone(&backend) as Arc<dyn AttachedRepoBackend>);

    let parent_goal_id = "goal-build-saas";
    let group_id = "auth-phase";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &dispatcher,
        parent_goal_id,
        group_id,
        "repo:locked",
        "main",
    );

    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-build-saas".into()),
        room_id: "!thread-build-saas:example".into(),
        now: NOW_UNIX,
        answers,
    };

    let outcome = dispatcher.on_unblocked(ctx).await.expect("routes");

    assert_eq!(outcome.verdict, Verdict::Failed);

    let failed = outcome
        .events
        .iter()
        .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.failed"))
        .expect("failure event present");
    let detail = failed.envelope.sym.d.as_ref().unwrap();
    let reason = detail
        .get("reason")
        .and_then(|v| v.as_str())
        .expect("reason populated");
    assert!(
        reason.contains("scope-denied"),
        "expected scope-denied reason, got: {reason}"
    );
    assert!(
        reason.contains("push_external"),
        "reason must name the missing scope: {reason}"
    );
    assert!(
        reason.contains("repo:locked"),
        "reason must name the rejected repo: {reason}"
    );

    // No archive_note_draft on a scope-denied failure — there is no work to
    // record durably (the scope check fails before any session is opened).
    assert!(outcome.archive_note_draft.is_none());
}

#[tokio::test]
async fn default_not_ready_backend_emits_backend_not_ready_event() {
    // Contract test: until a wiring site plugs in a production backend via
    // `Dispatcher::new_with_attached_repo`, the default backend is
    // `NotReadyAttachedRepoBackend`. The dispatcher renders this as a
    // `goal.subgoal.failed { outcome: BackendNotReady, reason: <names the
    // missing wiring> }` event so observers can distinguish "wiring not
    // shipped" from "operator denied" / "scope denied" / "push failed".
    let tmp = TempDir::new().unwrap();
    let goal_mgr = Arc::new(Mutex::new(GoalProcessManager::new(
        tmp.path().to_path_buf(),
    )));
    let researcher = fixture_researcher();
    let dispatcher = Dispatcher::new(SpawnBudget::default(), researcher, Arc::clone(&goal_mgr));

    let parent_goal_id = "goal-x";
    let group_id = "ar-phase";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &dispatcher,
        parent_goal_id,
        group_id,
        "repo:default-not-ready",
        "main",
    );

    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-x".into()),
        room_id: "!thread-x:example".into(),
        now: NOW_UNIX,
        answers,
    };

    let outcome = dispatcher.on_unblocked(ctx).await.expect("routes");

    assert_eq!(outcome.verdict, Verdict::BackendNotReady);

    let slug = outcome
        .child_goal_slug
        .expect("child slug minted even on not-ready");
    {
        let mgr = goal_mgr.lock().unwrap();
        let child = mgr.get(&slug).expect("child persisted");
        assert_eq!(child.parent_goal_id.as_deref(), Some(parent_goal_id));
    }

    let failed = outcome
        .events
        .iter()
        .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.failed"))
        .expect("failure event present");
    let detail = failed.envelope.sym.d.as_ref().unwrap();
    assert_eq!(
        detail.get("outcome").and_then(|v| v.as_str()),
        Some("BackendNotReady")
    );
    let reason = detail
        .get("reason")
        .and_then(|v| v.as_str())
        .expect("reason populated");
    assert!(
        reason.contains("MirrorPushAttachedRepoBackend"),
        "expected reason to name the production backend, got: {reason}"
    );
}

#[tokio::test]
async fn explicit_not_ready_backend_can_carry_a_custom_reason() {
    // Operators / future wiring may want to narrow the BackendNotReady
    // reason as primitives land incrementally. Verify that a custom reason
    // round-trips through the dispatcher → event detail.
    let backend: Arc<dyn AttachedRepoBackend> = Arc::new(NotReadyAttachedRepoBackend::with_reason(
        "T126 wiring pending: MatrixPoster + PushProvider not yet threaded through to Dispatcher",
    ));
    let (dispatcher, _goal_mgr, _tmp) = build_dispatcher_with_backend(backend);

    let parent_goal_id = "goal-y";
    let group_id = "ar-phase";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &dispatcher,
        parent_goal_id,
        group_id,
        "repo:custom-not-ready",
        "main",
    );

    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-y".into()),
        room_id: "!thread-y:example".into(),
        now: NOW_UNIX,
        answers,
    };

    let outcome = dispatcher.on_unblocked(ctx).await.expect("routes");
    assert_eq!(outcome.verdict, Verdict::BackendNotReady);
    let failed = outcome
        .events
        .iter()
        .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.failed"))
        .expect("failure event");
    let reason = failed
        .envelope
        .sym
        .d
        .as_ref()
        .unwrap()
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    assert!(
        reason.contains("MatrixPoster + PushProvider not yet threaded through"),
        "custom reason should round-trip; got: {reason}"
    );
}
