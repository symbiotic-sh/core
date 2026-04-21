//! Declarative Cognitive Control Plane for Symbiotic.
//!
//! This crate implements the Kubernetes-style reconciliation model where
//! the desired state is declared in Markdown files (SOUL.md, goal manifests,
//! skill definitions) and the daemon continuously reconciles runtime state
//! to match.
//!
//! # Architecture
//!
//! ```text
//! Desired State (Markdown)     Actual State (Runtime)
//!   identity/SOUL.md             GoalProcessManager
//!   identity/preferences.md ──>  AgentPool
//!   operations/projects/*/       WorkQueue
//!   operations/skills/*/         MetricsStore
//!         │                           │
//!         └──── StateDiffer ──────────┘
//!                    │
//!              ReconciliationAction[]
//!                    │
//!              ActionExecutor
//! ```
//!
//! # Modules
//!
//! - [`types`] — Core types (ProjectManifest, GoalManifest, ProcessManifest, DesiredState, ActualState, ActionType)
//! - [`manifest`] — YAML frontmatter parser for Markdown files

pub mod claims;
pub mod diff;
pub mod goals;
pub mod leases;
pub mod management_store;
pub mod manifest;
pub mod reconciler;
pub mod repo_manifest;
pub mod types;
pub mod work_items;

pub use claims::{
    claims_conflict, scopes_overlap, CollaborationScope, ScopeClaim, ScopeClaimStatus, ScopeMode,
    ScopeRequirement,
};
pub use diff::{prioritize_and_limit, StateDiffer};
pub use goals::{GoalMetrics, GoalProcess, GoalProcessManager};
pub use leases::{HeartbeatStatus, HeartbeatUpdate, Lease, ManagementLeaseConfig};
pub use management_store::ManagementStore;
pub use manifest::ManifestParser;
pub use reconciler::{Reconciler, ReconcilerConfig, StateQuery};
pub use repo_manifest::{
    AkbTier, ArcheologyPolicy, CredentialScope, MirrorDirection, RepoAgentScopes,
    RepoCheckoutPolicy, RepoCredentialBinding, RepoDistilleryConfig, RepoHooks, RepoIndexingPolicy,
    RepoManifest, RepoManifestError, RepoMetadata, RepoMirrorPolicy, RepoProvider,
    RepoRefreshPolicy, RepoRole, RepoSource, RepoState, RepoTierPolicy,
};
pub use types::{
    ActionType, ActiveGoalState, ActualState, AutonomyLevel, AvailabilityRuleManifest,
    CheckFrequency, DesiredState, GoalConstraints, GoalManifest, GoalPhase, GoalState,
    IdentityManifest, PolicyScopeKind, PolicyScopeManifest, PreferencesManifest, ProcessCadence,
    ProcessCadenceKind, ProcessConfig, ProcessGeneratorConfig, ProcessGeneratorMode,
    ProcessManifest, ProcessState, ProcessTaskTemplate, ProcessType, ProjectManifest, ProjectState,
    ReconciliationAction, RiskTolerance, SkillManifest, StreamConfig,
};
pub use work_items::{
    AgentAssignment, AssignmentMode, CancellationState, ReviewMode, WorkItem, WorkItemKind,
    WorkItemStatus, WorkPriority, WorkUrgency,
};
