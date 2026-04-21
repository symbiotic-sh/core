//! End-to-end integration test for T130 §06 — Sub-Goal Dispatcher with the
//! `Exploratory` (T116 swarm) backend.
//!
//! This test covers the full flow a real wiring site exercises:
//!
//! 1. A [`QuestionGroup`] with `unblock_key = Exploratory { topic }` is
//!    registered on both the `QuestionResolver` and the `Dispatcher`.
//! 2. Operator answers come in via `submit_answer`; the resolver produces
//!    a `GroupResolutionOutcome::Unblocked { answers, .. }`.
//! 3. The caller builds an [`UnblockedContext`] from the outcome + the
//!    `room_id` it knows about and calls `Dispatcher::on_unblocked`.
//! 4. The dispatcher routes to the [`ExploratoryBackend`] trait
//!    implementation it was constructed with.
//!
//! Two backend implementations are exercised:
//!
//! - **`StubSwarmBackend`** — emulates the production
//!   `SwarmExploratoryBackend` (which composes
//!   `daemon.create_swarm_repo` + the sub-goal authoring sandbox + the
//!   T116 PR / distillery pipeline) without depending on the missing T116
//!   primitives. Demonstrates the full success path: spawn → backend
//!   execute → goal.subgoal.completed event with the artifact refs the
//!   distillery would surface.
//!
//! - **`NotReadyExploratoryBackend`** (default) — the contract test for
//!   the fallback path that ships in this chunk while the T116 sub-goal
//!   authoring runner role is still pending. Asserts the dispatcher emits
//!   `goal.subgoal.failed { outcome: BackendNotReady, reason: <names the
//!   missing primitive> }`.
//!
//! Per the §06 chunk spec's guidance: "If T116 primitives aren't ready
//! yet, your `Exploratory` branch should fall back to a temporary stub
//! that emits `goal.subgoal.failed { outcome: BackendNotReady, reason:
//! \"T116 primitives pending\" }`. ... The integration test in this case
//! becomes a contract test (verifying the stub fall-back behavior) and
//! the full T116-integrated test waits."
//!
//! Audited at chunk-write time: `GitServerManager::create_repo` and the
//! post-merge distillery pipeline are present, but the **sub-goal
//! authoring runner role** (`tools = [file_edit, git_push_swarm,
//! ask_user_group, request_review]`) is not yet wired — see
//! `subgoal/exploratory.rs` module docs for the full audit. The tests
//! below therefore exercise the trait seam (so the production wire-in is
//! a one-line change once the missing role lands) and the fallback
//! contract (so the daemon doesn't silently crash when an Exploratory
//! `goal.unblocked` arrives today).

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
    Dispatcher, ExploratoryBackend, ExploratoryError, ExploratoryOutcome, ExploratoryRequest,
    NotReadyExploratoryBackend, RecallSnippet, ResearchRecall, ResearcherAgent, SpawnBudget,
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

/// Records the request seen by `execute` so the test can assert that the
/// dispatcher fed the operator answers + topic through the trait seam.
#[derive(Default)]
struct StubSwarmBackend {
    seen: Mutex<Option<ExploratoryRequest>>,
    summary: String,
    artifact_refs: Vec<String>,
}

impl StubSwarmBackend {
    fn new(summary: impl Into<String>, artifact_refs: Vec<String>) -> Self {
        Self {
            seen: Mutex::new(None),
            summary: summary.into(),
            artifact_refs,
        }
    }

    fn last_request(&self) -> Option<ExploratoryRequest> {
        self.seen.lock().ok().and_then(|g| g.clone())
    }
}

#[async_trait]
impl ExploratoryBackend for StubSwarmBackend {
    async fn execute(
        &self,
        request: ExploratoryRequest,
    ) -> Result<ExploratoryOutcome, ExploratoryError> {
        if let Ok(mut slot) = self.seen.lock() {
            *slot = Some(request.clone());
        }
        Ok(ExploratoryOutcome {
            sub_goal_id: request.sub_goal_id,
            summary: self.summary.clone(),
            artifact_refs: self.artifact_refs.clone(),
        })
    }
}

fn annotated(text: &str) -> AnnotatedQuestion {
    AnnotatedQuestion {
        text: text.to_string(),
        quick_replies: None,
        recommendation: Some("Vue".to_string()),
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
    goal_mgr: &Arc<Mutex<GoalProcessManager>>,
    dispatcher: &Dispatcher,
    parent_goal_id: &str,
    group_id: &str,
    topic: &str,
) -> (UnblockKey, std::collections::HashMap<usize, String>) {
    let _ = goal_mgr;
    let mut resolver = QuestionResolver::new();
    let unblock_key = UnblockKey::Exploratory {
        topic: topic.to_string(),
    };

    let group = QuestionGroup {
        group_id: group_id.to_string(),
        parent_goal_id: parent_goal_id.to_string(),
        unblock_key: unblock_key.clone(),
        questions: vec![annotated("Framework?"), annotated("SSR?")],
        created_at: NOW_ISO.to_string(),
        resolved_at: None,
        resolution_mode: ResolutionMode::AllRequired,
    };
    resolver.register(group);

    // Wiring contract: dispatcher remembers the unblock key when the group
    // is registered, so the later `goal.unblocked` event routes correctly.
    dispatcher.remember_group(group_id, unblock_key.clone());

    // Operator answers both questions; resolver fires Unblocked.
    let _ = resolver
        .submit_answer(
            QuestionAnswer {
                group_id: group_id.into(),
                question_index: 0,
                answer: Some("Vue".into()),
            },
            NOW_ISO,
        )
        .expect("first submit ok");
    let resolved = resolver
        .submit_answer(
            QuestionAnswer {
                group_id: group_id.into(),
                question_index: 1,
                answer: Some("Yes".into()),
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exploratory_round_trip_with_stub_swarm_backend_emits_completed_event() {
    // ── Arrange ──────────────────────────────────────────────────────────
    let tmp = TempDir::new().unwrap();
    let goal_mgr = Arc::new(Mutex::new(GoalProcessManager::new(
        tmp.path().to_path_buf(),
    )));
    let researcher = fixture_researcher();
    let backend = Arc::new(StubSwarmBackend::new(
        "exploratory complete: chose Vue + SSR",
        vec![
            "archive://episodic/subgoals/sg-x/result.md".to_string(),
            "archive://methodology/patterns/vue-ssr.md".to_string(),
        ],
    ));
    let dispatcher = Dispatcher::new_with_exploratory(
        SpawnBudget::default(),
        researcher,
        Arc::clone(&backend) as Arc<dyn ExploratoryBackend>,
        Arc::clone(&goal_mgr),
    );

    let parent_goal_id = "goal-build-frontend";
    let group_id = "design-phase";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &goal_mgr,
        &dispatcher,
        parent_goal_id,
        group_id,
        "frontend-framework-choice",
    );

    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-build-frontend".into()),
        room_id: "!thread-build-frontend:example".into(),
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
            Some(UnblockKey::Exploratory { topic }) => {
                assert_eq!(topic, "frontend-framework-choice");
            }
            other => panic!("expected Exploratory, got {other:?}"),
        }
    }

    // ── Assert: backend received the correct request ─────────────────────
    let seen = backend.last_request().expect("backend.execute was called");
    assert_eq!(seen.sub_goal_id, child_slug);
    assert_eq!(seen.topic, "frontend-framework-choice");
    // Operator answers carried through (folded into seed prompt by the
    // production backend).
    assert_eq!(seen.operator_answers.len(), 2);

    // ── Assert: event channel ────────────────────────────────────────────
    // Two events: spawned + completed (event channel of the three-channel
    // merge-back; Archive write + thread pill are deferred to §08).
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
        detail.get("summary").and_then(|v| v.as_str()),
        Some("exploratory complete: chose Vue + SSR")
    );

    // §08 will surface the artifact refs through the merge-back; for now
    // we only assert the dispatcher does NOT yet emit a draft (this would
    // change when §08 lands and the assertion will be flipped).
    assert!(
        outcome.archive_note_draft.is_none(),
        "Exploratory archive_note_draft is deferred to §08 (artifact_refs flow); \
         see TODO in dispatcher.run_exploratory"
    );
}

#[tokio::test]
async fn exploratory_with_default_not_ready_backend_emits_backend_not_ready_event() {
    // Contract test: until the T116 sub-goal authoring runner role is
    // wired (see `subgoal/exploratory.rs` module docs for the audit), the
    // dispatcher's default Exploratory backend is
    // `NotReadyExploratoryBackend`, which surfaces `BackendNotReady`.
    let tmp = TempDir::new().unwrap();
    let goal_mgr = Arc::new(Mutex::new(GoalProcessManager::new(
        tmp.path().to_path_buf(),
    )));
    let researcher = fixture_researcher();
    // Construct via plain `Dispatcher::new` to exercise the default backend
    // selection. (Equivalent to `new_with_exploratory(.., NotReadyExploratoryBackend::new(), ..)`.)
    let dispatcher = Dispatcher::new(SpawnBudget::default(), researcher, Arc::clone(&goal_mgr));

    let parent_goal_id = "goal-x";
    let group_id = "exp-phase";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &goal_mgr,
        &dispatcher,
        parent_goal_id,
        group_id,
        "frontend",
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

    // Child goal is still minted up-front so the event stream + control
    // plane stay consistent with the Success path.
    let slug = outcome
        .child_goal_slug
        .expect("child slug minted even on not-ready");
    {
        let mgr = goal_mgr.lock().unwrap();
        let child = mgr.get(&slug).expect("child persisted");
        assert_eq!(child.parent_goal_id.as_deref(), Some(parent_goal_id));
    }

    // Two events: spawned + failed(BackendNotReady).
    assert_eq!(outcome.events.len(), 2);
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
        reason.contains("T116 primitives pending"),
        "expected reason to name T116, got: {reason}"
    );
    assert!(
        reason.contains("sub-goal authoring runner role"),
        "expected reason to identify the missing role, got: {reason}"
    );

    // No Archive note draft (BackendNotReady is a structural failure, not a
    // research output).
    assert!(outcome.archive_note_draft.is_none());
}

#[tokio::test]
async fn explicit_not_ready_backend_can_carry_a_custom_reason() {
    // Operators / future wiring may want to narrow the BackendNotReady
    // reason as primitives land incrementally. Verify that a custom reason
    // round-trips through the dispatcher → event detail.
    let tmp = TempDir::new().unwrap();
    let goal_mgr = Arc::new(Mutex::new(GoalProcessManager::new(
        tmp.path().to_path_buf(),
    )));
    let researcher = fixture_researcher();
    let backend: Arc<dyn ExploratoryBackend> = Arc::new(NotReadyExploratoryBackend::with_reason(
        "T116 primitives pending: still resolving symbiotic-agent-runner tool registration",
    ));
    let dispatcher = Dispatcher::new_with_exploratory(
        SpawnBudget::default(),
        researcher,
        backend,
        Arc::clone(&goal_mgr),
    );

    let group_id = "exp-phase";
    let parent_goal_id = "goal-y";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &goal_mgr,
        &dispatcher,
        parent_goal_id,
        group_id,
        "topic",
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
        reason.contains("symbiotic-agent-runner tool registration"),
        "custom reason should round-trip; got: {reason}"
    );
}

#[tokio::test]
async fn exploratory_backend_failure_surfaces_as_failed_verdict() {
    // When the production backend ships, transient errors (sandbox crash,
    // distillery rejection, etc.) should map to `Verdict::Failed` with a
    // typed reason — not `BackendNotReady` (which is reserved for "the
    // wiring isn't done").
    struct FailingBackend;
    #[async_trait]
    impl ExploratoryBackend for FailingBackend {
        async fn execute(
            &self,
            _request: ExploratoryRequest,
        ) -> Result<ExploratoryOutcome, ExploratoryError> {
            Err(ExploratoryError::AuthoringFailed(
                "agent ran out of context window".into(),
            ))
        }
    }

    let tmp = TempDir::new().unwrap();
    let goal_mgr = Arc::new(Mutex::new(GoalProcessManager::new(
        tmp.path().to_path_buf(),
    )));
    let researcher = fixture_researcher();
    let backend: Arc<dyn ExploratoryBackend> = Arc::new(FailingBackend);
    let dispatcher = Dispatcher::new_with_exploratory(
        SpawnBudget::default(),
        researcher,
        backend,
        Arc::clone(&goal_mgr),
    );

    let group_id = "exp-phase";
    let parent_goal_id = "goal-z";
    let (_unblock_key, answers) = make_resolver_and_unblocked_context(
        &goal_mgr,
        &dispatcher,
        parent_goal_id,
        group_id,
        "topic",
    );

    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-z".into()),
        room_id: "!thread-z:example".into(),
        now: NOW_UNIX,
        answers,
    };

    let outcome = dispatcher.on_unblocked(ctx).await.expect("routes");
    assert_eq!(outcome.verdict, Verdict::Failed);

    let failed = outcome
        .events
        .iter()
        .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.failed"))
        .expect("failure event");
    let detail = failed.envelope.sym.d.as_ref().unwrap();
    assert_eq!(
        detail.get("outcome").and_then(|v| v.as_str()),
        Some("ExploratoryError")
    );
    let reason = detail.get("reason").and_then(|v| v.as_str()).unwrap();
    assert!(
        reason.contains("authoring failed"),
        "expected typed authoring error reason, got: {reason}"
    );
}
