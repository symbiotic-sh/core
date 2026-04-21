//! Integration tests for T130 §04a — wiring hookup.
//!
//! This chunk bridges the §03 Inquisitor batch-emit surface
//! (AskUserGroupTool + InquisitorTools bundle) with the §04
//! QuestionResolver + goal.answer handler. These tests exercise the whole
//! contract on the **public seams** of the daemon crate:
//!
//! 1. Flag-on path: the Inquisitor's `ask_user_group` tool captures a
//!    canned LLM-response `QuestionGroup` into the adapter's pending handle.
//!    The daemon serializes it into a `goal.question_group` event, then the
//!    resolver resolves it as 3 `goal.answer` submissions arrive, yielding
//!    `GroupResolutionOutcome::Unblocked` with the full answers map and a
//!    proper `goal.unblocked` envelope.
//!
//! 2. Flag-off path (regression guard): the sequential `AskUserTool` stays
//!    live and no `pending_group` appears on the tool bundle, preserving the
//!    existing single-question flow.
//!
//! # Why not end-to-end via workers.rs?
//!
//! `workers.rs::execute_react` pulls in the full `SymbioticDaemon` + tokio
//! runtime + a live LLM. Spinning that up in a test file bloats the harness
//! well beyond the §04a scope. Instead, we assert the contract at two seams:
//!
//! - **Tool seam**: `AskUserGroupTool` populates `pending_group` — exactly
//!   what `execute_react` reads after the ReAct loop and writes into
//!   `outputs.insert("pending_question_group", ...)`.
//! - **Wire seam**: the resulting `QuestionGroup` JSON is the same string
//!   that lands in a `goal.question_group` `DaemonEvent::detail`; passing it
//!   through `DaemonEvent::to_envelope` and back proves round-trip fidelity.
//! - **Resolver seam**: `goal.answer` submissions routed through the
//!   resolver yield `GroupResolutionOutcome::Unblocked` with the answers
//!   map matching the commands.rs emission shape.
//!
//! If any of these contracts drift, the wiring in `workers.rs` /
//! `goals.rs` / `commands.rs` no longer lines up and this test breaks.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use symbiotic_agents::builtin_tools::{AskUserGroupTool, AskUserTool};
use symbiotic_agents::tools::Tool;
use symbiotic_core::protocol::{Kind, Status};
use symbiotic_core::types::question_group::QuestionGroup;
use symbiotic_daemon::events::{DaemonEvent, EventType};
use symbiotic_daemon::goal_pipeline::inquisitor_adapter::InquisitorTools;
use symbiotic_daemon::goal_pipeline::question_resolver::{
    GroupResolutionOutcome, QuestionAnswer, QuestionResolver,
};

// Shared env guard — `SYMBIOTIC_BATCH_INQUISITOR` is process-wide so these
// tests serialise on it.
static ENV_LOCK: StdMutex<()> = StdMutex::new(());

fn scoped_flag<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("SYMBIOTIC_BATCH_INQUISITOR").ok();
    match value {
        Some(v) => std::env::set_var("SYMBIOTIC_BATCH_INQUISITOR", v),
        None => std::env::remove_var("SYMBIOTIC_BATCH_INQUISITOR"),
    }
    let out = body();
    match prev {
        Some(v) => std::env::set_var("SYMBIOTIC_BATCH_INQUISITOR", v),
        None => std::env::remove_var("SYMBIOTIC_BATCH_INQUISITOR"),
    }
    out
}

/// Build the canned Inquisitor JSON payload the LLM would produce when it
/// has 3 clarifications for a single sub-goal. Matches the §03 wire schema.
fn canned_three_question_group() -> serde_json::Value {
    serde_json::json!({
        "group_id": "design-phase",
        "parent_goal_id": "goal-build-frontend",
        "unblock_key": {"type": "Exploratory", "topic": "frontend-framework-choice"},
        "resolution_mode": {"mode": "AllRequired"},
        "questions": [
            {
                "text": "Framework preference: Vue, React, or Svelte?",
                "quick_replies": ["Vue", "React", "Svelte"],
                "recommendation": "Vue",
                "confidence": 0.72,
                "severity": "Decision",
                "expected_answer_type": "SingleChoice"
            },
            {
                "text": "SSR required for initial load?",
                "quick_replies": ["Yes", "No"],
                "recommendation": "Yes",
                "confidence": 0.85,
                "severity": "Decision",
                "expected_answer_type": "Boolean"
            },
            {
                "text": "Which browsers need to be supported?",
                "recommendation": "modern evergreen",
                "confidence": 0.80,
                "severity": "Informational",
                "expected_answer_type": "FreeText"
            }
        ]
    })
}

#[tokio::test]
async fn flag_on_full_roundtrip_inquisitor_to_unblocked_answers() {
    // --- Phase 1: Inquisitor fires ask_user_group (simulates the ReAct loop
    //     calling the tool with the canned LLM-response JSON). -----
    let (tools, pending_group) = scoped_flag(Some("1"), || {
        let tools = InquisitorTools::new(Some("goal-build-frontend"));
        let handle = tools
            .pending_group
            .clone()
            .expect("flag-on must register pending_group");
        (tools, handle)
    });
    assert!(
        tools.batch_enabled(),
        "flag-on must register ask_user_group"
    );

    let group_tool = tools
        .ask_user_group
        .as_ref()
        .expect("flag-on must register the ask_user_group tool");
    let tool_result = group_tool
        .execute(canned_three_question_group())
        .await
        .expect("ask_user_group execute should succeed on valid payload");
    assert!(tool_result.success, "tool must acknowledge success");

    let captured: QuestionGroup = pending_group
        .lock()
        .unwrap()
        .clone()
        .expect("pending_group handle must carry the drafted group");
    assert_eq!(captured.group_id, "design-phase");
    assert_eq!(captured.questions.len(), 3);

    // --- Phase 2: Daemon (goals.rs) serializes the group as the
    //     `pending_question_group` output + builds a DaemonEvent. -----
    let group_json = serde_json::to_string(&captured).expect("group must serialize");
    let event = DaemonEvent {
        event_type: EventType::GoalQuestionGroup,
        status: "awaiting_input".to_string(),
        job_id: Some("job-xyz".to_string()),
        detail: group_json.clone(),
        goal_room: Some("#goal-room".to_string()),
        goal_template: Some("inquisition".to_string()),
        goal_run_id: Some("run-123".to_string()),
        goal_id: Some("goal-build-frontend".to_string()),
        intake_run_id: None,
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };
    let envelope = event.to_envelope(1_700_000_000);

    // Envelope must be a Question/Awaiting v2 event, not the catch-all
    // "completed/failed" case — this tests the classify() branch added in
    // §04a.
    assert_eq!(envelope.sym.k, Kind::Question);
    assert_eq!(envelope.sym.s, Some(Status::Awaiting));

    // The full group JSON lives under `detail.group` so the app can parse
    // the annotated questions without re-reading the body. The body itself
    // is a short human-readable summary.
    let detail = envelope
        .sym
        .d
        .as_ref()
        .expect("question_group envelope must have detail");
    let group_field = detail
        .get("group")
        .expect("detail must embed the full group under `group`");
    let parsed_back: QuestionGroup =
        serde_json::from_value(group_field.clone()).expect("group payload round-trips");
    assert_eq!(parsed_back.group_id, captured.group_id);
    assert_eq!(parsed_back.questions.len(), 3);
    assert!(envelope.body.contains("3 clarification"));

    // --- Phase 3: QuestionResolver receives 3 answers (simulating the
    //     `goal.answer` handler in commands.rs routing through
    //     `submit_answer`). -----
    let mut resolver = QuestionResolver::new();
    resolver.register(captured.clone());

    // Submit out of order to mirror real operator behaviour (§3.2.1 allows
    // free-order answering on `AllRequired`).
    let submissions = [(1, "Yes"), (2, "last 2 versions"), (0, "Vue")];
    let mut final_outcome: Option<GroupResolutionOutcome> = None;
    for (idx, text) in submissions {
        let outcome = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "design-phase".to_string(),
                    question_index: idx,
                    answer: Some(text.to_string()),
                },
                "2026-04-18T10:42:00Z",
            )
            .expect("submit_answer should succeed");
        if outcome.is_some() {
            final_outcome = outcome;
        }
    }

    let outcome = final_outcome.expect("third submission must resolve the group under AllRequired");
    match outcome {
        GroupResolutionOutcome::Unblocked {
            group_id,
            parent_goal_id,
            answers,
            auto_records,
        } => {
            assert_eq!(group_id, "design-phase");
            assert_eq!(parent_goal_id, "goal-build-frontend");
            let mut expected: HashMap<usize, String> = HashMap::new();
            expected.insert(0, "Vue".into());
            expected.insert(1, "Yes".into());
            expected.insert(2, "last 2 versions".into());
            assert_eq!(answers, expected);
            assert!(
                auto_records.is_empty(),
                "no auto-records for direct operator answers"
            );
        }
        other => panic!("expected Unblocked, got {other:?}"),
    }

    // --- Phase 4: goal.unblocked envelope shape (what commands.rs emits
    //     onto Matrix) round-trips cleanly for the app. -----
    let unblocked_detail = serde_json::json!({
        "group_id": "design-phase",
        "parent_goal_id": "goal-build-frontend",
        "answers": { "0": "Vue", "1": "Yes", "2": "last 2 versions" },
        "auto_records": [],
    })
    .to_string();
    let unblocked_event = DaemonEvent {
        event_type: EventType::GoalUnblocked,
        status: "completed".to_string(),
        job_id: None,
        detail: unblocked_detail,
        goal_room: Some("#goal-room".to_string()),
        goal_template: Some("inquisition".to_string()),
        goal_run_id: None,
        goal_id: Some("goal-build-frontend".to_string()),
        intake_run_id: None,
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };
    let unblocked_envelope = unblocked_event.to_envelope(1_700_000_001);
    assert_eq!(unblocked_envelope.sym.k, Kind::Message);
    assert_eq!(unblocked_envelope.sym.s, Some(Status::Success));
    let unblock_detail = unblocked_envelope
        .sym
        .d
        .as_ref()
        .expect("goal.unblocked must carry detail");
    let unblock_payload = unblock_detail
        .get("unblock")
        .expect("detail must embed answers under `unblock`");
    assert_eq!(
        unblock_payload
            .get("answers")
            .and_then(|v| v.get("0"))
            .and_then(|v| v.as_str()),
        Some("Vue")
    );
}

#[tokio::test]
async fn flag_off_sequential_path_no_group_emission() {
    // Regression guard for the existing AskUserTool path. Under flag-off,
    // the Inquisitor adapter must register only the single-question tool;
    // populating it must NOT populate any group handle.
    let (tools, pending_question) = scoped_flag(None, || {
        let tools = InquisitorTools::new(None);
        let handle = tools.pending_question.clone();
        (tools, handle)
    });
    assert!(
        !tools.batch_enabled(),
        "flag-off must not register group tool"
    );
    assert!(tools.ask_user_group.is_none());
    assert!(tools.pending_group.is_none());

    // Fire the sequential tool as the legacy LLM would.
    let _ = tools
        .ask_user
        .execute(serde_json::json!({
            "question": "What framework?",
            "quick_replies": ["Vue", "React", "Svelte"],
        }))
        .await
        .expect("ask_user execute must succeed");

    let pq = pending_question
        .lock()
        .unwrap()
        .clone()
        .expect("flag-off path must still populate pending_question");
    assert_eq!(pq.text, "What framework?");

    // Sanity: a standalone AskUserGroupTool never crosses into this flow.
    let (standalone_group, standalone_pending) = AskUserGroupTool::new();
    // ...and tool behaves independently when called directly.
    let _ = standalone_group
        .execute(canned_three_question_group())
        .await
        .expect("standalone tool works regardless of flag");
    assert!(
        standalone_pending.lock().unwrap().is_some(),
        "standalone tool call populates its own local handle"
    );

    // Final contract: under flag-off the daemon's execute_react never
    // constructs `ask_user_group`, so the `pending_question_group` output
    // key never appears — meaning the goals.rs emission branch added in
    // §04a stays dormant. Nothing more to assert here; the test passes by
    // not seeing a group in the flag-off adapter.
}

#[tokio::test]
async fn flag_on_resolver_rejects_answer_to_unknown_group() {
    // Protects against silent routing bugs: if `commands.rs` ever forwards a
    // grouped-answer submission without the daemon having registered the
    // group, the resolver must reject it cleanly — not panic or swallow.
    let mut resolver = QuestionResolver::new();
    let err = resolver
        .submit_answer(
            QuestionAnswer {
                group_id: "does-not-exist".to_string(),
                question_index: 0,
                answer: Some("whatever".to_string()),
            },
            "2026-04-18T10:42:00Z",
        )
        .expect_err("unknown-group submission must error");
    assert!(err.to_string().contains("not registered"), "got: {err}");
}

#[tokio::test]
async fn flag_on_adapter_registers_ask_user_group_tool_for_react_loop() {
    // Contract test for the `execute_react` wiring. The ReAct loop builds a
    // `&[dyn Tool]` slice; when the flag is on, the Inquisitor bundle must
    // contribute both `ask_user` and `ask_user_group` to that slice so the
    // LLM's canned choice of `ask_user_group` actually hits a registered
    // tool.
    scoped_flag(Some("1"), || {
        let tools = InquisitorTools::new(Some("goal-any"));
        assert_eq!(tools.ask_user.name(), "ask_user");
        let group = tools
            .ask_user_group
            .as_ref()
            .expect("flag-on registers ask_user_group");
        assert_eq!(group.name(), "ask_user_group");
    });
}

#[tokio::test]
async fn flag_off_adapter_only_registers_ask_user() {
    scoped_flag(None, || {
        let tools = InquisitorTools::new(None);
        assert_eq!(tools.ask_user.name(), "ask_user");
        assert!(tools.ask_user_group.is_none());
    });

    // A standalone AskUserTool still works as before.
    let (tool, _pending) = AskUserTool::new();
    assert_eq!(tool.name(), "ask_user");
}
