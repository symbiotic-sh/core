//! Brain Bootstrap goal — auto-starts after onboarding conversation reaches
//! critical mass, demonstrating the full product mechanic:
//! conversation -> classification -> goal promotion -> plan -> execution.
//!
//! The bootstrap goal tracks what the brain has learned and offers next steps
//! through a plan card. It progresses through tiers (Onboarding -> Ambient ->
//! Connectors -> Passive -> PowerImport) at the user's own pace.
//!
//! Persistence: state is stored at `{data_dir}/brain/bootstrap_state.json`.
//!
//! See `docs/design/memory-system.md` §Brain Bootstrap for the full spec.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::events::{DaemonEvent, EventType};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Bootstrap tier — describes the current depth of brain-filling activity.
///
/// These are NOT time-locked stages. The user controls the pace:
/// - Power through all tiers in 20 minutes, or
/// - Take it slow over a week with ambient questions, or
/// - Skip everything and let passive extraction do the work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapTier {
    /// First 3-5 min: role, projects, stack, priorities (~10-15 core facts).
    #[default]
    Onboarding,
    /// First week (or all at once): 2-3 questions/day, key people, tools,
    /// routines (~50-100 facts).
    Ambient,
    /// Optional: OAuth for calendar, email, GitHub (~200-5000 facts).
    Connectors,
    /// Ongoing: extract from natural conversation. No explicit questions.
    Passive,
    /// Optional: CLI import of past AI sessions (~200-1000 facts from history).
    PowerImport,
}

/// State machine for the bootstrap lifecycle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapState {
    /// Bootstrap has not started (pre-greeting or greeting just sent).
    #[default]
    NotStarted,
    /// User is answering onboarding questions in the stream thread.
    InterviewActive,
    /// Plan card has been shown with steps the user can approve/skip.
    PlanProposed,
    /// User approved the plan; running connectors, entity generation, etc.
    ExecutingSteps,
    /// Background ambient question mode (2-3 questions/day for first week).
    AmbientMode,
    /// Bootstrap is complete; only passive extraction from now on.
    Complete,
}

/// A single step in the Brain Bootstrap plan card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapPlanStep {
    /// Short name shown in the plan card (e.g. "Learn your basics").
    pub name: String,
    /// One-line description (e.g. "Role, projects, stack, priorities").
    pub description: String,
    /// Status: "completed", "pending", "skipped", "in_progress".
    pub status: String,
}

/// The Brain Bootstrap plan — rendered as a plan card in the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BootstrapPlan {
    pub title: String,
    pub summary: String,
    pub steps: Vec<BootstrapPlanStep>,
    /// Confidence that this plan is appropriate (0.0-1.0).
    pub confidence: f64,
}

/// Persistent state for the Brain Bootstrap goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainBootstrapGoal {
    /// Current lifecycle state.
    pub state: BootstrapState,
    /// Total facts extracted so far.
    pub facts_extracted: u32,
    /// Total entities created (people, projects, tools, etc.).
    pub entities_created: u32,
    /// Current bootstrap tier/depth.
    pub tier: BootstrapTier,
    /// Which plan steps the user has approved (by index).
    pub approved_steps: Vec<usize>,
    /// Which plan steps the user has skipped (by index).
    pub skipped_steps: Vec<usize>,
    /// Ambient questions already asked (to avoid repeats).
    pub asked_questions: Vec<String>,
}

impl Default for BrainBootstrapGoal {
    fn default() -> Self {
        Self {
            state: BootstrapState::NotStarted,
            facts_extracted: 0,
            entities_created: 0,
            tier: BootstrapTier::Onboarding,
            approved_steps: Vec::new(),
            skipped_steps: Vec::new(),
            asked_questions: Vec::new(),
        }
    }
}

impl BrainBootstrapGoal {
    /// Create the Brain Bootstrap plan card.
    ///
    /// This generates the plan the user sees after enough onboarding
    /// conversation. Steps are approve-or-skip; the user controls the pace.
    pub fn create_plan(facts_extracted: u32) -> BootstrapPlan {
        let basics_status = if facts_extracted >= 5 {
            "completed"
        } else {
            "in_progress"
        };

        BootstrapPlan {
            title: "Brain Bootstrap".to_string(),
            summary: "Build your personal knowledge graph".to_string(),
            steps: vec![
                BootstrapPlanStep {
                    name: "Learn your basics".to_string(),
                    description: "Role, projects, stack, priorities".to_string(),
                    status: basics_status.to_string(),
                },
                BootstrapPlanStep {
                    name: "Connect calendar".to_string(),
                    description: "Import events and contacts (optional)".to_string(),
                    status: "pending".to_string(),
                },
                BootstrapPlanStep {
                    name: "Import email contacts".to_string(),
                    description: "Build your people graph (optional)".to_string(),
                    status: "pending".to_string(),
                },
                BootstrapPlanStep {
                    name: "Set up daily brain-fill".to_string(),
                    description: "2-3 questions/day for the first week".to_string(),
                    status: "pending".to_string(),
                },
                BootstrapPlanStep {
                    name: "Generate entity profiles".to_string(),
                    description: "Create profiles for people, projects, tools".to_string(),
                    status: "pending".to_string(),
                },
            ],
            confidence: 0.9,
        }
    }

    /// Check if the onboarding conversation has enough content to promote
    /// to a goal with a plan card.
    ///
    /// Returns `true` after ~5 user messages in the stream thread OR when
    /// at least 10 facts have been extracted (whichever comes first).
    pub fn should_promote_to_goal(message_count: usize, facts_extracted: u32) -> bool {
        message_count >= 5 || facts_extracted >= 10
    }

    /// Generate the next ambient question based on what the brain already knows.
    ///
    /// Adapts to known context: if the user mentioned Stripe in their tech
    /// stack, don't ask about payment providers. Returns `None` when all
    /// ambient questions have been exhausted.
    pub fn next_ambient_question(
        known_entities: &[String],
        known_facts: &[String],
        asked_questions: &[String],
    ) -> Option<String> {
        // Ambient question bank — ordered by importance.
        // Each tuple: (question, skip_if_any_keyword_in_facts_or_entities).
        let question_bank: &[(&str, &[&str])] = &[
            (
                "Who are the key people you work with? (co-founders, clients, collaborators)",
                &["co-founder", "partner", "collaborator", "client", "team"],
            ),
            (
                "What tools do you use daily? (IDE, project management, communication)",
                &["vscode", "jetbrains", "slack", "notion", "jira", "linear"],
            ),
            (
                "What's your typical work routine? (hours, focus blocks, meetings)",
                &["routine", "schedule", "morning", "focus block", "standup"],
            ),
            (
                "Any personal goals or health habits you're tracking?",
                &["exercise", "meditation", "sleep", "health", "fitness"],
            ),
            (
                "What's your communication style preference? (async vs sync, detailed vs brief)",
                &[
                    "async",
                    "sync",
                    "communication style",
                    "prefer email",
                    "prefer slack",
                ],
            ),
            (
                "What domains or topics do you follow closely?",
                &["follow", "newsletter", "blog", "podcast", "industry news"],
            ),
            (
                "What's been the biggest challenge in your work recently?",
                &["challenge", "bottleneck", "blocker", "struggle"],
            ),
            (
                "Are there recurring tasks you wish were automated?",
                &[
                    "automate",
                    "automation",
                    "recurring task",
                    "repetitive",
                    "cron",
                ],
            ),
        ];

        let all_context: Vec<String> = known_entities
            .iter()
            .chain(known_facts.iter())
            .map(|s| s.to_lowercase())
            .collect();

        for (question, skip_keywords) in question_bank {
            // Skip if already asked.
            if asked_questions.iter().any(|q| q == question) {
                continue;
            }

            // Skip if the brain already knows about this topic.
            let already_known = skip_keywords.iter().any(|keyword| {
                all_context
                    .iter()
                    .any(|ctx| ctx.contains(&keyword.to_lowercase()))
            });

            if !already_known {
                return Some(question.to_string());
            }
        }

        None
    }

    /// Update progress counters and advance the tier when appropriate.
    pub fn update_progress(&mut self, new_facts: u32, new_entities: u32) {
        self.facts_extracted = self.facts_extracted.saturating_add(new_facts);
        self.entities_created = self.entities_created.saturating_add(new_entities);

        // Auto-advance tier based on accumulated knowledge.
        self.tier = Self::tier_for_counts(self.facts_extracted, self.entities_created);
    }

    /// Determine the appropriate tier based on fact/entity counts.
    fn tier_for_counts(facts: u32, entities: u32) -> BootstrapTier {
        if facts >= 200 || entities >= 50 {
            BootstrapTier::Passive
        } else if facts >= 50 || entities >= 15 {
            BootstrapTier::Ambient
        } else {
            BootstrapTier::Onboarding
        }
    }

    /// Transition the state machine. Returns `true` if the transition was valid.
    pub fn transition_to(&mut self, new_state: BootstrapState) -> bool {
        let valid = match (&self.state, &new_state) {
            (BootstrapState::NotStarted, BootstrapState::InterviewActive) => true,
            (BootstrapState::InterviewActive, BootstrapState::PlanProposed) => true,
            (BootstrapState::PlanProposed, BootstrapState::ExecutingSteps) => true,
            (BootstrapState::PlanProposed, BootstrapState::AmbientMode) => true,
            // User can skip plan entirely.
            (BootstrapState::PlanProposed, BootstrapState::Complete) => true,
            (BootstrapState::ExecutingSteps, BootstrapState::AmbientMode) => true,
            (BootstrapState::ExecutingSteps, BootstrapState::Complete) => true,
            (BootstrapState::AmbientMode, BootstrapState::Complete) => true,
            _ => false,
        };

        if valid {
            self.state = new_state;
        }
        valid
    }

    /// Build the `goal.plan.proposed` DaemonEvent for the Brain Bootstrap.
    ///
    /// The event's `detail` field contains the plan as JSON (matching the
    /// ProposedPlan format the app already renders). The `goal_id` is a
    /// stable identifier so all bootstrap events group together.
    pub fn create_plan_proposed_event(&self, thread_id: Option<&str>) -> Result<DaemonEvent> {
        let plan = Self::create_plan(self.facts_extracted);

        // Serialize plan into the same JSON shape the app expects for
        // goal.plan.proposed events (steps + confidence + summary).
        let plan_json =
            serde_json::to_string(&plan).context("failed to serialize Brain Bootstrap plan")?;

        Ok(DaemonEvent {
            event_type: EventType::GoalPlanProposed,
            status: "proposed".to_string(),
            job_id: None,
            detail: plan_json,
            goal_room: None,
            goal_template: Some("brain-bootstrap".to_string()),
            goal_run_id: None,
            goal_id: Some("brain-bootstrap".to_string()),
            intake_run_id: None,
            url: None,
            title: Some(plan.title),
            sensitivity: None,
            quick_replies: Some(
                serde_json::to_string(&["Approve", "Edit", "Skip to chatting"]).unwrap_or_default(),
            ),
            thread_id: thread_id.map(|s| s.to_string()),
        })
    }

    /// Build a progress update event showing current bootstrap stats.
    pub fn create_progress_event(&self, thread_id: Option<&str>) -> DaemonEvent {
        let detail = format!(
            "facts={} entities={} tier={:?} state={:?}",
            self.facts_extracted, self.entities_created, self.tier, self.state,
        );

        DaemonEvent {
            event_type: EventType::GoalProgress,
            status: "running".to_string(),
            job_id: None,
            detail,
            goal_room: None,
            goal_template: Some("brain-bootstrap".to_string()),
            goal_run_id: None,
            goal_id: Some("brain-bootstrap".to_string()),
            intake_run_id: None,
            url: None,
            title: Some("Brain Bootstrap".to_string()),
            sensitivity: None,
            quick_replies: None,
            thread_id: thread_id.map(|s| s.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Path to the bootstrap state file within a data directory.
pub fn state_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("brain").join("bootstrap_state.json")
}

/// Load bootstrap state from disk. Returns default state if the file
/// does not exist or is unreadable.
pub fn load_state(data_dir: &Path) -> BrainBootstrapGoal {
    let path = state_file_path(data_dir);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => BrainBootstrapGoal::default(),
    }
}

/// Persist bootstrap state to disk. Creates the parent directory if needed.
pub fn save_state(data_dir: &Path, state: &BrainBootstrapGoal) -> Result<()> {
    let path = state_file_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create brain directory {}", parent.display()))?;
    }

    let json =
        serde_json::to_string_pretty(state).context("failed to serialize bootstrap state")?;

    // Atomic write via temp file.
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, &json)
        .with_context(|| format!("failed to write bootstrap state to {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- should_promote_to_goal ---

    #[test]
    fn promote_after_five_messages() {
        assert!(BrainBootstrapGoal::should_promote_to_goal(5, 0));
        assert!(BrainBootstrapGoal::should_promote_to_goal(10, 0));
    }

    #[test]
    fn promote_after_ten_facts() {
        assert!(BrainBootstrapGoal::should_promote_to_goal(0, 10));
        assert!(BrainBootstrapGoal::should_promote_to_goal(2, 15));
    }

    #[test]
    fn no_promote_below_thresholds() {
        assert!(!BrainBootstrapGoal::should_promote_to_goal(0, 0));
        assert!(!BrainBootstrapGoal::should_promote_to_goal(4, 9));
        assert!(!BrainBootstrapGoal::should_promote_to_goal(3, 5));
    }

    #[test]
    fn promote_at_exact_thresholds() {
        // Exactly 5 messages.
        assert!(BrainBootstrapGoal::should_promote_to_goal(5, 0));
        // Exactly 10 facts.
        assert!(BrainBootstrapGoal::should_promote_to_goal(0, 10));
    }

    // --- create_plan ---

    #[test]
    fn create_plan_has_five_steps() {
        let plan = BrainBootstrapGoal::create_plan(0);
        assert_eq!(plan.steps.len(), 5);
        assert_eq!(plan.title, "Brain Bootstrap");
        assert!((plan.confidence - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn create_plan_marks_basics_completed_when_enough_facts() {
        let plan = BrainBootstrapGoal::create_plan(5);
        assert_eq!(plan.steps[0].status, "completed");
    }

    #[test]
    fn create_plan_marks_basics_in_progress_when_few_facts() {
        let plan = BrainBootstrapGoal::create_plan(3);
        assert_eq!(plan.steps[0].status, "in_progress");
    }

    #[test]
    fn create_plan_other_steps_are_pending() {
        let plan = BrainBootstrapGoal::create_plan(10);
        for step in &plan.steps[1..] {
            assert_eq!(step.status, "pending");
        }
    }

    #[test]
    fn create_plan_serializes_to_valid_json() {
        let plan = BrainBootstrapGoal::create_plan(7);
        let json = serde_json::to_string(&plan).expect("plan should serialize");
        assert!(json.contains("Brain Bootstrap"));
        assert!(json.contains("Learn your basics"));

        // Round-trip.
        let deserialized: BootstrapPlan =
            serde_json::from_str(&json).expect("plan should deserialize");
        assert_eq!(deserialized.steps.len(), 5);
    }

    // --- next_ambient_question ---

    #[test]
    fn ambient_question_returns_first_when_nothing_known() {
        let q = BrainBootstrapGoal::next_ambient_question(&[], &[], &[]);
        assert!(q.is_some());
        assert!(q.unwrap().contains("key people"));
    }

    #[test]
    fn ambient_question_skips_known_topics() {
        let entities = vec!["Alice (co-founder)".to_string()];
        let facts = vec![];
        let q = BrainBootstrapGoal::next_ambient_question(&entities, &facts, &[]);
        assert!(q.is_some());
        // Should skip the "key people" question since "co-founder" is known.
        assert!(!q.unwrap().contains("key people"));
    }

    #[test]
    fn ambient_question_skips_already_asked() {
        let asked = vec![
            "Who are the key people you work with? (co-founders, clients, collaborators)"
                .to_string(),
        ];
        let q = BrainBootstrapGoal::next_ambient_question(&[], &[], &asked);
        assert!(q.is_some());
        // Should return the second question.
        assert!(q.unwrap().contains("tools"));
    }

    #[test]
    fn ambient_question_returns_none_when_all_exhausted() {
        // Mark all topics as known.
        let entities = vec![
            "co-founder".to_string(),
            "vscode".to_string(),
            "morning routine".to_string(),
            "exercise".to_string(),
            "async communication style".to_string(),
            "newsletter".to_string(),
            "challenge".to_string(),
            "automation".to_string(),
        ];
        let q = BrainBootstrapGoal::next_ambient_question(&entities, &[], &[]);
        assert!(q.is_none());
    }

    #[test]
    fn ambient_question_adapts_to_facts() {
        // User mentioned Slack in facts -> tools question should be skipped.
        let facts = vec!["Uses Slack for team communication".to_string()];
        let q = BrainBootstrapGoal::next_ambient_question(&[], &facts, &[]);
        assert!(q.is_some());
        // "tools" question should be skipped because "slack" is in facts.
        let question = q.unwrap();
        assert!(!question.contains("tools do you use"));
    }

    // --- State transitions ---

    #[test]
    fn valid_transitions() {
        let mut goal = BrainBootstrapGoal::default();
        assert_eq!(goal.state, BootstrapState::NotStarted);

        assert!(goal.transition_to(BootstrapState::InterviewActive));
        assert_eq!(goal.state, BootstrapState::InterviewActive);

        assert!(goal.transition_to(BootstrapState::PlanProposed));
        assert_eq!(goal.state, BootstrapState::PlanProposed);

        assert!(goal.transition_to(BootstrapState::ExecutingSteps));
        assert_eq!(goal.state, BootstrapState::ExecutingSteps);

        assert!(goal.transition_to(BootstrapState::AmbientMode));
        assert_eq!(goal.state, BootstrapState::AmbientMode);

        assert!(goal.transition_to(BootstrapState::Complete));
        assert_eq!(goal.state, BootstrapState::Complete);
    }

    #[test]
    fn invalid_transition_rejected() {
        let mut goal = BrainBootstrapGoal::default();

        // Cannot go directly from NotStarted to PlanProposed.
        assert!(!goal.transition_to(BootstrapState::PlanProposed));
        assert_eq!(goal.state, BootstrapState::NotStarted);

        // Cannot go from NotStarted to Complete.
        assert!(!goal.transition_to(BootstrapState::Complete));
        assert_eq!(goal.state, BootstrapState::NotStarted);
    }

    #[test]
    fn skip_plan_to_complete() {
        let mut goal = BrainBootstrapGoal::default();
        goal.transition_to(BootstrapState::InterviewActive);
        goal.transition_to(BootstrapState::PlanProposed);

        // User hits "Skip to chatting" on plan card.
        assert!(goal.transition_to(BootstrapState::Complete));
        assert_eq!(goal.state, BootstrapState::Complete);
    }

    #[test]
    fn plan_proposed_to_ambient() {
        let mut goal = BrainBootstrapGoal::default();
        goal.transition_to(BootstrapState::InterviewActive);
        goal.transition_to(BootstrapState::PlanProposed);

        // User approves just the ambient questions step.
        assert!(goal.transition_to(BootstrapState::AmbientMode));
        assert_eq!(goal.state, BootstrapState::AmbientMode);
    }

    // --- Progress tracking ---

    #[test]
    fn update_progress_accumulates() {
        let mut goal = BrainBootstrapGoal::default();
        goal.update_progress(5, 2);
        assert_eq!(goal.facts_extracted, 5);
        assert_eq!(goal.entities_created, 2);

        goal.update_progress(10, 3);
        assert_eq!(goal.facts_extracted, 15);
        assert_eq!(goal.entities_created, 5);
    }

    #[test]
    fn update_progress_advances_tier() {
        let mut goal = BrainBootstrapGoal::default();
        assert_eq!(goal.tier, BootstrapTier::Onboarding);

        goal.update_progress(50, 0);
        assert_eq!(goal.tier, BootstrapTier::Ambient);

        goal.update_progress(150, 0);
        assert_eq!(goal.tier, BootstrapTier::Passive);
    }

    #[test]
    fn tier_advances_on_entity_count() {
        let mut goal = BrainBootstrapGoal::default();

        goal.update_progress(0, 15);
        assert_eq!(goal.tier, BootstrapTier::Ambient);

        goal.update_progress(0, 35);
        assert_eq!(goal.tier, BootstrapTier::Passive);
    }

    #[test]
    fn progress_saturates_instead_of_overflowing() {
        let mut goal = BrainBootstrapGoal {
            facts_extracted: u32::MAX - 5,
            ..BrainBootstrapGoal::default()
        };
        goal.update_progress(100, 0);
        assert_eq!(goal.facts_extracted, u32::MAX);
    }

    // --- Plan proposed event ---

    #[test]
    fn plan_proposed_event_is_valid() {
        let goal = BrainBootstrapGoal {
            state: BootstrapState::InterviewActive,
            facts_extracted: 12,
            entities_created: 3,
            tier: BootstrapTier::Onboarding,
            approved_steps: Vec::new(),
            skipped_steps: Vec::new(),
            asked_questions: Vec::new(),
        };

        let event = goal
            .create_plan_proposed_event(Some("thread-stream"))
            .expect("event should build");

        assert_eq!(event.event_type, EventType::GoalPlanProposed);
        assert_eq!(event.status, "proposed");
        assert_eq!(event.goal_id, Some("brain-bootstrap".to_string()));
        assert_eq!(event.goal_template, Some("brain-bootstrap".to_string()));
        assert_eq!(event.thread_id, Some("thread-stream".to_string()));
        assert_eq!(event.title, Some("Brain Bootstrap".to_string()));

        // Detail should be valid JSON.
        let plan: BootstrapPlan =
            serde_json::from_str(&event.detail).expect("detail should be valid plan JSON");
        assert_eq!(plan.steps.len(), 5);
        assert_eq!(plan.steps[0].status, "completed"); // 12 facts >= 5

        // Quick replies should be present.
        assert!(event.quick_replies.is_some());
        let replies: Vec<String> =
            serde_json::from_str(event.quick_replies.as_ref().unwrap()).unwrap();
        assert_eq!(replies.len(), 3);
        assert!(replies.contains(&"Approve".to_string()));
    }

    #[test]
    fn plan_proposed_event_without_thread_id() {
        let goal = BrainBootstrapGoal::default();
        let event = goal
            .create_plan_proposed_event(None)
            .expect("event should build");
        assert!(event.thread_id.is_none());
    }

    // --- Progress event ---

    #[test]
    fn progress_event_contains_stats() {
        let mut goal = BrainBootstrapGoal::default();
        goal.update_progress(25, 8);

        let event = goal.create_progress_event(Some("thread-stream"));
        assert_eq!(event.event_type, EventType::GoalProgress);
        assert_eq!(event.goal_id, Some("brain-bootstrap".to_string()));
        assert!(event.detail.contains("facts=25"));
        assert!(event.detail.contains("entities=8"));
    }

    // --- Persistence ---

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut goal = BrainBootstrapGoal::default();
        goal.update_progress(42, 7);
        goal.transition_to(BootstrapState::InterviewActive);
        goal.asked_questions
            .push("What tools do you use daily?".to_string());

        save_state(dir.path(), &goal).expect("save should succeed");
        let loaded = load_state(dir.path());

        assert_eq!(loaded.facts_extracted, 42);
        assert_eq!(loaded.entities_created, 7);
        assert_eq!(loaded.state, BootstrapState::InterviewActive);
        assert_eq!(loaded.asked_questions.len(), 1);
    }

    #[test]
    fn load_state_returns_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_state(dir.path());
        assert_eq!(loaded.state, BootstrapState::NotStarted);
        assert_eq!(loaded.facts_extracted, 0);
    }

    #[test]
    fn load_state_returns_default_on_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let brain_dir = dir.path().join("brain");
        fs::create_dir_all(&brain_dir).unwrap();
        fs::write(brain_dir.join("bootstrap_state.json"), "not valid json").unwrap();

        let loaded = load_state(dir.path());
        assert_eq!(loaded.state, BootstrapState::NotStarted);
    }

    #[test]
    fn state_file_path_is_correct() {
        let path = state_file_path(Path::new("/data"));
        assert_eq!(path, PathBuf::from("/data/brain/bootstrap_state.json"));
    }

    // --- Serde round-trip for all enums ---

    #[test]
    fn bootstrap_tier_serde_round_trip() {
        let tiers = vec![
            BootstrapTier::Onboarding,
            BootstrapTier::Ambient,
            BootstrapTier::Connectors,
            BootstrapTier::Passive,
            BootstrapTier::PowerImport,
        ];
        for tier in tiers {
            let json = serde_json::to_string(&tier).unwrap();
            let back: BootstrapTier = serde_json::from_str(&json).unwrap();
            assert_eq!(back, tier);
        }
    }

    #[test]
    fn bootstrap_state_serde_round_trip() {
        let states = vec![
            BootstrapState::NotStarted,
            BootstrapState::InterviewActive,
            BootstrapState::PlanProposed,
            BootstrapState::ExecutingSteps,
            BootstrapState::AmbientMode,
            BootstrapState::Complete,
        ];
        for state in states {
            let json = serde_json::to_string(&state).unwrap();
            let back: BootstrapState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
        }
    }

    // --- Default values ---

    #[test]
    fn default_goal_is_not_started_onboarding() {
        let goal = BrainBootstrapGoal::default();
        assert_eq!(goal.state, BootstrapState::NotStarted);
        assert_eq!(goal.tier, BootstrapTier::Onboarding);
        assert_eq!(goal.facts_extracted, 0);
        assert_eq!(goal.entities_created, 0);
        assert!(goal.approved_steps.is_empty());
        assert!(goal.skipped_steps.is_empty());
        assert!(goal.asked_questions.is_empty());
    }
}
