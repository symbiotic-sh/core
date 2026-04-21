//! Lightweight UX classifier for incoming user messages.
//!
//! Categorizes messages into action types (Quick, ShortTask, Goal, FollowUp,
//! Routing, Intake) so the daemon can route them to the correct handler without
//! an LLM call. Phase 1 is rule-based only (<1ms); Phase 2 will add an LLM
//! fallback for ambiguous cases.
//!
//! See `docs/design/thread-architecture.md` §4 "Classification Pipeline".

use std::fmt;

/// Classification result for a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UxClass {
    /// Direct question or quick lookup — respond inline with LLM call.
    /// Examples: "what time is it in Tokyo?", "convert 5km to miles"
    Quick,

    /// Short task requiring one agent pass — respond with task.result.
    /// Examples: "summarize this article", "draft a reply to this email"
    ShortTask,

    /// Complex/multi-step work — create or promote to a goal.
    /// Examples: "build me a landing page", "research competitors and write a report"
    Goal,

    /// Follow-up to an existing thread — route to that thread.
    FollowUp {
        /// The thread_id this message should route to.
        thread_id: String,
    },

    /// Routing action — user wants to create, move, split, or manage threads.
    Routing,

    /// URL intake — auto-detected link paste.
    Intake,
}

impl UxClass {
    /// Short lowercase label for use in events and logs.
    pub fn label(&self) -> &'static str {
        match self {
            UxClass::Quick => "quick",
            UxClass::ShortTask => "short_task",
            UxClass::Goal => "goal",
            UxClass::FollowUp { .. } => "follow_up",
            UxClass::Routing => "routing",
            UxClass::Intake => "intake",
        }
    }
}

impl fmt::Display for UxClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Classifies user messages for the UX pipeline.
///
/// Phase 1: rule-based heuristics only (<1ms).
/// Phase 2: LLM fallback for ambiguous cases.
pub struct UxClassifier;

impl UxClassifier {
    /// Classify a user message. Returns the classification and confidence (0.0–1.0).
    ///
    /// `_active_threads` is reserved for Phase 2 follow-up detection (topic
    /// similarity against recent exchanges). Currently unused.
    pub fn classify(message: &str, _active_threads: &[&str]) -> (UxClass, f32) {
        let msg = message.trim();

        // Empty message — treat as quick (no-op reply).
        if msg.is_empty() {
            return (UxClass::Quick, 1.0);
        }

        // URL detection → Intake
        if Self::looks_like_url(msg) {
            return (UxClass::Intake, 0.95);
        }

        // Routing commands (split, merge, move, create/archive thread)
        if Self::is_routing_command(msg) {
            return (UxClass::Routing, 0.9);
        }

        // Short factual questions — catch before goal/task checks so they
        // never accidentally trigger the inquisition pipeline.
        if Self::is_factual_question(msg) {
            return (UxClass::Quick, 0.9);
        }

        // Goal indicators (multi-step, complex)
        if Self::has_goal_indicators(msg) {
            return (UxClass::Goal, 0.8);
        }

        // Short task indicators (single-verb actions)
        if Self::has_task_indicators(msg) {
            return (UxClass::ShortTask, 0.75);
        }

        // Default: quick reply (simple question or chat)
        (UxClass::Quick, 0.6)
    }

    /// Detect whether the message is primarily a URL paste.
    fn looks_like_url(msg: &str) -> bool {
        // Explicit scheme
        if msg.starts_with("http://") || msg.starts_with("https://") {
            return true;
        }

        // Other schemes (ftp://, ssh://, etc.)
        if msg.contains("://") {
            return true;
        }

        // Bare domain pattern: ≤3 words, at least one contains a dot
        // (not ending with a dot — that's a sentence ending).
        if msg.split_whitespace().count() <= 3
            && msg
                .split_whitespace()
                .any(|w| w.contains('.') && !w.ends_with('.'))
        {
            return true;
        }

        false
    }

    /// Detect explicit routing/thread-management commands.
    fn is_routing_command(msg: &str) -> bool {
        let lower = msg.to_lowercase();
        lower.starts_with("split ")
            || lower.starts_with("merge ")
            || lower.starts_with("move to ")
            || lower.starts_with("create thread ")
            || lower.starts_with("archive thread ")
    }

    /// Detect short factual questions that should be answered directly.
    ///
    /// Matches common question starters ("What is", "Who was", "How many", etc.)
    /// but only when the message is short (≤20 words) and interrogative. This
    /// prevents complex requests like "What is the best strategy to grow my
    /// startup over the next 5 years?" from being fast-pathed.
    fn is_factual_question(msg: &str) -> bool {
        let lower = msg.to_lowercase();
        let word_count = msg.split_whitespace().count();

        // Must be short (≤20 words) and contain a question mark.
        if word_count > 20 || !msg.contains('?') {
            return false;
        }

        // Factual question starters — these ask for a specific fact, not
        // a multi-step action.
        let factual_starters: &[&str] = &[
            // What
            "what is ",
            "what are ",
            "what was ",
            "what were ",
            "what's ",
            "whats ",
            // Who
            "who is ",
            "who are ",
            "who was ",
            "who's ",
            "whos ",
            // Where
            "where is ",
            "where are ",
            "where's ",
            "wheres ",
            // When
            "when did ",
            "when was ",
            "when is ",
            "when's ",
            // How (quantitative)
            "how many ",
            "how much ",
            "how old ",
            "how long ",
            "how far ",
            "how tall ",
            "how big ",
            // Yes/no factual
            "is it ",
            "is there ",
            "are there ",
            "can you tell me ",
            "do you know ",
            // Definition / explanation (short)
            "define ",
            "explain ",
        ];

        factual_starters.iter().any(|s| lower.starts_with(s))
    }

    /// Detect goal-level complexity (multi-step work).
    ///
    /// Only triggers on imperative/command-style messages where the user is
    /// requesting the system to DO something complex. Conversational responses
    /// (answering questions, describing context) should NOT match.
    fn has_goal_indicators(msg: &str) -> bool {
        let lower = msg.to_lowercase();

        // Explicit goal prefix
        if lower.starts_with("goal:") || lower.starts_with("goal ") {
            return true;
        }

        // Imperative multi-step verbs — must START with the verb (command form).
        // "build me X" is a goal. "I am building X" is a conversation.
        if lower.starts_with("build ")
            || lower.starts_with("create a ")
            || lower.starts_with("set up ")
            || lower.starts_with("implement ")
            || lower.starts_with("design ")
            || lower.starts_with("plan ")
            || lower.starts_with("research and ")
            || lower.starts_with("write a report")
            || lower.starts_with("automate ")
            || lower.starts_with("make me ")
            || lower.starts_with("i want you to ")
            || lower.starts_with("i need you to ")
        {
            return true;
        }

        // Intent patterns — user expresses a desire or need for something complex.
        // "help me " is a superset of the old "help me build" pattern.
        if lower.starts_with("i want to ")
            || lower.starts_with("i need to ")
            || lower.starts_with("i'd like to ")
            || lower.starts_with("i would like to ")
            || lower.starts_with("help me ")
        {
            return true;
        }

        // Request patterns — user asks for complex help.
        // Note: "can you help me " is specific enough to avoid matching factual
        // questions like "can you tell me X?" (which is_factual_question handles).
        if lower.starts_with("can you help me ") || lower.starts_with("could you help me ") {
            return true;
        }

        // How-to patterns — user wants a process or plan.
        // These do NOT conflict with factual "how many/much/old/long/far/tall/big"
        // patterns because is_factual_question() runs first in classify().
        if lower.starts_with("how do i ")
            || lower.starts_with("how can i ")
            || lower.starts_with("how should i ")
        {
            return true;
        }

        false
    }

    /// Detect single-agent short task verbs.
    fn has_task_indicators(msg: &str) -> bool {
        let lower = msg.to_lowercase();
        lower.starts_with("summarize ")
            || lower.starts_with("draft ")
            || lower.starts_with("translate ")
            || lower.starts_with("rewrite ")
            || lower.starts_with("explain ")
            || lower.starts_with("compare ")
            || lower.starts_with("analyze ")
            || lower.starts_with("find ")
            || lower.starts_with("search ")
            || lower.starts_with("look up ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_message_is_quick() {
        let (class, conf) = UxClassifier::classify("", &[]);
        assert_eq!(class, UxClass::Quick);
        assert!((conf - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn whitespace_only_is_quick() {
        let (class, _) = UxClassifier::classify("   \t\n  ", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn classifies_https_url_as_intake() {
        let (class, conf) = UxClassifier::classify("https://example.com/article", &[]);
        assert_eq!(class, UxClass::Intake);
        assert!(conf > 0.9);
    }

    #[test]
    fn classifies_http_url_as_intake() {
        let (class, _) = UxClassifier::classify("http://localhost:8080/test", &[]);
        assert_eq!(class, UxClass::Intake);
    }

    #[test]
    fn classifies_bare_domain_as_intake() {
        let (class, _) = UxClassifier::classify("example.com", &[]);
        assert_eq!(class, UxClass::Intake);
    }

    #[test]
    fn classifies_url_with_scheme_as_intake() {
        let (class, _) = UxClassifier::classify("ftp://files.example.org/data.zip", &[]);
        assert_eq!(class, UxClass::Intake);
    }

    #[test]
    fn classifies_simple_question_as_quick() {
        let (class, _) = UxClassifier::classify("what time is it in Tokyo?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn classifies_short_chat_as_quick() {
        let (class, _) = UxClassifier::classify("thanks!", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn classifies_build_request_as_goal() {
        let (class, conf) = UxClassifier::classify("build me a landing page for my startup", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.8);
    }

    #[test]
    fn classifies_research_and_report_as_goal() {
        let (class, _) =
            UxClassifier::classify("research and write a report on competitor pricing", &[]);
        assert_eq!(class, UxClass::Goal);
    }

    #[test]
    fn classifies_explicit_goal_prefix_as_goal() {
        let (class, _) = UxClassifier::classify("goal: migrate the database to postgres", &[]);
        assert_eq!(class, UxClass::Goal);
    }

    #[test]
    fn long_conversational_message_is_quick_not_goal() {
        // Long messages that are answering a question should NOT be goals.
        let long = "I am working on 4 projects actively, one is a virtual copy of real influencers saas tool, second one is symbiotic itself, third one is clapp which is an event app. I have been training for health for a long time but stopped recently. I want to get back to it.";
        let (class, _) = UxClassifier::classify(long, &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn classifies_summarize_as_short_task() {
        let (class, conf) = UxClassifier::classify("summarize this article for me", &[]);
        assert_eq!(class, UxClass::ShortTask);
        assert!(conf >= 0.7);
    }

    #[test]
    fn classifies_translate_as_short_task() {
        let (class, _) = UxClassifier::classify("translate this to Spanish", &[]);
        assert_eq!(class, UxClass::ShortTask);
    }

    #[test]
    fn classifies_draft_as_short_task() {
        let (class, _) = UxClassifier::classify("draft a reply to this email", &[]);
        assert_eq!(class, UxClass::ShortTask);
    }

    #[test]
    fn classifies_explain_as_short_task() {
        let (class, _) = UxClassifier::classify("explain quantum entanglement", &[]);
        assert_eq!(class, UxClass::ShortTask);
    }

    #[test]
    fn classifies_split_as_routing() {
        let (class, conf) = UxClassifier::classify("split this into a new thread", &[]);
        assert_eq!(class, UxClass::Routing);
        assert!(conf >= 0.9);
    }

    #[test]
    fn classifies_merge_as_routing() {
        let (class, _) = UxClassifier::classify("merge these two threads", &[]);
        assert_eq!(class, UxClass::Routing);
    }

    #[test]
    fn classifies_move_to_as_routing() {
        let (class, _) = UxClassifier::classify("move to the design thread", &[]);
        assert_eq!(class, UxClass::Routing);
    }

    #[test]
    fn classifies_create_thread_as_routing() {
        let (class, _) = UxClassifier::classify("create thread for deployment", &[]);
        assert_eq!(class, UxClass::Routing);
    }

    #[test]
    fn classifies_archive_thread_as_routing() {
        let (class, _) = UxClassifier::classify("archive thread old-project", &[]);
        assert_eq!(class, UxClass::Routing);
    }

    #[test]
    fn url_takes_priority_over_task_verbs() {
        // "find" is a task verb, but a bare domain should still be Intake
        let (class, _) = UxClassifier::classify("find.example.com", &[]);
        assert_eq!(class, UxClass::Intake);
    }

    #[test]
    fn sentence_ending_dot_is_not_url() {
        // "thanks." ends with a dot but is not a URL
        let (class, _) = UxClassifier::classify("thanks.", &[]);
        // Single word with trailing dot — not intake, should be quick
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn confidence_ranges_are_valid() {
        let test_cases = vec![
            "",
            "hello?",
            "https://x.com/post/123",
            "split this thread",
            "build a website",
            "summarize the doc",
        ];
        for msg in test_cases {
            let (_, conf) = UxClassifier::classify(msg, &[]);
            assert!(
                (0.0..=1.0).contains(&conf),
                "confidence {conf} out of range for message: {msg:?}"
            );
        }
    }

    #[test]
    fn classify_with_active_threads_does_not_panic() {
        // Phase 2 will use active_threads for FollowUp detection.
        // For now, verify it doesn't panic.
        let threads = vec!["thread-abc", "thread-def"];
        let (class, _) = UxClassifier::classify("hello", &threads);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn create_a_is_goal_not_task() {
        // "create a" triggers goal, not short-task
        let (class, _) = UxClassifier::classify("create a new CI pipeline", &[]);
        assert_eq!(class, UxClass::Goal);
    }

    #[test]
    fn search_is_short_task() {
        let (class, _) = UxClassifier::classify("search for rust async patterns", &[]);
        assert_eq!(class, UxClass::ShortTask);
    }

    #[test]
    fn look_up_is_short_task() {
        let (class, _) = UxClassifier::classify("look up the API rate limits", &[]);
        assert_eq!(class, UxClass::ShortTask);
    }

    #[test]
    fn automate_is_goal() {
        let (class, _) = UxClassifier::classify("automate our deployment process", &[]);
        assert_eq!(class, UxClass::Goal);
    }

    #[test]
    fn implement_is_goal() {
        let (class, _) = UxClassifier::classify("implement OAuth2 for the admin panel", &[]);
        assert_eq!(class, UxClass::Goal);
    }

    #[test]
    fn set_up_is_goal() {
        let (class, _) = UxClassifier::classify("set up monitoring for production", &[]);
        assert_eq!(class, UxClass::Goal);
    }

    #[test]
    fn routing_is_case_insensitive() {
        let (class, _) = UxClassifier::classify("Split this conversation", &[]);
        assert_eq!(class, UxClass::Routing);
    }

    #[test]
    fn label_returns_correct_strings() {
        assert_eq!(UxClass::Quick.label(), "quick");
        assert_eq!(UxClass::ShortTask.label(), "short_task");
        assert_eq!(UxClass::Goal.label(), "goal");
        assert_eq!(
            UxClass::FollowUp {
                thread_id: "t1".to_string()
            }
            .label(),
            "follow_up"
        );
        assert_eq!(UxClass::Routing.label(), "routing");
        assert_eq!(UxClass::Intake.label(), "intake");
    }

    #[test]
    fn display_matches_label() {
        assert_eq!(format!("{}", UxClass::Quick), "quick");
        assert_eq!(format!("{}", UxClass::ShortTask), "short_task");
        assert_eq!(format!("{}", UxClass::Goal), "goal");
        assert_eq!(format!("{}", UxClass::Intake), "intake");
        assert_eq!(format!("{}", UxClass::Routing), "routing");
        assert_eq!(
            format!(
                "{}",
                UxClass::FollowUp {
                    thread_id: "t1".to_string()
                }
            ),
            "follow_up"
        );
    }

    // --- Factual question fast-path tests ---

    #[test]
    fn factual_what_is_quick() {
        let (class, conf) = UxClassifier::classify("What is the capital of Japan?", &[]);
        assert_eq!(class, UxClass::Quick);
        assert!(conf >= 0.9, "factual question should have high confidence");
    }

    #[test]
    fn factual_who_is_quick() {
        let (class, conf) = UxClassifier::classify("Who is the president of France?", &[]);
        assert_eq!(class, UxClass::Quick);
        assert!(conf >= 0.9);
    }

    #[test]
    fn factual_where_is_quick() {
        let (class, _) = UxClassifier::classify("Where is the Eiffel Tower?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_when_was_quick() {
        let (class, _) = UxClassifier::classify("When was the first moon landing?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_how_many_quick() {
        let (class, _) = UxClassifier::classify("How many continents are there?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_how_much_quick() {
        let (class, _) = UxClassifier::classify("How much does a mass of water weigh?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_how_old_quick() {
        let (class, _) = UxClassifier::classify("How old is the universe?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_is_there_quick() {
        let (class, _) = UxClassifier::classify("Are there any planets with rings?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_define_quick() {
        let (class, _) = UxClassifier::classify("Define photosynthesis?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_explain_short_quick() {
        // Short "explain X?" with question mark → factual fast-path (Quick).
        // Contrast: "explain quantum entanglement" (no ?) → ShortTask.
        let (class, _) = UxClassifier::classify("Explain quantum entanglement?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_whats_quick() {
        let (class, _) = UxClassifier::classify("What's the speed of light?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_whos_quick() {
        let (class, _) = UxClassifier::classify("Who's the CEO of Apple?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn long_what_question_is_not_factual() {
        // >20 words — complex question, should NOT be fast-pathed as factual.
        let long = "What is the best strategy to grow my startup over the next 5 years considering the current market conditions and competitive landscape in AI?";
        let (class, _) = UxClassifier::classify(long, &[]);
        // Should fall through to default Quick (0.6) since it doesn't match
        // goal indicators either — but NOT the factual fast-path (0.9).
        assert_eq!(class, UxClass::Quick);
        let (_, conf) = UxClassifier::classify(long, &[]);
        assert!(
            conf < 0.9,
            "long question should not get factual fast-path confidence"
        );
    }

    #[test]
    fn factual_question_without_question_mark_not_fast_pathed() {
        // No "?" → does not trigger factual fast-path.
        let (_, conf) = UxClassifier::classify("What is the capital of Japan", &[]);
        // Falls through to default Quick with 0.6 confidence, not 0.9.
        assert!(
            conf < 0.9,
            "missing question mark should not trigger factual fast-path"
        );
    }

    #[test]
    fn factual_how_long_quick() {
        let (class, _) = UxClassifier::classify("How long is the Great Wall of China?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_can_you_tell_me_quick() {
        let (class, _) = UxClassifier::classify("Can you tell me the time in London?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn factual_do_you_know_quick() {
        let (class, _) = UxClassifier::classify("Do you know the population of Tokyo?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn design_imperative_still_goal() {
        // "design" as imperative command (no ?) should still be Goal.
        let (class, _) = UxClassifier::classify("design a new logo for my brand", &[]);
        assert_eq!(class, UxClass::Goal);
    }

    #[test]
    fn plan_imperative_still_goal() {
        // "plan" as imperative command should still be Goal.
        let (class, _) = UxClassifier::classify("plan our Q3 roadmap", &[]);
        assert_eq!(class, UxClass::Goal);
    }

    #[test]
    fn explain_without_question_mark_still_short_task() {
        // "explain X" without ? → ShortTask (existing behavior preserved).
        let (class, _) = UxClassifier::classify("explain the theory of relativity", &[]);
        assert_eq!(class, UxClass::ShortTask);
    }

    // --- Intent pattern goal tests ---

    #[test]
    fn i_want_to_is_goal() {
        let (class, conf) = UxClassifier::classify("I want to improve my morning routine", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    #[test]
    fn i_need_to_is_goal() {
        let (class, conf) = UxClassifier::classify("I need to organize my finances", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    #[test]
    fn id_like_to_is_goal() {
        let (class, conf) = UxClassifier::classify("I'd like to learn Spanish", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    #[test]
    fn i_would_like_to_is_goal() {
        let (class, conf) = UxClassifier::classify("I would like to start a side project", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    #[test]
    fn help_me_is_goal() {
        let (class, conf) = UxClassifier::classify("Help me plan my vacation", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    #[test]
    fn help_me_build_still_goal() {
        // "help me build" was previously its own pattern; now covered by "help me ".
        let (class, conf) = UxClassifier::classify("help me build a portfolio website", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    // --- Request pattern goal tests ---

    #[test]
    fn can_you_help_me_is_goal() {
        let (class, conf) = UxClassifier::classify("Can you help me design a workout plan", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    #[test]
    fn could_you_help_me_is_goal() {
        let (class, conf) = UxClassifier::classify("Could you help me restructure my resume", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    // --- How-to pattern goal tests ---

    #[test]
    fn how_do_i_is_goal() {
        let (class, conf) = UxClassifier::classify("How do I start investing", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    #[test]
    fn how_can_i_is_goal() {
        let (class, conf) = UxClassifier::classify("How can I improve my sleep schedule", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    #[test]
    fn how_should_i_is_goal() {
        let (class, conf) = UxClassifier::classify("How should I approach learning Rust", &[]);
        assert_eq!(class, UxClass::Goal);
        assert!(conf >= 0.75);
    }

    // --- Negative cases: ensure new patterns don't break existing classification ---

    #[test]
    fn weather_question_still_quick() {
        let (class, _) = UxClassifier::classify("What is the weather?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn summarize_still_short_task() {
        let (class, _) = UxClassifier::classify("Summarize this article", &[]);
        assert_eq!(class, UxClass::ShortTask);
    }

    #[test]
    fn how_many_still_factual_quick() {
        // "how many" is a factual question, not a how-to goal pattern.
        let (class, conf) = UxClassifier::classify("How many countries are in Europe?", &[]);
        assert_eq!(class, UxClass::Quick);
        assert!(
            conf >= 0.9,
            "factual 'how many' should stay Quick with high conf"
        );
    }

    #[test]
    fn how_much_still_factual_quick() {
        let (class, _) = UxClassifier::classify("How much does an iPhone cost?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn can_you_tell_me_still_factual_quick() {
        // "can you tell me" is a factual question, not "can you help me" goal.
        let (class, _) = UxClassifier::classify("Can you tell me the capital of France?", &[]);
        assert_eq!(class, UxClass::Quick);
    }

    #[test]
    fn long_conversational_i_want_not_goal() {
        // "I want to" buried in a long conversational message should NOT be Goal
        // because it doesn't start with "I want to".
        let long = "I am working on multiple projects and I want to get back to fitness training.";
        let (class, _) = UxClassifier::classify(long, &[]);
        assert_eq!(class, UxClass::Quick);
    }
}
