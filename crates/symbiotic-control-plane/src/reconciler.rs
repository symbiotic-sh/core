//! Core reconciliation loop: observe manifests, diff against runtime, plan actions.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::diff::{prioritize_and_limit, StateDiffer};
use crate::manifest::ManifestParser;
use crate::types::{ActualState, ReconciliationAction};

/// Configuration for the reconciliation loop.
#[derive(Debug, Clone)]
pub struct ReconcilerConfig {
    /// How often to run the full reconciliation loop (seconds).
    /// Default: 30. Minimum: 5.
    pub reconcile_interval_secs: u64,

    /// Whether to watch for file changes (immediate reconciliation on write).
    /// If false, only runs on the interval.
    pub watch_filesystem: bool,

    /// Maximum actions per reconciliation tick (prevents runaway).
    pub max_actions_per_tick: usize,

    /// Path to the Archive root (filesystem path: `knowledge-base/`).
    pub archive_path: PathBuf,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            reconcile_interval_secs: 30,
            watch_filesystem: false,
            max_actions_per_tick: 10,
            archive_path: PathBuf::from("knowledge-base"),
        }
    }
}

/// Queries the current runtime state. Implemented by the daemon.
#[async_trait]
pub trait StateQuery: Send + Sync {
    async fn query(&self) -> Result<ActualState>;
}

/// The reconciler: reads manifests, diffs against runtime, generates actions.
pub struct Reconciler {
    config: ReconcilerConfig,
    manifest_parser: ManifestParser,
    differ: StateDiffer,
    state_query: Box<dyn StateQuery>,
}

impl Reconciler {
    pub fn new(config: ReconcilerConfig, state_query: Box<dyn StateQuery>) -> Self {
        Self {
            config,
            manifest_parser: ManifestParser::new(),
            differ: StateDiffer::new(),
            state_query,
        }
    }

    /// Run one reconciliation tick. Returns the actions that were generated.
    ///
    /// The caller is responsible for executing the actions (the reconciler
    /// only observes, diffs, and plans — it does not execute).
    pub async fn reconcile(&self) -> Result<Vec<ReconciliationAction>> {
        // 1. Observe: read all manifests from the Archive
        let desired = self.manifest_parser.parse_all(&self.config.archive_path)?;

        // 2. Query: get actual runtime state
        let actual = self.state_query.query().await?;

        // 3. Diff: compare desired vs actual
        let actions = self.differ.diff(&desired, &actual);

        // 4. Prioritize and cap
        let actions = prioritize_and_limit(actions, self.config.max_actions_per_tick);

        Ok(actions)
    }

    /// Access the reconciler configuration.
    pub fn config(&self) -> &ReconcilerConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct MockStateQuery {
        state: Mutex<ActualState>,
    }

    impl MockStateQuery {
        fn new(state: ActualState) -> Self {
            Self {
                state: Mutex::new(state),
            }
        }
    }

    #[async_trait]
    impl StateQuery for MockStateQuery {
        async fn query(&self) -> Result<ActualState> {
            Ok(self.state.lock().unwrap().clone())
        }
    }

    fn write_file(dir: &std::path::Path, rel_path: &str, content: &str) {
        let path = dir.join(rel_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    #[tokio::test]
    async fn reconcile_detects_new_goal() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "operations/projects/test-project/project.md",
            r#"---
id: "project:test-project"
slug: test-project
title: "Test Project"
state: active
---

# Project
"#,
        );
        write_file(
            tmp.path(),
            "operations/projects/test-project/goals/test/plan.md",
            r#"---
id: "test-id"
project_id: "project:test-project"
slug: test
title: "Test Goal"
state: active
priority: 1
autonomy_level: auto
phase: research
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
domains: [engineering]
vault_namespace: goal-test
---

# Plan
"#,
        );

        let reconciler = Reconciler::new(
            ReconcilerConfig {
                archive_path: tmp.path().to_path_buf(),
                ..Default::default()
            },
            Box::new(MockStateQuery::new(ActualState::default())),
        );

        let actions = reconciler.reconcile().await.unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, ActionType::StartGoal);
        assert_eq!(actions[0].target, "test");
    }

    #[tokio::test]
    async fn reconcile_no_actions_when_state_matches() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "operations/projects/test-project/project.md",
            r#"---
id: "project:test-project"
slug: test-project
title: "Test Project"
state: active
---

# Project
"#,
        );
        write_file(
            tmp.path(),
            "operations/projects/test-project/goals/test/plan.md",
            r#"---
id: "test-id"
project_id: "project:test-project"
slug: test
title: "Test Goal"
state: active
priority: 1
autonomy_level: semi
phase: research
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
domains: []
vault_namespace: goal-test
---

# Plan
"#,
        );

        let actual = ActualState {
            active_goals: vec![ActiveGoalState {
                slug: "test".to_string(),
                state: GoalState::Active,
                phase: GoalPhase::Research,
                running_agents: 1,
            }],
            ..Default::default()
        };

        let reconciler = Reconciler::new(
            ReconcilerConfig {
                archive_path: tmp.path().to_path_buf(),
                ..Default::default()
            },
            Box::new(MockStateQuery::new(actual)),
        );

        let actions = reconciler.reconcile().await.unwrap();
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn reconcile_caps_at_max_actions() {
        let tmp = TempDir::new().unwrap();

        // Create 15 goals — more than the default max of 10
        for i in 0..15 {
            write_file(
                tmp.path(),
                &format!("operations/projects/project-{i}/project.md"),
                &format!(
                    r#"---
id: "project:project-{i}"
slug: project-{i}
title: "Project {i}"
state: active
---

# Project {i}
"#
                ),
            );
            write_file(
                tmp.path(),
                &format!("operations/projects/project-{i}/goals/goal-{i}/plan.md"),
                &format!(
                    r#"---
id: "id-{i}"
project_id: "project:project-{i}"
slug: goal-{i}
title: "Goal {i}"
state: active
priority: 2
autonomy_level: auto
phase: research
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
domains: []
vault_namespace: goal-{i}
---

# Plan {i}
"#
                ),
            );
        }

        let reconciler = Reconciler::new(
            ReconcilerConfig {
                archive_path: tmp.path().to_path_buf(),
                max_actions_per_tick: 10,
                ..Default::default()
            },
            Box::new(MockStateQuery::new(ActualState::default())),
        );

        let actions = reconciler.reconcile().await.unwrap();
        assert!(
            actions.len() <= 10,
            "expected at most 10 actions, got {}",
            actions.len()
        );
    }

    #[tokio::test]
    async fn reconcile_empty_kb_no_actions() {
        let tmp = TempDir::new().unwrap();

        let reconciler = Reconciler::new(
            ReconcilerConfig {
                archive_path: tmp.path().to_path_buf(),
                ..Default::default()
            },
            Box::new(MockStateQuery::new(ActualState::default())),
        );

        let actions = reconciler.reconcile().await.unwrap();
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn reconcile_detects_identity_change() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "identity/SOUL.md",
            "---\nversion: 1\n---\n\n# SOUL v2\n",
        );

        let actual = ActualState {
            identity_hash: Some("old-hash".to_string()),
            ..Default::default()
        };

        let reconciler = Reconciler::new(
            ReconcilerConfig {
                archive_path: tmp.path().to_path_buf(),
                ..Default::default()
            },
            Box::new(MockStateQuery::new(actual)),
        );

        let actions = reconciler.reconcile().await.unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_type, ActionType::ReloadIdentity);
    }
}
