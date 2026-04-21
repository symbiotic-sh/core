//! Adapter layer that wires the Inquisitor agent's user-question tools.
//!
//! This module is intentionally thin: it owns the **construction of the
//! question-emission tools** (`ask_user` and/or `ask_user_group`) + their
//! shared pending handles, gated by the `SYMBIOTIC_BATCH_INQUISITOR` feature
//! flag.
//!
//! Why it exists separately from the ReAct wiring in `workers.rs`:
//!
//! - The worker-level wiring (see `workers.rs::execute_react`) is a large
//!   tuple-returning function that needs to be refactored into a proper
//!   bundle type in T130 §04 (the sibling task is already queued to register
//!   a `question_resolver` module here).
//! - Until that refactor lands, §03 ships the **tool-set constructor** here
//!   so consumers can opt in incrementally without forcing a big-bang change
//!   to the execution pipeline.
//!
//! Pending handles on this bundle mirror the pattern used by `AskUserTool`
//! and `GeneratePlanTool`: the handle is an `Arc<Mutex<Option<T>>>` the
//! worker inspects after the agent loop returns to decide what event to emit.
//!
//! TODO: coordinate mod.rs registration — T130 §04 sibling will add sibling
//! modules (e.g. `question_resolver`) to `goal_pipeline/mod.rs`. This module
//! is currently the only entry there; merge-in should be trivial (one more
//! `pub mod` line).

use std::sync::{Arc, Mutex};

use symbiotic_agents::builtin_tools::{AskUserGroupTool, AskUserTool, PendingQuestion};
use symbiotic_core::types::question_group::QuestionGroup;

use crate::config::feature_flags;

/// Bundle of user-question tools + shared pending handles produced for one
/// Inquisitor execution round.
///
/// When the batched feature flag is **off**: only [`InquisitorTools::ask_user`]
/// and [`InquisitorTools::pending_question`] are populated; the group fields
/// are `None`.
///
/// When the flag is **on**: both tools are registered so the LLM can choose
/// between a single question and a batched group based on how many
/// clarifications are needed (see the Inquisitor system prompt for the
/// selection rule). Keeping the single-question tool available under flag-on
/// preserves backward compatibility for operator tests that still exercise
/// the one-question path.
pub struct InquisitorTools {
    /// Always populated — sequential one-question tool.
    pub ask_user: AskUserTool,
    /// Always populated — handle the worker inspects after the agent loop.
    pub pending_question: Arc<Mutex<Option<PendingQuestion>>>,
    /// Populated only when the batch feature flag is active for the goal.
    pub ask_user_group: Option<AskUserGroupTool>,
    /// Populated only when the batch feature flag is active for the goal.
    pub pending_group: Option<Arc<Mutex<Option<QuestionGroup>>>>,
}

impl InquisitorTools {
    /// Build the Inquisitor's question-emission tool set.
    ///
    /// `goal_id_hint` threads through to the feature-flag reader so future
    /// per-goal overrides (T130 §04) don't force a signature change here.
    pub fn new(goal_id_hint: Option<&str>) -> Self {
        let (ask_user, pending_question) = AskUserTool::new();

        if feature_flags::batch_inquisitor_enabled(goal_id_hint) {
            let (group_tool, pending_group) = AskUserGroupTool::new();
            Self {
                ask_user,
                pending_question,
                ask_user_group: Some(group_tool),
                pending_group: Some(pending_group),
            }
        } else {
            Self {
                ask_user,
                pending_question,
                ask_user_group: None,
                pending_group: None,
            }
        }
    }

    /// Returns `true` when the batched emission tool is registered.
    pub fn batch_enabled(&self) -> bool {
        self.ask_user_group.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // Shared env lock — same rationale as feature_flags tests: tests manipulate
    // the same env var and must serialise to avoid races under `cargo test`
    // parallelism.
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn scoped<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
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

    #[test]
    fn flag_off_registers_only_sequential_tool() {
        scoped(None, || {
            let tools = InquisitorTools::new(Some("goal-test"));
            assert!(!tools.batch_enabled());
            assert!(tools.ask_user_group.is_none());
            assert!(tools.pending_group.is_none());
            assert!(tools.pending_question.lock().unwrap().is_none());
        });
    }

    #[test]
    fn flag_on_registers_both_tools() {
        scoped(Some("1"), || {
            let tools = InquisitorTools::new(Some("goal-test"));
            assert!(tools.batch_enabled());
            assert!(tools.ask_user_group.is_some());
            let pending_group = tools
                .pending_group
                .as_ref()
                .expect("pending_group must be some when flag on");
            assert!(pending_group.lock().unwrap().is_none());
        });
    }

    #[tokio::test]
    async fn flag_on_batched_emission_populates_pending_group() {
        // Integration-style: simulate the LLM invoking `ask_user_group` with
        // a canned JSON payload and verify the tool stores the parsed group
        // in the handle the worker would inspect after the loop.
        use symbiotic_agents::tools::Tool;
        use symbiotic_core::types::question_group::{
            AnswerType, QuestionSeverity, ResolutionMode, UnblockKey,
        };

        let (tools, pending_group) = scoped(Some("1"), || {
            let tools = InquisitorTools::new(Some("goal-build-frontend"));
            let pending = tools.pending_group.clone().unwrap();
            (tools, pending)
        });

        let group_tool = tools
            .ask_user_group
            .as_ref()
            .expect("ask_user_group must be registered under flag-on");

        let canned = serde_json::json!({
            "group_id": "design-phase",
            "parent_goal_id": "goal-build-frontend",
            "unblock_key": {"type": "Exploratory", "topic": "frontend-framework-choice"},
            "resolution_mode": {"mode": "AllRequired"},
            "questions": [
                {
                    "text": "Framework preference?",
                    "quick_replies": ["Vue", "React", "Svelte"],
                    "recommendation": "Vue",
                    "confidence": 0.72,
                    "severity": "Decision",
                    "expected_answer_type": "SingleChoice"
                },
                {
                    "text": "SSR required?",
                    "recommendation": "Yes",
                    "confidence": 0.85,
                    "severity": "Decision",
                    "expected_answer_type": "Boolean"
                }
            ]
        });

        let result = group_tool.execute(canned).await.expect("execute");
        assert!(result.success);

        let group = pending_group
            .lock()
            .unwrap()
            .clone()
            .expect("group must land in pending handle");
        assert_eq!(group.group_id, "design-phase");
        assert_eq!(group.questions.len(), 2);
        match &group.unblock_key {
            UnblockKey::Exploratory { topic } => {
                assert_eq!(topic, "frontend-framework-choice");
            }
            other => panic!("unexpected unblock_key: {other:?}"),
        }
        assert!(matches!(group.resolution_mode, ResolutionMode::AllRequired));
        assert!(matches!(
            group.questions[0].severity,
            QuestionSeverity::Decision
        ));
        assert!(matches!(
            group.questions[1].expected_answer_type,
            AnswerType::Boolean
        ));
    }

    #[tokio::test]
    async fn flag_off_sequential_path_unchanged() {
        // Regression guard: when the flag is off, calling the single-question
        // tool still stores a `PendingQuestion` in the pending-question handle
        // exactly as before §03 landed. If the sequential path ever changes
        // shape, this test fails — forcing the author to update the §03 notes.
        use symbiotic_agents::tools::Tool;

        let (tools, pending_q) = scoped(None, || {
            let tools = InquisitorTools::new(None);
            let pending = tools.pending_question.clone();
            (tools, pending)
        });
        assert!(!tools.batch_enabled());

        let result = tools
            .ask_user
            .execute(serde_json::json!({
                "question": "What's the budget?",
                "quick_replies": ["Low", "Mid", "High"]
            }))
            .await
            .expect("execute");
        assert!(result.success);

        let pq = pending_q
            .lock()
            .unwrap()
            .clone()
            .expect("pending question must be set");
        assert_eq!(pq.text, "What's the budget?");
        assert_eq!(pq.quick_replies.as_ref().map(|r| r.len()).unwrap_or(0), 3);
    }
}
