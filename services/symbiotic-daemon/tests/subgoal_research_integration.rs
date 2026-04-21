//! End-to-end integration test for T130 §05 — Sub-Goal Dispatcher with
//! the `ResearchOnly` backend.
//!
//! Covers the full flow a real wiring site exercises:
//!
//! 1. A [`QuestionGroup`] with `unblock_key = ResearchOnly { .. }` is
//!    registered on both the `QuestionResolver` and the `Dispatcher`.
//! 2. Operator answers come in via `submit_answer`; the resolver produces
//!    a `GroupResolutionOutcome::Unblocked { answers, .. }`.
//! 3. The caller builds an [`UnblockedContext`] from the outcome + the
//!    `room_id` it knows about and calls `Dispatcher::on_unblocked`.
//! 4. The dispatcher routes to a stub `ResearcherAgent`, mints a child
//!    `GoalProcess`, and returns:
//!    - `Verdict::Success`
//!    - a `child_goal_slug` that resolves on the control-plane store
//!    - events for `goal.subgoal.spawned` and `goal.subgoal.completed`
//!    - an `ArchiveNoteDraft` (not yet written — §08 territory).
//!
//! This test intentionally stays above the Matrix transport layer and
//! below the LLM / Recall providers. Production composition plugs the
//! real `RecallGateway` + `ProviderRouter` behind the two small traits
//! the researcher consumes.

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
    Dispatcher, RecallSnippet, ResearchRecall, ResearcherAgent, SpawnBudget, UnblockedContext,
    Verdict,
};
use tempfile::TempDir;

const NOW_ISO: &str = "2026-04-18T10:42:00Z";
const NOW_UNIX: u64 = 1_713_437_320;

struct FixtureRecall {
    snippets: Vec<RecallSnippet>,
}

#[async_trait]
impl ResearchRecall for FixtureRecall {
    async fn query(&self, _topic: &str, _top_k: usize) -> anyhow::Result<Vec<RecallSnippet>> {
        Ok(self.snippets.clone())
    }
}

struct FixtureLlm {
    reply: String,
}

#[async_trait]
impl LlmClient for FixtureLlm {
    async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> anyhow::Result<String> {
        Ok(self.reply.clone())
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

#[tokio::test]
async fn research_only_round_trip_creates_child_goal_and_fires_events() {
    // ── Arrange ──────────────────────────────────────────────────────────
    let tmp = TempDir::new().unwrap();
    let goal_mgr = Arc::new(Mutex::new(GoalProcessManager::new(
        tmp.path().to_path_buf(),
    )));

    let recall = Arc::new(FixtureRecall {
        snippets: vec![
            RecallSnippet {
                source: "archive://semantic/oauth-rfc.md".into(),
                content: "OAuth 2.1 mandates PKCE.".into(),
                score: 0.92,
            },
            RecallSnippet {
                source: "archive://semantic/oauth-libs.md".into(),
                content: "`oauth2` crate is idiomatic.".into(),
                score: 0.88,
            },
        ],
    }) as Arc<dyn ResearchRecall>;

    let llm = Arc::new(FixtureLlm {
        reply: "TL;DR: pick the `oauth2` crate.\n\nRationale: RFC + prior art align.\n".to_string(),
    }) as Arc<dyn LlmClient>;

    let researcher = Arc::new(ResearcherAgent::new(recall, llm));
    let dispatcher = Dispatcher::new(SpawnBudget::default(), researcher, Arc::clone(&goal_mgr));

    let mut resolver = QuestionResolver::new();
    let group_id = "design-auth";
    let parent_goal_id = "goal-build-frontend";
    let unblock_key = UnblockKey::ResearchOnly {
        question: "Which OAuth crate should we use?".to_string(),
    };

    let group = QuestionGroup {
        group_id: group_id.to_string(),
        parent_goal_id: parent_goal_id.to_string(),
        unblock_key: unblock_key.clone(),
        questions: vec![annotated("Prefer SSR?"), annotated("Target Rust edition?")],
        created_at: NOW_ISO.to_string(),
        resolved_at: None,
        resolution_mode: ResolutionMode::AllRequired,
    };
    resolver.register(group);

    // At registration time the wiring also tells the dispatcher about the
    // unblock key so it can route the later resolution.
    dispatcher.remember_group(group_id, unblock_key.clone());

    // ── Act: operator answers both questions ─────────────────────────────
    let first = resolver
        .submit_answer(
            QuestionAnswer {
                group_id: group_id.into(),
                question_index: 0,
                answer: Some("Yes".into()),
            },
            NOW_ISO,
        )
        .expect("first submit ok");
    assert!(first.is_none(), "first answer doesn't resolve the group");

    let second = resolver
        .submit_answer(
            QuestionAnswer {
                group_id: group_id.into(),
                question_index: 1,
                answer: Some("2024".into()),
            },
            NOW_ISO,
        )
        .expect("second submit ok")
        .expect("second answer resolves the group");

    let (resolved_group_id, resolved_parent, answers) = match second {
        GroupResolutionOutcome::Unblocked {
            group_id,
            parent_goal_id,
            answers,
            ..
        } => (group_id, parent_goal_id, answers),
        other => panic!("expected Unblocked, got {other:?}"),
    };
    assert_eq!(resolved_group_id, group_id);
    assert_eq!(resolved_parent, parent_goal_id);
    assert_eq!(answers.len(), 2);

    // ── Act: dispatcher routes the unblock ───────────────────────────────
    let ctx = UnblockedContext {
        parent_goal_id: parent_goal_id.to_string(),
        group_id: group_id.to_string(),
        thread_id: Some("thread-build-frontend".into()),
        room_id: "!thread-build-frontend:example".into(),
        now: NOW_UNIX,
        answers,
    };

    let outcome = dispatcher
        .on_unblocked(ctx)
        .await
        .expect("dispatcher route succeeds");

    // ── Assert ───────────────────────────────────────────────────────────
    assert_eq!(outcome.verdict, Verdict::Success);
    let child_slug = outcome.child_goal_slug.expect("child goal minted");
    assert!(child_slug.starts_with("sg-"));

    // Child goal persisted with parent linkage + unblock key carried forward.
    {
        let mgr = goal_mgr.lock().unwrap();
        let child = mgr.get(&child_slug).expect("child persisted");
        assert_eq!(child.parent_goal_id.as_deref(), Some(parent_goal_id));
        match &child.unblock_key {
            Some(UnblockKey::ResearchOnly { question }) => {
                assert_eq!(question, "Which OAuth crate should we use?");
            }
            other => panic!("expected ResearchOnly, got {other:?}"),
        }
    }

    // Two events: goal.subgoal.spawned + goal.subgoal.completed.
    assert_eq!(outcome.events.len(), 2, "expected spawned + completed");
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

    // Completion event carries the parent + child ids + Success outcome.
    let completed = outcome
        .events
        .iter()
        .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.completed"))
        .unwrap();
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

    // Archive note draft is produced (but not yet written — that's §08).
    let draft = outcome.archive_note_draft.expect("draft emitted");
    assert_eq!(draft.sub_goal_id, child_slug);
    assert!(draft.markdown.contains("Which OAuth crate"));
    assert!(!draft.sources.is_empty());
}

#[tokio::test]
async fn unsupported_backend_emits_failure_but_does_not_persist_child() {
    // Targets the still-stubbed Composite variant — ResearchOnly is live
    // (§05), Exploratory has its own backend trait via the ExploratoryBackend
    // seam (§06), AttachedRepo has its own backend trait via the
    // AttachedRepoBackend seam (§07). Composite is now the canonical
    // "wiring not yet shipped" outcome (§09 lands the fan-out logic).
    let tmp = TempDir::new().unwrap();
    let goal_mgr = Arc::new(Mutex::new(GoalProcessManager::new(
        tmp.path().to_path_buf(),
    )));
    let recall = Arc::new(FixtureRecall { snippets: vec![] }) as Arc<dyn ResearchRecall>;
    let llm = Arc::new(FixtureLlm {
        reply: "unreached".into(),
    }) as Arc<dyn LlmClient>;
    let researcher = Arc::new(ResearcherAgent::new(recall, llm));
    let dispatcher = Dispatcher::new(SpawnBudget::default(), researcher, Arc::clone(&goal_mgr));

    let group_id = "composite-phase";
    let key = UnblockKey::Composite {
        children: vec![UnblockKey::ResearchOnly {
            question: "child q".into(),
        }],
    };
    dispatcher.remember_group(group_id, key);

    let ctx = UnblockedContext {
        parent_goal_id: "goal-x".into(),
        group_id: group_id.into(),
        thread_id: Some("thread-x".into()),
        room_id: "!thread-x:example".into(),
        now: NOW_UNIX,
        answers: Default::default(),
    };

    let outcome = dispatcher.on_unblocked(ctx).await.expect("routes");
    assert_eq!(outcome.verdict, Verdict::UnsupportedBackend);
    assert!(outcome.child_goal_slug.is_none());
    assert_eq!(outcome.events.len(), 1);
    let detail = outcome.events[0].envelope.sym.d.as_ref().unwrap();
    assert_eq!(
        detail.get("outcome").and_then(|v| v.as_str()),
        Some("UnsupportedBackend")
    );
    let reason = detail.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        reason.contains("Composite") && reason.contains("§09"),
        "expected Composite/§09 reason, got: {reason}"
    );

    // No child goal persisted.
    let mgr = goal_mgr.lock().unwrap();
    assert_eq!(mgr.all_goals().count(), 0);
}
