//! Wires the `symbiotic-control-plane` reconciler into the daemon.
//!
//! Provides `DaemonStateQuery` (implements `StateQuery`), reconciliation
//! action execution, SOUL.md loading on startup, and a background
//! reconciliation loop that can be spawned as an independent tokio task.
//!
//! The preferred entry point is [`spawn_reconciler_with_dispatcher`], which
//! uses the trait-based `ActionDispatcher` (from `symbiotic-agents`) with
//! concrete daemon ops (`DaemonGoalOps`, `DaemonAgentOps`, etc.) to route
//! reconciliation actions to the appropriate subsystems.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use symbiotic_agents::action_dispatch::{ActionDispatcher, DispatchAction, DispatchActionType};
use symbiotic_control_plane::manifest::ManifestParser;
use symbiotic_control_plane::reconciler::{Reconciler, ReconcilerConfig, StateQuery};
use symbiotic_control_plane::types::{ActionType, ActualState, ReconciliationAction};
use symbiotic_control_plane::{HeartbeatUpdate, ScopeClaim, WorkItem};

/// Implements `StateQuery` for the daemon by returning a shared `ActualState`.
///
/// The daemon (or its subsystems) can update the state through the
/// `state_handle()` at any time; the reconciler reads it on each tick.
pub struct DaemonStateQuery {
    state: Arc<Mutex<ActualState>>,
}

impl Default for DaemonStateQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonStateQuery {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ActualState::default())),
        }
    }

    /// Returns a clone of the shared state handle so other daemon subsystems
    /// can update the actual state (e.g., when goals start, agents spawn, etc.).
    pub fn state_handle(&self) -> Arc<Mutex<ActualState>> {
        Arc::clone(&self.state)
    }
}

#[async_trait]
impl StateQuery for DaemonStateQuery {
    async fn query(&self) -> Result<ActualState> {
        let state = self
            .state
            .lock()
            .map_err(|e| anyhow::anyhow!("failed to lock actual state: {e}"))?;
        Ok(state.clone())
    }
}

/// Bundled return value from `spawn_reconciler`.
pub struct ReconcilerHandle {
    pub state: Arc<Mutex<ActualState>>,
    pub identity_content: Arc<Mutex<Option<String>>>,
    pub task: tokio::task::JoinHandle<()>,
}

/// Result of executing a reconciliation action.
#[derive(Debug)]
pub struct ActionResult {
    pub action_id: String,
    pub success: bool,
    pub detail: String,
}

/// Execute a single reconciliation action against daemon state.
///
/// Updates the shared `ActualState` to reflect the action. Actions that
/// require operator approval are skipped (logged but not executed).
/// The `identity_content` handle is updated when `ReloadIdentity` fires.
///
/// When `goal_state_file` is provided, goal lifecycle actions (Start, Stop,
/// Pause, Resume) also update the daemon's persistent `goal_state.tsv`,
/// bridging the control-plane reconciler to the daemon's actual subsystems.
pub fn execute_reconciliation_action(
    action: &ReconciliationAction,
    state_handle: &Arc<Mutex<ActualState>>,
    identity_content: &Arc<Mutex<Option<String>>>,
    kb_path: &std::path::Path,
) -> ActionResult {
    execute_reconciliation_action_with_state(action, state_handle, identity_content, kb_path, None)
}

/// Like [`execute_reconciliation_action`] but accepts an optional goal state
/// file path. When provided, goal lifecycle actions also update the daemon's
/// persistent state on disk.
pub fn execute_reconciliation_action_with_state(
    action: &ReconciliationAction,
    state_handle: &Arc<Mutex<ActualState>>,
    identity_content: &Arc<Mutex<Option<String>>>,
    kb_path: &std::path::Path,
    goal_state_file: Option<&std::path::Path>,
) -> ActionResult {
    if action.requires_approval {
        tracing::info!(
            action_id = %action.id,
            action_type = ?action.action_type,
            target = %action.target,
            "control_plane: action requires operator approval — skipped"
        );
        return ActionResult {
            action_id: action.id.clone(),
            success: false,
            detail: "requires operator approval".to_string(),
        };
    }

    match &action.action_type {
        ActionType::StartGoal => {
            let mut state = match state_handle.lock() {
                Ok(s) => s,
                Err(e) => {
                    return ActionResult {
                        action_id: action.id.clone(),
                        success: false,
                        detail: format!("lock error: {e}"),
                    }
                }
            };
            // Add the goal to active_goals if not already present.
            if !state.active_goals.iter().any(|g| g.slug == action.target) {
                state
                    .active_goals
                    .push(symbiotic_control_plane::types::ActiveGoalState {
                        slug: action.target.clone(),
                        state: symbiotic_control_plane::types::GoalState::Active,
                        phase: symbiotic_control_plane::types::GoalPhase::Inquisition,
                        running_agents: 0,
                    });
            }
            drop(state); // Release lock before I/O
                         // Persist to daemon's goal state file when available.
            if let Some(gsf) = goal_state_file {
                let now = symbiotic_queue::now_unix();
                let _ = crate::goal_state::upsert_goal_state(
                    gsf,
                    crate::goal_state::GoalState {
                        goal_room: format!("control-plane:{}", action.target),
                        thread_id: None,
                        project_id: crate::goals::default_unscoped_project_id(),
                        template: "control-plane".to_string(),
                        status: "starting".to_string(),
                        last_job_id: action.id.clone(),
                        last_run_id: None,
                        owner: Some("control-plane".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("starting".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                );
            }
            tracing::info!(target = %action.target, "control_plane: started goal");
            ActionResult {
                action_id: action.id.clone(),
                success: true,
                detail: format!("goal '{}' started", action.target),
            }
        }

        ActionType::StopGoal => {
            let mut state = match state_handle.lock() {
                Ok(s) => s,
                Err(e) => {
                    return ActionResult {
                        action_id: action.id.clone(),
                        success: false,
                        detail: format!("lock error: {e}"),
                    }
                }
            };
            state.active_goals.retain(|g| g.slug != action.target);
            drop(state);
            if let Some(gsf) = goal_state_file {
                let now = symbiotic_queue::now_unix();
                let _ = crate::goal_state::upsert_goal_state(
                    gsf,
                    crate::goal_state::GoalState {
                        goal_room: format!("control-plane:{}", action.target),
                        thread_id: None,
                        project_id: crate::goals::default_unscoped_project_id(),
                        template: "control-plane".to_string(),
                        status: "stopped".to_string(),
                        last_job_id: action.id.clone(),
                        last_run_id: None,
                        owner: Some("control-plane".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("stopped".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                );
            }
            tracing::info!(target = %action.target, "control_plane: stopped goal");
            ActionResult {
                action_id: action.id.clone(),
                success: true,
                detail: format!("goal '{}' stopped", action.target),
            }
        }

        ActionType::PauseGoal => {
            let mut state = match state_handle.lock() {
                Ok(s) => s,
                Err(e) => {
                    return ActionResult {
                        action_id: action.id.clone(),
                        success: false,
                        detail: format!("lock error: {e}"),
                    }
                }
            };
            if let Some(goal) = state
                .active_goals
                .iter_mut()
                .find(|g| g.slug == action.target)
            {
                goal.state = symbiotic_control_plane::types::GoalState::Paused;
            }
            drop(state);
            if let Some(gsf) = goal_state_file {
                let now = symbiotic_queue::now_unix();
                let _ = crate::goal_state::upsert_goal_state(
                    gsf,
                    crate::goal_state::GoalState {
                        goal_room: format!("control-plane:{}", action.target),
                        thread_id: None,
                        project_id: crate::goals::default_unscoped_project_id(),
                        template: "control-plane".to_string(),
                        status: "paused".to_string(),
                        last_job_id: action.id.clone(),
                        last_run_id: None,
                        owner: Some("control-plane".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("paused".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                );
            }
            tracing::info!(target = %action.target, "control_plane: paused goal");
            ActionResult {
                action_id: action.id.clone(),
                success: true,
                detail: format!("goal '{}' paused", action.target),
            }
        }

        ActionType::ResumeGoal => {
            let mut state = match state_handle.lock() {
                Ok(s) => s,
                Err(e) => {
                    return ActionResult {
                        action_id: action.id.clone(),
                        success: false,
                        detail: format!("lock error: {e}"),
                    }
                }
            };
            if let Some(goal) = state
                .active_goals
                .iter_mut()
                .find(|g| g.slug == action.target)
            {
                goal.state = symbiotic_control_plane::types::GoalState::Active;
            }
            drop(state);
            if let Some(gsf) = goal_state_file {
                let now = symbiotic_queue::now_unix();
                let _ = crate::goal_state::upsert_goal_state(
                    gsf,
                    crate::goal_state::GoalState {
                        goal_room: format!("control-plane:{}", action.target),
                        thread_id: None,
                        project_id: crate::goals::default_unscoped_project_id(),
                        template: "control-plane".to_string(),
                        status: "running".to_string(),
                        last_job_id: action.id.clone(),
                        last_run_id: None,
                        owner: Some("control-plane".to_string()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("running".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                );
            }
            tracing::info!(target = %action.target, "control_plane: resumed goal");
            ActionResult {
                action_id: action.id.clone(),
                success: true,
                detail: format!("goal '{}' resumed", action.target),
            }
        }

        ActionType::AdvancePhase { from, to } => {
            tracing::info!(
                target = %action.target,
                from = %from,
                to = %to,
                "control_plane: phase advance logged (manual follow-up needed)"
            );
            ActionResult {
                action_id: action.id.clone(),
                success: true,
                detail: format!("goal '{}' phase: {} -> {}", action.target, from, to),
            }
        }

        ActionType::SpawnAgents { goal, count } => {
            tracing::info!(
                goal = %goal,
                count = count,
                "control_plane: agent spawn requested (will be fulfilled by agent framework)"
            );
            ActionResult {
                action_id: action.id.clone(),
                success: true,
                detail: format!("spawn {} agent(s) for '{}'", count, goal),
            }
        }

        ActionType::ReloadIdentity => {
            let parser = ManifestParser::new();
            let Some(soul_path) = parser.resolve_identity_path(kb_path) else {
                return ActionResult {
                    action_id: action.id.clone(),
                    success: false,
                    detail: "no identity/SOUL.md found".to_string(),
                };
            };
            match parser.parse_identity(&soul_path) {
                Ok(identity) => {
                    // Update the identity content for agent spawning.
                    if let Ok(mut content) = identity_content.lock() {
                        *content = Some(identity.content.clone());
                    }
                    // Update the identity hash in actual state.
                    if let Ok(mut state) = state_handle.lock() {
                        state.identity_hash = Some(identity.content_hash.clone());
                    }
                    tracing::info!(
                        hash = %identity.content_hash,
                        "control_plane: identity reloaded from SOUL.md"
                    );
                    ActionResult {
                        action_id: action.id.clone(),
                        success: true,
                        detail: format!("identity reloaded (hash={})", identity.content_hash),
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "control_plane: failed to reload SOUL.md");
                    ActionResult {
                        action_id: action.id.clone(),
                        success: false,
                        detail: format!("failed to reload identity: {e}"),
                    }
                }
            }
        }

        ActionType::ReloadPreferences => {
            let parser = ManifestParser::new();
            let Some(prefs_path) = parser.resolve_preferences_path(kb_path) else {
                return ActionResult {
                    action_id: action.id.clone(),
                    success: false,
                    detail: "no identity/preferences.md found".to_string(),
                };
            };
            match parser.parse_preferences(&prefs_path) {
                Ok(prefs) => {
                    if let Ok(mut state) = state_handle.lock() {
                        state.preferences_hash = Some(prefs.content_hash.clone());
                    }
                    tracing::info!(
                        hash = %prefs.content_hash,
                        "control_plane: preferences reloaded"
                    );
                    ActionResult {
                        action_id: action.id.clone(),
                        success: true,
                        detail: format!("preferences reloaded (hash={})", prefs.content_hash),
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "control_plane: failed to reload preferences");
                    ActionResult {
                        action_id: action.id.clone(),
                        success: false,
                        detail: format!("failed to reload preferences: {e}"),
                    }
                }
            }
        }

        ActionType::LoadSkill { name } => {
            if let Ok(mut state) = state_handle.lock() {
                if !state.loaded_skills.contains(name) {
                    state.loaded_skills.push(name.clone());
                }
            }
            tracing::info!(skill = %name, "control_plane: skill loaded");
            ActionResult {
                action_id: action.id.clone(),
                success: true,
                detail: format!("skill '{}' loaded", name),
            }
        }

        ActionType::UnloadSkill { name } => {
            if let Ok(mut state) = state_handle.lock() {
                state.loaded_skills.retain(|s| s != name);
            }
            tracing::info!(skill = %name, "control_plane: skill unloaded");
            ActionResult {
                action_id: action.id.clone(),
                success: true,
                detail: format!("skill '{}' unloaded", name),
            }
        }
    }
}

/// Load SOUL.md identity on daemon startup.
///
/// Parses `{kb_path}/identity/SOUL.md` and returns `(content, content_hash)`.
/// On parse failure, returns `None` (non-fatal — daemon runs without identity).
pub fn load_identity_on_startup(kb_path: &std::path::Path) -> Option<(String, String)> {
    let parser = ManifestParser::new();
    let Some(soul_path) = parser.resolve_identity_path(kb_path) else {
        tracing::info!(
            "control_plane: no identity/SOUL.md found under {}",
            kb_path.display()
        );
        return None;
    };
    match parser.parse_identity(&soul_path) {
        Ok(identity) => {
            tracing::info!(
                hash = %identity.content_hash,
                "control_plane: loaded SOUL.md identity on startup"
            );
            Some((identity.content, identity.content_hash))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %soul_path.display(),
                "control_plane: failed to parse SOUL.md on startup"
            );
            None
        }
    }
}

/// Load SOUL.md identity from an explicit file path.
///
/// Parses the given path as a SOUL.md file and returns `(content, content_hash)`.
/// On parse failure or missing file, returns `None` (non-fatal -- daemon runs without identity).
pub fn load_identity_from_file(path: &std::path::Path) -> Option<(String, String)> {
    if !path.exists() {
        tracing::info!(
            "control_plane: no SOUL.md found at explicit path {}",
            path.display()
        );
        return None;
    }
    let parser = ManifestParser::new();
    match parser.parse_identity(path) {
        Ok(identity) => {
            tracing::info!(
                hash = %identity.content_hash,
                path = %path.display(),
                "control_plane: loaded SOUL.md identity from explicit path"
            );
            Some((identity.content, identity.content_hash))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "control_plane: failed to parse SOUL.md from explicit path"
            );
            None
        }
    }
}

/// Load SOUL.md identity from the home directory default (`~/.symbiotic/SOUL.md`).
///
/// Returns `None` if the home directory cannot be determined, the file does not
/// exist, or parsing fails. This is the lowest-priority fallback.
pub fn load_identity_from_home() -> Option<(String, String)> {
    let home = match std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
    {
        Some(h) if !h.is_empty() => std::path::PathBuf::from(h),
        _ => {
            tracing::debug!("control_plane: cannot determine home directory for SOUL.md fallback");
            return None;
        }
    };
    let soul_path = home.join(".symbiotic").join("SOUL.md");
    if !soul_path.exists() {
        tracing::debug!(
            "control_plane: no SOUL.md at home fallback {}",
            soul_path.display()
        );
        return None;
    }
    let parser = ManifestParser::new();
    match parser.parse_identity(&soul_path) {
        Ok(identity) => {
            tracing::info!(
                hash = %identity.content_hash,
                path = %soul_path.display(),
                "control_plane: loaded SOUL.md identity from home directory"
            );
            Some((identity.content, identity.content_hash))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %soul_path.display(),
                "control_plane: failed to parse SOUL.md from home directory"
            );
            None
        }
    }
}

impl crate::SymbioticDaemon {
    /// Persist or update a management-layer work item in the daemon-owned store.
    pub fn upsert_management_work_item(&self, work_item: WorkItem) -> Result<()> {
        let mut store = self
            .management_store
            .lock()
            .map_err(|e| anyhow::anyhow!("failed to lock management store: {e}"))?;
        store.upsert_work_item(work_item)
    }

    /// Grant a scope claim through the daemon-owned management store.
    pub fn grant_management_scope_claim(&self, claim: ScopeClaim) -> Result<()> {
        let mut store = self
            .management_store
            .lock()
            .map_err(|e| anyhow::anyhow!("failed to lock management store: {e}"))?;
        store.grant_claim(claim)
    }

    /// Record a worker heartbeat and return the updated claim IDs.
    pub fn record_management_heartbeat(&self, heartbeat: HeartbeatUpdate) -> Result<Vec<String>> {
        let mut store = self
            .management_store
            .lock()
            .map_err(|e| anyhow::anyhow!("failed to lock management store: {e}"))?;
        store.record_heartbeat(heartbeat)
    }

    /// Force a stale-claim scan using the daemon-owned management store.
    pub fn expire_stale_management_claims(&self, now_ts: i64) -> Result<Vec<String>> {
        let mut store = self
            .management_store
            .lock()
            .map_err(|e| anyhow::anyhow!("failed to lock management store: {e}"))?;
        store.expire_stale_claims(now_ts)
    }
}

/// Spawn the reconciliation background loop as a tokio task.
///
/// The loop runs every `config.reconcile_interval_secs` seconds, calling
/// `Reconciler::reconcile()` and executing each generated action. The task
/// runs until the returned `JoinHandle` is aborted or the process exits.
///
/// Returns a [`ReconcilerHandle`] containing the shared state, identity
/// content, and the background task handle.
pub fn spawn_reconciler(archive_path: PathBuf) -> ReconcilerHandle {
    spawn_reconciler_with_identity(archive_path, Arc::new(Mutex::new(None)))
}

/// Like [`spawn_reconciler_with_identity`] but also accepts a goal state file path.
/// When provided, reconciler actions that affect goal lifecycle also update
/// the daemon's persistent `goal_state.tsv` file.
pub fn spawn_reconciler_full(
    archive_path: PathBuf,
    identity_content: Arc<Mutex<Option<String>>>,
    goal_state_file: Option<PathBuf>,
) -> ReconcilerHandle {
    let state_query = DaemonStateQuery::new();
    let state_handle = state_query.state_handle();

    // Load SOUL.md on startup and seed the state.
    if let Some((content, hash)) = load_identity_on_startup(&archive_path) {
        if let Ok(mut ic) = identity_content.lock() {
            *ic = Some(content);
        }
        if let Ok(mut state) = state_handle.lock() {
            state.identity_hash = Some(hash);
        }
    }

    let config = ReconcilerConfig {
        archive_path,
        reconcile_interval_secs: 30,
        watch_filesystem: false,
        max_actions_per_tick: 10,
    };

    let interval_secs = config.reconcile_interval_secs;
    let kb_path = config.archive_path.clone();
    let reconciler = Reconciler::new(config, Box::new(state_query));

    let loop_state = Arc::clone(&state_handle);
    let loop_identity = Arc::clone(&identity_content);

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

        tracing::info!(
            interval_secs = interval_secs,
            "control_plane: reconciler background loop started (with state persistence)"
        );

        loop {
            interval.tick().await;

            match reconciler.reconcile().await {
                Ok(actions) if actions.is_empty() => {
                    tracing::debug!("control_plane: reconcile tick — no actions");
                }
                Ok(actions) => {
                    tracing::info!(
                        action_count = actions.len(),
                        "control_plane: reconcile tick — generated actions"
                    );
                    for action in &actions {
                        let result = execute_reconciliation_action_with_state(
                            action,
                            &loop_state,
                            &loop_identity,
                            &kb_path,
                            goal_state_file.as_deref(),
                        );
                        if result.success {
                            tracing::info!(
                                action_id = %result.action_id,
                                detail = %result.detail,
                                "control_plane: action executed"
                            );
                        } else {
                            tracing::warn!(
                                action_id = %result.action_id,
                                detail = %result.detail,
                                "control_plane: action failed"
                            );
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "control_plane: reconcile tick failed"
                    );
                }
            }
        }
    });

    ReconcilerHandle {
        state: state_handle,
        identity_content,
        task: handle,
    }
}

/// Like [`spawn_reconciler`] but accepts an external `identity_content` handle
/// so the caller (typically the daemon) can share the same Arc and observe
/// identity updates from the reconciler's `ReloadIdentity` action in real time.
pub fn spawn_reconciler_with_identity(
    archive_path: PathBuf,
    identity_content: Arc<Mutex<Option<String>>>,
) -> ReconcilerHandle {
    let state_query = DaemonStateQuery::new();
    let state_handle = state_query.state_handle();

    // Load SOUL.md on startup and seed the state.
    if let Some((content, hash)) = load_identity_on_startup(&archive_path) {
        if let Ok(mut ic) = identity_content.lock() {
            *ic = Some(content);
        }
        if let Ok(mut state) = state_handle.lock() {
            state.identity_hash = Some(hash);
        }
    }

    let config = ReconcilerConfig {
        archive_path,
        reconcile_interval_secs: 30,
        watch_filesystem: false,
        max_actions_per_tick: 10,
    };

    let interval_secs = config.reconcile_interval_secs;
    let kb_path = config.archive_path.clone();
    let reconciler = Reconciler::new(config, Box::new(state_query));

    let loop_state = Arc::clone(&state_handle);
    let loop_identity = Arc::clone(&identity_content);

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

        tracing::info!(
            interval_secs = interval_secs,
            "control_plane: reconciler background loop started"
        );

        loop {
            interval.tick().await;

            match reconciler.reconcile().await {
                Ok(actions) if actions.is_empty() => {
                    tracing::debug!("control_plane: reconcile tick — no actions");
                }
                Ok(actions) => {
                    tracing::info!(
                        action_count = actions.len(),
                        "control_plane: reconcile tick — generated actions"
                    );
                    for action in &actions {
                        let result = execute_reconciliation_action(
                            action,
                            &loop_state,
                            &loop_identity,
                            &kb_path,
                        );
                        if result.success {
                            tracing::info!(
                                action_id = %result.action_id,
                                detail = %result.detail,
                                "control_plane: action executed"
                            );
                        } else {
                            tracing::warn!(
                                action_id = %result.action_id,
                                detail = %result.detail,
                                "control_plane: action failed"
                            );
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "control_plane: reconcile tick failed"
                    );
                }
            }
        }
    });

    ReconcilerHandle {
        state: state_handle,
        identity_content,
        task: handle,
    }
}

// ---------------------------------------------------------------------------
// ActionDispatcher integration — bridges control-plane actions to daemon ops
// ---------------------------------------------------------------------------

/// Convert a control-plane `ReconciliationAction` into an agent-crate
/// `DispatchAction`.
///
/// The two types are structurally identical but live in different crates to
/// avoid a circular dependency (control-plane depends on agents, so agents
/// cannot import control-plane types). This free function bridges them
/// (a `From` impl is not possible due to Rust's orphan rule).
pub fn to_dispatch_action(action: &ReconciliationAction) -> DispatchAction {
    let action_type = match &action.action_type {
        ActionType::StartGoal => DispatchActionType::StartGoal,
        ActionType::StopGoal => DispatchActionType::StopGoal,
        ActionType::PauseGoal => DispatchActionType::PauseGoal,
        ActionType::ResumeGoal => DispatchActionType::ResumeGoal,
        ActionType::AdvancePhase { from, to } => DispatchActionType::AdvancePhase {
            from: from.clone(),
            to: to.clone(),
        },
        ActionType::SpawnAgents { goal, count } => DispatchActionType::SpawnAgents {
            goal: goal.clone(),
            count: *count,
        },
        ActionType::ReloadIdentity => DispatchActionType::ReloadIdentity,
        ActionType::ReloadPreferences => DispatchActionType::ReloadPreferences,
        ActionType::LoadSkill { name } => DispatchActionType::LoadSkill { name: name.clone() },
        ActionType::UnloadSkill { name } => DispatchActionType::UnloadSkill { name: name.clone() },
    };

    DispatchAction {
        id: action.id.clone(),
        action_type,
        target: action.target.clone(),
        description: action.description.clone(),
        requires_approval: action.requires_approval,
        estimated_cost: action.estimated_cost,
    }
}

/// Spawn the reconciliation background loop with the trait-based
/// `ActionDispatcher`.
///
/// This is the preferred entry point for wiring the reconciler into the
/// daemon. Instead of the manual `execute_reconciliation_action_with_state`
/// function, reconciliation actions are routed through the `ActionDispatcher`
/// which delegates to concrete `DaemonGoalOps`, `DaemonAgentOps`,
/// `DaemonSkillOps`, and `DaemonIdentityOps` implementations.
///
/// The `ActionDispatcher` is `Send + Sync` (all ops use `Arc<Mutex<..>>`),
/// so it is safe to move into the tokio task.
///
/// The `state_handle` and `identity_content` Arcs are still maintained for
/// backward compatibility with subsystems that read them directly.
pub fn spawn_reconciler_with_dispatcher(
    archive_path: PathBuf,
    identity_content: Arc<Mutex<Option<String>>>,
    dispatcher: Arc<ActionDispatcher>,
) -> ReconcilerHandle {
    let state_query = DaemonStateQuery::new();
    let state_handle = state_query.state_handle();

    // Load SOUL.md on startup and seed the state.
    if let Some((content, hash)) = load_identity_on_startup(&archive_path) {
        if let Ok(mut ic) = identity_content.lock() {
            *ic = Some(content);
        }
        if let Ok(mut state) = state_handle.lock() {
            state.identity_hash = Some(hash);
        }
    }

    let config = ReconcilerConfig {
        archive_path,
        reconcile_interval_secs: 30,
        watch_filesystem: false,
        max_actions_per_tick: 10,
    };

    let interval_secs = config.reconcile_interval_secs;
    let reconciler = Reconciler::new(config, Box::new(state_query));

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

        tracing::info!(
            interval_secs = interval_secs,
            "control_plane: reconciler background loop started (with ActionDispatcher)"
        );

        loop {
            interval.tick().await;

            match reconciler.reconcile().await {
                Ok(actions) if actions.is_empty() => {
                    tracing::debug!("control_plane: reconcile tick — no actions");
                }
                Ok(actions) => {
                    tracing::info!(
                        action_count = actions.len(),
                        "control_plane: reconcile tick — dispatching actions"
                    );

                    // Convert control-plane actions to dispatch actions.
                    let dispatch_actions: Vec<DispatchAction> =
                        actions.iter().map(to_dispatch_action).collect();

                    let summary = dispatcher.dispatch_batch(&dispatch_actions).await;

                    if summary.failed > 0 {
                        tracing::warn!(
                            total = summary.total,
                            succeeded = summary.succeeded,
                            failed = summary.failed,
                            skipped = summary.skipped,
                            "control_plane: dispatch batch completed with failures"
                        );
                    } else {
                        tracing::info!(
                            total = summary.total,
                            succeeded = summary.succeeded,
                            skipped = summary.skipped,
                            "control_plane: dispatch batch completed"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "control_plane: reconcile tick failed"
                    );
                }
            }
        }
    });

    ReconcilerHandle {
        state: state_handle,
        identity_content,
        task: handle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn daemon_state_query_returns_default() {
        let query = DaemonStateQuery::new();
        let state = query.query().await.unwrap();
        assert!(state.active_goals.is_empty());
        assert_eq!(state.running_agents, 0);
        assert!(state.loaded_skills.is_empty());
        assert!(state.identity_hash.is_none());
        assert!(state.preferences_hash.is_none());
    }

    #[tokio::test]
    async fn daemon_state_query_reflects_updates() {
        let query = DaemonStateQuery::new();
        let handle = query.state_handle();

        // Update through the shared handle
        {
            let mut state = handle.lock().unwrap();
            state.running_agents = 3;
            state.loaded_skills = vec!["web-search".to_string()];
            state.identity_hash = Some("abc123".to_string());
        }

        let state = query.query().await.unwrap();
        assert_eq!(state.running_agents, 3);
        assert_eq!(state.loaded_skills, vec!["web-search".to_string()]);
        assert_eq!(state.identity_hash, Some("abc123".to_string()));
    }

    #[test]
    fn execute_start_goal_adds_to_state() {
        let state_handle = Arc::new(Mutex::new(ActualState::default()));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let action = ReconciliationAction {
            id: "act-1".to_string(),
            action_type: ActionType::StartGoal,
            target: "test-goal".to_string(),
            description: "Start test goal".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(result.success);

        let state = state_handle.lock().unwrap();
        assert_eq!(state.active_goals.len(), 1);
        assert_eq!(state.active_goals[0].slug, "test-goal");
    }

    #[test]
    fn execute_stop_goal_removes_from_state() {
        let state_handle = Arc::new(Mutex::new(ActualState {
            active_goals: vec![symbiotic_control_plane::types::ActiveGoalState {
                slug: "remove-me".to_string(),
                state: symbiotic_control_plane::types::GoalState::Active,
                phase: symbiotic_control_plane::types::GoalPhase::Research,
                running_agents: 1,
            }],
            ..Default::default()
        }));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let action = ReconciliationAction {
            id: "act-2".to_string(),
            action_type: ActionType::StopGoal,
            target: "remove-me".to_string(),
            description: "Stop goal".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(result.success);

        let state = state_handle.lock().unwrap();
        assert!(state.active_goals.is_empty());
    }

    #[test]
    fn execute_requires_approval_is_skipped() {
        let state_handle = Arc::new(Mutex::new(ActualState::default()));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let action = ReconciliationAction {
            id: "act-3".to_string(),
            action_type: ActionType::StartGoal,
            target: "manual-goal".to_string(),
            description: "Requires approval".to_string(),
            requires_approval: true,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(!result.success);
        assert!(result.detail.contains("approval"));

        let state = state_handle.lock().unwrap();
        assert!(state.active_goals.is_empty());
    }

    #[test]
    fn execute_reload_identity_updates_content_and_hash() {
        let state_handle = Arc::new(Mutex::new(ActualState::default()));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        // Write a SOUL.md file
        let soul_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&soul_dir).unwrap();
        std::fs::write(
            soul_dir.join("SOUL.md"),
            "---\nversion: 1\n---\n\n# Identity\nI am Symbiotic.\n",
        )
        .unwrap();

        let action = ReconciliationAction {
            id: "act-4".to_string(),
            action_type: ActionType::ReloadIdentity,
            target: "SOUL.md".to_string(),
            description: "Reload identity".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(result.success);

        // Identity content should be set.
        let ic = identity.lock().unwrap();
        let content = ic.as_ref().unwrap();
        assert!(content.contains("I am Symbiotic."));

        // Identity hash should be set in actual state.
        let state = state_handle.lock().unwrap();
        assert!(state.identity_hash.is_some());
    }

    #[test]
    fn execute_load_skill_adds_to_state() {
        let state_handle = Arc::new(Mutex::new(ActualState::default()));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let action = ReconciliationAction {
            id: "act-5".to_string(),
            action_type: ActionType::LoadSkill {
                name: "web-scraper".to_string(),
            },
            target: "web-scraper".to_string(),
            description: "Load skill".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(result.success);

        let state = state_handle.lock().unwrap();
        assert_eq!(state.loaded_skills, vec!["web-scraper".to_string()]);
    }

    #[test]
    fn execute_unload_skill_removes_from_state() {
        let state_handle = Arc::new(Mutex::new(ActualState {
            loaded_skills: vec!["old-skill".to_string(), "keep-me".to_string()],
            ..Default::default()
        }));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let action = ReconciliationAction {
            id: "act-6".to_string(),
            action_type: ActionType::UnloadSkill {
                name: "old-skill".to_string(),
            },
            target: "old-skill".to_string(),
            description: "Unload skill".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(result.success);

        let state = state_handle.lock().unwrap();
        assert_eq!(state.loaded_skills, vec!["keep-me".to_string()]);
    }

    #[test]
    fn execute_pause_goal_updates_state() {
        let state_handle = Arc::new(Mutex::new(ActualState {
            active_goals: vec![symbiotic_control_plane::types::ActiveGoalState {
                slug: "pausable".to_string(),
                state: symbiotic_control_plane::types::GoalState::Active,
                phase: symbiotic_control_plane::types::GoalPhase::Research,
                running_agents: 1,
            }],
            ..Default::default()
        }));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let action = ReconciliationAction {
            id: "act-7".to_string(),
            action_type: ActionType::PauseGoal,
            target: "pausable".to_string(),
            description: "Pause goal".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(result.success);

        let state = state_handle.lock().unwrap();
        assert_eq!(
            state.active_goals[0].state,
            symbiotic_control_plane::types::GoalState::Paused
        );
    }

    #[test]
    fn execute_resume_goal_updates_state() {
        let state_handle = Arc::new(Mutex::new(ActualState {
            active_goals: vec![symbiotic_control_plane::types::ActiveGoalState {
                slug: "resumable".to_string(),
                state: symbiotic_control_plane::types::GoalState::Paused,
                phase: symbiotic_control_plane::types::GoalPhase::Research,
                running_agents: 0,
            }],
            ..Default::default()
        }));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let action = ReconciliationAction {
            id: "act-8".to_string(),
            action_type: ActionType::ResumeGoal,
            target: "resumable".to_string(),
            description: "Resume goal".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(result.success);

        let state = state_handle.lock().unwrap();
        assert_eq!(
            state.active_goals[0].state,
            symbiotic_control_plane::types::GoalState::Active
        );
    }

    #[test]
    fn load_identity_on_startup_returns_content() {
        let tmp = tempfile::tempdir().unwrap();
        let soul_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&soul_dir).unwrap();
        std::fs::write(
            soul_dir.join("SOUL.md"),
            "---\nversion: 1\n---\n\n# SOUL\nDirective: Act with sovereignty.\n",
        )
        .unwrap();

        let result = load_identity_on_startup(tmp.path());
        assert!(result.is_some());

        let (content, hash) = result.unwrap();
        assert!(content.contains("Directive: Act with sovereignty."));
        assert!(!hash.is_empty());
    }

    #[test]
    fn load_identity_on_startup_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load_identity_on_startup(tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn execute_reload_preferences_updates_hash() {
        let state_handle = Arc::new(Mutex::new(ActualState::default()));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let prefs_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&prefs_dir).unwrap();
        std::fs::write(
            prefs_dir.join("preferences.md"),
            "---\nversion: 1\n---\n\n# Preferences\nauto_approve: 0.85\n",
        )
        .unwrap();

        let action = ReconciliationAction {
            id: "act-9".to_string(),
            action_type: ActionType::ReloadPreferences,
            target: "preferences.md".to_string(),
            description: "Reload preferences".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action(&action, &state_handle, &identity, tmp.path());
        assert!(result.success);

        let state = state_handle.lock().unwrap();
        assert!(state.preferences_hash.is_some());
    }

    #[tokio::test]
    async fn spawn_reconciler_with_identity_shares_arc() {
        // Verify that an externally-provided identity Arc is used by the reconciler
        // and receives SOUL.md content on startup.
        let tmp = tempfile::tempdir().unwrap();
        let soul_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&soul_dir).unwrap();
        std::fs::write(
            soul_dir.join("SOUL.md"),
            "---\nversion: 1\n---\n\n# SOUL\nI am the shared identity.\n",
        )
        .unwrap();

        let shared_identity: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // spawn_reconciler_with_identity should load SOUL.md into the shared handle
        let handle =
            spawn_reconciler_with_identity(tmp.path().to_path_buf(), shared_identity.clone());

        // The identity_content in the handle should be the SAME Arc we passed in
        assert!(Arc::ptr_eq(&handle.identity_content, &shared_identity));

        // Content should have been loaded from SOUL.md
        let content = shared_identity.lock().unwrap();
        assert!(content.is_some());
        assert!(content
            .as_ref()
            .unwrap()
            .contains("I am the shared identity."));

        // Cleanup: abort the background task
        handle.task.abort();
    }

    #[tokio::test]
    async fn spawn_reconciler_with_identity_no_soul_file() {
        let tmp = tempfile::tempdir().unwrap();
        // No SOUL.md created

        let shared_identity: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let handle =
            spawn_reconciler_with_identity(tmp.path().to_path_buf(), shared_identity.clone());

        // No SOUL.md means identity stays None
        let content = shared_identity.lock().unwrap();
        assert!(content.is_none());

        handle.task.abort();
    }

    // -----------------------------------------------------------------------
    // Tests for execute_reconciliation_action_with_state (goal state file persistence)
    // -----------------------------------------------------------------------

    #[test]
    fn execute_start_goal_persists_to_goal_state_file() {
        let state_handle = Arc::new(Mutex::new(ActualState::default()));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();
        let goal_state_file = tmp.path().join("goal-state.tsv");

        let action = ReconciliationAction {
            id: "persist-1".to_string(),
            action_type: ActionType::StartGoal,
            target: "persistent-goal".to_string(),
            description: "Start persistent goal".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action_with_state(
            &action,
            &state_handle,
            &identity,
            tmp.path(),
            Some(&goal_state_file),
        );
        assert!(result.success);

        // Verify in-memory state.
        let state = state_handle.lock().unwrap();
        assert_eq!(state.active_goals.len(), 1);
        assert_eq!(state.active_goals[0].slug, "persistent-goal");
        drop(state);

        // Verify on-disk state.
        let persisted = crate::goal_state::load_goal_states(&goal_state_file).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].goal_room, "control-plane:persistent-goal");
        assert_eq!(persisted[0].status, "starting");
        assert_eq!(persisted[0].template, "control-plane");
    }

    #[test]
    fn execute_stop_goal_persists_to_goal_state_file() {
        let state_handle = Arc::new(Mutex::new(ActualState {
            active_goals: vec![symbiotic_control_plane::types::ActiveGoalState {
                slug: "stop-me".to_string(),
                state: symbiotic_control_plane::types::GoalState::Active,
                phase: symbiotic_control_plane::types::GoalPhase::Research,
                running_agents: 1,
            }],
            ..Default::default()
        }));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();
        let goal_state_file = tmp.path().join("goal-state.tsv");

        let action = ReconciliationAction {
            id: "persist-2".to_string(),
            action_type: ActionType::StopGoal,
            target: "stop-me".to_string(),
            description: "Stop goal".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action_with_state(
            &action,
            &state_handle,
            &identity,
            tmp.path(),
            Some(&goal_state_file),
        );
        assert!(result.success);

        let persisted = crate::goal_state::load_goal_states(&goal_state_file).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].status, "stopped");
    }

    #[test]
    fn execute_pause_goal_persists_to_goal_state_file() {
        let state_handle = Arc::new(Mutex::new(ActualState {
            active_goals: vec![symbiotic_control_plane::types::ActiveGoalState {
                slug: "pause-me".to_string(),
                state: symbiotic_control_plane::types::GoalState::Active,
                phase: symbiotic_control_plane::types::GoalPhase::Research,
                running_agents: 1,
            }],
            ..Default::default()
        }));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();
        let goal_state_file = tmp.path().join("goal-state.tsv");

        let action = ReconciliationAction {
            id: "persist-3".to_string(),
            action_type: ActionType::PauseGoal,
            target: "pause-me".to_string(),
            description: "Pause goal".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action_with_state(
            &action,
            &state_handle,
            &identity,
            tmp.path(),
            Some(&goal_state_file),
        );
        assert!(result.success);

        let persisted = crate::goal_state::load_goal_states(&goal_state_file).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].status, "paused");
    }

    #[test]
    fn execute_resume_goal_persists_to_goal_state_file() {
        let state_handle = Arc::new(Mutex::new(ActualState {
            active_goals: vec![symbiotic_control_plane::types::ActiveGoalState {
                slug: "resume-me".to_string(),
                state: symbiotic_control_plane::types::GoalState::Paused,
                phase: symbiotic_control_plane::types::GoalPhase::Research,
                running_agents: 0,
            }],
            ..Default::default()
        }));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();
        let goal_state_file = tmp.path().join("goal-state.tsv");

        let action = ReconciliationAction {
            id: "persist-4".to_string(),
            action_type: ActionType::ResumeGoal,
            target: "resume-me".to_string(),
            description: "Resume goal".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action_with_state(
            &action,
            &state_handle,
            &identity,
            tmp.path(),
            Some(&goal_state_file),
        );
        assert!(result.success);

        let persisted = crate::goal_state::load_goal_states(&goal_state_file).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].status, "running");
    }

    #[test]
    fn execute_without_goal_state_file_still_works() {
        // When goal_state_file is None, actions should still succeed
        // (only in-memory state is updated).
        let state_handle = Arc::new(Mutex::new(ActualState::default()));
        let identity = Arc::new(Mutex::new(None));
        let tmp = tempfile::tempdir().unwrap();

        let action = ReconciliationAction {
            id: "no-file-1".to_string(),
            action_type: ActionType::StartGoal,
            target: "memory-only".to_string(),
            description: "Start without file".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };

        let result = execute_reconciliation_action_with_state(
            &action,
            &state_handle,
            &identity,
            tmp.path(),
            None, // No file
        );
        assert!(result.success);

        let state = state_handle.lock().unwrap();
        assert_eq!(state.active_goals.len(), 1);
    }

    #[tokio::test]
    async fn spawn_reconciler_full_loads_identity_and_accepts_state_file() {
        let tmp = tempfile::tempdir().unwrap();
        let soul_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&soul_dir).unwrap();
        std::fs::write(
            soul_dir.join("SOUL.md"),
            "---\nversion: 1\n---\n\n# SOUL\nFull reconciler test.\n",
        )
        .unwrap();

        let shared_identity: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let goal_state_file = tmp.path().join("goal-state.tsv");

        let handle = spawn_reconciler_full(
            tmp.path().to_path_buf(),
            shared_identity.clone(),
            Some(goal_state_file),
        );

        // Identity should be loaded
        let content = shared_identity.lock().unwrap();
        assert!(content.is_some());
        assert!(content.as_ref().unwrap().contains("Full reconciler test."));

        handle.task.abort();
    }

    // -----------------------------------------------------------------------
    // Tests for load_identity_from_file and load_identity_from_home
    // -----------------------------------------------------------------------

    #[test]
    fn load_identity_from_file_returns_content() {
        let tmp = tempfile::tempdir().unwrap();
        let soul_path = tmp.path().join("SOUL.md");
        std::fs::write(
            &soul_path,
            "---\nversion: 1\n---\n\n# SOUL\nDirect file identity.\n",
        )
        .unwrap();

        let result = load_identity_from_file(&soul_path);
        assert!(result.is_some());

        let (content, hash) = result.unwrap();
        assert!(content.contains("Direct file identity."));
        assert!(!hash.is_empty());
    }

    #[test]
    fn load_identity_from_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let soul_path = tmp.path().join("nonexistent/SOUL.md");

        let result = load_identity_from_file(&soul_path);
        assert!(result.is_none());
    }

    #[test]
    fn load_identity_from_home_resolves_symbiotic_dir() {
        // Test the resolution logic by verifying that load_identity_from_file
        // works with the expected ~/.symbiotic/SOUL.md path structure.
        // We avoid setting HOME env var because it's process-global and
        // causes races with parallel tests.
        let tmp = tempfile::tempdir().unwrap();
        let sym_dir = tmp.path().join(".symbiotic");
        std::fs::create_dir_all(&sym_dir).unwrap();
        std::fs::write(
            sym_dir.join("SOUL.md"),
            "---\nversion: 1\n---\n\n# SOUL\nHome directory identity.\n",
        )
        .unwrap();

        // Directly test with the expected path structure.
        let soul_path = sym_dir.join("SOUL.md");
        let result = load_identity_from_file(&soul_path);
        assert!(result.is_some());
        let (content, hash) = result.unwrap();
        assert!(content.contains("Home directory identity."));
        assert!(!hash.is_empty());
    }

    // -----------------------------------------------------------------------
    // to_dispatch_action conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn to_dispatch_action_converts_start_goal() {
        let action = ReconciliationAction {
            id: "conv-1".to_string(),
            action_type: ActionType::StartGoal,
            target: "my-goal".to_string(),
            description: "Start my goal".to_string(),
            requires_approval: false,
            estimated_cost: Some(1.5),
        };
        let dispatch = to_dispatch_action(&action);
        assert_eq!(dispatch.id, "conv-1");
        assert_eq!(dispatch.target, "my-goal");
        assert_eq!(dispatch.description, "Start my goal");
        assert!(!dispatch.requires_approval);
        assert_eq!(dispatch.estimated_cost, Some(1.5));
        assert!(matches!(
            dispatch.action_type,
            DispatchActionType::StartGoal
        ));
    }

    #[test]
    fn to_dispatch_action_converts_advance_phase() {
        let action = ReconciliationAction {
            id: "conv-2".to_string(),
            action_type: ActionType::AdvancePhase {
                from: "research".to_string(),
                to: "implementation".to_string(),
            },
            target: "goal-x".to_string(),
            description: "Advance phase".to_string(),
            requires_approval: true,
            estimated_cost: None,
        };
        let dispatch = to_dispatch_action(&action);
        assert!(dispatch.requires_approval);
        match &dispatch.action_type {
            DispatchActionType::AdvancePhase { from, to } => {
                assert_eq!(from, "research");
                assert_eq!(to, "implementation");
            }
            other => panic!("expected AdvancePhase, got {:?}", other),
        }
    }

    #[test]
    fn to_dispatch_action_converts_spawn_agents() {
        let action = ReconciliationAction {
            id: "conv-3".to_string(),
            action_type: ActionType::SpawnAgents {
                goal: "trading".to_string(),
                count: 5,
            },
            target: "trading".to_string(),
            description: "Spawn".to_string(),
            requires_approval: false,
            estimated_cost: None,
        };
        let dispatch = to_dispatch_action(&action);
        match &dispatch.action_type {
            DispatchActionType::SpawnAgents { goal, count } => {
                assert_eq!(goal, "trading");
                assert_eq!(*count, 5);
            }
            other => panic!("expected SpawnAgents, got {:?}", other),
        }
    }

    #[test]
    fn to_dispatch_action_converts_all_action_types() {
        // Verify all variants convert without panicking.
        let types = vec![
            ActionType::StartGoal,
            ActionType::StopGoal,
            ActionType::PauseGoal,
            ActionType::ResumeGoal,
            ActionType::AdvancePhase {
                from: "a".to_string(),
                to: "b".to_string(),
            },
            ActionType::SpawnAgents {
                goal: "g".to_string(),
                count: 1,
            },
            ActionType::ReloadIdentity,
            ActionType::ReloadPreferences,
            ActionType::LoadSkill {
                name: "s".to_string(),
            },
            ActionType::UnloadSkill {
                name: "s".to_string(),
            },
        ];

        for at in types {
            let action = ReconciliationAction {
                id: "test".to_string(),
                action_type: at,
                target: "t".to_string(),
                description: "d".to_string(),
                requires_approval: false,
                estimated_cost: None,
            };
            let _dispatch = to_dispatch_action(&action);
            // No panic — conversion succeeded.
        }
    }

    // -----------------------------------------------------------------------
    // spawn_reconciler_with_dispatcher tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_reconciler_with_dispatcher_loads_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let soul_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&soul_dir).unwrap();
        std::fs::write(
            soul_dir.join("SOUL.md"),
            "---\nversion: 1\n---\n\n# SOUL\nDispatcher test identity.\n",
        )
        .unwrap();

        let shared_identity: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Build a dispatcher with mock ops (from the agents crate test helpers).
        // We use the simplest possible concrete ops since this test focuses
        // on the spawning and identity loading, not action dispatch.
        let gpm_store = tmp.path().join("gpm");
        let gpm = symbiotic_control_plane::GoalProcessManager::new(gpm_store);
        let actual_state = Arc::new(Mutex::new(ActualState::default()));

        let goal_ops = Arc::new(crate::dispatch_ops::DaemonGoalOps {
            goal_process_manager: Arc::new(Mutex::new(gpm)),
            goal_state_file: tmp.path().join("state.tsv"),
            goal_log_file: tmp.path().join("runs.log"),
            management_store: Arc::new(Mutex::new(symbiotic_control_plane::ManagementStore::new(
                tmp.path().join("control-plane"),
            ))),
        });
        let agent_ops = Arc::new(crate::dispatch_ops::DaemonAgentOps {
            goal_state_file: tmp.path().join("state.tsv"),
            agent_log_file: tmp.path().join("agents.log"),
        });
        let skill_ops = Arc::new(crate::dispatch_ops::DaemonSkillOps {
            actual_state: Arc::clone(&actual_state),
        });
        let identity_ops = Arc::new(crate::dispatch_ops::DaemonIdentityOps {
            identity_content: Arc::clone(&shared_identity),
            actual_state,
            kb_path: tmp.path().to_path_buf(),
        });

        let dispatcher = Arc::new(ActionDispatcher::new(
            goal_ops,
            agent_ops,
            skill_ops,
            identity_ops,
        ));

        let handle = spawn_reconciler_with_dispatcher(
            tmp.path().to_path_buf(),
            shared_identity.clone(),
            dispatcher,
        );

        // Identity should have been loaded from SOUL.md on startup.
        let content = shared_identity.lock().unwrap();
        assert!(content.is_some());
        assert!(content
            .as_ref()
            .unwrap()
            .contains("Dispatcher test identity."));

        handle.task.abort();
    }
}
