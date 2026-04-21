//! Declarative Reconciliation — bridges file-system project/goal manifests to the
//! deliberation pipeline.
//!
//! When project/goal manifests (Markdown files with YAML frontmatter) change on disk,
//! the reconciler detects differences between desired state (manifest) and
//! actual state (running goals) and produces corrective actions.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use symbiotic_control_plane::{GoalProcessManager, ManagementStore, WorkItemStatus};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the goal reconciler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcilerConfig {
    /// Directory containing project manifests (e.g., `operations/projects/`).
    pub manifest_dir: PathBuf,
    /// Debounce interval for file change events (seconds). Default: 2.
    pub debounce_secs: u64,
    /// Whether to auto-reconcile on startup.
    pub reconcile_on_startup: bool,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            manifest_dir: PathBuf::from("operations/projects"),
            debounce_secs: 2,
            reconcile_on_startup: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Manifest types
// ---------------------------------------------------------------------------

/// State declared in a goal manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManifestState {
    Active,
    Paused,
    Completed,
    Archived,
}

/// YAML frontmatter fields of a goal manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestFrontmatter {
    project_id: String,
    slug: String,
    title: String,
    state: ManifestState,
    #[serde(default = "default_priority")]
    priority: u32,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default = "default_autonomy")]
    autonomy_level: String,
    #[serde(default)]
    domains: Vec<String>,
}

fn default_priority() -> u32 {
    50
}

fn default_autonomy() -> String {
    "semi".to_string()
}

/// A parsed goal manifest from a Markdown file with YAML frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GoalManifest {
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub state: ManifestState,
    /// Priority 0-100. Default: 50.
    pub priority: u32,
    /// Current phase name (e.g. "research", "implementation").
    pub phase: Option<String>,
    /// Autonomy level: "auto", "semi", "manual".
    pub autonomy_level: String,
    /// Markdown body below the frontmatter.
    pub description: String,
    /// Domains this goal belongs to.
    pub domains: Vec<String>,
    /// File path the manifest was loaded from.
    pub file_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Reconciliation actions
// ---------------------------------------------------------------------------

/// Actions the reconciler wants to take after comparing desired vs actual state.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconciliationAction {
    /// New manifest found, no running goal -- start pipeline processing.
    StartGoal { manifest: GoalManifest },
    /// Manifest state changed to paused -- pause running agents.
    PauseGoal { slug: String },
    /// Manifest state changed from paused to active -- resume goal.
    ResumeGoal {
        slug: String,
        manifest: GoalManifest,
    },
    /// Manifest phase advanced -- generate new plan for next phase.
    AdvancePhase {
        slug: String,
        new_phase: String,
        manifest: GoalManifest,
    },
    /// Manifest removed -- stop goal and cleanup.
    StopGoal { slug: String },
    /// Manifest changed but goal is already at the right state -- no action.
    NoAction { slug: String },
}

/// Represents the runtime state of a goal (from GoalState or scheduler).
#[derive(Debug, Clone)]
pub struct RuntimeGoalState {
    pub slug: String,
    pub is_running: bool,
    pub is_paused: bool,
    pub current_phase: Option<String>,
}

// ---------------------------------------------------------------------------
// GoalReconciler
// ---------------------------------------------------------------------------

/// Bridges the reconciliation loop to the deliberation pipeline.
pub struct GoalReconciler {
    manifest_dir: PathBuf,
    config: ReconcilerConfig,
}

impl GoalReconciler {
    /// Create a new reconciler with the given config.
    pub fn new(config: ReconcilerConfig) -> Self {
        let manifest_dir = config.manifest_dir.clone();
        Self {
            manifest_dir,
            config,
        }
    }

    /// Access the reconciler config.
    pub fn config(&self) -> &ReconcilerConfig {
        &self.config
    }

    /// Parse a single Markdown file with YAML frontmatter into a `GoalManifest`.
    ///
    /// The file must contain a YAML frontmatter block delimited by `---` on
    /// both sides. Everything below the closing `---` is the description body.
    pub fn parse_manifest(path: &Path) -> Result<GoalManifest> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest {}", path.display()))?;
        Self::parse_manifest_content(&content, path)
    }

    /// Parse manifest content (for testability without filesystem).
    pub fn parse_manifest_content(content: &str, path: &Path) -> Result<GoalManifest> {
        let trimmed = content.trim();
        if !trimmed.starts_with("---") {
            return Err(anyhow!(
                "manifest {} does not start with YAML frontmatter delimiter (---)",
                path.display()
            ));
        }

        // Find the closing `---` after the opening one.
        let after_opening = &trimmed[3..];
        let closing_pos = after_opening.find("\n---").ok_or_else(|| {
            anyhow!(
                "manifest {} missing closing frontmatter delimiter (---)",
                path.display()
            )
        })? + 1; // skip the '\n'

        let yaml_str = &after_opening[..closing_pos - 1]; // exclude the '\n'
        let body_start = closing_pos + 3; // skip "---"
        let description = if body_start < after_opening.len() {
            after_opening[body_start..].trim().to_string()
        } else {
            String::new()
        };

        let frontmatter: ManifestFrontmatter = serde_yml::from_str(yaml_str)
            .with_context(|| format!("failed to parse YAML frontmatter in {}", path.display()))?;

        Ok(GoalManifest {
            project_id: frontmatter.project_id,
            slug: frontmatter.slug,
            title: frontmatter.title,
            state: frontmatter.state,
            priority: frontmatter.priority,
            phase: frontmatter.phase,
            autonomy_level: frontmatter.autonomy_level,
            description,
            domains: frontmatter.domains,
            file_path: path.to_path_buf(),
        })
    }

    /// Scan all `.md` files in the manifest directory and parse them.
    ///
    /// Files that fail to parse are skipped with a warning log (non-fatal).
    pub fn scan_manifests(&self) -> Result<Vec<GoalManifest>> {
        let dir = &self.manifest_dir;
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(dir)
            .with_context(|| format!("failed to read manifest directory {}", dir.display()))?;

        let mut manifests = Vec::new();

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            // Only process .md files.
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            match Self::parse_manifest(&path) {
                Ok(manifest) => manifests.push(manifest),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "reconciler: skipping unparseable manifest"
                    );
                }
            }
        }

        // Sort by slug for deterministic ordering.
        manifests.sort_by(|a, b| a.slug.cmp(&b.slug));

        Ok(manifests)
    }

    /// Compare desired state (manifests) to actual state (runtime) and
    /// produce reconciliation actions.
    ///
    /// Rules:
    /// - Manifest exists, no runtime state -> `StartGoal`
    /// - Manifest state is `paused`, runtime is running -> `PauseGoal`
    /// - Manifest state is `active`, runtime is paused -> `ResumeGoal`
    /// - Manifest phase differs from runtime phase -> `AdvancePhase`
    /// - Runtime state exists, no manifest -> `StopGoal`
    /// - States match -> `NoAction`
    pub fn reconcile(
        manifests: &[GoalManifest],
        runtime_states: &[RuntimeGoalState],
    ) -> Vec<ReconciliationAction> {
        let mut actions = Vec::new();

        // Index runtime states by slug for O(1) lookup.
        let runtime_map: HashMap<&str, &RuntimeGoalState> = runtime_states
            .iter()
            .map(|s| (s.slug.as_str(), s))
            .collect();

        // Index manifests by slug for detecting removed goals.
        let manifest_map: HashMap<&str, &GoalManifest> =
            manifests.iter().map(|m| (m.slug.as_str(), m)).collect();

        // Check each manifest against runtime state.
        for manifest in manifests {
            match runtime_map.get(manifest.slug.as_str()) {
                None => {
                    // New manifest, no runtime state -> start the goal
                    // (but only if it's in an actionable state).
                    if manifest.state == ManifestState::Active {
                        actions.push(ReconciliationAction::StartGoal {
                            manifest: manifest.clone(),
                        });
                    } else {
                        actions.push(ReconciliationAction::NoAction {
                            slug: manifest.slug.clone(),
                        });
                    }
                }
                Some(runtime) => {
                    let action = Self::diff_manifest_vs_runtime(manifest, runtime);
                    actions.push(action);
                }
            }
        }

        // Check for runtime states with no manifest -> stop goal.
        for runtime in runtime_states {
            if !manifest_map.contains_key(runtime.slug.as_str()) {
                actions.push(ReconciliationAction::StopGoal {
                    slug: runtime.slug.clone(),
                });
            }
        }

        actions
    }

    /// Diff a single manifest against its runtime state.
    fn diff_manifest_vs_runtime(
        manifest: &GoalManifest,
        runtime: &RuntimeGoalState,
    ) -> ReconciliationAction {
        // Manifest paused, runtime running -> pause.
        if manifest.state == ManifestState::Paused && runtime.is_running && !runtime.is_paused {
            return ReconciliationAction::PauseGoal {
                slug: manifest.slug.clone(),
            };
        }

        // Manifest active, runtime paused -> resume.
        if manifest.state == ManifestState::Active && runtime.is_paused {
            return ReconciliationAction::ResumeGoal {
                slug: manifest.slug.clone(),
                manifest: manifest.clone(),
            };
        }

        // Manifest phase advanced (phase differs and manifest is active).
        if manifest.state == ManifestState::Active {
            if let Some(ref manifest_phase) = manifest.phase {
                if runtime.current_phase.as_deref() != Some(manifest_phase.as_str()) {
                    return ReconciliationAction::AdvancePhase {
                        slug: manifest.slug.clone(),
                        new_phase: manifest_phase.clone(),
                        manifest: manifest.clone(),
                    };
                }
            }
        }

        // Manifest completed or archived -> stop the goal if it is still running.
        if matches!(
            manifest.state,
            ManifestState::Completed | ManifestState::Archived
        ) && runtime.is_running
        {
            return ReconciliationAction::StopGoal {
                slug: manifest.slug.clone(),
            };
        }

        ReconciliationAction::NoAction {
            slug: manifest.slug.clone(),
        }
    }

    /// Handle a single file change event by comparing the new manifest
    /// against a previously known manifest.
    ///
    /// If `previous` is `None`, this is treated as a new manifest.
    pub fn handle_manifest_change(
        new_manifest: &GoalManifest,
        previous: Option<&GoalManifest>,
    ) -> ReconciliationAction {
        let Some(prev) = previous else {
            // No previous manifest -> treat as new.
            if new_manifest.state == ManifestState::Active {
                return ReconciliationAction::StartGoal {
                    manifest: new_manifest.clone(),
                };
            }
            return ReconciliationAction::NoAction {
                slug: new_manifest.slug.clone(),
            };
        };

        // State changed to paused.
        if new_manifest.state == ManifestState::Paused && prev.state != ManifestState::Paused {
            return ReconciliationAction::PauseGoal {
                slug: new_manifest.slug.clone(),
            };
        }

        // State changed to active from paused.
        if new_manifest.state == ManifestState::Active && prev.state == ManifestState::Paused {
            return ReconciliationAction::ResumeGoal {
                slug: new_manifest.slug.clone(),
                manifest: new_manifest.clone(),
            };
        }

        // Phase changed.
        if new_manifest.phase != prev.phase {
            if let Some(ref new_phase) = new_manifest.phase {
                return ReconciliationAction::AdvancePhase {
                    slug: new_manifest.slug.clone(),
                    new_phase: new_phase.clone(),
                    manifest: new_manifest.clone(),
                };
            }
        }

        // State changed to completed or archived.
        if matches!(
            new_manifest.state,
            ManifestState::Completed | ManifestState::Archived
        ) && matches!(prev.state, ManifestState::Active | ManifestState::Paused)
        {
            return ReconciliationAction::StopGoal {
                slug: new_manifest.slug.clone(),
            };
        }

        ReconciliationAction::NoAction {
            slug: new_manifest.slug.clone(),
        }
    }

    /// Handle a manifest file being removed. Always returns `StopGoal`.
    pub fn handle_manifest_removed(slug: &str) -> ReconciliationAction {
        ReconciliationAction::StopGoal {
            slug: slug.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Action Executor — bridges reconciler actions to daemon subsystem calls
// ---------------------------------------------------------------------------

/// Result of executing a single reconciliation action.
#[derive(Debug, Clone)]
pub struct ActionExecResult {
    pub slug: String,
    pub action_type: &'static str,
    pub success: bool,
    pub detail: String,
}

/// Trait for executing reconciliation actions against daemon subsystems.
///
/// Implementors bridge the gap between the reconciler's declarative actions
/// and the daemon's imperative subsystem calls (goal state file updates,
/// pipeline dispatch, agent spawning, etc.).
pub trait ActionExecutor {
    /// Execute a single reconciliation action. Returns a result indicating
    /// whether the action succeeded or failed.
    fn execute(&self, action: &ReconciliationAction) -> ActionExecResult;
}

/// Executes reconciliation actions by updating the daemon's goal state file
/// and (optionally) the `GoalProcessManager` for validated lifecycle state.
///
/// This is the production executor that bridges reconciler actions to the
/// daemon's `goal_state.tsv` (audit trail) and the `GoalProcessManager`
/// (source of truth for goal state with validation, metrics, and JSON
/// persistence for recovery). For `StartGoal`, it also queues the goal
/// through the deliberation pipeline.
pub struct GoalStateExecutor {
    pub goal_state_file: std::path::PathBuf,
    pub goal_log_file: std::path::PathBuf,
    /// Optional GoalProcessManager for validated lifecycle management.
    /// When `Some`, reconciliation actions update the GPM alongside TSV writes.
    /// The GPM provides state transition validation, metrics tracking, phase
    /// advancement, and JSON persistence for crash recovery.
    pub goal_process_manager: Option<Arc<Mutex<GoalProcessManager>>>,
    /// Optional management store projection for top-level goal ownership.
    pub management_store: Option<Arc<Mutex<ManagementStore>>>,
}

/// Convert a reconciler `GoalManifest` into a control-plane `GoalManifest`.
///
/// The two types live in different crates with different levels of detail.
/// This adapter fills in sensible defaults for fields that the reconciler's
/// simpler manifest doesn't carry (process config, streams, constraints,
/// vault namespace).
fn to_control_plane_manifest(
    manifest: &GoalManifest,
) -> symbiotic_control_plane::types::GoalManifest {
    use symbiotic_control_plane::types::{
        AutonomyLevel, CheckFrequency, GoalConstraints, GoalPhase, GoalState as CpGoalState,
        ProcessConfig, ProcessType, StreamConfig,
    };

    let state = match manifest.state {
        ManifestState::Active => CpGoalState::Active,
        ManifestState::Paused => CpGoalState::Paused,
        ManifestState::Completed => CpGoalState::Achieved,
        ManifestState::Archived => CpGoalState::Abandoned,
    };

    let phase = match manifest.phase.as_deref() {
        Some("research") => GoalPhase::Research,
        Some("implementation") => GoalPhase::Implementation,
        Some("provisioning") => GoalPhase::Provisioning,
        Some("maintenance") => GoalPhase::Maintenance,
        Some("inquisition") => GoalPhase::Inquisition,
        _ => GoalPhase::Inquisition, // default for unknown/missing
    };

    let autonomy_level = match manifest.autonomy_level.as_str() {
        "auto" => AutonomyLevel::Auto,
        "manual" => AutonomyLevel::Manual,
        _ => AutonomyLevel::Semi,
    };

    symbiotic_control_plane::types::GoalManifest {
        id: format!("reconciler-{}", manifest.slug),
        project_id: manifest.project_id.clone(),
        slug: manifest.slug.clone(),
        title: manifest.title.clone(),
        state,
        priority: manifest.priority.min(255) as u8,
        autonomy_level,
        phase,
        process: ProcessConfig {
            process_type: ProcessType::Periodic,
            check_frequency: CheckFrequency::Daily,
            max_parallel_agents: 1,
        },
        streams: vec![StreamConfig {
            name: "main".to_string(),
            domain: manifest.domains.first().cloned().unwrap_or_default(),
            focus: manifest.title.clone(),
            autonomy: AutonomyLevel::Semi,
        }],
        domains: manifest.domains.clone(),
        vault_namespace: format!("goal-{}", manifest.slug),
        thread_id: None,
        plan_version: 1,
        policy_scopes: Vec::new(),
        task_policy_defaults: symbiotic_control_plane::types::GoalTaskPolicyDefaults::default(),
        constraints: GoalConstraints::default(),
        plan_markdown: manifest.description.clone(),
        tasks: Vec::new(),
    }
}

/// Convert a phase name string to a control-plane `GoalPhase`.
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

impl GoalStateExecutor {
    fn sync_goal_management_work_item(
        &self,
        slug: &str,
        title: &str,
        phase: Option<&str>,
        priority: u8,
        status: WorkItemStatus,
        observed_at: i64,
    ) {
        let Some(ref management_store) = self.management_store else {
            return;
        };

        crate::goal_management::sync_goal_work_item(
            management_store,
            crate::goal_management::GoalWorkItemUpdate {
                slug,
                title,
                project_id: crate::goals::DEFAULT_UNSCOPED_PROJECT_ID,
                phase,
                owner: "reconciler",
                thread_id: None,
                priority: crate::goal_management::priority_from_goal_priority(priority),
                status,
                observed_at,
            },
        );
    }

    /// Helper: call GoalProcessManager for a StartGoal action.
    /// Logs errors but never fails — GPM is best-effort alongside TSV.
    fn gpm_start_goal(&self, manifest: &GoalManifest) {
        let Some(ref gpm_arc) = self.goal_process_manager else {
            return;
        };
        let mut gpm = match gpm_arc.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    slug = %manifest.slug,
                    error = %e,
                    "reconciler: GPM lock poisoned on start_goal"
                );
                return;
            }
        };
        let cp_manifest = to_control_plane_manifest(manifest);
        match gpm.create_goal(&cp_manifest) {
            Ok(_) => {
                tracing::info!(
                    slug = %manifest.slug,
                    "reconciler: GPM goal created"
                );
            }
            Err(e) => {
                tracing::warn!(
                    slug = %manifest.slug,
                    error = %e,
                    "reconciler: GPM create_goal failed (continuing with TSV)"
                );
            }
        }
    }

    /// Helper: transition a goal in GoalProcessManager.
    fn gpm_transition(&self, slug: &str, new_state: symbiotic_control_plane::types::GoalState) {
        let Some(ref gpm_arc) = self.goal_process_manager else {
            return;
        };
        let mut gpm = match gpm_arc.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    slug = %slug,
                    error = %e,
                    "reconciler: GPM lock poisoned on transition"
                );
                return;
            }
        };
        match gpm.transition(slug, new_state.clone()) {
            Ok(()) => {
                tracing::info!(
                    slug = %slug,
                    new_state = ?new_state,
                    "reconciler: GPM state transitioned"
                );
            }
            Err(e) => {
                tracing::warn!(
                    slug = %slug,
                    new_state = ?new_state,
                    error = %e,
                    "reconciler: GPM transition failed (continuing with TSV)"
                );
            }
        }
    }

    /// Helper: advance a goal's phase in GoalProcessManager.
    fn gpm_advance_phase(&self, slug: &str, new_phase: &str) {
        let Some(ref gpm_arc) = self.goal_process_manager else {
            return;
        };
        let mut gpm = match gpm_arc.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    slug = %slug,
                    error = %e,
                    "reconciler: GPM lock poisoned on advance_phase"
                );
                return;
            }
        };
        let phase = parse_goal_phase(new_phase);
        match gpm.advance_phase(slug, phase.clone()) {
            Ok(()) => {
                tracing::info!(
                    slug = %slug,
                    new_phase = ?phase,
                    "reconciler: GPM phase advanced"
                );
            }
            Err(e) => {
                tracing::warn!(
                    slug = %slug,
                    new_phase = ?phase,
                    error = %e,
                    "reconciler: GPM advance_phase failed (continuing with TSV)"
                );
            }
        }
    }

    /// Helper: stop a goal in GoalProcessManager (transition to terminal state + remove).
    fn gpm_stop_goal(&self, slug: &str) {
        let Some(ref gpm_arc) = self.goal_process_manager else {
            return;
        };
        let mut gpm = match gpm_arc.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    slug = %slug,
                    error = %e,
                    "reconciler: GPM lock poisoned on stop_goal"
                );
                return;
            }
        };
        // Try to transition to Abandoned (terminal state). If the goal is
        // already terminal or doesn't exist, log and continue.
        if gpm.get(slug).is_some() {
            if let Err(e) =
                gpm.transition(slug, symbiotic_control_plane::types::GoalState::Abandoned)
            {
                tracing::warn!(
                    slug = %slug,
                    error = %e,
                    "reconciler: GPM transition to Abandoned failed (attempting remove)"
                );
            }
            // Remove from in-memory tracking regardless of transition outcome.
            if let Err(e) = gpm.remove(slug) {
                tracing::warn!(
                    slug = %slug,
                    error = %e,
                    "reconciler: GPM remove failed"
                );
            } else {
                tracing::info!(
                    slug = %slug,
                    "reconciler: GPM goal stopped and removed"
                );
            }
        } else {
            tracing::debug!(
                slug = %slug,
                "reconciler: GPM stop_goal — goal not found in GPM (may not have been created)"
            );
        }
    }
}

impl ActionExecutor for GoalStateExecutor {
    fn execute(&self, action: &ReconciliationAction) -> ActionExecResult {
        match action {
            ReconciliationAction::StartGoal { manifest } => {
                let now = symbiotic_queue::now_unix();
                let goal_room = format!("reconciler:{}", manifest.slug);

                // Update GoalProcessManager (source of truth for validated state).
                self.gpm_start_goal(manifest);

                // Record initial goal state as "starting" in the goal state file (audit trail).
                let upsert_result = crate::goal_state::upsert_goal_state(
                    &self.goal_state_file,
                    crate::goal_state::GoalState {
                        goal_room: goal_room.clone(),
                        thread_id: None,
                        project_id: manifest.project_id.clone(),
                        template: "reconciler".to_string(),
                        status: "starting".to_string(),
                        last_job_id: format!("reconcile-{}", now),
                        last_run_id: None,
                        owner: Some("reconciler".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("starting".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                );

                if let Err(e) = upsert_result {
                    return ActionExecResult {
                        slug: manifest.slug.clone(),
                        action_type: "start_goal",
                        success: false,
                        detail: format!("failed to update goal state: {e}"),
                    };
                }

                // Log the start event.
                let _ = crate::goal_state::append_goal_log(
                    &self.goal_log_file,
                    crate::goal_state::GoalLogEntry {
                        ts: now,
                        event: "goal.reconciler.started",
                        workflow_job_id: &format!("reconcile-{}", now),
                        goal_room: Some(&goal_room),
                        goal_sender: Some("reconciler"),
                        template: "reconciler",
                        detail: &manifest.title,
                    },
                );

                self.sync_goal_management_work_item(
                    &manifest.slug,
                    &manifest.title,
                    manifest.phase.as_deref().or(Some("starting")),
                    manifest.priority.min(255) as u8,
                    WorkItemStatus::Running,
                    now as i64,
                );

                tracing::info!(
                    slug = %manifest.slug,
                    title = %manifest.title,
                    phase = ?manifest.phase,
                    "reconciler: goal started via state file"
                );

                ActionExecResult {
                    slug: manifest.slug.clone(),
                    action_type: "start_goal",
                    success: true,
                    detail: format!("goal '{}' started ({})", manifest.slug, manifest.title),
                }
            }

            ReconciliationAction::PauseGoal { slug } => {
                let now = symbiotic_queue::now_unix();
                let goal_room = format!("reconciler:{}", slug);

                // Update GoalProcessManager (source of truth for validated state).
                self.gpm_transition(slug, symbiotic_control_plane::types::GoalState::Paused);

                let upsert_result = crate::goal_state::upsert_goal_state(
                    &self.goal_state_file,
                    crate::goal_state::GoalState {
                        goal_room: goal_room.clone(),
                        thread_id: None,
                        project_id: crate::goals::default_unscoped_project_id(),
                        template: "reconciler".to_string(),
                        status: "paused".to_string(),
                        last_job_id: format!("reconcile-{}", now),
                        last_run_id: None,
                        owner: Some("reconciler".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("paused".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                );

                if let Err(e) = upsert_result {
                    return ActionExecResult {
                        slug: slug.clone(),
                        action_type: "pause_goal",
                        success: false,
                        detail: format!("failed to update goal state: {e}"),
                    };
                }

                self.sync_goal_management_work_item(
                    slug,
                    slug,
                    Some("paused"),
                    50,
                    WorkItemStatus::Blocked,
                    now as i64,
                );

                tracing::info!(slug = %slug, "reconciler: goal paused");

                ActionExecResult {
                    slug: slug.clone(),
                    action_type: "pause_goal",
                    success: true,
                    detail: format!("goal '{}' paused", slug),
                }
            }

            ReconciliationAction::ResumeGoal { slug, manifest } => {
                let now = symbiotic_queue::now_unix();
                let goal_room = format!("reconciler:{}", slug);

                // Update GoalProcessManager (source of truth for validated state).
                self.gpm_transition(slug, symbiotic_control_plane::types::GoalState::Active);

                let upsert_result = crate::goal_state::upsert_goal_state(
                    &self.goal_state_file,
                    crate::goal_state::GoalState {
                        goal_room: goal_room.clone(),
                        thread_id: None,
                        project_id: manifest.project_id.clone(),
                        template: "reconciler".to_string(),
                        status: "running".to_string(),
                        last_job_id: format!("reconcile-{}", now),
                        last_run_id: None,
                        owner: Some("reconciler".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("running".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                );

                if let Err(e) = upsert_result {
                    return ActionExecResult {
                        slug: slug.clone(),
                        action_type: "resume_goal",
                        success: false,
                        detail: format!("failed to update goal state: {e}"),
                    };
                }

                self.sync_goal_management_work_item(
                    slug,
                    &manifest.title,
                    manifest.phase.as_deref().or(Some("running")),
                    manifest.priority.min(255) as u8,
                    WorkItemStatus::Running,
                    now as i64,
                );

                tracing::info!(slug = %slug, title = %manifest.title, "reconciler: goal resumed");

                ActionExecResult {
                    slug: slug.clone(),
                    action_type: "resume_goal",
                    success: true,
                    detail: format!("goal '{}' resumed", slug),
                }
            }

            ReconciliationAction::AdvancePhase {
                slug,
                new_phase,
                manifest,
            } => {
                let now = symbiotic_queue::now_unix();
                let goal_room = format!("reconciler:{}", slug);

                // Update GoalProcessManager (source of truth for validated state).
                self.gpm_advance_phase(slug, new_phase);

                let upsert_result = crate::goal_state::upsert_goal_state(
                    &self.goal_state_file,
                    crate::goal_state::GoalState {
                        goal_room: goal_room.clone(),
                        thread_id: None,
                        project_id: manifest.project_id.clone(),
                        template: "reconciler".to_string(),
                        status: "running".to_string(),
                        last_job_id: format!("reconcile-{}", now),
                        last_run_id: None,
                        owner: Some("reconciler".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some(new_phase.clone()),
                        audit_id: None,
                        plan_id: None,
                    },
                );

                if let Err(e) = upsert_result {
                    return ActionExecResult {
                        slug: slug.clone(),
                        action_type: "advance_phase",
                        success: false,
                        detail: format!("failed to update goal state: {e}"),
                    };
                }

                self.sync_goal_management_work_item(
                    slug,
                    &manifest.title,
                    Some(new_phase),
                    manifest.priority.min(255) as u8,
                    WorkItemStatus::Running,
                    now as i64,
                );

                tracing::info!(
                    slug = %slug,
                    new_phase = %new_phase,
                    title = %manifest.title,
                    "reconciler: goal phase advanced"
                );

                ActionExecResult {
                    slug: slug.clone(),
                    action_type: "advance_phase",
                    success: true,
                    detail: format!("goal '{}' advanced to phase '{}'", slug, new_phase),
                }
            }

            ReconciliationAction::StopGoal { slug } => {
                let now = symbiotic_queue::now_unix();
                let goal_room = format!("reconciler:{}", slug);

                // Update GoalProcessManager (source of truth for validated state).
                self.gpm_stop_goal(slug);

                let upsert_result = crate::goal_state::upsert_goal_state(
                    &self.goal_state_file,
                    crate::goal_state::GoalState {
                        goal_room: goal_room.clone(),
                        thread_id: None,
                        project_id: crate::goals::default_unscoped_project_id(),
                        template: "reconciler".to_string(),
                        status: "stopped".to_string(),
                        last_job_id: format!("reconcile-{}", now),
                        last_run_id: None,
                        owner: Some("reconciler".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("stopped".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                );

                if let Err(e) = upsert_result {
                    return ActionExecResult {
                        slug: slug.clone(),
                        action_type: "stop_goal",
                        success: false,
                        detail: format!("failed to update goal state: {e}"),
                    };
                }

                let _ = crate::goal_state::append_goal_log(
                    &self.goal_log_file,
                    crate::goal_state::GoalLogEntry {
                        ts: now,
                        event: "goal.reconciler.stopped",
                        workflow_job_id: &format!("reconcile-{}", now),
                        goal_room: Some(&goal_room),
                        goal_sender: Some("reconciler"),
                        template: "reconciler",
                        detail: slug,
                    },
                );

                self.sync_goal_management_work_item(
                    slug,
                    slug,
                    Some("stopped"),
                    50,
                    WorkItemStatus::Cancelled,
                    now as i64,
                );

                tracing::info!(slug = %slug, "reconciler: goal stopped");

                ActionExecResult {
                    slug: slug.clone(),
                    action_type: "stop_goal",
                    success: true,
                    detail: format!("goal '{}' stopped", slug),
                }
            }

            ReconciliationAction::NoAction { slug } => ActionExecResult {
                slug: slug.clone(),
                action_type: "no_action",
                success: true,
                detail: format!("no action needed for '{}'", slug),
            },
        }
    }
}

/// Execute a batch of reconciliation actions and return results.
///
/// Filters out `NoAction` entries and executes the remaining actions
/// through the given executor. Returns results for all executed actions.
pub fn execute_actions(
    actions: &[ReconciliationAction],
    executor: &dyn ActionExecutor,
) -> Vec<ActionExecResult> {
    actions
        .iter()
        .filter(|a| !matches!(a, ReconciliationAction::NoAction { .. }))
        .map(|action| {
            let result = executor.execute(action);
            if result.success {
                tracing::info!(
                    slug = %result.slug,
                    action = %result.action_type,
                    detail = %result.detail,
                    "reconciler: action executed"
                );
            } else {
                tracing::warn!(
                    slug = %result.slug,
                    action = %result.action_type,
                    detail = %result.detail,
                    "reconciler: action failed"
                );
            }
            result
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Manifest parsing tests
    // -----------------------------------------------------------------------

    const VALID_MANIFEST: &str = r#"---
project_id: project:test
slug: learn-rust
title: Learn Rust Programming
state: active
priority: 80
phase: research
autonomy_level: semi
domains:
  - programming
  - learning
---

Learn Rust by working through the official book and building small projects.
Focus on ownership, lifetimes, and async patterns."#;

    const MINIMAL_MANIFEST: &str = r#"---
project_id: project:test
slug: minimal-goal
title: A Minimal Goal
state: active
---

Just the basics."#;

    #[test]
    fn test_parse_manifest_valid_all_fields() {
        let path = PathBuf::from("test/learn-rust.md");
        let manifest = GoalReconciler::parse_manifest_content(VALID_MANIFEST, &path).unwrap();

        assert_eq!(manifest.slug, "learn-rust");
        assert_eq!(manifest.title, "Learn Rust Programming");
        assert_eq!(manifest.state, ManifestState::Active);
        assert_eq!(manifest.priority, 80);
        assert_eq!(manifest.phase, Some("research".to_string()));
        assert_eq!(manifest.autonomy_level, "semi");
        assert_eq!(manifest.domains, vec!["programming", "learning"]);
        assert!(manifest
            .description
            .contains("Learn Rust by working through"));
        assert!(manifest.description.contains("async patterns."));
        assert_eq!(manifest.file_path, path);
    }

    #[test]
    fn test_parse_manifest_minimal_fields_with_defaults() {
        let path = PathBuf::from("test/minimal.md");
        let manifest = GoalReconciler::parse_manifest_content(MINIMAL_MANIFEST, &path).unwrap();

        assert_eq!(manifest.slug, "minimal-goal");
        assert_eq!(manifest.title, "A Minimal Goal");
        assert_eq!(manifest.state, ManifestState::Active);
        assert_eq!(manifest.priority, 50); // default
        assert_eq!(manifest.phase, None); // default
        assert_eq!(manifest.autonomy_level, "semi"); // default
        assert!(manifest.domains.is_empty()); // default
        assert_eq!(manifest.description, "Just the basics.");
    }

    #[test]
    fn test_parse_manifest_invalid_yaml() {
        let content = r#"---
slug: [broken
title: "Missing bracket
---

Body."#;
        let path = PathBuf::from("test/broken.md");
        let result = GoalReconciler::parse_manifest_content(content, &path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("failed to parse YAML frontmatter"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn test_parse_manifest_missing_required_field_slug() {
        let content = r#"---
title: Missing Slug
state: active
---

Body."#;
        let path = PathBuf::from("test/no-slug.md");
        let result = GoalReconciler::parse_manifest_content(content, &path);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_manifest_missing_required_field_title() {
        let content = r#"---
slug: no-title
state: active
---

Body."#;
        let path = PathBuf::from("test/no-title.md");
        let result = GoalReconciler::parse_manifest_content(content, &path);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_manifest_missing_frontmatter_delimiter() {
        let content = "Just a plain Markdown file without frontmatter.";
        let path = PathBuf::from("test/plain.md");
        let result = GoalReconciler::parse_manifest_content(content, &path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("does not start with YAML frontmatter"));
    }

    #[test]
    fn test_parse_manifest_missing_closing_delimiter() {
        let content = r#"---
slug: unclosed
title: Unclosed Frontmatter
state: active
"#;
        let path = PathBuf::from("test/unclosed.md");
        let result = GoalReconciler::parse_manifest_content(content, &path);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("missing closing frontmatter"));
    }

    #[test]
    fn test_parse_manifest_empty_body() {
        let content = r#"---
project_id: "project:test"
slug: empty-body
title: Empty Body Goal
state: paused
---
"#;
        let path = PathBuf::from("test/empty-body.md");
        let manifest = GoalReconciler::parse_manifest_content(content, &path).unwrap();
        assert_eq!(manifest.slug, "empty-body");
        assert_eq!(manifest.state, ManifestState::Paused);
        assert!(manifest.description.is_empty());
    }

    #[test]
    fn test_parse_manifest_all_states() {
        for (state_str, expected) in [
            ("active", ManifestState::Active),
            ("paused", ManifestState::Paused),
            ("completed", ManifestState::Completed),
            ("archived", ManifestState::Archived),
        ] {
            let content = format!(
                "---\nproject_id: \"project:test\"\nslug: test-{state_str}\ntitle: Test\nstate: {state_str}\n---\n\nBody."
            );
            let path = PathBuf::from(format!("test/{state_str}.md"));
            let manifest = GoalReconciler::parse_manifest_content(&content, &path).unwrap();
            assert_eq!(manifest.state, expected, "state mismatch for {state_str}");
        }
    }

    // -----------------------------------------------------------------------
    // Scan manifests tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_scan_manifests_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let config = ReconcilerConfig {
            manifest_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let reconciler = GoalReconciler::new(config);
        let manifests = reconciler.scan_manifests().unwrap();
        assert!(manifests.is_empty());
    }

    #[test]
    fn test_scan_manifests_nonexistent_directory() {
        let config = ReconcilerConfig {
            manifest_dir: PathBuf::from("/tmp/nonexistent-goal-dir-12345"),
            ..Default::default()
        };
        let reconciler = GoalReconciler::new(config);
        let manifests = reconciler.scan_manifests().unwrap();
        assert!(manifests.is_empty());
    }

    #[test]
    fn test_scan_manifests_only_md_files_parsed() {
        let tmp = TempDir::new().unwrap();

        // Valid .md manifest
        fs::write(tmp.path().join("goal-a.md"), VALID_MANIFEST).unwrap();

        // Non-.md files should be ignored.
        fs::write(tmp.path().join("notes.txt"), "just notes").unwrap();
        fs::write(tmp.path().join("config.toml"), "[section]\nkey=1").unwrap();
        fs::write(tmp.path().join("readme"), "no extension").unwrap();

        let config = ReconcilerConfig {
            manifest_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let reconciler = GoalReconciler::new(config);
        let manifests = reconciler.scan_manifests().unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].slug, "learn-rust");
    }

    #[test]
    fn test_scan_manifests_skips_invalid_files() {
        let tmp = TempDir::new().unwrap();

        // Valid manifest
        fs::write(tmp.path().join("good.md"), VALID_MANIFEST).unwrap();

        // Invalid manifest (no frontmatter)
        fs::write(tmp.path().join("bad.md"), "Just plain text.").unwrap();

        let config = ReconcilerConfig {
            manifest_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let reconciler = GoalReconciler::new(config);
        let manifests = reconciler.scan_manifests().unwrap();
        // Only the valid one should be returned.
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].slug, "learn-rust");
    }

    #[test]
    fn test_scan_manifests_multiple_sorted() {
        let tmp = TempDir::new().unwrap();

        let mk = |slug: &str| {
            format!("---\nproject_id: \"project:test\"\nslug: {slug}\ntitle: Goal {slug}\nstate: active\n---\n\nDescription.")
        };

        fs::write(tmp.path().join("z-goal.md"), mk("z-goal")).unwrap();
        fs::write(tmp.path().join("a-goal.md"), mk("a-goal")).unwrap();
        fs::write(tmp.path().join("m-goal.md"), mk("m-goal")).unwrap();

        let config = ReconcilerConfig {
            manifest_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let reconciler = GoalReconciler::new(config);
        let manifests = reconciler.scan_manifests().unwrap();
        assert_eq!(manifests.len(), 3);
        // Should be sorted by slug.
        assert_eq!(manifests[0].slug, "a-goal");
        assert_eq!(manifests[1].slug, "m-goal");
        assert_eq!(manifests[2].slug, "z-goal");
    }

    // -----------------------------------------------------------------------
    // Reconcile tests
    // -----------------------------------------------------------------------

    fn make_manifest(slug: &str, state: ManifestState, phase: Option<&str>) -> GoalManifest {
        GoalManifest {
            project_id: "project:test".to_string(),
            slug: slug.to_string(),
            title: format!("Goal {slug}"),
            state,
            priority: 50,
            phase: phase.map(|s| s.to_string()),
            autonomy_level: "semi".to_string(),
            description: "Test goal.".to_string(),
            domains: vec![],
            file_path: PathBuf::from(format!("goals/{slug}.md")),
        }
    }

    fn make_runtime(
        slug: &str,
        running: bool,
        paused: bool,
        phase: Option<&str>,
    ) -> RuntimeGoalState {
        RuntimeGoalState {
            slug: slug.to_string(),
            is_running: running,
            is_paused: paused,
            current_phase: phase.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_reconcile_new_manifest_start_goal() {
        let manifests = vec![make_manifest(
            "new-goal",
            ManifestState::Active,
            Some("research"),
        )];
        let runtime: Vec<RuntimeGoalState> = vec![];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ReconciliationAction::StartGoal { manifest } => {
                assert_eq!(manifest.slug, "new-goal");
            }
            other => panic!("expected StartGoal, got {other:?}"),
        }
    }

    #[test]
    fn test_reconcile_new_paused_manifest_no_action() {
        let manifests = vec![make_manifest("paused-goal", ManifestState::Paused, None)];
        let runtime: Vec<RuntimeGoalState> = vec![];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ReconciliationAction::NoAction { slug } if slug == "paused-goal"
        ));
    }

    #[test]
    fn test_reconcile_manifest_paused_runtime_running() {
        let manifests = vec![make_manifest(
            "goal-a",
            ManifestState::Paused,
            Some("research"),
        )];
        let runtime = vec![make_runtime("goal-a", true, false, Some("research"))];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ReconciliationAction::PauseGoal { slug } => {
                assert_eq!(slug, "goal-a");
            }
            other => panic!("expected PauseGoal, got {other:?}"),
        }
    }

    #[test]
    fn test_reconcile_manifest_active_runtime_paused() {
        let manifests = vec![make_manifest(
            "goal-b",
            ManifestState::Active,
            Some("research"),
        )];
        let runtime = vec![make_runtime("goal-b", true, true, Some("research"))];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ReconciliationAction::ResumeGoal { slug, manifest } => {
                assert_eq!(slug, "goal-b");
                assert_eq!(manifest.slug, "goal-b");
            }
            other => panic!("expected ResumeGoal, got {other:?}"),
        }
    }

    #[test]
    fn test_reconcile_phase_advanced() {
        let manifests = vec![make_manifest(
            "goal-c",
            ManifestState::Active,
            Some("implementation"),
        )];
        let runtime = vec![make_runtime("goal-c", true, false, Some("research"))];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ReconciliationAction::AdvancePhase {
                slug,
                new_phase,
                manifest,
            } => {
                assert_eq!(slug, "goal-c");
                assert_eq!(new_phase, "implementation");
                assert_eq!(manifest.slug, "goal-c");
            }
            other => panic!("expected AdvancePhase, got {other:?}"),
        }
    }

    #[test]
    fn test_reconcile_manifest_removed_stop_goal() {
        let manifests: Vec<GoalManifest> = vec![];
        let runtime = vec![make_runtime("orphan-goal", true, false, Some("research"))];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ReconciliationAction::StopGoal { slug } => {
                assert_eq!(slug, "orphan-goal");
            }
            other => panic!("expected StopGoal, got {other:?}"),
        }
    }

    #[test]
    fn test_reconcile_states_match_no_action() {
        let manifests = vec![make_manifest(
            "happy",
            ManifestState::Active,
            Some("research"),
        )];
        let runtime = vec![make_runtime("happy", true, false, Some("research"))];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ReconciliationAction::NoAction { slug } if slug == "happy"
        ));
    }

    #[test]
    fn test_reconcile_multiple_manifests_mixed_actions() {
        let manifests = vec![
            make_manifest("new", ManifestState::Active, Some("plan")),
            make_manifest("running", ManifestState::Active, Some("build")),
            make_manifest("pausing", ManifestState::Paused, Some("research")),
            make_manifest("resuming", ManifestState::Active, Some("research")),
        ];
        let runtime = vec![
            make_runtime("running", true, false, Some("build")),
            make_runtime("pausing", true, false, Some("research")),
            make_runtime("resuming", true, true, Some("research")),
            make_runtime("orphan", true, false, Some("deploy")),
        ];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);

        // Expect: StartGoal(new), NoAction(running), PauseGoal(pausing),
        //         ResumeGoal(resuming), StopGoal(orphan)
        assert_eq!(actions.len(), 5);

        let find_action = |slug: &str| -> &ReconciliationAction {
            actions
                .iter()
                .find(|a| match a {
                    ReconciliationAction::StartGoal { manifest } => manifest.slug == slug,
                    ReconciliationAction::PauseGoal { slug: s } => s == slug,
                    ReconciliationAction::ResumeGoal { slug: s, .. } => s == slug,
                    ReconciliationAction::AdvancePhase { slug: s, .. } => s == slug,
                    ReconciliationAction::StopGoal { slug: s } => s == slug,
                    ReconciliationAction::NoAction { slug: s } => s == slug,
                })
                .unwrap_or_else(|| panic!("no action found for slug {slug}"))
        };

        assert!(matches!(
            find_action("new"),
            ReconciliationAction::StartGoal { .. }
        ));
        assert!(matches!(
            find_action("running"),
            ReconciliationAction::NoAction { .. }
        ));
        assert!(matches!(
            find_action("pausing"),
            ReconciliationAction::PauseGoal { .. }
        ));
        assert!(matches!(
            find_action("resuming"),
            ReconciliationAction::ResumeGoal { .. }
        ));
        assert!(matches!(
            find_action("orphan"),
            ReconciliationAction::StopGoal { .. }
        ));
    }

    #[test]
    fn test_reconcile_completed_manifest_stops_running_goal() {
        let manifests = vec![make_manifest(
            "done",
            ManifestState::Completed,
            Some("final"),
        )];
        let runtime = vec![make_runtime("done", true, false, Some("final"))];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ReconciliationAction::StopGoal { slug } if slug == "done"
        ));
    }

    #[test]
    fn test_reconcile_archived_manifest_stops_running_goal() {
        let manifests = vec![make_manifest("old", ManifestState::Archived, None)];
        let runtime = vec![make_runtime("old", true, false, None)];

        let actions = GoalReconciler::reconcile(&manifests, &runtime);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            ReconciliationAction::StopGoal { slug } if slug == "old"
        ));
    }

    // -----------------------------------------------------------------------
    // handle_manifest_change tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_manifest_change_new_active() {
        let manifest = make_manifest("brand-new", ManifestState::Active, Some("plan"));
        let action = GoalReconciler::handle_manifest_change(&manifest, None);
        assert!(matches!(action, ReconciliationAction::StartGoal { .. }));
    }

    #[test]
    fn test_handle_manifest_change_new_paused() {
        let manifest = make_manifest("new-paused", ManifestState::Paused, None);
        let action = GoalReconciler::handle_manifest_change(&manifest, None);
        assert!(matches!(action, ReconciliationAction::NoAction { .. }));
    }

    #[test]
    fn test_handle_manifest_change_active_to_paused() {
        let prev = make_manifest("goal", ManifestState::Active, Some("research"));
        let new = make_manifest("goal", ManifestState::Paused, Some("research"));
        let action = GoalReconciler::handle_manifest_change(&new, Some(&prev));
        assert!(matches!(action, ReconciliationAction::PauseGoal { .. }));
    }

    #[test]
    fn test_handle_manifest_change_paused_to_active() {
        let prev = make_manifest("goal", ManifestState::Paused, Some("research"));
        let new = make_manifest("goal", ManifestState::Active, Some("research"));
        let action = GoalReconciler::handle_manifest_change(&new, Some(&prev));
        assert!(matches!(action, ReconciliationAction::ResumeGoal { .. }));
    }

    #[test]
    fn test_handle_manifest_change_phase_advanced() {
        let prev = make_manifest("goal", ManifestState::Active, Some("research"));
        let new = make_manifest("goal", ManifestState::Active, Some("implementation"));
        let action = GoalReconciler::handle_manifest_change(&new, Some(&prev));
        match action {
            ReconciliationAction::AdvancePhase {
                slug, new_phase, ..
            } => {
                assert_eq!(slug, "goal");
                assert_eq!(new_phase, "implementation");
            }
            other => panic!("expected AdvancePhase, got {other:?}"),
        }
    }

    #[test]
    fn test_handle_manifest_change_active_to_completed() {
        let prev = make_manifest("goal", ManifestState::Active, Some("done"));
        let new = make_manifest("goal", ManifestState::Completed, Some("done"));
        let action = GoalReconciler::handle_manifest_change(&new, Some(&prev));
        assert!(matches!(action, ReconciliationAction::StopGoal { .. }));
    }

    #[test]
    fn test_handle_manifest_change_no_change() {
        let prev = make_manifest("goal", ManifestState::Active, Some("research"));
        let new = make_manifest("goal", ManifestState::Active, Some("research"));
        let action = GoalReconciler::handle_manifest_change(&new, Some(&prev));
        assert!(matches!(action, ReconciliationAction::NoAction { .. }));
    }

    // -----------------------------------------------------------------------
    // handle_manifest_removed test
    // -----------------------------------------------------------------------

    #[test]
    fn test_handle_manifest_removed() {
        let action = GoalReconciler::handle_manifest_removed("deleted-goal");
        assert_eq!(
            action,
            ReconciliationAction::StopGoal {
                slug: "deleted-goal".to_string()
            }
        );
    }

    // -----------------------------------------------------------------------
    // Config tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_reconciler_config_defaults() {
        let config = ReconcilerConfig::default();
        assert_eq!(config.manifest_dir, PathBuf::from("operations/projects"));
        assert_eq!(config.debounce_secs, 2);
        assert!(config.reconcile_on_startup);
    }

    #[test]
    fn test_reconciler_config_serde_roundtrip() {
        let config = ReconcilerConfig {
            manifest_dir: PathBuf::from("/custom/goals"),
            debounce_secs: 5,
            reconcile_on_startup: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ReconcilerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.manifest_dir, PathBuf::from("/custom/goals"));
        assert_eq!(deserialized.debounce_secs, 5);
        assert!(!deserialized.reconcile_on_startup);
    }

    // -----------------------------------------------------------------------
    // ManifestState serde tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_manifest_state_serde_roundtrip() {
        for state in [
            ManifestState::Active,
            ManifestState::Paused,
            ManifestState::Completed,
            ManifestState::Archived,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let deserialized: ManifestState = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, state);
        }
    }

    // -----------------------------------------------------------------------
    // GoalManifest serde test
    // -----------------------------------------------------------------------

    #[test]
    fn test_goal_manifest_serde_roundtrip() {
        let manifest = make_manifest("serde-test", ManifestState::Active, Some("design"));
        let json = serde_json::to_string(&manifest).unwrap();
        let deserialized: GoalManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, manifest);
    }

    // -----------------------------------------------------------------------
    // Filesystem integration: parse_manifest reads from disk
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_manifest_from_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("goal.md");
        fs::write(&path, VALID_MANIFEST).unwrap();

        let manifest = GoalReconciler::parse_manifest(&path).unwrap();
        assert_eq!(manifest.slug, "learn-rust");
        assert_eq!(manifest.file_path, path);
    }

    #[test]
    fn test_parse_manifest_file_not_found() {
        let result = GoalReconciler::parse_manifest(Path::new("/tmp/no-such-file-12345.md"));
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // ActionExecutor tests (GoalStateExecutor)
    // -----------------------------------------------------------------------

    #[test]
    fn test_executor_start_goal_creates_state_file() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("state.tsv");
        let log_file = tmp.path().join("runs.log");

        let executor = GoalStateExecutor {
            goal_state_file: state_file.clone(),
            goal_log_file: log_file.clone(),
            goal_process_manager: None,
            management_store: None,
        };

        let manifest = make_manifest("test-start", ManifestState::Active, Some("research"));
        let action = ReconciliationAction::StartGoal { manifest };

        let result = executor.execute(&action);
        assert!(result.success, "expected success, got: {}", result.detail);
        assert_eq!(result.slug, "test-start");
        assert_eq!(result.action_type, "start_goal");

        // Verify goal state was written.
        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].goal_room, "reconciler:test-start");
        assert_eq!(states[0].status, "starting");
        assert_eq!(states[0].template, "reconciler");
        assert_eq!(states[0].pipeline_stage, Some("starting".to_string()));

        // Verify log was written.
        let log_content = fs::read_to_string(&log_file).unwrap();
        assert!(log_content.contains("goal.reconciler.started"));
    }

    #[test]
    fn test_executor_projects_goal_management_work_item() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("state.tsv");
        let log_file = tmp.path().join("runs.log");
        let management_store = Arc::new(Mutex::new(ManagementStore::new(
            tmp.path().join("control-plane"),
        )));

        let executor = GoalStateExecutor {
            goal_state_file: state_file,
            goal_log_file: log_file,
            goal_process_manager: None,
            management_store: Some(Arc::clone(&management_store)),
        };

        let manifest = make_manifest("projected", ManifestState::Active, Some("research"));
        let start = executor.execute(&ReconciliationAction::StartGoal {
            manifest: manifest.clone(),
        });
        assert!(start.success);

        let pause = executor.execute(&ReconciliationAction::PauseGoal {
            slug: "projected".to_string(),
        });
        assert!(pause.success);

        let store = management_store.lock().unwrap();
        let work_item = store.get_work_item("goal:projected").unwrap();
        assert_eq!(work_item.status, WorkItemStatus::Blocked);
        assert!(work_item.summary.contains("phase: paused"));
    }

    #[test]
    fn test_executor_pause_goal_updates_state() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("state.tsv");
        let log_file = tmp.path().join("runs.log");

        let executor = GoalStateExecutor {
            goal_state_file: state_file.clone(),
            goal_log_file: log_file,
            goal_process_manager: None,
            management_store: None,
        };

        // First start the goal.
        let manifest = make_manifest("test-pause", ManifestState::Active, Some("research"));
        executor.execute(&ReconciliationAction::StartGoal {
            manifest: manifest.clone(),
        });

        // Then pause it.
        let result = executor.execute(&ReconciliationAction::PauseGoal {
            slug: "test-pause".to_string(),
        });
        assert!(result.success);
        assert_eq!(result.action_type, "pause_goal");

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status, "paused");
        assert_eq!(states[0].pipeline_stage, Some("paused".to_string()));
    }

    #[test]
    fn test_executor_resume_goal_updates_state() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("state.tsv");
        let log_file = tmp.path().join("runs.log");

        let executor = GoalStateExecutor {
            goal_state_file: state_file.clone(),
            goal_log_file: log_file,
            goal_process_manager: None,
            management_store: None,
        };

        let manifest = make_manifest("test-resume", ManifestState::Paused, Some("research"));
        executor.execute(&ReconciliationAction::StartGoal {
            manifest: manifest.clone(),
        });

        let active_manifest = make_manifest("test-resume", ManifestState::Active, Some("research"));
        let result = executor.execute(&ReconciliationAction::ResumeGoal {
            slug: "test-resume".to_string(),
            manifest: active_manifest,
        });
        assert!(result.success);
        assert_eq!(result.action_type, "resume_goal");

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status, "running");
    }

    #[test]
    fn test_executor_stop_goal_updates_state() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("state.tsv");
        let log_file = tmp.path().join("runs.log");

        let executor = GoalStateExecutor {
            goal_state_file: state_file.clone(),
            goal_log_file: log_file.clone(),
            goal_process_manager: None,
            management_store: None,
        };

        let manifest = make_manifest("test-stop", ManifestState::Active, Some("research"));
        executor.execute(&ReconciliationAction::StartGoal { manifest });

        let result = executor.execute(&ReconciliationAction::StopGoal {
            slug: "test-stop".to_string(),
        });
        assert!(result.success);
        assert_eq!(result.action_type, "stop_goal");

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status, "stopped");

        // Verify stop was logged.
        let log_content = fs::read_to_string(&log_file).unwrap();
        assert!(log_content.contains("goal.reconciler.stopped"));
    }

    #[test]
    fn test_executor_advance_phase_updates_state() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("state.tsv");
        let log_file = tmp.path().join("runs.log");

        let executor = GoalStateExecutor {
            goal_state_file: state_file.clone(),
            goal_log_file: log_file,
            goal_process_manager: None,
            management_store: None,
        };

        let manifest = make_manifest("test-phase", ManifestState::Active, Some("research"));
        executor.execute(&ReconciliationAction::StartGoal {
            manifest: manifest.clone(),
        });

        let new_manifest =
            make_manifest("test-phase", ManifestState::Active, Some("implementation"));
        let result = executor.execute(&ReconciliationAction::AdvancePhase {
            slug: "test-phase".to_string(),
            new_phase: "implementation".to_string(),
            manifest: new_manifest,
        });
        assert!(result.success);
        assert_eq!(result.action_type, "advance_phase");
        assert!(result.detail.contains("implementation"));

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].pipeline_stage, Some("implementation".to_string()));
    }

    #[test]
    fn test_executor_no_action_succeeds() {
        let tmp = TempDir::new().unwrap();
        let executor = GoalStateExecutor {
            goal_state_file: tmp.path().join("state.tsv"),
            goal_log_file: tmp.path().join("runs.log"),
            goal_process_manager: None,
            management_store: None,
        };

        let result = executor.execute(&ReconciliationAction::NoAction {
            slug: "idle-goal".to_string(),
        });
        assert!(result.success);
        assert_eq!(result.action_type, "no_action");
    }

    #[test]
    fn test_execute_actions_filters_no_action() {
        let tmp = TempDir::new().unwrap();
        let executor = GoalStateExecutor {
            goal_state_file: tmp.path().join("state.tsv"),
            goal_log_file: tmp.path().join("runs.log"),
            goal_process_manager: None,
            management_store: None,
        };

        let actions = vec![
            ReconciliationAction::StartGoal {
                manifest: make_manifest("new-goal", ManifestState::Active, Some("plan")),
            },
            ReconciliationAction::NoAction {
                slug: "no-change".to_string(),
            },
            ReconciliationAction::StopGoal {
                slug: "orphan".to_string(),
            },
        ];

        let results = execute_actions(&actions, &executor);
        // NoAction should be filtered out.
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].action_type, "start_goal");
        assert_eq!(results[1].action_type, "stop_goal");
    }

    #[test]
    fn test_executor_full_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("state.tsv");
        let log_file = tmp.path().join("runs.log");

        let executor = GoalStateExecutor {
            goal_state_file: state_file.clone(),
            goal_log_file: log_file,
            goal_process_manager: None,
            management_store: None,
        };

        // Start
        let manifest = make_manifest("lifecycle", ManifestState::Active, Some("research"));
        let r = executor.execute(&ReconciliationAction::StartGoal {
            manifest: manifest.clone(),
        });
        assert!(r.success);

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states[0].status, "starting");

        // Pause
        let r = executor.execute(&ReconciliationAction::PauseGoal {
            slug: "lifecycle".to_string(),
        });
        assert!(r.success);

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states[0].status, "paused");

        // Resume
        let r = executor.execute(&ReconciliationAction::ResumeGoal {
            slug: "lifecycle".to_string(),
            manifest: manifest.clone(),
        });
        assert!(r.success);

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states[0].status, "running");

        // Advance phase
        let new_manifest = make_manifest("lifecycle", ManifestState::Active, Some("build"));
        let r = executor.execute(&ReconciliationAction::AdvancePhase {
            slug: "lifecycle".to_string(),
            new_phase: "build".to_string(),
            manifest: new_manifest,
        });
        assert!(r.success);

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states[0].pipeline_stage, Some("build".to_string()));

        // Stop
        let r = executor.execute(&ReconciliationAction::StopGoal {
            slug: "lifecycle".to_string(),
        });
        assert!(r.success);

        let states = crate::goal_state::load_goal_states(&state_file).unwrap();
        assert_eq!(states[0].status, "stopped");
    }
}
