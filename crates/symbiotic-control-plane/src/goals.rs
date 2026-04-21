//! Goal process lifecycle manager.
//!
//! Manages the runtime representation of active goals, including state
//! transitions, phase advancement, agent slot allocation, and metrics
//! tracking. Goals are persisted as JSON files in the store directory.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use symbiotic_agents::pool::{AgentPool, PoolLlmType, WorkerId};
use symbiotic_core::types::question_group::{PlannedSpawn, UnblockKey};

use crate::types::{
    AutonomyLevel, GoalConstraints, GoalManifest, GoalPhase, GoalState, ProcessConfig, StreamConfig,
};

// ── GoalMetrics ─────────────────────────────────────────────────────────

/// Runtime metrics for a goal process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalMetrics {
    pub tasks_completed: u32,
    pub tasks_failed: u32,
    pub total_cost_usd: f64,
    pub agent_hours: f64,
}

// ── GoalProcess ─────────────────────────────────────────────────────────

/// Runtime representation of an active goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalProcess {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub state: GoalState,
    pub priority: u8,
    pub autonomy_level: AutonomyLevel,
    pub phase: GoalPhase,
    pub process: ProcessConfig,
    pub streams: Vec<StreamConfig>,
    pub domains: Vec<String>,
    pub vault_namespace: String,
    pub constraints: GoalConstraints,
    pub metrics: GoalMetrics,
    /// Unix timestamp when the goal was created.
    pub created_at: i64,
    /// Unix timestamp of the last check-in.
    pub last_check_at: i64,
    /// Unix timestamp when the next check is due.
    pub next_check_at: i64,

    // ── GoalDAG extensions (T130 §04) ─────────────────────────────────────
    //
    // These fields layer on the `GoalDagExtensions` bundle from
    // `symbiotic-core::types::question_group` (§2.3). Every field is
    // `#[serde(default)]` so existing stored `GoalProcess` records
    // (written before §04) load cleanly with `None` / `Vec::new()`.
    /// Parent goal id if this goal was spawned from a `PlannedSpawn`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_goal_id: Option<String>,
    /// `UnblockKey` set on child goals — identifies which backend
    /// dispatched this goal (§2.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unblock_key: Option<UnblockKey>,
    /// Group ids that must resolve before this goal runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by_groups: Vec<String>,
    /// Pre-planned sub-goals this goal spawns when a referenced group
    /// resolves (§2.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawns_on_unblock: Vec<PlannedSpawn>,
}

impl GoalProcess {
    /// Build a minimal child [`GoalProcess`] for a dispatcher-spawned sub-goal
    /// (T130 §05).
    ///
    /// The child inherits no per-field customisation from the parent — it's
    /// a fresh process record with:
    ///
    /// - `slug` and `id` = `slug` (caller mints a stable slug; the dispatcher
    ///   uses `sg-<parent>-<group>-<uuid>` to keep child slugs deterministic-ish)
    /// - `parent_goal_id` set to the parent's slug/id
    /// - `unblock_key` carried forward (later chunks pivot on this)
    /// - `phase` = [`GoalPhase::Implementation`] (stand-in for the design's
    ///   `AgentExecute` phase — no enum variant exists yet)
    /// - Empty streams / domains / task defaults — the dispatcher's backend
    ///   populates actual work via its own event channels
    ///
    /// The caller is responsible for inserting the result into a
    /// [`GoalProcessManager`] via [`GoalProcessManager::insert_child`].
    pub fn new_child(
        slug: &str,
        parent_goal_id: &str,
        unblock_key: UnblockKey,
        title: &str,
    ) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            id: slug.to_string(),
            slug: slug.to_string(),
            title: title.to_string(),
            state: crate::types::GoalState::Active,
            priority: 40,
            autonomy_level: crate::types::AutonomyLevel::Semi,
            phase: crate::types::GoalPhase::Implementation,
            process: crate::types::ProcessConfig {
                process_type: crate::types::ProcessType::OnDemand,
                check_frequency: crate::types::CheckFrequency::Daily,
                max_parallel_agents: 1,
            },
            streams: Vec::new(),
            domains: Vec::new(),
            vault_namespace: format!("goal-{}", slug),
            constraints: crate::types::GoalConstraints::default(),
            metrics: GoalMetrics::default(),
            created_at: now,
            last_check_at: now,
            next_check_at: now,
            parent_goal_id: Some(parent_goal_id.to_string()),
            unblock_key: Some(unblock_key),
            blocked_by_groups: Vec::new(),
            spawns_on_unblock: Vec::new(),
        }
    }
}

// ── State transition validation ─────────────────────────────────────────

/// Validate whether a state transition is allowed.
///
/// Rules:
/// - Active -> Paused, Achieved, Abandoned: allowed
/// - Paused -> Active, Abandoned: allowed
/// - Achieved -> any: terminal (not allowed)
/// - Abandoned -> any: terminal (not allowed)
fn validate_state_transition(from: &GoalState, to: &GoalState) -> Result<()> {
    if from == to {
        bail!("goal is already in state {:?}", from);
    }

    match from {
        GoalState::Active => match to {
            GoalState::Paused | GoalState::Achieved | GoalState::Abandoned => Ok(()),
            _ => bail!("invalid transition from Active to {:?}", to),
        },
        GoalState::Paused => match to {
            GoalState::Active | GoalState::Abandoned => Ok(()),
            _ => bail!(
                "invalid transition from Paused to {:?} (only Active or Abandoned allowed)",
                to
            ),
        },
        GoalState::Achieved => {
            bail!("cannot transition from Achieved (terminal state)")
        }
        GoalState::Abandoned => {
            bail!("cannot transition from Abandoned (terminal state)")
        }
    }
}

// ── GoalProcessManager ──────────────────────────────────────────────────

/// Manages the lifecycle of goal processes.
///
/// Goals are stored as `{store_path}/{slug}.json` on disk and kept in
/// memory as a lookup table keyed by slug. An `AgentPool` is used to
/// track worker slot allocation across active goals.
pub struct GoalProcessManager {
    store_path: PathBuf,
    goals: HashMap<String, GoalProcess>,
    pool: AgentPool,
    /// Maps goal slug -> allocated worker IDs for that goal.
    goal_workers: HashMap<String, Vec<WorkerId>>,
}

impl GoalProcessManager {
    /// Default maximum number of agent slots in the pool.
    const DEFAULT_MAX_SLOTS: usize = 16;

    /// Create a new manager with an empty goal list and a default-sized agent pool.
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            goals: HashMap::new(),
            pool: AgentPool::new(Self::DEFAULT_MAX_SLOTS),
            goal_workers: HashMap::new(),
        }
    }

    /// Create a new manager with a custom agent pool size.
    pub fn with_pool_size(store_path: PathBuf, max_slots: usize) -> Self {
        Self {
            store_path,
            goals: HashMap::new(),
            pool: AgentPool::new(max_slots),
            goal_workers: HashMap::new(),
        }
    }

    /// Access the underlying agent pool (read-only).
    pub fn pool(&self) -> &AgentPool {
        &self.pool
    }

    /// Load all `{store_path}/{slug}.json` files into memory.
    pub fn load(&mut self) -> Result<()> {
        if !self.store_path.exists() {
            return Ok(());
        }

        let entries = std::fs::read_dir(&self.store_path)
            .with_context(|| format!("reading goal store: {}", self.store_path.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let content = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading goal file: {}", path.display()))?;
                let goal: GoalProcess = serde_json::from_str(&content)
                    .with_context(|| format!("parsing goal file: {}", path.display()))?;
                self.goals.insert(goal.slug.clone(), goal);
            }
        }

        Ok(())
    }

    /// Create a new goal process from a manifest.
    ///
    /// Sets `created_at` to the current time and computes `next_check_at`
    /// from the process check frequency. Allocates an initial worker slot
    /// from the agent pool if capacity is available (best-effort — goal
    /// creation succeeds even if the pool is saturated).
    pub fn create_goal(&mut self, manifest: &GoalManifest) -> Result<&GoalProcess> {
        if self.goals.contains_key(&manifest.slug) {
            bail!("goal '{}' already exists", manifest.slug);
        }

        let now = chrono::Utc::now().timestamp();
        let check_interval = manifest.process.check_frequency.to_secs() as i64;

        let process = GoalProcess {
            id: manifest.id.clone(),
            slug: manifest.slug.clone(),
            title: manifest.title.clone(),
            state: manifest.state.clone(),
            priority: manifest.priority,
            autonomy_level: manifest.autonomy_level.clone(),
            phase: manifest.phase.clone(),
            process: manifest.process.clone(),
            streams: manifest.streams.clone(),
            domains: manifest.domains.clone(),
            vault_namespace: manifest.vault_namespace.clone(),
            constraints: manifest.constraints.clone(),
            metrics: GoalMetrics::default(),
            created_at: now,
            last_check_at: now,
            next_check_at: now + check_interval,
            // GoalDAG extensions default to empty; `create_goal_with_dag`
            // and direct field writes are the supported mutation paths.
            parent_goal_id: None,
            unblock_key: None,
            blocked_by_groups: Vec::new(),
            spawns_on_unblock: Vec::new(),
        };

        self.goals.insert(manifest.slug.clone(), process);
        self.save_goal(&manifest.slug)?;

        // Best-effort: allocate an initial worker slot for this goal.
        if manifest.state == GoalState::Active {
            if let Ok(worker_id) = self.pool.allocate(PoolLlmType::Cloud) {
                self.goal_workers
                    .entry(manifest.slug.clone())
                    .or_default()
                    .push(worker_id);
            }
        }

        Ok(self.goals.get(&manifest.slug).expect("just inserted"))
    }

    /// Look up a goal by slug.
    pub fn get(&self, slug: &str) -> Option<&GoalProcess> {
        self.goals.get(slug)
    }

    /// Insert a pre-built child [`GoalProcess`] (typically from
    /// [`GoalProcess::new_child`]) into the manager. Used by the Sub-Goal
    /// Dispatcher (T130 §05) to persist dispatcher-spawned sub-goals.
    ///
    /// Errors if the slug already exists — the caller is expected to mint
    /// unique slugs (the dispatcher appends a UUID suffix).
    pub fn insert_child(&mut self, child: GoalProcess) -> Result<&GoalProcess> {
        let slug = child.slug.clone();
        if self.goals.contains_key(&slug) {
            bail!("goal '{}' already exists", slug);
        }
        self.goals.insert(slug.clone(), child);
        self.save_goal(&slug)?;
        Ok(self.goals.get(&slug).expect("just inserted"))
    }

    /// Return all active goals whose `next_check_at` is at or before `now_ts`.
    pub fn get_due_goals(&self, now_ts: i64) -> Vec<&GoalProcess> {
        self.goals
            .values()
            .filter(|g| g.state == GoalState::Active && g.next_check_at <= now_ts)
            .collect()
    }

    /// Transition a goal to a new state, enforcing the state machine rules.
    ///
    /// When transitioning to a terminal state (Achieved, Abandoned) or Paused,
    /// all worker slots for this goal are released back to the pool.
    pub fn transition(&mut self, slug: &str, new_state: GoalState) -> Result<()> {
        let goal = self
            .goals
            .get(slug)
            .ok_or_else(|| anyhow::anyhow!("goal '{}' not found", slug))?;

        validate_state_transition(&goal.state, &new_state)?;

        // Release workers for terminal or paused states.
        let should_release = matches!(
            new_state,
            GoalState::Achieved | GoalState::Abandoned | GoalState::Paused
        );
        if should_release {
            if let Some(workers) = self.goal_workers.remove(slug) {
                for worker_id in workers {
                    let _ = self.pool.release(worker_id);
                }
            }
        }

        let goal = self.goals.get_mut(slug).expect("checked above");
        goal.state = new_state;
        Ok(())
    }

    /// Advance a goal to a new execution phase.
    pub fn advance_phase(&mut self, slug: &str, new_phase: GoalPhase) -> Result<()> {
        let goal = self
            .goals
            .get_mut(slug)
            .ok_or_else(|| anyhow::anyhow!("goal '{}' not found", slug))?;

        if goal.phase == new_phase {
            bail!("goal '{}' is already in phase {:?}", slug, new_phase);
        }

        goal.phase = new_phase;
        Ok(())
    }

    /// Allocate agent slots across active goals, weighted by inverse priority.
    ///
    /// Priority 1 receives more slots than priority 5. Only active goals
    /// participate in allocation. Returns a vec of `(slug, slot_count)`.
    pub fn allocate_agent_slots(&self, total_slots: usize) -> Vec<(String, usize)> {
        let active: Vec<&GoalProcess> = self
            .goals
            .values()
            .filter(|g| g.state == GoalState::Active)
            .collect();

        if active.is_empty() || total_slots == 0 {
            return Vec::new();
        }

        // Compute weights as inverse of priority (priority 1 -> weight 10, priority 10 -> weight 1).
        // Use max_priority + 1 - priority to ensure higher priority (lower number) gets more weight.
        let max_priority = active.iter().map(|g| g.priority as u32).max().unwrap_or(1);
        let weights: Vec<(String, u32)> = active
            .iter()
            .map(|g| {
                let weight = max_priority + 1 - g.priority as u32;
                (g.slug.clone(), weight)
            })
            .collect();

        let total_weight: u32 = weights.iter().map(|(_, w)| *w).sum();
        if total_weight == 0 {
            return Vec::new();
        }

        let mut result: Vec<(String, usize)> = Vec::new();
        let mut remaining = total_slots;

        for (i, (slug, weight)) in weights.iter().enumerate() {
            let slots = if i == weights.len() - 1 {
                // Give the remainder to the last goal to avoid rounding loss.
                remaining
            } else {
                let proportional =
                    (total_slots as f64 * (*weight as f64 / total_weight as f64)).floor() as usize;
                let slots = proportional.min(remaining);
                remaining = remaining.saturating_sub(slots);
                slots
            };

            if slots > 0 {
                result.push((slug.clone(), slots));
            }
        }

        result
    }

    /// Increment metrics for a goal.
    pub fn update_metrics(
        &mut self,
        slug: &str,
        completed: u32,
        failed: u32,
        cost: f64,
    ) -> Result<()> {
        let goal = self
            .goals
            .get_mut(slug)
            .ok_or_else(|| anyhow::anyhow!("goal '{}' not found", slug))?;

        goal.metrics.tasks_completed += completed;
        goal.metrics.tasks_failed += failed;
        goal.metrics.total_cost_usd += cost;
        Ok(())
    }

    /// Persist a single goal to `{store_path}/{slug}.json`.
    pub fn save_goal(&self, slug: &str) -> Result<()> {
        let goal = self
            .goals
            .get(slug)
            .ok_or_else(|| anyhow::anyhow!("goal '{}' not found", slug))?;

        std::fs::create_dir_all(&self.store_path)
            .with_context(|| format!("creating store dir: {}", self.store_path.display()))?;

        let path = self.store_path.join(format!("{}.json", slug));
        let json = serde_json::to_string_pretty(goal)
            .with_context(|| format!("serializing goal '{}'", slug))?;

        std::fs::write(&path, json)
            .with_context(|| format!("writing goal file: {}", path.display()))?;

        Ok(())
    }

    /// Iterate over all managed goals.
    pub fn all_goals(&self) -> impl Iterator<Item = &GoalProcess> {
        self.goals.values()
    }

    /// Remove a goal from the in-memory manager (does not delete from disk).
    ///
    /// Releases all worker slots allocated to this goal back to the pool.
    pub fn remove(&mut self, slug: &str) -> Result<()> {
        self.goals
            .remove(slug)
            .ok_or_else(|| anyhow::anyhow!("goal '{}' not found", slug))?;

        // Release all worker slots for this goal.
        if let Some(workers) = self.goal_workers.remove(slug) {
            for worker_id in workers {
                // Ignore errors — worker may have already been released.
                let _ = self.pool.release(worker_id);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CheckFrequency, GoalTaskPolicyDefaults, ProcessType};
    use tempfile::TempDir;

    /// Helper: build a GoalManifest for testing.
    fn test_manifest(slug: &str, priority: u8) -> GoalManifest {
        GoalManifest {
            id: format!("id-{}", slug),
            project_id: "project:test".to_string(),
            slug: slug.to_string(),
            title: format!("Test Goal: {}", slug),
            state: GoalState::Active,
            priority,
            autonomy_level: AutonomyLevel::Semi,
            phase: GoalPhase::Research,
            process: ProcessConfig {
                process_type: ProcessType::Periodic,
                check_frequency: CheckFrequency::Daily,
                max_parallel_agents: 2,
            },
            streams: vec![StreamConfig {
                name: "main".to_string(),
                domain: "engineering".to_string(),
                focus: "core work".to_string(),
                autonomy: AutonomyLevel::Semi,
            }],
            domains: vec!["engineering".to_string()],
            vault_namespace: format!("goal-{}", slug),
            thread_id: None,
            plan_version: 1,
            policy_scopes: Vec::new(),
            task_policy_defaults: GoalTaskPolicyDefaults::default(),
            constraints: GoalConstraints {
                budget_usd: Some(100.0),
                time_horizon_days: Some(30),
                risk_tolerance: None,
            },
            plan_markdown: String::new(),
            tasks: Vec::new(),
        }
    }

    #[test]
    fn create_goal_from_manifest() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());

        let manifest = test_manifest("alpha", 2);
        let goal = mgr.create_goal(&manifest).unwrap();

        assert_eq!(goal.slug, "alpha");
        assert_eq!(goal.title, "Test Goal: alpha");
        assert_eq!(goal.state, GoalState::Active);
        assert_eq!(goal.priority, 2);
        assert_eq!(goal.autonomy_level, AutonomyLevel::Semi);
        assert_eq!(goal.phase, GoalPhase::Research);
        assert_eq!(goal.domains, vec!["engineering"]);
        assert_eq!(goal.vault_namespace, "goal-alpha");
        assert_eq!(goal.streams.len(), 1);
        assert_eq!(goal.constraints.budget_usd, Some(100.0));
        assert_eq!(goal.metrics.tasks_completed, 0);
        assert!(goal.created_at > 0);
        assert!(goal.next_check_at > goal.created_at);
    }

    #[test]
    fn load_save_round_trip() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("goals");

        // Create and save a goal.
        {
            let mut mgr = GoalProcessManager::new(store.clone());
            let manifest = test_manifest("beta", 1);
            mgr.create_goal(&manifest).unwrap();
        }

        // Load in a fresh manager and verify.
        {
            let mut mgr = GoalProcessManager::new(store);
            mgr.load().unwrap();
            let goal = mgr.get("beta").expect("goal should be loaded");
            assert_eq!(goal.slug, "beta");
            assert_eq!(goal.title, "Test Goal: beta");
            assert_eq!(goal.state, GoalState::Active);
            assert_eq!(goal.priority, 1);
        }
    }

    #[test]
    fn transition_active_to_paused() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("gamma", 1)).unwrap();

        mgr.transition("gamma", GoalState::Paused).unwrap();
        assert_eq!(mgr.get("gamma").unwrap().state, GoalState::Paused);
    }

    #[test]
    fn transition_paused_to_active() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("delta", 1)).unwrap();

        mgr.transition("delta", GoalState::Paused).unwrap();
        mgr.transition("delta", GoalState::Active).unwrap();
        assert_eq!(mgr.get("delta").unwrap().state, GoalState::Active);
    }

    #[test]
    fn transition_active_to_achieved() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("epsilon", 1)).unwrap();

        mgr.transition("epsilon", GoalState::Achieved).unwrap();
        assert_eq!(mgr.get("epsilon").unwrap().state, GoalState::Achieved);
    }

    #[test]
    fn transition_active_to_abandoned() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("zeta", 1)).unwrap();

        mgr.transition("zeta", GoalState::Abandoned).unwrap();
        assert_eq!(mgr.get("zeta").unwrap().state, GoalState::Abandoned);
    }

    #[test]
    fn transition_achieved_is_terminal() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("eta", 1)).unwrap();

        mgr.transition("eta", GoalState::Achieved).unwrap();
        let result = mgr.transition("eta", GoalState::Active);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("terminal"));
    }

    #[test]
    fn transition_abandoned_is_terminal() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("theta", 1)).unwrap();

        mgr.transition("theta", GoalState::Abandoned).unwrap();
        let result = mgr.transition("theta", GoalState::Paused);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("terminal"));
    }

    #[test]
    fn get_due_goals_filters_correctly() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());

        mgr.create_goal(&test_manifest("due-1", 1)).unwrap();
        mgr.create_goal(&test_manifest("due-2", 2)).unwrap();
        mgr.create_goal(&test_manifest("not-due", 3)).unwrap();

        // Force due-1 and due-2 to be past their check time.
        let far_future = chrono::Utc::now().timestamp() + 999_999;
        mgr.goals.get_mut("due-1").unwrap().next_check_at = 100;
        mgr.goals.get_mut("due-2").unwrap().next_check_at = 200;
        mgr.goals.get_mut("not-due").unwrap().next_check_at = far_future;

        let due = mgr.get_due_goals(300);
        let slugs: Vec<&str> = due.iter().map(|g| g.slug.as_str()).collect();
        assert!(slugs.contains(&"due-1"));
        assert!(slugs.contains(&"due-2"));
        assert!(!slugs.contains(&"not-due"));
    }

    #[test]
    fn get_due_goals_excludes_non_active() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());

        mgr.create_goal(&test_manifest("active-goal", 1)).unwrap();
        mgr.create_goal(&test_manifest("paused-goal", 1)).unwrap();
        mgr.create_goal(&test_manifest("achieved-goal", 1)).unwrap();

        // Make all goals past their check time.
        for goal in mgr.goals.values_mut() {
            goal.next_check_at = 100;
        }

        mgr.transition("paused-goal", GoalState::Paused).unwrap();
        mgr.transition("achieved-goal", GoalState::Achieved)
            .unwrap();

        let due = mgr.get_due_goals(999);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].slug, "active-goal");
    }

    #[test]
    fn allocate_agent_slots_weighted_by_priority() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());

        // Priority 1 should get more slots than priority 5.
        mgr.create_goal(&test_manifest("high", 1)).unwrap();
        mgr.create_goal(&test_manifest("low", 5)).unwrap();

        let alloc = mgr.allocate_agent_slots(10);
        let high_slots = alloc.iter().find(|(s, _)| s == "high").map(|(_, n)| *n);
        let low_slots = alloc.iter().find(|(s, _)| s == "low").map(|(_, n)| *n);

        assert!(high_slots.is_some());
        assert!(low_slots.is_some());
        assert!(
            high_slots.unwrap() > low_slots.unwrap(),
            "priority-1 goal should get more slots than priority-5 goal"
        );

        let total: usize = alloc.iter().map(|(_, n)| n).sum();
        assert_eq!(total, 10, "all slots must be allocated");
    }

    #[test]
    fn update_metrics_increments() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("iota", 1)).unwrap();

        mgr.update_metrics("iota", 5, 2, 10.50).unwrap();
        mgr.update_metrics("iota", 3, 1, 5.25).unwrap();

        let goal = mgr.get("iota").unwrap();
        assert_eq!(goal.metrics.tasks_completed, 8);
        assert_eq!(goal.metrics.tasks_failed, 3);
        assert!((goal.metrics.total_cost_usd - 15.75).abs() < f64::EPSILON);
    }

    #[test]
    fn advance_phase() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("kappa", 1)).unwrap();

        assert_eq!(mgr.get("kappa").unwrap().phase, GoalPhase::Research);

        mgr.advance_phase("kappa", GoalPhase::Implementation)
            .unwrap();
        assert_eq!(mgr.get("kappa").unwrap().phase, GoalPhase::Implementation);

        // Same phase should error.
        let result = mgr.advance_phase("kappa", GoalPhase::Implementation);
        assert!(result.is_err());
    }

    #[test]
    fn remove_goal() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        mgr.create_goal(&test_manifest("lambda", 1)).unwrap();

        assert!(mgr.get("lambda").is_some());
        mgr.remove("lambda").unwrap();
        assert!(mgr.get("lambda").is_none());

        // Removing again should error.
        let result = mgr.remove("lambda");
        assert!(result.is_err());
    }

    #[test]
    fn load_from_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("nonexistent");
        let mut mgr = GoalProcessManager::new(store);

        // Should not error on missing directory.
        mgr.load().unwrap();
        assert_eq!(mgr.all_goals().count(), 0);
    }

    #[test]
    fn allocate_slots_no_active_goals() {
        let tmp = TempDir::new().unwrap();
        let mgr = GoalProcessManager::new(tmp.path().to_path_buf());

        let alloc = mgr.allocate_agent_slots(10);
        assert!(alloc.is_empty());
    }

    #[test]
    fn allocate_slots_excludes_paused() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());

        mgr.create_goal(&test_manifest("active-a", 1)).unwrap();
        mgr.create_goal(&test_manifest("paused-b", 1)).unwrap();
        mgr.transition("paused-b", GoalState::Paused).unwrap();

        let alloc = mgr.allocate_agent_slots(5);
        assert_eq!(alloc.len(), 1);
        assert_eq!(alloc[0].0, "active-a");
        assert_eq!(alloc[0].1, 5);
    }

    // ── GoalDAG extension fields (T130 §04) ──────────────────────────────

    #[test]
    fn new_goal_defaults_dag_fields_empty() {
        let tmp = TempDir::new().unwrap();
        let mut mgr = GoalProcessManager::new(tmp.path().to_path_buf());
        let goal = mgr.create_goal(&test_manifest("dag-default", 1)).unwrap();

        assert!(goal.parent_goal_id.is_none());
        assert!(goal.unblock_key.is_none());
        assert!(goal.blocked_by_groups.is_empty());
        assert!(goal.spawns_on_unblock.is_empty());
    }

    #[test]
    fn dag_fields_round_trip_on_save_load() {
        use symbiotic_core::types::question_group::{PlannedSpawn, UnblockKey};

        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("goals");

        // Create a goal, mutate its DAG fields, and persist.
        {
            let mut mgr = GoalProcessManager::new(store.clone());
            mgr.create_goal(&test_manifest("dag-child", 2)).unwrap();
            {
                let g = mgr.goals.get_mut("dag-child").unwrap();
                g.parent_goal_id = Some("goal-parent".to_string());
                g.unblock_key = Some(UnblockKey::Exploratory {
                    topic: "frontend".to_string(),
                });
                g.blocked_by_groups = vec!["design-phase".to_string()];
                g.spawns_on_unblock = vec![PlannedSpawn {
                    unblock_key: UnblockKey::ResearchOnly {
                        question: "which crate?".to_string(),
                    },
                    when_group_resolved: "design-phase".to_string(),
                    initial_prompt: "survey oauth crates".to_string(),
                }];
            }
            mgr.save_goal("dag-child").unwrap();
        }

        // Reload in a fresh manager.
        let mut mgr = GoalProcessManager::new(store);
        mgr.load().unwrap();
        let goal = mgr.get("dag-child").expect("goal should reload");
        assert_eq!(goal.parent_goal_id.as_deref(), Some("goal-parent"));
        assert_eq!(goal.blocked_by_groups, vec!["design-phase".to_string()]);
        assert_eq!(goal.spawns_on_unblock.len(), 1);
        assert_eq!(
            goal.spawns_on_unblock[0].when_group_resolved,
            "design-phase"
        );
        assert!(matches!(
            goal.unblock_key,
            Some(UnblockKey::Exploratory { .. })
        ));
    }

    #[test]
    fn pre_migration_goal_file_loads_with_dag_defaults() {
        // Simulate a stored `GoalProcess` written before §04 (no DAG fields).
        // `#[serde(default)]` on every new field must make this parse cleanly.
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("goals");
        std::fs::create_dir_all(&store).unwrap();

        // Take a snapshot of the current canonical schema by creating a goal
        // via the manager, reading the JSON, and then stripping the DAG
        // fields to simulate a pre-§04 payload. This keeps the fixture in
        // sync with field-name changes on unrelated nested types.
        {
            let mut mgr = GoalProcessManager::new(store.clone());
            mgr.create_goal(&test_manifest("legacy", 1)).unwrap();
            mgr.save_goal("legacy").unwrap();
        }
        let raw = std::fs::read_to_string(store.join("legacy.json")).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Strip the DAG extension fields to prove defaults kick in.
        if let Some(obj) = value.as_object_mut() {
            obj.remove("parent_goal_id");
            obj.remove("unblock_key");
            obj.remove("blocked_by_groups");
            obj.remove("spawns_on_unblock");
        }
        let legacy_json = serde_json::to_string_pretty(&value).unwrap();

        std::fs::write(store.join("legacy.json"), legacy_json).unwrap();

        let mut mgr = GoalProcessManager::new(store);
        mgr.load().expect("pre-migration file must load cleanly");

        let goal = mgr.get("legacy").expect("legacy goal loaded");
        // New fields defaulted:
        assert!(goal.parent_goal_id.is_none());
        assert!(goal.unblock_key.is_none());
        assert!(goal.blocked_by_groups.is_empty());
        assert!(goal.spawns_on_unblock.is_empty());
        // Old fields preserved:
        assert_eq!(goal.slug, "legacy");
        assert_eq!(goal.priority, 1);
    }

    // ── GoalProcess::new_child + insert_child helpers (T130 §05) ─────────

    #[test]
    fn new_child_carries_parent_linkage_and_unblock_key() {
        use symbiotic_core::types::question_group::UnblockKey;

        let key = UnblockKey::ResearchOnly {
            question: "which crate?".into(),
        };
        let child = GoalProcess::new_child("sg-parent-grp-1-deadbeef", "parent", key, "Research");
        assert_eq!(child.slug, "sg-parent-grp-1-deadbeef");
        assert_eq!(child.id, "sg-parent-grp-1-deadbeef");
        assert_eq!(child.parent_goal_id.as_deref(), Some("parent"));
        assert!(matches!(
            child.unblock_key,
            Some(UnblockKey::ResearchOnly { .. })
        ));
        assert_eq!(child.phase, GoalPhase::Implementation);
        assert!(child.blocked_by_groups.is_empty());
        assert!(child.spawns_on_unblock.is_empty());
        assert_eq!(child.state, GoalState::Active);
        assert_eq!(child.title, "Research");
        assert_eq!(child.vault_namespace, "goal-sg-parent-grp-1-deadbeef");
    }

    #[test]
    fn insert_child_persists_and_rejects_duplicate() {
        use symbiotic_core::types::question_group::UnblockKey;

        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("goals");
        let mut mgr = GoalProcessManager::new(store.clone());

        let key = UnblockKey::ResearchOnly {
            question: "q".into(),
        };
        let child = GoalProcess::new_child("sg-a-deadbeef", "parent-a", key.clone(), "t");
        mgr.insert_child(child).expect("first insert ok");

        // Round-trip check
        let path = store.join("sg-a-deadbeef.json");
        assert!(path.exists(), "child goal must be persisted to disk");

        // Duplicate should fail.
        let dup = GoalProcess::new_child("sg-a-deadbeef", "parent-a", key, "t");
        assert!(mgr.insert_child(dup).is_err());
    }

    #[test]
    fn dag_defaults_omitted_in_serialized_json() {
        // Goals with empty DAG fields should not bloat the on-disk JSON.
        let tmp = TempDir::new().unwrap();
        let store = tmp.path().join("goals");

        let mut mgr = GoalProcessManager::new(store.clone());
        mgr.create_goal(&test_manifest("slim", 1)).unwrap();
        mgr.save_goal("slim").unwrap();

        let raw = std::fs::read_to_string(store.join("slim.json")).unwrap();
        assert!(
            !raw.contains("parent_goal_id"),
            "empty parent_goal_id must be omitted"
        );
        assert!(
            !raw.contains("unblock_key"),
            "empty unblock_key must be omitted"
        );
        assert!(
            !raw.contains("blocked_by_groups"),
            "empty blocked_by_groups must be omitted"
        );
        assert!(
            !raw.contains("spawns_on_unblock"),
            "empty spawns_on_unblock must be omitted"
        );
    }
}
