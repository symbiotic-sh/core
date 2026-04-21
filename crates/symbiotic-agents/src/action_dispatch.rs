//! Action Dispatcher — routes reconciler actions to daemon subsystem calls.
//!
//! The control-plane reconciler produces `ReconciliationAction`s (desired state
//! diffs). This module provides the `ActionDispatcher` that routes each action
//! type to the appropriate daemon subsystem (goal lifecycle, agent execution,
//! skill loading, etc.).
//!
//! # Architecture
//!
//! ```text
//! Reconciler -> Vec<ReconciliationAction>
//!                    |
//!            ActionDispatcher::dispatch()
//!                    |
//!     +------+------+------+------+------+
//!     |      |      |      |      |      |
//!  GoalOps AgentOps SkillOps IdentityOps
//! ```
//!
//! The daemon implements the subsystem traits and injects them into the
//! dispatcher. The dispatcher handles error isolation (one failed action
//! does not abort the batch), logging, and metrics.
//!
//! # Dependency Notes
//!
//! `symbiotic-control-plane` depends on `symbiotic-agents` (for AgentPool),
//! so we cannot import control-plane types here without creating a cycle.
//! Instead, we define local `DispatchAction` / `DispatchActionType` types
//! that mirror the control-plane's `ReconciliationAction` / `ActionType`.
//! The daemon bridges the two using `DispatchAction::from()`.

use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Dispatch action types (local mirrors of control-plane types)
// ---------------------------------------------------------------------------

/// Action type enum — mirrors `symbiotic-control-plane`'s `ActionType` to
/// avoid a circular dependency.  The daemon converts between the two.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DispatchActionType {
    StartGoal,
    StopGoal,
    PauseGoal,
    ResumeGoal,
    AdvancePhase { from: String, to: String },
    SpawnAgents { goal: String, count: usize },
    ReloadIdentity,
    ReloadPreferences,
    LoadSkill { name: String },
    UnloadSkill { name: String },
}

/// A single reconciliation action to be dispatched.
///
/// This is the local mirror of the control-plane's `ReconciliationAction`.
/// The daemon converts from control-plane types using `From` impls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchAction {
    pub id: String,
    pub action_type: DispatchActionType,
    pub target: String,
    pub description: String,
    pub requires_approval: bool,
    pub estimated_cost: Option<f64>,
}

// Private type aliases used internally for readability.
type ReconciliationAction = DispatchAction;
type ActionType = DispatchActionType;

// ---------------------------------------------------------------------------
// Dispatch result
// ---------------------------------------------------------------------------

/// Outcome of dispatching a single reconciliation action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchResult {
    /// Action ID from the reconciliation action.
    pub action_id: String,
    /// Target slug/name from the action.
    pub target: String,
    /// What type of action was dispatched.
    pub action_type: String,
    /// Whether the dispatch succeeded.
    pub success: bool,
    /// Human-readable detail or error message.
    pub detail: String,
}

impl fmt::Display for DispatchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mark = if self.success { "OK" } else { "FAIL" };
        write!(
            f,
            "[{}] {}/{}: {}",
            mark, self.action_type, self.target, self.detail
        )
    }
}

/// Summary of a batch dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchDispatchSummary {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub results: Vec<DispatchResult>,
}

// ---------------------------------------------------------------------------
// Subsystem traits
// ---------------------------------------------------------------------------

/// Goal lifecycle operations (start, stop, pause, resume, advance phase).
///
/// The daemon implements this by writing to goal_state.tsv, updating the
/// GoalProcessManager, and queueing workflow runs.
#[async_trait]
pub trait GoalOps: Send + Sync {
    /// Start a new goal process. Should create the goal in the GPM,
    /// queue it through the deliberation pipeline, and record the state.
    async fn start_goal(&self, slug: &str) -> Result<String>;

    /// Stop a running goal. Should transition to terminal state, release
    /// agent slots, and cancel any queued work.
    async fn stop_goal(&self, slug: &str) -> Result<String>;

    /// Pause a running goal. Should transition to paused, release agent
    /// slots, but preserve state for later resumption.
    async fn pause_goal(&self, slug: &str) -> Result<String>;

    /// Resume a paused goal. Should transition to active and re-allocate
    /// agent slots.
    async fn resume_goal(&self, slug: &str) -> Result<String>;

    /// Advance a goal to a new execution phase.
    async fn advance_phase(&self, slug: &str, from: &str, to: &str) -> Result<String>;
}

/// Agent spawning and lifecycle operations.
///
/// The daemon implements this using the AgentPool and SecureAgentFramework.
#[async_trait]
pub trait AgentOps: Send + Sync {
    /// Spawn `count` agents for the given goal slug.
    /// Returns a description of what was spawned.
    async fn spawn_agents(&self, goal_slug: &str, count: usize) -> Result<String>;
}

/// Skill loading/unloading operations.
///
/// The daemon implements this using the skills registry.
#[async_trait]
pub trait SkillOps: Send + Sync {
    /// Load a skill by name from the skill bundle registry.
    async fn load_skill(&self, name: &str) -> Result<String>;

    /// Unload a skill by name.
    async fn unload_skill(&self, name: &str) -> Result<String>;
}

/// Identity and preferences reload operations.
///
/// The daemon implements this by re-reading SOUL.md / preferences.md
/// and injecting the new identity context into active agents.
#[async_trait]
pub trait IdentityOps: Send + Sync {
    /// Reload identity from SOUL.md. Should update the identity hash
    /// and re-inject into running agents' system prompts.
    async fn reload_identity(&self) -> Result<String>;

    /// Reload preferences from preferences.md. Should update thresholds,
    /// cost limits, and other preference-driven configuration.
    async fn reload_preferences(&self) -> Result<String>;
}

// ---------------------------------------------------------------------------
// ActionDispatcher
// ---------------------------------------------------------------------------

/// Routes reconciliation actions to the appropriate subsystem.
///
/// The dispatcher is the bridge between the reconciler's declarative output
/// and the daemon's imperative subsystem calls. It handles:
///
/// - Action type routing to the correct subsystem trait
/// - Approval gating (actions requiring approval are skipped with a message)
/// - Error isolation (one failed action does not abort the batch)
/// - Structured logging via tracing
pub struct ActionDispatcher {
    goal_ops: Arc<dyn GoalOps>,
    agent_ops: Arc<dyn AgentOps>,
    skill_ops: Arc<dyn SkillOps>,
    identity_ops: Arc<dyn IdentityOps>,
}

impl ActionDispatcher {
    /// Create a new dispatcher with all subsystem handles.
    pub fn new(
        goal_ops: Arc<dyn GoalOps>,
        agent_ops: Arc<dyn AgentOps>,
        skill_ops: Arc<dyn SkillOps>,
        identity_ops: Arc<dyn IdentityOps>,
    ) -> Self {
        Self {
            goal_ops,
            agent_ops,
            skill_ops,
            identity_ops,
        }
    }

    /// Dispatch a single reconciliation action to the appropriate subsystem.
    ///
    /// If the action requires approval and has not been approved, it is
    /// skipped (returned as success=true with a "pending_approval" detail).
    pub async fn dispatch(&self, action: &ReconciliationAction) -> DispatchResult {
        let action_type_str = format_action_type(&action.action_type);

        // Gate on approval requirement.
        if action.requires_approval {
            tracing::info!(
                action_id = %action.id,
                target = %action.target,
                action_type = %action_type_str,
                "action_dispatch: action requires approval — skipping"
            );
            return DispatchResult {
                action_id: action.id.clone(),
                target: action.target.clone(),
                action_type: action_type_str,
                success: true,
                detail: "pending_approval: action requires operator approval before execution"
                    .to_string(),
            };
        }

        tracing::info!(
            action_id = %action.id,
            target = %action.target,
            action_type = %action_type_str,
            description = %action.description,
            "action_dispatch: dispatching action"
        );

        let result = self.dispatch_inner(action).await;

        match result {
            Ok(detail) => {
                tracing::info!(
                    action_id = %action.id,
                    target = %action.target,
                    action_type = %action_type_str,
                    detail = %detail,
                    "action_dispatch: action succeeded"
                );
                DispatchResult {
                    action_id: action.id.clone(),
                    target: action.target.clone(),
                    action_type: action_type_str,
                    success: true,
                    detail,
                }
            }
            Err(err) => {
                tracing::warn!(
                    action_id = %action.id,
                    target = %action.target,
                    action_type = %action_type_str,
                    error = %err,
                    "action_dispatch: action failed"
                );
                DispatchResult {
                    action_id: action.id.clone(),
                    target: action.target.clone(),
                    action_type: action_type_str,
                    success: false,
                    detail: err.to_string(),
                }
            }
        }
    }

    /// Route the action to the correct subsystem call.
    async fn dispatch_inner(&self, action: &ReconciliationAction) -> Result<String> {
        match &action.action_type {
            ActionType::StartGoal => self.goal_ops.start_goal(&action.target).await,

            ActionType::StopGoal => self.goal_ops.stop_goal(&action.target).await,

            ActionType::PauseGoal => self.goal_ops.pause_goal(&action.target).await,

            ActionType::ResumeGoal => self.goal_ops.resume_goal(&action.target).await,

            ActionType::AdvancePhase { from, to } => {
                self.goal_ops.advance_phase(&action.target, from, to).await
            }

            ActionType::SpawnAgents { goal, count } => {
                self.agent_ops.spawn_agents(goal, *count).await
            }

            ActionType::ReloadIdentity => self.identity_ops.reload_identity().await,

            ActionType::ReloadPreferences => self.identity_ops.reload_preferences().await,

            ActionType::LoadSkill { name } => self.skill_ops.load_skill(name).await,

            ActionType::UnloadSkill { name } => self.skill_ops.unload_skill(name).await,
        }
    }

    /// Dispatch a batch of reconciliation actions, isolating errors.
    ///
    /// Each action is dispatched independently. Failed actions do not
    /// prevent subsequent actions from executing. Returns a summary
    /// with per-action results.
    pub async fn dispatch_batch(&self, actions: &[ReconciliationAction]) -> BatchDispatchSummary {
        let mut results = Vec::with_capacity(actions.len());
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;

        for action in actions {
            let result = self.dispatch(action).await;

            if result.detail.starts_with("pending_approval") {
                skipped += 1;
            } else if result.success {
                succeeded += 1;
            } else {
                failed += 1;
            }

            results.push(result);
        }

        tracing::info!(
            total = actions.len(),
            succeeded = succeeded,
            failed = failed,
            skipped = skipped,
            "action_dispatch: batch complete"
        );

        BatchDispatchSummary {
            total: actions.len(),
            succeeded,
            failed,
            skipped,
            results,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Format an `ActionType` as a human-readable string.
fn format_action_type(action_type: &ActionType) -> String {
    match action_type {
        ActionType::StartGoal => "start_goal".to_string(),
        ActionType::StopGoal => "stop_goal".to_string(),
        ActionType::PauseGoal => "pause_goal".to_string(),
        ActionType::ResumeGoal => "resume_goal".to_string(),
        ActionType::AdvancePhase { from, to } => format!("advance_phase({from}->{to})"),
        ActionType::SpawnAgents { goal, count } => format!("spawn_agents({goal}, {count})"),
        ActionType::ReloadIdentity => "reload_identity".to_string(),
        ActionType::ReloadPreferences => "reload_preferences".to_string(),
        ActionType::LoadSkill { name } => format!("load_skill({name})"),
        ActionType::UnloadSkill { name } => format!("unload_skill({name})"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // -- Mock subsystems --

    #[derive(Default)]
    struct MockGoalOps {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl GoalOps for MockGoalOps {
        async fn start_goal(&self, slug: &str) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start_goal:{slug}"));
            Ok(format!("started goal '{slug}'"))
        }

        async fn stop_goal(&self, slug: &str) -> Result<String> {
            self.calls.lock().unwrap().push(format!("stop_goal:{slug}"));
            Ok(format!("stopped goal '{slug}'"))
        }

        async fn pause_goal(&self, slug: &str) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("pause_goal:{slug}"));
            Ok(format!("paused goal '{slug}'"))
        }

        async fn resume_goal(&self, slug: &str) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("resume_goal:{slug}"));
            Ok(format!("resumed goal '{slug}'"))
        }

        async fn advance_phase(&self, slug: &str, from: &str, to: &str) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("advance_phase:{slug}:{from}->{to}"));
            Ok(format!("advanced '{slug}' from {from} to {to}"))
        }
    }

    #[derive(Default)]
    struct MockAgentOps {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AgentOps for MockAgentOps {
        async fn spawn_agents(&self, goal_slug: &str, count: usize) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("spawn_agents:{goal_slug}:{count}"));
            Ok(format!("spawned {count} agents for '{goal_slug}'"))
        }
    }

    #[derive(Default)]
    struct MockSkillOps {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SkillOps for MockSkillOps {
        async fn load_skill(&self, name: &str) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("load_skill:{name}"));
            Ok(format!("loaded skill '{name}'"))
        }

        async fn unload_skill(&self, name: &str) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("unload_skill:{name}"));
            Ok(format!("unloaded skill '{name}'"))
        }
    }

    #[derive(Default)]
    struct MockIdentityOps {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl IdentityOps for MockIdentityOps {
        async fn reload_identity(&self) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push("reload_identity".to_string());
            Ok("identity reloaded from SOUL.md".to_string())
        }

        async fn reload_preferences(&self) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push("reload_preferences".to_string());
            Ok("preferences reloaded".to_string())
        }
    }

    /// Failing mock that errors on every call.
    struct FailingGoalOps;

    #[async_trait]
    impl GoalOps for FailingGoalOps {
        async fn start_goal(&self, slug: &str) -> Result<String> {
            Err(anyhow::anyhow!(
                "subsystem error: failed to start goal '{slug}'"
            ))
        }
        async fn stop_goal(&self, slug: &str) -> Result<String> {
            Err(anyhow::anyhow!(
                "subsystem error: failed to stop goal '{slug}'"
            ))
        }
        async fn pause_goal(&self, _slug: &str) -> Result<String> {
            Err(anyhow::anyhow!("subsystem error: pause failed"))
        }
        async fn resume_goal(&self, _slug: &str) -> Result<String> {
            Err(anyhow::anyhow!("subsystem error: resume failed"))
        }
        async fn advance_phase(&self, _slug: &str, _from: &str, _to: &str) -> Result<String> {
            Err(anyhow::anyhow!("subsystem error: advance failed"))
        }
    }

    // -- Test helpers --

    fn make_dispatcher() -> (
        ActionDispatcher,
        Arc<MockGoalOps>,
        Arc<MockAgentOps>,
        Arc<MockSkillOps>,
        Arc<MockIdentityOps>,
    ) {
        let goal_ops = Arc::new(MockGoalOps::default());
        let agent_ops = Arc::new(MockAgentOps::default());
        let skill_ops = Arc::new(MockSkillOps::default());
        let identity_ops = Arc::new(MockIdentityOps::default());

        let dispatcher = ActionDispatcher::new(
            goal_ops.clone(),
            agent_ops.clone(),
            skill_ops.clone(),
            identity_ops.clone(),
        );

        (dispatcher, goal_ops, agent_ops, skill_ops, identity_ops)
    }

    fn make_action(action_type: ActionType, target: &str) -> ReconciliationAction {
        ReconciliationAction {
            id: format!("test-{target}"),
            action_type,
            target: target.to_string(),
            description: format!("Test action for {target}"),
            requires_approval: false,
            estimated_cost: None,
        }
    }

    fn make_action_with_approval(action_type: ActionType, target: &str) -> ReconciliationAction {
        ReconciliationAction {
            id: format!("test-{target}"),
            action_type,
            target: target.to_string(),
            description: format!("Test action for {target}"),
            requires_approval: true,
            estimated_cost: None,
        }
    }

    // -- Tests --

    #[tokio::test]
    async fn dispatch_start_goal() {
        let (dispatcher, goal_ops, _, _, _) = make_dispatcher();
        let action = make_action(ActionType::StartGoal, "learn-rust");

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert_eq!(result.action_type, "start_goal");
        assert_eq!(result.target, "learn-rust");
        assert!(result.detail.contains("started"));

        let calls = goal_ops.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "start_goal:learn-rust");
    }

    #[tokio::test]
    async fn dispatch_stop_goal() {
        let (dispatcher, goal_ops, _, _, _) = make_dispatcher();
        let action = make_action(ActionType::StopGoal, "old-goal");

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert_eq!(result.action_type, "stop_goal");
        assert!(result.detail.contains("stopped"));

        let calls = goal_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "stop_goal:old-goal");
    }

    #[tokio::test]
    async fn dispatch_pause_goal() {
        let (dispatcher, goal_ops, _, _, _) = make_dispatcher();
        let action = make_action(ActionType::PauseGoal, "pausing-goal");

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert_eq!(result.action_type, "pause_goal");

        let calls = goal_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "pause_goal:pausing-goal");
    }

    #[tokio::test]
    async fn dispatch_resume_goal() {
        let (dispatcher, goal_ops, _, _, _) = make_dispatcher();
        let action = make_action(ActionType::ResumeGoal, "resuming-goal");

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert_eq!(result.action_type, "resume_goal");

        let calls = goal_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "resume_goal:resuming-goal");
    }

    #[tokio::test]
    async fn dispatch_advance_phase() {
        let (dispatcher, goal_ops, _, _, _) = make_dispatcher();
        let action = make_action(
            ActionType::AdvancePhase {
                from: "research".to_string(),
                to: "implementation".to_string(),
            },
            "goal-x",
        );

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert!(result
            .action_type
            .contains("advance_phase(research->implementation)"));
        assert!(result.detail.contains("research"));
        assert!(result.detail.contains("implementation"));

        let calls = goal_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "advance_phase:goal-x:research->implementation");
    }

    #[tokio::test]
    async fn dispatch_spawn_agents() {
        let (dispatcher, _, agent_ops, _, _) = make_dispatcher();
        let action = make_action(
            ActionType::SpawnAgents {
                goal: "trading".to_string(),
                count: 3,
            },
            "trading",
        );

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert!(result.action_type.contains("spawn_agents"));
        assert!(result.detail.contains("3"));
        assert!(result.detail.contains("trading"));

        let calls = agent_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "spawn_agents:trading:3");
    }

    #[tokio::test]
    async fn dispatch_reload_identity() {
        let (dispatcher, _, _, _, identity_ops) = make_dispatcher();
        let action = make_action(ActionType::ReloadIdentity, "SOUL.md");

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert_eq!(result.action_type, "reload_identity");
        assert!(result.detail.contains("identity reloaded"));

        let calls = identity_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "reload_identity");
    }

    #[tokio::test]
    async fn dispatch_reload_preferences() {
        let (dispatcher, _, _, _, identity_ops) = make_dispatcher();
        let action = make_action(ActionType::ReloadPreferences, "preferences.md");

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert_eq!(result.action_type, "reload_preferences");

        let calls = identity_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "reload_preferences");
    }

    #[tokio::test]
    async fn dispatch_load_skill() {
        let (dispatcher, _, _, skill_ops, _) = make_dispatcher();
        let action = make_action(
            ActionType::LoadSkill {
                name: "web-scraper".to_string(),
            },
            "web-scraper",
        );

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert!(result.action_type.contains("load_skill"));
        assert!(result.detail.contains("web-scraper"));

        let calls = skill_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "load_skill:web-scraper");
    }

    #[tokio::test]
    async fn dispatch_unload_skill() {
        let (dispatcher, _, _, skill_ops, _) = make_dispatcher();
        let action = make_action(
            ActionType::UnloadSkill {
                name: "old-skill".to_string(),
            },
            "old-skill",
        );

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert!(result.action_type.contains("unload_skill"));

        let calls = skill_ops.calls.lock().unwrap();
        assert_eq!(calls[0], "unload_skill:old-skill");
    }

    #[tokio::test]
    async fn dispatch_skips_actions_requiring_approval() {
        let (dispatcher, goal_ops, _, _, _) = make_dispatcher();
        let action = make_action_with_approval(ActionType::StartGoal, "manual-goal");

        let result = dispatcher.dispatch(&action).await;

        assert!(result.success);
        assert!(result.detail.contains("pending_approval"));

        // GoalOps should NOT have been called.
        let calls = goal_ops.calls.lock().unwrap();
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn dispatch_error_returns_failure_result() {
        let goal_ops = Arc::new(FailingGoalOps);
        let agent_ops: Arc<dyn AgentOps> = Arc::new(MockAgentOps::default());
        let skill_ops: Arc<dyn SkillOps> = Arc::new(MockSkillOps::default());
        let identity_ops: Arc<dyn IdentityOps> = Arc::new(MockIdentityOps::default());

        let dispatcher = ActionDispatcher::new(goal_ops, agent_ops, skill_ops, identity_ops);
        let action = make_action(ActionType::StartGoal, "will-fail");

        let result = dispatcher.dispatch(&action).await;

        assert!(!result.success);
        assert!(result.detail.contains("subsystem error"));
        assert!(result.detail.contains("will-fail"));
    }

    #[tokio::test]
    async fn dispatch_batch_processes_all_actions() {
        let (dispatcher, goal_ops, agent_ops, _, _) = make_dispatcher();

        let actions = vec![
            make_action(ActionType::StartGoal, "goal-a"),
            make_action(
                ActionType::SpawnAgents {
                    goal: "goal-a".to_string(),
                    count: 2,
                },
                "goal-a",
            ),
            make_action(ActionType::StopGoal, "goal-b"),
        ];

        let summary = dispatcher.dispatch_batch(&actions).await;

        assert_eq!(summary.total, 3);
        assert_eq!(summary.succeeded, 3);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.results.len(), 3);
        assert!(summary.results.iter().all(|r| r.success));

        let goal_calls = goal_ops.calls.lock().unwrap();
        assert_eq!(goal_calls.len(), 2);
        assert_eq!(goal_calls[0], "start_goal:goal-a");
        assert_eq!(goal_calls[1], "stop_goal:goal-b");

        let agent_calls = agent_ops.calls.lock().unwrap();
        assert_eq!(agent_calls.len(), 1);
        assert_eq!(agent_calls[0], "spawn_agents:goal-a:2");
    }

    #[tokio::test]
    async fn dispatch_batch_isolates_errors() {
        let goal_ops = Arc::new(FailingGoalOps);
        let agent_ops = Arc::new(MockAgentOps::default());
        let skill_ops = Arc::new(MockSkillOps::default());
        let identity_ops = Arc::new(MockIdentityOps::default());

        let dispatcher =
            ActionDispatcher::new(goal_ops, agent_ops.clone(), skill_ops, identity_ops);

        let actions = vec![
            make_action(ActionType::StartGoal, "fail-goal"),
            make_action(
                ActionType::SpawnAgents {
                    goal: "other-goal".to_string(),
                    count: 1,
                },
                "other-goal",
            ),
        ];

        let summary = dispatcher.dispatch_batch(&actions).await;

        assert_eq!(summary.total, 2);
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failed, 1);

        // The first action should have failed.
        assert!(!summary.results[0].success);
        // The second should have succeeded despite the first failing.
        assert!(summary.results[1].success);

        // AgentOps was still called.
        let agent_calls = agent_ops.calls.lock().unwrap();
        assert_eq!(agent_calls.len(), 1);
    }

    #[tokio::test]
    async fn dispatch_batch_with_approval_skips() {
        let (dispatcher, goal_ops, _, _, _) = make_dispatcher();

        let actions = vec![
            make_action(ActionType::StartGoal, "auto-goal"),
            make_action_with_approval(ActionType::StartGoal, "manual-goal"),
            make_action(ActionType::StopGoal, "old-goal"),
        ];

        let summary = dispatcher.dispatch_batch(&actions).await;

        assert_eq!(summary.total, 3);
        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 1);

        // Only auto-goal and old-goal were actually dispatched.
        let calls = goal_ops.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], "start_goal:auto-goal");
        assert_eq!(calls[1], "stop_goal:old-goal");
    }

    #[tokio::test]
    async fn dispatch_batch_empty() {
        let (dispatcher, _, _, _, _) = make_dispatcher();

        let summary = dispatcher.dispatch_batch(&[]).await;

        assert_eq!(summary.total, 0);
        assert_eq!(summary.succeeded, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
        assert!(summary.results.is_empty());
    }

    #[test]
    fn dispatch_result_display() {
        let result = DispatchResult {
            action_id: "act-1".to_string(),
            target: "my-goal".to_string(),
            action_type: "start_goal".to_string(),
            success: true,
            detail: "started goal 'my-goal'".to_string(),
        };
        let display = format!("{result}");
        assert!(display.contains("[OK]"));
        assert!(display.contains("start_goal"));
        assert!(display.contains("my-goal"));
    }

    #[test]
    fn dispatch_result_display_failure() {
        let result = DispatchResult {
            action_id: "act-2".to_string(),
            target: "bad-goal".to_string(),
            action_type: "start_goal".to_string(),
            success: false,
            detail: "subsystem error".to_string(),
        };
        let display = format!("{result}");
        assert!(display.contains("[FAIL]"));
    }

    #[test]
    fn format_action_type_all_variants() {
        assert_eq!(format_action_type(&ActionType::StartGoal), "start_goal");
        assert_eq!(format_action_type(&ActionType::StopGoal), "stop_goal");
        assert_eq!(format_action_type(&ActionType::PauseGoal), "pause_goal");
        assert_eq!(format_action_type(&ActionType::ResumeGoal), "resume_goal");
        assert_eq!(
            format_action_type(&ActionType::AdvancePhase {
                from: "a".to_string(),
                to: "b".to_string()
            }),
            "advance_phase(a->b)"
        );
        assert_eq!(
            format_action_type(&ActionType::SpawnAgents {
                goal: "g".to_string(),
                count: 5
            }),
            "spawn_agents(g, 5)"
        );
        assert_eq!(
            format_action_type(&ActionType::ReloadIdentity),
            "reload_identity"
        );
        assert_eq!(
            format_action_type(&ActionType::ReloadPreferences),
            "reload_preferences"
        );
        assert_eq!(
            format_action_type(&ActionType::LoadSkill {
                name: "s".to_string()
            }),
            "load_skill(s)"
        );
        assert_eq!(
            format_action_type(&ActionType::UnloadSkill {
                name: "s".to_string()
            }),
            "unload_skill(s)"
        );
    }

    #[test]
    fn batch_summary_serde_roundtrip() {
        let summary = BatchDispatchSummary {
            total: 3,
            succeeded: 2,
            failed: 1,
            skipped: 0,
            results: vec![DispatchResult {
                action_id: "a".to_string(),
                target: "t".to_string(),
                action_type: "start_goal".to_string(),
                success: true,
                detail: "ok".to_string(),
            }],
        };
        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: BatchDispatchSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.total, 3);
        assert_eq!(deserialized.succeeded, 2);
        assert_eq!(deserialized.failed, 1);
        assert_eq!(deserialized.results.len(), 1);
    }
}
