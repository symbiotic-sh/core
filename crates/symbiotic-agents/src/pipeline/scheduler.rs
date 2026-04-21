//! Goal Scheduler — cron-like scheduling loop for periodic goal check-ins.
//!
//! Evaluates which goals are due for execution and allocates agent slots
//! across them based on priority weights and autonomy levels.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::types::AutonomyLevel;

/// Configuration for the goal scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// How often to check for due goals (seconds). Default: 60.
    pub check_interval_secs: u64,
    /// Maximum agents across all goals. Default: 10.
    pub total_agent_slots: usize,
    /// Whether to run due goals automatically or queue for approval.
    pub auto_run_due_goals: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            check_interval_secs: 60,
            total_agent_slots: 10,
            auto_run_due_goals: true,
        }
    }
}

/// Actions the scheduler wants to take.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SchedulerAction {
    /// Spawn agents for a goal.
    SpawnAgents { goal_slug: String, count: usize },
    /// Request approval before running a goal.
    RequestApproval { goal_slug: String, reason: String },
    /// Pause a goal due to resource constraints or errors.
    PauseGoal { goal_slug: String, reason: String },
}

/// A scheduled goal entry for the scheduler to evaluate.
#[derive(Debug, Clone)]
pub struct ScheduledGoal {
    /// Unique slug identifying the goal.
    pub slug: String,
    /// Priority weight (higher = more important, gets more slots).
    pub priority: u32,
    /// Unix timestamp when this goal next needs a check-in.
    pub next_check_at: u64,
    /// Autonomy level governing how the goal should be executed.
    pub autonomy_level: AutonomyLevel,
    /// Whether the goal currently has running agents.
    pub is_running: bool,
    /// Number of agents currently working on this goal.
    pub current_agents: usize,
}

/// Manages scheduled/periodic goal check-ins.
pub struct GoalScheduler {
    config: SchedulerConfig,
}

impl GoalScheduler {
    /// Create a new goal scheduler with the given config.
    pub fn new(config: SchedulerConfig) -> Self {
        Self { config }
    }

    /// Access the scheduler config.
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Run one scheduling tick. Evaluates which goals are due at `now`
    /// and returns the actions to take.
    pub fn tick(&self, now: u64, goals: &[ScheduledGoal]) -> Vec<SchedulerAction> {
        // Find goals that are due (next_check_at <= now).
        let due_goals: Vec<&ScheduledGoal> =
            goals.iter().filter(|g| g.next_check_at <= now).collect();

        if due_goals.is_empty() {
            return Vec::new();
        }

        // Allocate agent slots across due goals by priority.
        let allocation = self.allocate_slots(&due_goals, self.config.total_agent_slots);

        let mut actions = Vec::new();

        for goal in &due_goals {
            let slots = allocation.get(goal.slug.as_str()).copied().unwrap_or(0);
            if slots == 0 {
                continue;
            }

            match goal.autonomy_level {
                AutonomyLevel::Auto => {
                    if self.config.auto_run_due_goals {
                        actions.push(SchedulerAction::SpawnAgents {
                            goal_slug: goal.slug.clone(),
                            count: slots,
                        });
                    } else {
                        actions.push(SchedulerAction::RequestApproval {
                            goal_slug: goal.slug.clone(),
                            reason: "Auto-run disabled; goal is due".to_string(),
                        });
                    }
                }
                AutonomyLevel::Semi => {
                    // Semi: spawn agents but also notify (spawn action implies notification).
                    actions.push(SchedulerAction::SpawnAgents {
                        goal_slug: goal.slug.clone(),
                        count: slots,
                    });
                }
                AutonomyLevel::Manual => {
                    actions.push(SchedulerAction::RequestApproval {
                        goal_slug: goal.slug.clone(),
                        reason: "Manual goal due for check-in".to_string(),
                    });
                }
            }
        }

        actions
    }

    /// Allocate agent slots across due goals proportional to priority weight.
    ///
    /// Rules:
    /// - Each due goal gets at least 1 slot (if total_slots permits).
    /// - Remaining slots distributed proportionally to priority weight.
    /// - Goals already running get their current agent count deducted.
    /// - A goal that is already at or above its allocation gets 0 additional slots.
    pub fn allocate_slots(
        &self,
        goals: &[&ScheduledGoal],
        total_slots: usize,
    ) -> HashMap<String, usize> {
        let mut result: HashMap<String, usize> = HashMap::new();

        if goals.is_empty() || total_slots == 0 {
            return result;
        }

        // Calculate total available slots after deducting currently running agents.
        let total_running: usize = goals.iter().map(|g| g.current_agents).sum();
        let available = total_slots.saturating_sub(total_running);

        if available == 0 {
            // All slots consumed by running agents.
            for g in goals {
                result.insert(g.slug.clone(), 0);
            }
            return result;
        }

        // Total priority weight across all due goals.
        let total_priority: u32 = goals.iter().map(|g| g.priority.max(1)).sum();

        if total_priority == 0 {
            // Edge case: all priorities are 0, distribute evenly.
            let per_goal = available / goals.len();
            for g in goals {
                result.insert(g.slug.clone(), per_goal);
            }
            return result;
        }

        // Phase 1: Minimum 1 slot per goal (up to available).
        let min_per_goal = if available >= goals.len() { 1 } else { 0 };
        let mut allocated: Vec<(String, usize)> = goals
            .iter()
            .map(|g| (g.slug.clone(), min_per_goal))
            .collect();

        let remaining_after_min = available.saturating_sub(min_per_goal * goals.len());

        // Phase 2: Distribute remaining proportionally by priority.
        if remaining_after_min > 0 {
            let mut fractional_slots: Vec<(usize, f64)> = goals
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    let weight = g.priority.max(1) as f64 / total_priority as f64;
                    let raw = weight * remaining_after_min as f64;
                    (i, raw)
                })
                .collect();

            // Integer part first.
            let mut distributed = 0usize;
            for (i, raw) in &fractional_slots {
                let int_part = *raw as usize;
                allocated[*i].1 += int_part;
                distributed += int_part;
            }

            // Distribute remainder by largest fractional part.
            let mut leftover = remaining_after_min.saturating_sub(distributed);
            if leftover > 0 {
                fractional_slots.sort_by(|a, b| (b.1.fract()).partial_cmp(&a.1.fract()).unwrap());
                for (i, _) in &fractional_slots {
                    if leftover == 0 {
                        break;
                    }
                    allocated[*i].1 += 1;
                    leftover -= 1;
                }
            }
        }

        for (slug, slots) in allocated {
            result.insert(slug, slots);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_goal(
        slug: &str,
        priority: u32,
        next_check_at: u64,
        autonomy: AutonomyLevel,
    ) -> ScheduledGoal {
        ScheduledGoal {
            slug: slug.to_string(),
            priority,
            next_check_at,
            autonomy_level: autonomy,
            is_running: false,
            current_agents: 0,
        }
    }

    fn make_running_goal(
        slug: &str,
        priority: u32,
        next_check_at: u64,
        current_agents: usize,
    ) -> ScheduledGoal {
        ScheduledGoal {
            slug: slug.to_string(),
            priority,
            next_check_at,
            autonomy_level: AutonomyLevel::Auto,
            is_running: true,
            current_agents,
        }
    }

    // -----------------------------------------------------------------------
    // Config tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_scheduler_config_default() {
        let config = SchedulerConfig::default();
        assert_eq!(config.check_interval_secs, 60);
        assert_eq!(config.total_agent_slots, 10);
        assert!(config.auto_run_due_goals);
    }

    #[test]
    fn test_scheduler_config_serde_roundtrip() {
        let config = SchedulerConfig {
            check_interval_secs: 120,
            total_agent_slots: 5,
            auto_run_due_goals: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: SchedulerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.check_interval_secs, 120);
        assert_eq!(deserialized.total_agent_slots, 5);
        assert!(!deserialized.auto_run_due_goals);
    }

    // -----------------------------------------------------------------------
    // SchedulerAction serde tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_scheduler_action_serde() {
        let actions = vec![
            SchedulerAction::SpawnAgents {
                goal_slug: "daily-digest".to_string(),
                count: 3,
            },
            SchedulerAction::RequestApproval {
                goal_slug: "deploy".to_string(),
                reason: "Manual goal".to_string(),
            },
            SchedulerAction::PauseGoal {
                goal_slug: "broken".to_string(),
                reason: "Too many failures".to_string(),
            },
        ];
        for action in &actions {
            let json = serde_json::to_string(action).unwrap();
            let deserialized: SchedulerAction = serde_json::from_str(&json).unwrap();
            assert_eq!(&deserialized, action);
        }
    }

    // -----------------------------------------------------------------------
    // tick() tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_tick_no_due_goals() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goals = vec![make_goal("a", 5, 1000, AutonomyLevel::Auto)];

        // now=500, goal due at 1000 -> not due
        let actions = scheduler.tick(500, &goals);
        assert!(actions.is_empty());
    }

    #[test]
    fn test_tick_empty_goals() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let actions = scheduler.tick(1000, &[]);
        assert!(actions.is_empty());
    }

    #[test]
    fn test_tick_single_auto_goal() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goals = vec![make_goal("daily-digest", 5, 100, AutonomyLevel::Auto)];

        let actions = scheduler.tick(200, &goals);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SchedulerAction::SpawnAgents { goal_slug, count } => {
                assert_eq!(goal_slug, "daily-digest");
                // Single goal gets all available slots (10)
                assert_eq!(*count, 10);
            }
            other => panic!("expected SpawnAgents, got {other:?}"),
        }
    }

    #[test]
    fn test_tick_single_semi_goal() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goals = vec![make_goal("research", 5, 100, AutonomyLevel::Semi)];

        let actions = scheduler.tick(200, &goals);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SchedulerAction::SpawnAgents { goal_slug, count } => {
                assert_eq!(goal_slug, "research");
                assert_eq!(*count, 10);
            }
            other => panic!("expected SpawnAgents, got {other:?}"),
        }
    }

    #[test]
    fn test_tick_single_manual_goal() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goals = vec![make_goal("deploy", 5, 100, AutonomyLevel::Manual)];

        let actions = scheduler.tick(200, &goals);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SchedulerAction::RequestApproval { goal_slug, reason } => {
                assert_eq!(goal_slug, "deploy");
                assert!(reason.contains("Manual"));
            }
            other => panic!("expected RequestApproval, got {other:?}"),
        }
    }

    #[test]
    fn test_tick_auto_run_disabled() {
        let config = SchedulerConfig {
            auto_run_due_goals: false,
            ..Default::default()
        };
        let scheduler = GoalScheduler::new(config);
        let goals = vec![make_goal("daily-digest", 5, 100, AutonomyLevel::Auto)];

        let actions = scheduler.tick(200, &goals);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SchedulerAction::RequestApproval { goal_slug, .. } => {
                assert_eq!(goal_slug, "daily-digest");
            }
            other => panic!("expected RequestApproval, got {other:?}"),
        }
    }

    #[test]
    fn test_tick_mixed_due_and_not_due() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goals = vec![
            make_goal("due-goal", 5, 100, AutonomyLevel::Auto),
            make_goal("future-goal", 5, 500, AutonomyLevel::Auto),
        ];

        let actions = scheduler.tick(200, &goals);
        // Only one goal is due.
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            SchedulerAction::SpawnAgents { goal_slug, .. } => {
                assert_eq!(goal_slug, "due-goal");
            }
            other => panic!("expected SpawnAgents, got {other:?}"),
        }
    }

    #[test]
    fn test_tick_multiple_due_goals_mixed_autonomy() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goals = vec![
            make_goal("auto-goal", 5, 100, AutonomyLevel::Auto),
            make_goal("manual-goal", 5, 100, AutonomyLevel::Manual),
        ];

        let actions = scheduler.tick(200, &goals);
        assert_eq!(actions.len(), 2);

        // Find each action by slug.
        let auto_action = actions
            .iter()
            .find(|a| match a {
                SchedulerAction::SpawnAgents { goal_slug, .. } => goal_slug == "auto-goal",
                _ => false,
            })
            .expect("should have SpawnAgents for auto-goal");
        assert!(matches!(auto_action, SchedulerAction::SpawnAgents { .. }));

        let manual_action = actions
            .iter()
            .find(|a| match a {
                SchedulerAction::RequestApproval { goal_slug, .. } => goal_slug == "manual-goal",
                _ => false,
            })
            .expect("should have RequestApproval for manual-goal");
        assert!(matches!(
            manual_action,
            SchedulerAction::RequestApproval { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // allocate_slots() tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_allocate_slots_single_goal() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goal = make_goal("a", 5, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&goal];

        let alloc = scheduler.allocate_slots(&goals, 10);
        assert_eq!(alloc["a"], 10);
    }

    #[test]
    fn test_allocate_slots_equal_priority() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let g1 = make_goal("a", 5, 100, AutonomyLevel::Auto);
        let g2 = make_goal("b", 5, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&g1, &g2];

        let alloc = scheduler.allocate_slots(&goals, 10);
        assert_eq!(alloc["a"], 5);
        assert_eq!(alloc["b"], 5);
    }

    #[test]
    fn test_allocate_slots_weighted_priority() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        // priority 3:1 ratio -> should get ~7.5:2.5 of 10 slots
        let g1 = make_goal("high", 9, 100, AutonomyLevel::Auto);
        let g2 = make_goal("low", 3, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&g1, &g2];

        let alloc = scheduler.allocate_slots(&goals, 10);
        // Min 1 each = 2, remaining 8 distributed 9:3 (3:1)
        // high: 1 + 6 = 7, low: 1 + 2 = 3
        assert_eq!(alloc["high"], 7);
        assert_eq!(alloc["low"], 3);
    }

    #[test]
    fn test_allocate_slots_running_goals_deducted() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let g1 = make_running_goal("running", 5, 100, 4); // already has 4 agents
        let g2 = make_goal("fresh", 5, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&g1, &g2];

        let alloc = scheduler.allocate_slots(&goals, 10);
        // Total running = 4, available = 6
        // Equal priority: 3 each
        assert_eq!(alloc["running"], 3);
        assert_eq!(alloc["fresh"], 3);
    }

    #[test]
    fn test_allocate_slots_all_running_no_available() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let g1 = make_running_goal("a", 5, 100, 5);
        let g2 = make_running_goal("b", 5, 100, 5);
        let goals: Vec<&ScheduledGoal> = vec![&g1, &g2];

        let alloc = scheduler.allocate_slots(&goals, 10);
        // 10 already running, 0 available
        assert_eq!(alloc["a"], 0);
        assert_eq!(alloc["b"], 0);
    }

    #[test]
    fn test_allocate_slots_zero_total() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let g1 = make_goal("a", 5, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&g1];

        let alloc = scheduler.allocate_slots(&goals, 0);
        assert!(alloc.is_empty() || alloc.values().all(|&v| v == 0));
    }

    #[test]
    fn test_allocate_slots_empty_goals() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goals: Vec<&ScheduledGoal> = vec![];

        let alloc = scheduler.allocate_slots(&goals, 10);
        assert!(alloc.is_empty());
    }

    #[test]
    fn test_allocate_slots_fewer_slots_than_goals() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let g1 = make_goal("a", 5, 100, AutonomyLevel::Auto);
        let g2 = make_goal("b", 5, 100, AutonomyLevel::Auto);
        let g3 = make_goal("c", 5, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&g1, &g2, &g3];

        // Only 2 slots for 3 goals -> cannot give min 1 to each
        let alloc = scheduler.allocate_slots(&goals, 2);
        let total: usize = alloc.values().sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_allocate_slots_one_slot() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let g1 = make_goal("a", 10, 100, AutonomyLevel::Auto);
        let g2 = make_goal("b", 1, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&g1, &g2];

        let alloc = scheduler.allocate_slots(&goals, 1);
        let total: usize = alloc.values().sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn test_allocate_slots_large_priority_difference() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let g1 = make_goal("critical", 100, 100, AutonomyLevel::Auto);
        let g2 = make_goal("minor", 1, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&g1, &g2];

        let alloc = scheduler.allocate_slots(&goals, 20);
        // critical should get the vast majority of slots.
        assert!(alloc["critical"] > alloc["minor"]);
        assert!(alloc["critical"] >= 18); // ~99% of remaining after min
        let total: usize = alloc.values().sum();
        assert_eq!(total, 20);
    }

    #[test]
    fn test_allocate_slots_preserves_total() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let g1 = make_goal("a", 7, 100, AutonomyLevel::Auto);
        let g2 = make_goal("b", 3, 100, AutonomyLevel::Auto);
        let g3 = make_goal("c", 5, 100, AutonomyLevel::Auto);
        let goals: Vec<&ScheduledGoal> = vec![&g1, &g2, &g3];

        let alloc = scheduler.allocate_slots(&goals, 15);
        let total: usize = alloc.values().sum();
        assert_eq!(total, 15);
    }

    #[test]
    fn test_tick_goal_exactly_at_due_time() {
        let scheduler = GoalScheduler::new(SchedulerConfig::default());
        let goals = vec![make_goal("exact", 5, 200, AutonomyLevel::Auto)];

        // now == next_check_at -> should be due
        let actions = scheduler.tick(200, &goals);
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn test_tick_zero_slots_config() {
        let config = SchedulerConfig {
            total_agent_slots: 0,
            ..Default::default()
        };
        let scheduler = GoalScheduler::new(config);
        let goals = vec![make_goal("a", 5, 100, AutonomyLevel::Auto)];

        // Due but 0 slots available -> no spawn actions
        let actions = scheduler.tick(200, &goals);
        assert!(actions.is_empty());
    }
}
