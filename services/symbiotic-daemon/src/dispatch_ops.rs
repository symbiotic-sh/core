//! Concrete implementations of the `ActionDispatcher` subsystem traits.
//!
//! These structs implement `GoalOps`, `AgentOps`, `SkillOps`, and
//! `IdentityOps` from `symbiotic-agents::action_dispatch` by delegating
//! to the daemon's actual subsystem handles (GoalProcessManager, agent
//! framework, skill registry, identity loader).
//!
//! Each implementation is designed to be `Send + Sync` and holds only
//! shared references (`Arc`, `Arc<Mutex<..>>`) to daemon state. The
//! `SymbioticDaemon` itself is NOT `Send + Sync` (it holds `Rc` types),
//! so these ops structs extract only the thread-safe parts they need.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use symbiotic_agents::action_dispatch::{AgentOps, GoalOps, IdentityOps, SkillOps};
use symbiotic_control_plane::{GoalProcessManager, ManagementStore, WorkItemStatus, WorkPriority};

// ---------------------------------------------------------------------------
// DaemonGoalOps
// ---------------------------------------------------------------------------

/// Concrete `GoalOps` implementation that delegates to the daemon's
/// `GoalProcessManager` for validated state transitions and persists
/// changes to the goal state TSV file.
pub struct DaemonGoalOps {
    /// GoalProcessManager — validated lifecycle with metrics and JSON persistence.
    pub goal_process_manager: Arc<Mutex<GoalProcessManager>>,
    /// Path to the goal state TSV file (audit trail).
    pub goal_state_file: PathBuf,
    /// Path to the goal log file (event log).
    pub goal_log_file: PathBuf,
    /// Shared management store for top-level goal ownership tracking.
    pub management_store: Arc<Mutex<ManagementStore>>,
}

#[async_trait]
impl GoalOps for DaemonGoalOps {
    async fn start_goal(&self, slug: &str) -> Result<String> {
        let now = symbiotic_queue::now_unix();
        let goal_room = format!("dispatch:{slug}");

        // Create the goal in the GoalProcessManager if it doesn't exist.
        {
            let mut gpm = self
                .goal_process_manager
                .lock()
                .map_err(|e| anyhow!("GPM lock poisoned: {e}"))?;

            // Only create if not already tracked.
            if gpm.get(slug).is_none() {
                use symbiotic_control_plane::types::{
                    AutonomyLevel, CheckFrequency, GoalConstraints, GoalManifest, GoalPhase,
                    GoalState as CpGoalState, GoalTaskPolicyDefaults, ProcessConfig, ProcessType,
                    StreamConfig,
                };

                let manifest = GoalManifest {
                    id: format!("dispatch-{slug}"),
                    project_id: format!("project:{slug}"),
                    slug: slug.to_string(),
                    title: slug.to_string(),
                    state: CpGoalState::Active,
                    priority: 50,
                    autonomy_level: AutonomyLevel::Semi,
                    phase: GoalPhase::Inquisition,
                    process: ProcessConfig {
                        process_type: ProcessType::Periodic,
                        check_frequency: CheckFrequency::Daily,
                        max_parallel_agents: 1,
                    },
                    streams: vec![StreamConfig {
                        name: "main".to_string(),
                        domain: String::new(),
                        focus: slug.to_string(),
                        autonomy: AutonomyLevel::Semi,
                    }],
                    domains: vec![],
                    vault_namespace: format!("goal-{slug}"),
                    thread_id: None,
                    plan_version: 1,
                    policy_scopes: Vec::new(),
                    task_policy_defaults: GoalTaskPolicyDefaults::default(),
                    constraints: GoalConstraints::default(),
                    plan_markdown: String::new(),
                    tasks: Vec::new(),
                };

                gpm.create_goal(&manifest)
                    .map_err(|e| anyhow!("failed to create goal '{slug}' in GPM: {e}"))?;
            }
        }

        // Record state in TSV audit trail.
        crate::goal_state::upsert_goal_state(
            &self.goal_state_file,
            crate::goal_state::GoalState {
                goal_room: goal_room.clone(),
                thread_id: None,
                project_id: format!("project:{slug}"),
                template: "dispatch".to_string(),
                status: "starting".to_string(),
                last_job_id: format!("dispatch-{now}"),
                last_run_id: None,
                owner: Some("action_dispatcher".to_string()),
                updated_at: now,
                complexity: None,
                pipeline_stage: Some("starting".to_string()),
                audit_id: None,
                plan_id: None,
            },
        )?;

        // Log the start event.
        let _ = crate::goal_state::append_goal_log(
            &self.goal_log_file,
            crate::goal_state::GoalLogEntry {
                ts: now,
                event: "goal.dispatch.started",
                workflow_job_id: &format!("dispatch-{now}"),
                goal_room: Some(&goal_room),
                goal_sender: Some("action_dispatcher"),
                template: "dispatch",
                detail: slug,
            },
        );

        crate::goal_management::sync_goal_work_item(
            &self.management_store,
            crate::goal_management::GoalWorkItemUpdate {
                slug,
                title: slug,
                project_id: crate::goals::DEFAULT_UNSCOPED_PROJECT_ID,
                phase: Some("starting"),
                owner: "action_dispatcher",
                thread_id: None,
                priority: WorkPriority::P1,
                status: WorkItemStatus::Running,
                observed_at: now as i64,
            },
        );
        tracing::info!(slug = %slug, "dispatch_ops: goal started");
        Ok(format!("goal '{slug}' started"))
    }

    async fn stop_goal(&self, slug: &str) -> Result<String> {
        let now = symbiotic_queue::now_unix();
        let goal_room = format!("dispatch:{slug}");

        // Transition to Abandoned in GPM, then remove.
        {
            let mut gpm = self
                .goal_process_manager
                .lock()
                .map_err(|e| anyhow!("GPM lock poisoned: {e}"))?;

            if gpm.get(slug).is_some() {
                let _ = gpm.transition(slug, symbiotic_control_plane::types::GoalState::Abandoned);
                let _ = gpm.remove(slug);
            }
        }

        crate::goal_state::upsert_goal_state(
            &self.goal_state_file,
            crate::goal_state::GoalState {
                goal_room: goal_room.clone(),
                thread_id: None,
                project_id: format!("project:{slug}"),
                template: "dispatch".to_string(),
                status: "stopped".to_string(),
                last_job_id: format!("dispatch-{now}"),
                last_run_id: None,
                owner: Some("action_dispatcher".to_string()),
                updated_at: now,
                complexity: None,
                pipeline_stage: Some("stopped".to_string()),
                audit_id: None,
                plan_id: None,
            },
        )?;

        let _ = crate::goal_state::append_goal_log(
            &self.goal_log_file,
            crate::goal_state::GoalLogEntry {
                ts: now,
                event: "goal.dispatch.stopped",
                workflow_job_id: &format!("dispatch-{now}"),
                goal_room: Some(&goal_room),
                goal_sender: Some("action_dispatcher"),
                template: "dispatch",
                detail: slug,
            },
        );

        crate::goal_management::sync_goal_work_item(
            &self.management_store,
            crate::goal_management::GoalWorkItemUpdate {
                slug,
                title: slug,
                project_id: crate::goals::DEFAULT_UNSCOPED_PROJECT_ID,
                phase: Some("stopped"),
                owner: "action_dispatcher",
                thread_id: None,
                priority: WorkPriority::P1,
                status: WorkItemStatus::Cancelled,
                observed_at: now as i64,
            },
        );
        tracing::info!(slug = %slug, "dispatch_ops: goal stopped");
        Ok(format!("goal '{slug}' stopped"))
    }

    async fn pause_goal(&self, slug: &str) -> Result<String> {
        let now = symbiotic_queue::now_unix();
        let goal_room = format!("dispatch:{slug}");

        {
            let mut gpm = self
                .goal_process_manager
                .lock()
                .map_err(|e| anyhow!("GPM lock poisoned: {e}"))?;

            if gpm.get(slug).is_some() {
                gpm.transition(slug, symbiotic_control_plane::types::GoalState::Paused)
                    .map_err(|e| anyhow!("failed to pause goal '{slug}' in GPM: {e}"))?;
            }
        }

        crate::goal_state::upsert_goal_state(
            &self.goal_state_file,
            crate::goal_state::GoalState {
                goal_room,
                thread_id: None,
                project_id: format!("project:{slug}"),
                template: "dispatch".to_string(),
                status: "paused".to_string(),
                last_job_id: format!("dispatch-{now}"),
                last_run_id: None,
                owner: Some("action_dispatcher".to_string()),
                updated_at: now,
                complexity: None,
                pipeline_stage: Some("paused".to_string()),
                audit_id: None,
                plan_id: None,
            },
        )?;

        crate::goal_management::sync_goal_work_item(
            &self.management_store,
            crate::goal_management::GoalWorkItemUpdate {
                slug,
                title: slug,
                project_id: crate::goals::DEFAULT_UNSCOPED_PROJECT_ID,
                phase: Some("paused"),
                owner: "action_dispatcher",
                thread_id: None,
                priority: WorkPriority::P1,
                status: WorkItemStatus::Blocked,
                observed_at: now as i64,
            },
        );
        tracing::info!(slug = %slug, "dispatch_ops: goal paused");
        Ok(format!("goal '{slug}' paused"))
    }

    async fn resume_goal(&self, slug: &str) -> Result<String> {
        let now = symbiotic_queue::now_unix();
        let goal_room = format!("dispatch:{slug}");

        {
            let mut gpm = self
                .goal_process_manager
                .lock()
                .map_err(|e| anyhow!("GPM lock poisoned: {e}"))?;

            if gpm.get(slug).is_some() {
                gpm.transition(slug, symbiotic_control_plane::types::GoalState::Active)
                    .map_err(|e| anyhow!("failed to resume goal '{slug}' in GPM: {e}"))?;
            }
        }

        crate::goal_state::upsert_goal_state(
            &self.goal_state_file,
            crate::goal_state::GoalState {
                goal_room,
                thread_id: None,
                project_id: format!("project:{slug}"),
                template: "dispatch".to_string(),
                status: "running".to_string(),
                last_job_id: format!("dispatch-{now}"),
                last_run_id: None,
                owner: Some("action_dispatcher".to_string()),
                updated_at: now,
                complexity: None,
                pipeline_stage: Some("running".to_string()),
                audit_id: None,
                plan_id: None,
            },
        )?;

        crate::goal_management::sync_goal_work_item(
            &self.management_store,
            crate::goal_management::GoalWorkItemUpdate {
                slug,
                title: slug,
                project_id: crate::goals::DEFAULT_UNSCOPED_PROJECT_ID,
                phase: Some("running"),
                owner: "action_dispatcher",
                thread_id: None,
                priority: WorkPriority::P1,
                status: WorkItemStatus::Running,
                observed_at: now as i64,
            },
        );
        tracing::info!(slug = %slug, "dispatch_ops: goal resumed");
        Ok(format!("goal '{slug}' resumed"))
    }

    async fn advance_phase(&self, slug: &str, from: &str, to: &str) -> Result<String> {
        let now = symbiotic_queue::now_unix();
        let goal_room = format!("dispatch:{slug}");

        // Map phase string to GoalPhase enum.
        let to_phase = parse_goal_phase(to);

        {
            let mut gpm = self
                .goal_process_manager
                .lock()
                .map_err(|e| anyhow!("GPM lock poisoned: {e}"))?;

            if gpm.get(slug).is_some() {
                gpm.advance_phase(slug, to_phase)
                    .map_err(|e| anyhow!("failed to advance phase for '{slug}': {e}"))?;
            }
        }

        crate::goal_state::upsert_goal_state(
            &self.goal_state_file,
            crate::goal_state::GoalState {
                goal_room,
                thread_id: None,
                project_id: format!("project:{slug}"),
                template: "dispatch".to_string(),
                status: "running".to_string(),
                last_job_id: format!("dispatch-{now}"),
                last_run_id: None,
                owner: Some("action_dispatcher".to_string()),
                updated_at: now,
                complexity: None,
                pipeline_stage: Some(to.to_string()),
                audit_id: None,
                plan_id: None,
            },
        )?;

        crate::goal_management::sync_goal_work_item(
            &self.management_store,
            crate::goal_management::GoalWorkItemUpdate {
                slug,
                title: slug,
                project_id: crate::goals::DEFAULT_UNSCOPED_PROJECT_ID,
                phase: Some(to),
                owner: "action_dispatcher",
                thread_id: None,
                priority: WorkPriority::P1,
                status: WorkItemStatus::Running,
                observed_at: now as i64,
            },
        );
        tracing::info!(
            slug = %slug,
            from = %from,
            to = %to,
            "dispatch_ops: goal phase advanced"
        );
        Ok(format!("goal '{slug}' advanced from '{from}' to '{to}'"))
    }
}

/// Convert a phase string to a control-plane `GoalPhase`.
fn parse_goal_phase(phase: &str) -> symbiotic_control_plane::types::GoalPhase {
    use symbiotic_control_plane::types::GoalPhase;
    match phase {
        "research" => GoalPhase::Research,
        "implementation" => GoalPhase::Implementation,
        "provisioning" => GoalPhase::Provisioning,
        "maintenance" => GoalPhase::Maintenance,
        "inquisition" => GoalPhase::Inquisition,
        _ => GoalPhase::Inquisition,
    }
}

// ---------------------------------------------------------------------------
// DaemonAgentOps
// ---------------------------------------------------------------------------

/// Concrete `AgentOps` implementation that spawns agents using the daemon's
/// `SecureAgentFramework`.
///
/// Agent spawning requires the non-Send `SymbioticDaemon`, which cannot be
/// shared across threads. Instead, this implementation logs the spawn
/// request and records it in the goal state file so the daemon's main
/// loop can pick it up and execute `spawn_task_agent()` synchronously.
///
/// For the MVP, spawn requests are recorded as state updates. The daemon's
/// main event loop checks for pending spawn requests during each tick.
pub struct DaemonAgentOps {
    /// Path to the goal state TSV file for recording spawn requests.
    pub goal_state_file: PathBuf,
    /// Path to the agent log file.
    pub agent_log_file: PathBuf,
}

#[async_trait]
impl AgentOps for DaemonAgentOps {
    async fn spawn_agents(&self, goal_slug: &str, count: usize) -> Result<String> {
        let now = symbiotic_queue::now_unix();

        // Record the spawn request in the goal state so the daemon's
        // main loop can execute the actual spawn synchronously.
        crate::goal_state::upsert_goal_state(
            &self.goal_state_file,
            crate::goal_state::GoalState {
                goal_room: format!("dispatch:{goal_slug}"),
                thread_id: None,
                project_id: format!("project:{goal_slug}"),
                template: "dispatch".to_string(),
                status: "spawning_agents".to_string(),
                last_job_id: format!("spawn-{now}"),
                last_run_id: None,
                owner: Some("action_dispatcher".to_string()),
                updated_at: now,
                complexity: None,
                pipeline_stage: Some(format!("spawning:{count}")),
                audit_id: None,
                plan_id: None,
            },
        )?;

        // Log the spawn request.
        crate::goal_state::append_agent_lifecycle_log(
            &self.agent_log_file,
            crate::goal_state::AgentLogEntry {
                ts: now,
                event: "agent.spawn_requested",
                agent_id: &format!("pending-{goal_slug}"),
                status: "requested",
                scope: None,
                detail: &format!("count={count} goal={goal_slug}"),
            },
        )?;

        tracing::info!(
            goal_slug = %goal_slug,
            count = count,
            "dispatch_ops: agent spawn requested"
        );
        Ok(format!(
            "spawn of {count} agent(s) requested for '{goal_slug}'"
        ))
    }
}

// ---------------------------------------------------------------------------
// DaemonSkillOps
// ---------------------------------------------------------------------------

/// Concrete `SkillOps` implementation that manages the daemon's skill registry.
///
/// Loads and unloads skills by updating the shared `ActualState` in the
/// control-plane reconciler, which tracks loaded skills.
pub struct DaemonSkillOps {
    /// Shared actual state from the control-plane reconciler.
    pub actual_state: Arc<Mutex<symbiotic_control_plane::types::ActualState>>,
}

#[async_trait]
impl SkillOps for DaemonSkillOps {
    async fn load_skill(&self, name: &str) -> Result<String> {
        let mut state = self
            .actual_state
            .lock()
            .map_err(|e| anyhow!("actual state lock poisoned: {e}"))?;

        if !state.loaded_skills.contains(&name.to_string()) {
            state.loaded_skills.push(name.to_string());
        }

        tracing::info!(skill = %name, "dispatch_ops: skill loaded");
        Ok(format!("skill '{name}' loaded"))
    }

    async fn unload_skill(&self, name: &str) -> Result<String> {
        let mut state = self
            .actual_state
            .lock()
            .map_err(|e| anyhow!("actual state lock poisoned: {e}"))?;

        state.loaded_skills.retain(|s| s != name);

        tracing::info!(skill = %name, "dispatch_ops: skill unloaded");
        Ok(format!("skill '{name}' unloaded"))
    }
}

// ---------------------------------------------------------------------------
// DaemonIdentityOps
// ---------------------------------------------------------------------------

/// Concrete `IdentityOps` implementation that reloads SOUL.md and
/// preferences from the filesystem.
///
/// Shares the `identity_content` handle with the daemon so that
/// `resolve_role_config()` immediately sees updated identity context
/// after a reload.
pub struct DaemonIdentityOps {
    /// Shared identity content (Mutex<Option<String>>) — same Arc the daemon holds.
    pub identity_content: Arc<Mutex<Option<String>>>,
    /// Shared actual state for updating identity/preferences hashes.
    pub actual_state: Arc<Mutex<symbiotic_control_plane::types::ActualState>>,
    /// Path to the knowledge base / archive root.
    pub kb_path: PathBuf,
}

#[async_trait]
impl IdentityOps for DaemonIdentityOps {
    async fn reload_identity(&self) -> Result<String> {
        let parser = symbiotic_control_plane::manifest::ManifestParser::new();
        let soul_path = parser
            .resolve_identity_path(&self.kb_path)
            .ok_or_else(|| anyhow!("missing identity/SOUL.md"))?;

        let identity = parser
            .parse_identity(&soul_path)
            .map_err(|e| anyhow!("failed to reload SOUL.md: {e}"))?;

        // Update identity content for agents.
        if let Ok(mut content) = self.identity_content.lock() {
            *content = Some(identity.content.clone());
        }

        // Update identity hash in actual state.
        if let Ok(mut state) = self.actual_state.lock() {
            state.identity_hash = Some(identity.content_hash.clone());
        }

        tracing::info!(
            hash = %identity.content_hash,
            "dispatch_ops: identity reloaded from SOUL.md"
        );
        Ok(format!(
            "identity reloaded (hash={})",
            identity.content_hash
        ))
    }

    async fn reload_preferences(&self) -> Result<String> {
        let parser = symbiotic_control_plane::manifest::ManifestParser::new();
        let prefs_path = parser
            .resolve_preferences_path(&self.kb_path)
            .ok_or_else(|| anyhow!("missing identity/preferences.md"))?;

        let prefs = parser
            .parse_preferences(&prefs_path)
            .map_err(|e| anyhow!("failed to reload preferences: {e}"))?;

        if let Ok(mut state) = self.actual_state.lock() {
            state.preferences_hash = Some(prefs.content_hash.clone());
        }

        tracing::info!(
            hash = %prefs.content_hash,
            "dispatch_ops: preferences reloaded"
        );
        Ok(format!(
            "preferences reloaded (hash={})",
            prefs.content_hash
        ))
    }
}

// ---------------------------------------------------------------------------
// Factory: build ActionDispatcher from SymbioticDaemon
// ---------------------------------------------------------------------------

use symbiotic_agents::action_dispatch::ActionDispatcher;

impl crate::SymbioticDaemon {
    /// Build an `ActionDispatcher` wired to this daemon's subsystems.
    ///
    /// The returned dispatcher (and all its ops) is `Send + Sync` and can
    /// be moved into the tokio-spawned reconciler background loop.
    ///
    /// Requires an `actual_state` handle from `DaemonStateQuery` so that
    /// `DaemonSkillOps` and `DaemonIdentityOps` can update the reconciler's
    /// view of the world.
    pub fn build_action_dispatcher(
        &self,
        actual_state: Arc<Mutex<symbiotic_control_plane::types::ActualState>>,
    ) -> Arc<ActionDispatcher> {
        let goal_ops = Arc::new(DaemonGoalOps {
            goal_process_manager: Arc::clone(&self.goal_process_manager),
            goal_state_file: self.config.goal_state_file.clone(),
            goal_log_file: self.config.goal_log_file.clone(),
            management_store: Arc::clone(&self.management_store),
        });

        let agent_ops = Arc::new(DaemonAgentOps {
            goal_state_file: self.config.goal_state_file.clone(),
            agent_log_file: self.config.agent_log_file.clone(),
        });

        let skill_ops = Arc::new(DaemonSkillOps {
            actual_state: Arc::clone(&actual_state),
        });

        // Determine the kb_path for identity ops.
        // Resolution: explicit archive_path > data_dir fallback.
        let kb_path = self
            .config
            .archive_path
            .clone()
            .unwrap_or_else(|| self.config.data_dir.join("archive"));

        let identity_ops = Arc::new(DaemonIdentityOps {
            identity_content: Arc::clone(&self.identity_content),
            actual_state,
            kb_path,
        });

        Arc::new(ActionDispatcher::new(
            goal_ops,
            agent_ops,
            skill_ops,
            identity_ops,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // GoalOps tests
    // -----------------------------------------------------------------------

    fn make_goal_ops(tmp: &TempDir) -> DaemonGoalOps {
        let gpm_store_path = tmp.path().join("gpm");
        let gpm = GoalProcessManager::new(gpm_store_path);
        DaemonGoalOps {
            goal_process_manager: Arc::new(Mutex::new(gpm)),
            goal_state_file: tmp.path().join("state.tsv"),
            goal_log_file: tmp.path().join("runs.log"),
            management_store: Arc::new(Mutex::new(ManagementStore::new(
                tmp.path().join("control-plane"),
            ))),
        }
    }

    #[tokio::test]
    async fn goal_ops_start_creates_state() {
        let tmp = TempDir::new().unwrap();
        let ops = make_goal_ops(&tmp);

        let result = ops.start_goal("test-goal").await;
        assert!(result.is_ok());
        let detail = result.unwrap();
        assert!(detail.contains("test-goal"));
        assert!(detail.contains("started"));

        // Verify state file was written.
        let states = crate::goal_state::load_goal_states(&tmp.path().join("state.tsv")).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].goal_room, "dispatch:test-goal");
        assert_eq!(states[0].status, "starting");

        // Verify GPM has the goal.
        let gpm = ops.goal_process_manager.lock().unwrap();
        assert!(gpm.get("test-goal").is_some());

        let store = ops.management_store.lock().unwrap();
        let work_item = store.get_work_item("goal:test-goal").unwrap();
        assert_eq!(work_item.status, WorkItemStatus::Running);
    }

    #[tokio::test]
    async fn goal_ops_stop_removes_from_gpm() {
        let tmp = TempDir::new().unwrap();
        let ops = make_goal_ops(&tmp);

        // Start then stop.
        ops.start_goal("ephemeral").await.unwrap();
        let result = ops.stop_goal("ephemeral").await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("stopped"));

        // Verify state file.
        let states = crate::goal_state::load_goal_states(&tmp.path().join("state.tsv")).unwrap();
        assert_eq!(states[0].status, "stopped");

        // Verify GPM no longer has the goal.
        let gpm = ops.goal_process_manager.lock().unwrap();
        assert!(gpm.get("ephemeral").is_none());

        let store = ops.management_store.lock().unwrap();
        let work_item = store.get_work_item("goal:ephemeral").unwrap();
        assert_eq!(work_item.status, WorkItemStatus::Cancelled);
    }

    #[tokio::test]
    async fn goal_ops_pause_and_resume() {
        let tmp = TempDir::new().unwrap();
        let ops = make_goal_ops(&tmp);

        ops.start_goal("lifecycle").await.unwrap();

        // Pause.
        let result = ops.pause_goal("lifecycle").await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("paused"));

        let states = crate::goal_state::load_goal_states(&tmp.path().join("state.tsv")).unwrap();
        assert_eq!(states[0].status, "paused");

        // Resume.
        let result = ops.resume_goal("lifecycle").await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("resumed"));

        let states = crate::goal_state::load_goal_states(&tmp.path().join("state.tsv")).unwrap();
        assert_eq!(states[0].status, "running");

        let store = ops.management_store.lock().unwrap();
        let work_item = store.get_work_item("goal:lifecycle").unwrap();
        assert_eq!(work_item.status, WorkItemStatus::Running);
    }

    #[tokio::test]
    async fn goal_ops_advance_phase() {
        let tmp = TempDir::new().unwrap();
        let ops = make_goal_ops(&tmp);

        ops.start_goal("phased").await.unwrap();

        let result = ops.advance_phase("phased", "inquisition", "research").await;
        assert!(result.is_ok());
        let detail = result.unwrap();
        assert!(detail.contains("inquisition"));
        assert!(detail.contains("research"));

        let states = crate::goal_state::load_goal_states(&tmp.path().join("state.tsv")).unwrap();
        assert_eq!(states[0].pipeline_stage, Some("research".to_string()));

        let store = ops.management_store.lock().unwrap();
        let work_item = store.get_work_item("goal:phased").unwrap();
        assert!(work_item.summary.contains("phase: research"));
    }

    #[tokio::test]
    async fn goal_ops_start_idempotent() {
        let tmp = TempDir::new().unwrap();
        let ops = make_goal_ops(&tmp);

        // Start the same goal twice — should not error.
        ops.start_goal("idempotent").await.unwrap();
        let result = ops.start_goal("idempotent").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn goal_ops_stop_nonexistent_succeeds() {
        let tmp = TempDir::new().unwrap();
        let ops = make_goal_ops(&tmp);

        // Stopping a goal that was never started should still succeed.
        let result = ops.stop_goal("ghost").await;
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // AgentOps tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn agent_ops_spawn_records_request() {
        let tmp = TempDir::new().unwrap();
        let ops = DaemonAgentOps {
            goal_state_file: tmp.path().join("state.tsv"),
            agent_log_file: tmp.path().join("agents.log"),
        };

        let result = ops.spawn_agents("my-goal", 3).await;
        assert!(result.is_ok());
        let detail = result.unwrap();
        assert!(detail.contains("3"));
        assert!(detail.contains("my-goal"));

        // Verify state file was written.
        let states = crate::goal_state::load_goal_states(&tmp.path().join("state.tsv")).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status, "spawning_agents");
        assert!(states[0]
            .pipeline_stage
            .as_ref()
            .unwrap()
            .contains("spawning:3"));

        // Verify agent log was written.
        let log_content = std::fs::read_to_string(tmp.path().join("agents.log")).unwrap();
        assert!(log_content.contains("agent.spawn_requested"));
    }

    // -----------------------------------------------------------------------
    // SkillOps tests
    // -----------------------------------------------------------------------

    fn make_skill_ops() -> (
        DaemonSkillOps,
        Arc<Mutex<symbiotic_control_plane::types::ActualState>>,
    ) {
        let state = Arc::new(Mutex::new(
            symbiotic_control_plane::types::ActualState::default(),
        ));
        let ops = DaemonSkillOps {
            actual_state: Arc::clone(&state),
        };
        (ops, state)
    }

    #[tokio::test]
    async fn skill_ops_load_and_unload() {
        let (ops, state) = make_skill_ops();

        // Load.
        let result = ops.load_skill("web-scraper").await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("web-scraper"));

        {
            let s = state.lock().unwrap();
            assert_eq!(s.loaded_skills, vec!["web-scraper".to_string()]);
        }

        // Load again (idempotent).
        ops.load_skill("web-scraper").await.unwrap();
        {
            let s = state.lock().unwrap();
            assert_eq!(s.loaded_skills.len(), 1);
        }

        // Unload.
        let result = ops.unload_skill("web-scraper").await;
        assert!(result.is_ok());

        {
            let s = state.lock().unwrap();
            assert!(s.loaded_skills.is_empty());
        }
    }

    #[tokio::test]
    async fn skill_ops_unload_nonexistent() {
        let (ops, state) = make_skill_ops();

        // Unloading something not loaded should still succeed.
        let result = ops.unload_skill("phantom").await;
        assert!(result.is_ok());

        let s = state.lock().unwrap();
        assert!(s.loaded_skills.is_empty());
    }

    // -----------------------------------------------------------------------
    // IdentityOps tests
    // -----------------------------------------------------------------------

    type IdentityContentState = Arc<Mutex<Option<String>>>;
    type ActualStateHandle = Arc<Mutex<symbiotic_control_plane::types::ActualState>>;

    fn make_identity_ops(
        tmp: &TempDir,
    ) -> (DaemonIdentityOps, IdentityContentState, ActualStateHandle) {
        let identity_content = Arc::new(Mutex::new(None));
        let actual_state = Arc::new(Mutex::new(
            symbiotic_control_plane::types::ActualState::default(),
        ));
        let ops = DaemonIdentityOps {
            identity_content: Arc::clone(&identity_content),
            actual_state: Arc::clone(&actual_state),
            kb_path: tmp.path().to_path_buf(),
        };
        (ops, identity_content, actual_state)
    }

    #[tokio::test]
    async fn identity_ops_reload_identity() {
        let tmp = TempDir::new().unwrap();
        let soul_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&soul_dir).unwrap();
        std::fs::write(
            soul_dir.join("SOUL.md"),
            "---\nversion: 1\n---\n\n# SOUL\nI am the test identity.\n",
        )
        .unwrap();

        let (ops, identity_content, actual_state) = make_identity_ops(&tmp);

        let result = ops.reload_identity().await;
        assert!(result.is_ok());
        let detail = result.unwrap();
        assert!(detail.contains("identity reloaded"));

        // Verify identity content was updated.
        let content = identity_content.lock().unwrap();
        assert!(content.is_some());
        assert!(content
            .as_ref()
            .unwrap()
            .contains("I am the test identity."));

        // Verify hash was set.
        let state = actual_state.lock().unwrap();
        assert!(state.identity_hash.is_some());
    }

    #[tokio::test]
    async fn identity_ops_reload_identity_missing_file() {
        let tmp = TempDir::new().unwrap();
        // No SOUL.md file created.

        let (ops, _, _) = make_identity_ops(&tmp);

        let result = ops.reload_identity().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("SOUL.md"));
    }

    #[tokio::test]
    async fn identity_ops_reload_preferences() {
        let tmp = TempDir::new().unwrap();
        let prefs_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&prefs_dir).unwrap();
        std::fs::write(
            prefs_dir.join("preferences.md"),
            "---\nversion: 1\n---\n\n# Preferences\nauto_approve: 0.85\n",
        )
        .unwrap();

        let (ops, _, actual_state) = make_identity_ops(&tmp);

        let result = ops.reload_preferences().await;
        assert!(result.is_ok());
        let detail = result.unwrap();
        assert!(detail.contains("preferences reloaded"));

        let state = actual_state.lock().unwrap();
        assert!(state.preferences_hash.is_some());
    }

    #[tokio::test]
    async fn identity_ops_reload_preferences_missing_file() {
        let tmp = TempDir::new().unwrap();
        // No preferences.md created.

        let (ops, _, _) = make_identity_ops(&tmp);

        let result = ops.reload_preferences().await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Integration: ActionDispatcher with concrete ops
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn dispatcher_with_concrete_ops() {
        use symbiotic_agents::action_dispatch::{
            ActionDispatcher, DispatchAction, DispatchActionType,
        };

        let tmp = TempDir::new().unwrap();

        // Set up SOUL.md for identity ops.
        let soul_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&soul_dir).unwrap();
        std::fs::write(
            soul_dir.join("SOUL.md"),
            "---\nversion: 1\n---\n\n# SOUL\nIntegration test identity.\n",
        )
        .unwrap();

        let gpm_store_path = tmp.path().join("gpm");
        let gpm = GoalProcessManager::new(gpm_store_path);
        let actual_state = Arc::new(Mutex::new(
            symbiotic_control_plane::types::ActualState::default(),
        ));
        let identity_content: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let goal_ops = Arc::new(DaemonGoalOps {
            goal_process_manager: Arc::new(Mutex::new(gpm)),
            goal_state_file: tmp.path().join("state.tsv"),
            goal_log_file: tmp.path().join("runs.log"),
            management_store: Arc::new(Mutex::new(ManagementStore::new(
                tmp.path().join("control-plane"),
            ))),
        });

        let agent_ops = Arc::new(DaemonAgentOps {
            goal_state_file: tmp.path().join("state.tsv"),
            agent_log_file: tmp.path().join("agents.log"),
        });

        let skill_ops = Arc::new(DaemonSkillOps {
            actual_state: Arc::clone(&actual_state),
        });

        let identity_ops = Arc::new(DaemonIdentityOps {
            identity_content: Arc::clone(&identity_content),
            actual_state: Arc::clone(&actual_state),
            kb_path: tmp.path().to_path_buf(),
        });

        let dispatcher = ActionDispatcher::new(goal_ops, agent_ops, skill_ops, identity_ops);

        // Dispatch a batch of actions.
        let actions = vec![
            DispatchAction {
                id: "act-1".to_string(),
                action_type: DispatchActionType::StartGoal,
                target: "integration-goal".to_string(),
                description: "Start test goal".to_string(),
                requires_approval: false,
                estimated_cost: None,
            },
            DispatchAction {
                id: "act-2".to_string(),
                action_type: DispatchActionType::SpawnAgents {
                    goal: "integration-goal".to_string(),
                    count: 2,
                },
                target: "integration-goal".to_string(),
                description: "Spawn agents".to_string(),
                requires_approval: false,
                estimated_cost: None,
            },
            DispatchAction {
                id: "act-3".to_string(),
                action_type: DispatchActionType::LoadSkill {
                    name: "web-search".to_string(),
                },
                target: "web-search".to_string(),
                description: "Load skill".to_string(),
                requires_approval: false,
                estimated_cost: None,
            },
            DispatchAction {
                id: "act-4".to_string(),
                action_type: DispatchActionType::ReloadIdentity,
                target: "SOUL.md".to_string(),
                description: "Reload identity".to_string(),
                requires_approval: false,
                estimated_cost: None,
            },
        ];

        let summary = dispatcher.dispatch_batch(&actions).await;

        assert_eq!(summary.total, 4);
        assert_eq!(summary.succeeded, 4);
        assert_eq!(summary.failed, 0);

        // Verify side effects.
        let ic = identity_content.lock().unwrap();
        assert!(ic.is_some());
        assert!(ic.as_ref().unwrap().contains("Integration test identity."));

        let state = actual_state.lock().unwrap();
        assert!(state.loaded_skills.contains(&"web-search".to_string()));
        assert!(state.identity_hash.is_some());
    }
}
