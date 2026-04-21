//! State differ: compares desired state (manifests) against actual state (runtime)
//! to produce reconciliation actions.

use uuid::Uuid;

use crate::types::{
    ActionType, ActualState, AutonomyLevel, DesiredState, GoalState, ReconciliationAction,
};

/// Computes the diff between desired and actual state.
///
/// # Safety Rules
///
/// 1. Never auto-start a goal with `autonomy_level: manual` — mark `requires_approval`.
/// 2. Reconciliation is idempotent: no changes produces no actions.
/// 3. Parse failures in desired state are already handled by ManifestParser (skipped).
#[derive(Debug, Default)]
pub struct StateDiffer;

impl StateDiffer {
    pub fn new() -> Self {
        Self
    }

    /// Diff desired state against actual state, producing reconciliation actions.
    pub fn diff(&self, desired: &DesiredState, actual: &ActualState) -> Vec<ReconciliationAction> {
        let mut actions = Vec::new();

        self.diff_goals(desired, actual, &mut actions);
        self.diff_identity(desired, actual, &mut actions);
        self.diff_preferences(desired, actual, &mut actions);
        self.diff_skills(desired, actual, &mut actions);

        actions
    }

    fn diff_goals(
        &self,
        desired: &DesiredState,
        actual: &ActualState,
        actions: &mut Vec<ReconciliationAction>,
    ) {
        // Check each desired goal against actual
        for goal in &desired.goals {
            let actual_goal = actual.active_goals.iter().find(|ag| ag.slug == goal.slug);

            match actual_goal {
                None => {
                    // Goal manifest exists but no runtime process
                    if goal.state == GoalState::Active {
                        actions.push(ReconciliationAction {
                            id: Uuid::new_v4().to_string(),
                            action_type: ActionType::StartGoal,
                            target: goal.slug.clone(),
                            description: format!("Start goal: {}", goal.title),
                            requires_approval: goal.autonomy_level == AutonomyLevel::Manual,
                            estimated_cost: goal.constraints.budget_usd,
                        });
                    }
                    // Paused, achieved, or abandoned goals without runtime process — no action
                }
                Some(ag) => {
                    // Goal exists in both desired and actual — check for diffs

                    // State changes
                    if goal.state != ag.state {
                        match goal.state {
                            GoalState::Paused => {
                                actions.push(ReconciliationAction {
                                    id: Uuid::new_v4().to_string(),
                                    action_type: ActionType::PauseGoal,
                                    target: goal.slug.clone(),
                                    description: format!("Pause goal: {}", goal.title),
                                    requires_approval: false,
                                    estimated_cost: None,
                                });
                            }
                            GoalState::Active if ag.state == GoalState::Paused => {
                                actions.push(ReconciliationAction {
                                    id: Uuid::new_v4().to_string(),
                                    action_type: ActionType::ResumeGoal,
                                    target: goal.slug.clone(),
                                    description: format!("Resume goal: {}", goal.title),
                                    requires_approval: goal.autonomy_level == AutonomyLevel::Manual,
                                    estimated_cost: None,
                                });
                            }
                            GoalState::Abandoned | GoalState::Achieved => {
                                actions.push(ReconciliationAction {
                                    id: Uuid::new_v4().to_string(),
                                    action_type: ActionType::StopGoal,
                                    target: goal.slug.clone(),
                                    description: format!(
                                        "Stop goal ({}): {}",
                                        if goal.state == GoalState::Achieved {
                                            "achieved"
                                        } else {
                                            "abandoned"
                                        },
                                        goal.title
                                    ),
                                    requires_approval: false,
                                    estimated_cost: None,
                                });
                            }
                            _ => {}
                        }
                    }

                    // Phase changes
                    if goal.phase != ag.phase {
                        actions.push(ReconciliationAction {
                            id: Uuid::new_v4().to_string(),
                            action_type: ActionType::AdvancePhase {
                                from: format!("{:?}", ag.phase).to_lowercase(),
                                to: format!("{:?}", goal.phase).to_lowercase(),
                            },
                            target: goal.slug.clone(),
                            description: format!(
                                "Advance {} from {:?} to {:?}",
                                goal.title, ag.phase, goal.phase
                            ),
                            requires_approval: goal.autonomy_level == AutonomyLevel::Manual,
                            estimated_cost: None,
                        });
                    }

                    // Agent count
                    let desired_agents = goal.process.max_parallel_agents;
                    if desired_agents > ag.running_agents && goal.state == GoalState::Active {
                        let needed = desired_agents - ag.running_agents;
                        actions.push(ReconciliationAction {
                            id: Uuid::new_v4().to_string(),
                            action_type: ActionType::SpawnAgents {
                                goal: goal.slug.clone(),
                                count: needed,
                            },
                            target: goal.slug.clone(),
                            description: format!("Spawn {} agent(s) for {}", needed, goal.title),
                            requires_approval: goal.autonomy_level == AutonomyLevel::Manual,
                            estimated_cost: None,
                        });
                    }
                }
            }
        }

        // Check for runtime goals that no longer have a manifest (removed)
        for ag in &actual.active_goals {
            let still_desired = desired.goals.iter().any(|g| g.slug == ag.slug);
            if !still_desired {
                actions.push(ReconciliationAction {
                    id: Uuid::new_v4().to_string(),
                    action_type: ActionType::StopGoal,
                    target: ag.slug.clone(),
                    description: format!("Stop goal (manifest removed): {}", ag.slug),
                    requires_approval: false,
                    estimated_cost: None,
                });
            }
        }
    }

    fn diff_identity(
        &self,
        desired: &DesiredState,
        actual: &ActualState,
        actions: &mut Vec<ReconciliationAction>,
    ) {
        if let Some(ref identity) = desired.identity {
            let needs_reload = match &actual.identity_hash {
                Some(hash) => hash != &identity.content_hash,
                None => true, // No identity loaded yet
            };
            if needs_reload {
                actions.push(ReconciliationAction {
                    id: Uuid::new_v4().to_string(),
                    action_type: ActionType::ReloadIdentity,
                    target: "SOUL.md".to_string(),
                    description: "Reload identity from SOUL.md (content changed)".to_string(),
                    requires_approval: false,
                    estimated_cost: None,
                });
            }
        }
    }

    fn diff_preferences(
        &self,
        desired: &DesiredState,
        actual: &ActualState,
        actions: &mut Vec<ReconciliationAction>,
    ) {
        if let Some(ref prefs) = desired.preferences {
            let needs_reload = match &actual.preferences_hash {
                Some(hash) => hash != &prefs.content_hash,
                None => true,
            };
            if needs_reload {
                actions.push(ReconciliationAction {
                    id: Uuid::new_v4().to_string(),
                    action_type: ActionType::ReloadPreferences,
                    target: "preferences.md".to_string(),
                    description: "Reload preferences (content changed)".to_string(),
                    requires_approval: false,
                    estimated_cost: None,
                });
            }
        }
    }

    fn diff_skills(
        &self,
        desired: &DesiredState,
        actual: &ActualState,
        actions: &mut Vec<ReconciliationAction>,
    ) {
        // New skills (in desired but not actual)
        for skill in &desired.skills {
            if !actual.loaded_skills.contains(&skill.name) {
                actions.push(ReconciliationAction {
                    id: Uuid::new_v4().to_string(),
                    action_type: ActionType::LoadSkill {
                        name: skill.name.clone(),
                    },
                    target: skill.name.clone(),
                    description: format!("Load new skill: {}", skill.name),
                    requires_approval: false,
                    estimated_cost: None,
                });
            }
        }

        // Removed skills (in actual but not desired)
        for loaded in &actual.loaded_skills {
            if !desired.skills.iter().any(|s| &s.name == loaded) {
                actions.push(ReconciliationAction {
                    id: Uuid::new_v4().to_string(),
                    action_type: ActionType::UnloadSkill {
                        name: loaded.clone(),
                    },
                    target: loaded.clone(),
                    description: format!("Unload removed skill: {loaded}"),
                    requires_approval: false,
                    estimated_cost: None,
                });
            }
        }
    }
}

/// Prioritize and cap actions to prevent reconciliation storms.
pub fn prioritize_and_limit(
    mut actions: Vec<ReconciliationAction>,
    max_actions: usize,
) -> Vec<ReconciliationAction> {
    // Sort: stop/pause first (cleanup), then start/resume, then identity/prefs, then skills
    actions.sort_by_key(|a| match &a.action_type {
        ActionType::StopGoal => 0,
        ActionType::PauseGoal => 1,
        ActionType::ReloadIdentity => 2,
        ActionType::ReloadPreferences => 3,
        ActionType::ResumeGoal => 4,
        ActionType::StartGoal => 5,
        ActionType::AdvancePhase { .. } => 6,
        ActionType::SpawnAgents { .. } => 7,
        ActionType::LoadSkill { .. } => 8,
        ActionType::UnloadSkill { .. } => 1, // Unload before load
    });

    actions.truncate(max_actions);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use std::path::PathBuf;

    fn make_goal(
        slug: &str,
        state: GoalState,
        phase: GoalPhase,
        autonomy: AutonomyLevel,
    ) -> GoalManifest {
        GoalManifest {
            id: "test-id".to_string(),
            project_id: "project:test".to_string(),
            slug: slug.to_string(),
            title: format!("Test: {slug}"),
            state,
            priority: 2,
            autonomy_level: autonomy,
            phase,
            process: ProcessConfig {
                process_type: ProcessType::Persistent,
                check_frequency: CheckFrequency::Daily,
                max_parallel_agents: 2,
            },
            streams: vec![],
            domains: vec![],
            vault_namespace: format!("goal-{slug}"),
            thread_id: None,
            plan_version: 1,
            policy_scopes: Vec::new(),
            task_policy_defaults: GoalTaskPolicyDefaults::default(),
            constraints: GoalConstraints::default(),
            plan_markdown: String::new(),
            tasks: Vec::new(),
        }
    }

    fn make_actual_goal(
        slug: &str,
        state: GoalState,
        phase: GoalPhase,
        agents: usize,
    ) -> ActiveGoalState {
        ActiveGoalState {
            slug: slug.to_string(),
            state,
            phase,
            running_agents: agents,
        }
    }

    #[test]
    fn new_goal_generates_start_action() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            goals: vec![make_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                AutonomyLevel::Semi,
            )],
            ..Default::default()
        };
        let actual = ActualState::default();

        let actions = differ.diff(&desired, &actual);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, ActionType::StartGoal);
        assert_eq!(actions[0].target, "trading");
        assert!(!actions[0].requires_approval);
    }

    #[test]
    fn manual_goal_requires_approval() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            goals: vec![make_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                AutonomyLevel::Manual,
            )],
            ..Default::default()
        };
        let actual = ActualState::default();

        let actions = differ.diff(&desired, &actual);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].requires_approval);
    }

    #[test]
    fn removed_goal_generates_stop_action() {
        let differ = StateDiffer::new();
        let desired = DesiredState::default(); // No goals desired
        let actual = ActualState {
            active_goals: vec![make_actual_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                1,
            )],
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, ActionType::StopGoal);
    }

    #[test]
    fn paused_goal_generates_pause_action() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            goals: vec![make_goal(
                "trading",
                GoalState::Paused,
                GoalPhase::Research,
                AutonomyLevel::Semi,
            )],
            ..Default::default()
        };
        let actual = ActualState {
            active_goals: vec![make_actual_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                1,
            )],
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, ActionType::PauseGoal);
    }

    #[test]
    fn resumed_goal_generates_resume_action() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            goals: vec![make_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                AutonomyLevel::Semi,
            )],
            ..Default::default()
        };
        let actual = ActualState {
            active_goals: vec![make_actual_goal(
                "trading",
                GoalState::Paused,
                GoalPhase::Research,
                0,
            )],
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        // Should have ResumeGoal + SpawnAgents (since desired=2, running=0)
        let resume = actions
            .iter()
            .find(|a| a.action_type == ActionType::ResumeGoal);
        assert!(resume.is_some());
    }

    #[test]
    fn phase_change_generates_advance_action() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            goals: vec![make_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Implementation,
                AutonomyLevel::Semi,
            )],
            ..Default::default()
        };
        let actual = ActualState {
            active_goals: vec![make_actual_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                2,
            )],
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        let advance = actions
            .iter()
            .find(|a| matches!(&a.action_type, ActionType::AdvancePhase { .. }));
        assert!(advance.is_some());
    }

    #[test]
    fn insufficient_agents_generates_spawn_action() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            goals: vec![make_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                AutonomyLevel::Auto,
            )],
            ..Default::default()
        };
        let actual = ActualState {
            active_goals: vec![make_actual_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                0,
            )],
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        let spawn = actions
            .iter()
            .find(|a| matches!(&a.action_type, ActionType::SpawnAgents { .. }));
        assert!(spawn.is_some());
        if let ActionType::SpawnAgents { count, .. } = &spawn.unwrap().action_type {
            assert_eq!(*count, 2); // max_parallel_agents=2, running=0
        }
    }

    #[test]
    fn identity_change_generates_reload() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            identity: Some(IdentityManifest {
                version: 1,
                updated_at: "2026-02-22".to_string(),
                content: "# SOUL".to_string(),
                content_hash: "new-hash".to_string(),
            }),
            ..Default::default()
        };
        let actual = ActualState {
            identity_hash: Some("old-hash".to_string()),
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, ActionType::ReloadIdentity);
    }

    #[test]
    fn matching_identity_hash_no_action() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            identity: Some(IdentityManifest {
                version: 1,
                updated_at: "2026-02-22".to_string(),
                content: "# SOUL".to_string(),
                content_hash: "same-hash".to_string(),
            }),
            ..Default::default()
        };
        let actual = ActualState {
            identity_hash: Some("same-hash".to_string()),
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        assert!(actions.is_empty());
    }

    #[test]
    fn skill_changes_generate_load_unload() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            skills: vec![
                SkillManifest {
                    name: "web-scraper".to_string(),
                    path: PathBuf::new(),
                },
                SkillManifest {
                    name: "code-runner".to_string(),
                    path: PathBuf::new(),
                },
            ],
            ..Default::default()
        };
        let actual = ActualState {
            loaded_skills: vec!["web-scraper".to_string(), "old-skill".to_string()],
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        let load = actions.iter().find(
            |a| matches!(&a.action_type, ActionType::LoadSkill { name } if name == "code-runner"),
        );
        let unload = actions.iter().find(
            |a| matches!(&a.action_type, ActionType::UnloadSkill { name } if name == "old-skill"),
        );
        assert!(load.is_some());
        assert!(unload.is_some());
    }

    #[test]
    fn no_changes_no_actions() {
        let differ = StateDiffer::new();
        let desired = DesiredState {
            identity: Some(IdentityManifest {
                version: 1,
                updated_at: String::new(),
                content: String::new(),
                content_hash: "hash".to_string(),
            }),
            goals: vec![make_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                AutonomyLevel::Semi,
            )],
            skills: vec![SkillManifest {
                name: "s1".to_string(),
                path: PathBuf::new(),
            }],
            ..Default::default()
        };
        let actual = ActualState {
            identity_hash: Some("hash".to_string()),
            active_goals: vec![make_actual_goal(
                "trading",
                GoalState::Active,
                GoalPhase::Research,
                2,
            )],
            loaded_skills: vec!["s1".to_string()],
            ..Default::default()
        };

        let actions = differ.diff(&desired, &actual);
        assert!(actions.is_empty(), "expected no actions, got {actions:?}");
    }

    #[test]
    fn prioritize_and_limit_caps_actions() {
        let actions: Vec<ReconciliationAction> = (0..20)
            .map(|i| ReconciliationAction {
                id: format!("{i}"),
                action_type: ActionType::StartGoal,
                target: format!("goal-{i}"),
                description: format!("Start goal {i}"),
                requires_approval: false,
                estimated_cost: None,
            })
            .collect();

        let limited = prioritize_and_limit(actions, 10);
        assert_eq!(limited.len(), 10);
    }

    #[test]
    fn prioritize_puts_stop_before_start() {
        let actions = vec![
            ReconciliationAction {
                id: "1".to_string(),
                action_type: ActionType::StartGoal,
                target: "new".to_string(),
                description: "start".to_string(),
                requires_approval: false,
                estimated_cost: None,
            },
            ReconciliationAction {
                id: "2".to_string(),
                action_type: ActionType::StopGoal,
                target: "old".to_string(),
                description: "stop".to_string(),
                requires_approval: false,
                estimated_cost: None,
            },
        ];

        let sorted = prioritize_and_limit(actions, 10);
        assert_eq!(sorted[0].action_type, ActionType::StopGoal);
        assert_eq!(sorted[1].action_type, ActionType::StartGoal);
    }
}
