//! Core types for the Declarative Cognitive Control Plane.
//!
//! These types represent the desired state (parsed from Markdown manifests)
//! and the actual state (queried from the daemon runtime). The reconciler
//! diffs these to produce reconciliation actions.

use serde::{Deserialize, Serialize};

use crate::repo_manifest::RepoManifest;

// ── Project + Process Manifests (parsed from YAML frontmatter) ─────────

/// A project manifest parsed from `operations/projects/{project}/project.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub state: ProjectState,
    #[serde(default)]
    pub owner_hint: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub policy_scopes: Vec<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub domains: Vec<String>,
    /// The Markdown body (human-readable project notes). Not part of YAML frontmatter.
    #[serde(skip)]
    pub project_markdown: String,
    /// Nested goal manifests loaded from `operations/projects/{project}/goals/*/plan.md`.
    #[serde(skip)]
    pub goals: Vec<GoalManifest>,
    /// Nested process manifests loaded from `operations/projects/{project}/processes/*.md`.
    #[serde(skip)]
    pub processes: Vec<ProcessManifest>,
    /// Nested repo manifests resolved from `operations/projects/{project}/repos/*.md`.
    #[serde(skip)]
    pub resolved_repos: Vec<RepoManifest>,
}

/// A process manifest parsed from `operations/projects/{project}/processes/{process}.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessManifest {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub state: ProcessState,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub owner_hint: Option<String>,
    pub cadence: ProcessCadence,
    pub generator: ProcessGeneratorConfig,
    #[serde(default)]
    pub task_template: Option<ProcessTaskTemplate>,
    /// The Markdown body (human-readable process notes). Not part of YAML frontmatter.
    #[serde(skip)]
    pub process_markdown: String,
}

// ── Goal Manifest (parsed from YAML frontmatter) ───────────────────────

/// A goal manifest parsed from `operations/projects/{project}/goals/{goal}/plan.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalManifest {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub title: String,
    pub state: GoalState,
    pub priority: u8,
    pub autonomy_level: AutonomyLevel,
    pub phase: GoalPhase,
    pub process: ProcessConfig,
    #[serde(default)]
    pub streams: Vec<StreamConfig>,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub vault_namespace: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default = "default_goal_plan_version")]
    pub plan_version: u32,
    #[serde(default)]
    pub policy_scopes: Vec<String>,
    #[serde(default)]
    pub task_policy_defaults: GoalTaskPolicyDefaults,
    #[serde(default)]
    pub constraints: GoalConstraints,
    /// The Markdown body (human-readable plan). Not part of YAML frontmatter.
    #[serde(skip)]
    pub plan_markdown: String,
    /// Canonical child task records loaded from
    /// `operations/projects/{project}/goals/{goal}/tasks/*.md`.
    #[serde(skip)]
    pub tasks: Vec<GoalTaskManifest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectState {
    Active,
    Paused,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Active,
    Paused,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessCadence {
    pub kind: ProcessCadenceKind,
    #[serde(default)]
    pub weekday: Option<GoalTaskWeekday>,
    #[serde(default)]
    pub local_time: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessCadenceKind {
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessGeneratorConfig {
    pub mode: ProcessGeneratorMode,
    #[serde(default)]
    pub target_goal_id: Option<String>,
    /// Prefix for goal ids that this generator produces. Used by
    /// `OneShotBootstrap` mode to name onboarding goals consistently
    /// (e.g. `"onboard-"` → `onboard-flux`, `onboard-internal-tools`).
    /// Other generator modes may also use this prefix; `None` falls
    /// back to a mode-specific default at the Process runtime layer.
    #[serde(default)]
    pub produces_goal_prefix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessGeneratorMode {
    RecurringTasks,
    GoalTemplate,
    ReviewOnly,
    /// Extension per `docs/design/project-bootstrap-process.md`.
    /// A Process using this mode runs a one-shot project onboarding:
    /// Discovery → Doc Reconciliation → Vault Ingestion → Ready-State Handshake.
    /// Produces goals named `{produces_goal_prefix}{slug}` scoped to the
    /// bootstrapping project (typically `project:symbiotic`).
    OneShotBootstrap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessTaskTemplate {
    pub task_kind: GoalTaskKind,
    pub task_driver: GoalTaskDriver,
    pub title: String,
    #[serde(default)]
    pub policy: GoalTaskPolicy,
}

/// A planned task manifest parsed from
/// `operations/projects/{project}/goals/{goal}/tasks/*.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalTaskManifest {
    pub id: String,
    pub goal_id: String,
    pub task_id: String,
    pub task_slug: String,
    pub task_kind: GoalTaskKind,
    pub task_driver: GoalTaskDriver,
    pub title: String,
    #[serde(default)]
    pub state: GoalTaskPlanState,
    #[serde(default, alias = "status")]
    pub execution_status: GoalTaskStatus,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub questionnaire_context: Vec<String>,
    #[serde(default)]
    pub owner_hint: Option<String>,
    #[serde(default)]
    pub declared_context: GoalTaskDeclaredContext,
    #[serde(default)]
    pub policy: GoalTaskPolicy,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub reopen_count: u32,
    #[serde(default)]
    pub last_status_change_at: Option<i64>,
    #[serde(default = "default_goal_plan_version")]
    pub plan_version: u32,
    #[serde(default)]
    pub superseded_by: Vec<String>,
    #[serde(default)]
    pub derived_from: Vec<String>,
    #[serde(default)]
    pub replaces: Vec<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub source_step_id: Option<String>,
    /// Human-readable task body. Not part of YAML frontmatter.
    #[serde(skip)]
    pub task_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalEventManifest {
    pub goal_id: String,
    pub event_type: String,
    pub observed_at: i64,
    pub plan_version: u32,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub previous_status: Option<String>,
    #[serde(default)]
    pub next_status: Option<String>,
    #[serde(default)]
    pub previous_owner: Option<String>,
    #[serde(default)]
    pub next_owner: Option<String>,
    #[serde(default)]
    pub escalation_policy: Option<String>,
    #[serde(default)]
    pub escalation_trigger: Option<String>,
    #[serde(default)]
    pub escalation_audience: Option<String>,
    #[serde(default)]
    pub escalation_severity: Option<String>,
    #[serde(default)]
    pub escalation_count: Option<u32>,
    #[serde(default)]
    pub cooldown_until: Option<i64>,
    #[serde(default)]
    pub condition_kind: Option<String>,
    #[serde(default)]
    pub condition_value: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub added_task_ids: Vec<String>,
    #[serde(default)]
    pub preserved_task_ids: Vec<String>,
    #[serde(default)]
    pub deactivated_task_ids: Vec<String>,
    #[serde(default)]
    pub supersession_edges: Vec<String>,
    #[serde(default)]
    pub owner_change_edges: Vec<String>,
    #[serde(skip)]
    pub event_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyScopeManifest {
    pub id: String,
    pub kind: PolicyScopeKind,
    pub title: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: u16,
    #[serde(default)]
    pub delivery_subject: Option<String>,
    #[serde(default)]
    pub task_policy_defaults: GoalTaskPolicyDefaults,
    #[serde(skip)]
    pub body_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailabilityRuleManifest {
    pub id: String,
    pub subject: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub working_hours: Option<GoalTaskWorkingHoursWindow>,
    #[serde(default)]
    pub quiet_hours: Option<GoalTaskQuietHoursWindow>,
    #[serde(skip)]
    pub body_markdown: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyScopeKind {
    CompanyDefault,
    Team,
    OnCall,
}

fn default_goal_plan_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskKind {
    #[default]
    Execution,
    Review,
    Distillation,
    Coordination,
    Waiting,
    Approval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskDriver {
    #[default]
    Agent,
    Declared,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalTaskDeclaredContext {
    #[serde(default)]
    pub review_target: Option<String>,
    #[serde(default)]
    pub waiting_for: Option<String>,
    #[serde(default)]
    pub coordination_target: Option<String>,
    #[serde(default)]
    pub external_dependency: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalTaskPolicy {
    #[serde(default)]
    pub escalation: Option<GoalTaskEscalationConfig>,
    #[serde(default)]
    pub timing: Option<GoalTaskTimingConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalTaskPolicyDefaults {
    #[serde(default)]
    pub evaluator: Option<GoalTaskPolicyEvaluatorDefaults>,
    #[serde(default)]
    pub waiting: Option<GoalTaskEscalationDefaults>,
    #[serde(default)]
    pub approval: Option<GoalTaskEscalationDefaults>,
    #[serde(default)]
    pub coordination: Option<GoalTaskEscalationDefaults>,
    #[serde(default)]
    pub review: Option<GoalTaskEscalationDefaults>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalTaskEscalationDefaults {
    #[serde(default)]
    pub mode: Option<GoalTaskEscalationPolicy>,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub severity: Option<GoalTaskEscalationSeverity>,
    #[serde(default)]
    pub on_enter_blocked: Option<bool>,
    #[serde(default)]
    pub after_secs: Option<u64>,
    #[serde(default)]
    pub max_count: Option<u32>,
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreferencesTaskPolicyDefaults {
    #[serde(default)]
    pub evaluator: GoalTaskPolicyEvaluatorDefaults,
    #[serde(default)]
    pub declared_task_defaults: GoalTaskPolicyDefaults,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalTaskPolicyEvaluatorDefaults {
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub max_actions_per_tick: Option<usize>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub lateness_basis: Option<GoalTaskLatenessBasis>,
    #[serde(default)]
    pub delivery_window: Option<GoalTaskDeliveryWindowConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalTaskEscalationConfig {
    pub mode: GoalTaskEscalationPolicy,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub severity: Option<GoalTaskEscalationSeverity>,
    #[serde(default)]
    pub on_enter_blocked: bool,
    #[serde(default)]
    pub after_secs: Option<u64>,
    #[serde(default)]
    pub max_count: Option<u32>,
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalTaskTimingConfig {
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub lateness_basis: Option<GoalTaskLatenessBasis>,
    #[serde(default)]
    pub delivery_window: Option<GoalTaskDeliveryWindowConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskEscalationPolicy {
    NotifyOperator,
    RaiseAlert,
    AutoReplan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskEscalationSeverity {
    Normal,
    High,
    Urgent,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskLatenessBasis {
    WallClock,
    DeliveryWindowElapsed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalTaskDeliveryWindowConfig {
    pub mode: GoalTaskDeliveryWindowMode,
    #[serde(default)]
    pub quiet_hours: Option<GoalTaskQuietHoursWindow>,
    #[serde(default)]
    pub working_hours: Option<GoalTaskWorkingHoursWindow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskDeliveryWindowMode {
    Anytime,
    OutsideQuietHours,
    WorkingHours,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalTaskQuietHoursWindow {
    pub start_local: String,
    pub end_local: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalTaskWorkingHoursWindow {
    #[serde(default)]
    pub weekdays: Vec<GoalTaskWeekday>,
    pub start_local: String,
    pub end_local: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskWeekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Default for GoalTaskEscalationConfig {
    fn default() -> Self {
        Self {
            mode: GoalTaskEscalationPolicy::NotifyOperator,
            audience: None,
            severity: None,
            on_enter_blocked: true,
            after_secs: None,
            max_count: None,
            cooldown_secs: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskPlanState {
    #[default]
    Active,
    Cancelled,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalTaskStatus {
    #[default]
    Planned,
    InProgress,
    Blocked,
    Done,
    Cancelled,
}

/// Goal lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Active,
    Paused,
    Achieved,
    Abandoned,
}

/// Goal execution phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    Inquisition,
    Research,
    Provisioning,
    Implementation,
    Maintenance,
}

/// Operator autonomy level for a goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    /// Operator must approve every action.
    Manual,
    /// Auto-approve routine work, escalate significant actions.
    Semi,
    /// Fully autonomous within declared constraints.
    Auto,
}

/// How the goal process runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessConfig {
    #[serde(rename = "type")]
    pub process_type: ProcessType,
    #[serde(default = "default_check_frequency")]
    pub check_frequency: CheckFrequency,
    #[serde(default = "default_max_parallel_agents")]
    pub max_parallel_agents: usize,
}

fn default_check_frequency() -> CheckFrequency {
    CheckFrequency::Daily
}

fn default_max_parallel_agents() -> usize {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessType {
    /// Runs continuously until achieved/abandoned.
    Persistent,
    /// Runs on a schedule (check_frequency).
    Periodic,
    /// Runs only when explicitly triggered.
    OnDemand,
}

/// How often the goal process checks in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckFrequency {
    Hourly,
    Daily,
    Weekly,
    Monthly,
}

impl CheckFrequency {
    /// Convert to seconds for scheduling.
    pub fn to_secs(&self) -> u64 {
        match self {
            Self::Hourly => 3_600,
            Self::Daily => 86_400,
            Self::Weekly => 604_800,
            Self::Monthly => 2_592_000,
        }
    }
}

/// A named work stream within a goal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub name: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub focus: String,
    #[serde(default = "default_stream_autonomy")]
    pub autonomy: AutonomyLevel,
}

fn default_stream_autonomy() -> AutonomyLevel {
    AutonomyLevel::Semi
}

/// Resource and risk constraints for a goal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalConstraints {
    pub budget_usd: Option<f64>,
    pub time_horizon_days: Option<u64>,
    pub risk_tolerance: Option<RiskTolerance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTolerance {
    Low,
    Medium,
    High,
}

// ── Identity Manifest (parsed from identity/SOUL.md) ───────────────────

/// Parsed identity manifest from `identity/SOUL.md`.
#[derive(Debug, Clone)]
pub struct IdentityManifest {
    /// Frontmatter version.
    pub version: u32,
    /// Last update timestamp from frontmatter.
    pub updated_at: String,
    /// The full Markdown content (body after frontmatter).
    pub content: String,
    /// SHA-256 hash of the full file content for change detection.
    pub content_hash: String,
}

/// YAML frontmatter for identity files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IdentityFrontmatter {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub updated_at: String,
}

fn default_version() -> u32 {
    1
}

// ── Preferences Manifest (parsed from identity/preferences.md) ─────────

/// Parsed preferences manifest from `identity/preferences.md`.
#[derive(Debug, Clone)]
pub struct PreferencesManifest {
    /// Frontmatter version.
    pub version: u32,
    /// Last update timestamp from frontmatter.
    pub updated_at: String,
    /// Archive-native defaults for task policy evaluation and declared-task
    /// escalation behavior.
    pub task_policy_defaults: PreferencesTaskPolicyDefaults,
    /// The full Markdown content (body after frontmatter).
    pub content: String,
    /// SHA-256 hash of the full file content for change detection.
    pub content_hash: String,
}

/// YAML frontmatter for preferences files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PreferencesFrontmatter {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub task_policy_defaults: PreferencesTaskPolicyDefaults,
}

// ── Skill Reference ────────────────────────────────────────────────────

/// A skill manifest reference (name derived from directory).
#[derive(Debug, Clone)]
pub struct SkillManifest {
    pub name: String,
    pub path: std::path::PathBuf,
}

// ── Desired vs Actual State ────────────────────────────────────────────

/// The desired state parsed from Markdown manifests in the Archive.
#[derive(Debug, Clone, Default)]
pub struct DesiredState {
    pub identity: Option<IdentityManifest>,
    pub preferences: Option<PreferencesManifest>,
    pub policy_scopes: Vec<PolicyScopeManifest>,
    pub availability_rules: Vec<AvailabilityRuleManifest>,
    pub projects: Vec<ProjectManifest>,
    pub processes: Vec<ProcessManifest>,
    pub goals: Vec<GoalManifest>,
    pub skills: Vec<SkillManifest>,
}

/// The actual runtime state queried from the daemon.
#[derive(Debug, Clone, Default)]
pub struct ActualState {
    pub active_goals: Vec<ActiveGoalState>,
    pub running_agents: usize,
    pub loaded_skills: Vec<String>,
    /// Hash of the currently loaded SOUL content.
    pub identity_hash: Option<String>,
    /// Hash of the currently loaded preferences content.
    pub preferences_hash: Option<String>,
}

/// Runtime state of an active goal process.
#[derive(Debug, Clone)]
pub struct ActiveGoalState {
    pub slug: String,
    pub state: GoalState,
    pub phase: GoalPhase,
    pub running_agents: usize,
}

// ── Reconciliation Actions ─────────────────────────────────────────────

/// A single reconciliation action to bring actual state in line with desired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationAction {
    pub id: String,
    pub action_type: ActionType,
    /// Goal slug, skill name, or other target identifier.
    pub target: String,
    pub description: String,
    /// Whether this action requires operator approval before execution.
    pub requires_approval: bool,
    pub estimated_cost: Option<f64>,
}

/// The type of reconciliation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ActionType {
    /// Start a new goal process (manifest exists, no runtime process).
    StartGoal,
    /// Stop a goal process (manifest removed or state changed to abandoned).
    StopGoal,
    /// Pause a goal process (manifest state changed to paused).
    PauseGoal,
    /// Resume a paused goal.
    ResumeGoal,
    /// Advance goal to next phase.
    AdvancePhase { from: String, to: String },
    /// Spawn agents for a goal's current phase.
    SpawnAgents { goal: String, count: usize },
    /// Update agent identity from SOUL changes.
    ReloadIdentity,
    /// Update preferences (thresholds, limits).
    ReloadPreferences,
    /// Load a newly added skill.
    LoadSkill { name: String },
    /// Unload a removed skill.
    UnloadSkill { name: String },
}

#[cfg(test)]
mod process_generator_tests {
    use super::*;

    #[test]
    fn process_generator_mode_one_shot_bootstrap_serializes_correctly() {
        let yaml = serde_yml::to_string(&ProcessGeneratorMode::OneShotBootstrap)
            .expect("serialize OneShotBootstrap");
        assert!(
            yaml.contains("one_shot_bootstrap"),
            "expected YAML to contain `one_shot_bootstrap`, got: {yaml:?}"
        );
    }

    #[test]
    fn process_generator_mode_all_variants_roundtrip() {
        let variants = [
            ProcessGeneratorMode::RecurringTasks,
            ProcessGeneratorMode::GoalTemplate,
            ProcessGeneratorMode::ReviewOnly,
            ProcessGeneratorMode::OneShotBootstrap,
        ];
        for variant in variants {
            let yaml = serde_yml::to_string(&variant).expect("serialize variant");
            let parsed: ProcessGeneratorMode =
                serde_yml::from_str(&yaml).expect("deserialize variant");
            assert_eq!(parsed, variant, "roundtrip mismatch for {variant:?}");
        }
    }

    #[test]
    fn process_generator_config_produces_goal_prefix_roundtrip_populated() {
        let cfg = ProcessGeneratorConfig {
            mode: ProcessGeneratorMode::OneShotBootstrap,
            target_goal_id: None,
            produces_goal_prefix: Some("onboard-".into()),
        };
        let yaml = serde_yml::to_string(&cfg).expect("serialize config");
        let parsed: ProcessGeneratorConfig =
            serde_yml::from_str(&yaml).expect("deserialize config");
        assert_eq!(parsed.mode, ProcessGeneratorMode::OneShotBootstrap);
        assert_eq!(parsed.target_goal_id, None);
        assert_eq!(parsed.produces_goal_prefix, Some("onboard-".to_string()));
    }

    #[test]
    fn process_generator_config_produces_goal_prefix_default_none() {
        let yaml = "mode: one_shot_bootstrap\n";
        let parsed: ProcessGeneratorConfig =
            serde_yml::from_str(yaml).expect("deserialize minimal config");
        assert_eq!(parsed.mode, ProcessGeneratorMode::OneShotBootstrap);
        assert_eq!(parsed.target_goal_id, None);
        assert_eq!(parsed.produces_goal_prefix, None);
    }
}
