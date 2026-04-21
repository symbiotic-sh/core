//! Sub-Goal Dispatcher and backend agents (T130 §05 + §06 + §07).
//!
//! Consumes `goal.unblocked` events produced by
//! [`crate::goal_pipeline::question_resolver::QuestionResolver`] and routes
//! them to the appropriate backend per the design decision tree (§4.1):
//!
//! - [`UnblockKey::ResearchOnly`] — **live**; handled by
//!   [`researcher::ResearcherAgent`] (§05).
//! - [`UnblockKey::Exploratory`] — **wired via the [`exploratory::ExploratoryBackend`]
//!   trait** (§06). The default [`exploratory::NotReadyExploratoryBackend`]
//!   surfaces `BackendNotReady` until the T116 sub-goal authoring runner
//!   role lands; the production `SwarmExploratoryBackend` swaps in once
//!   the missing primitives ship (see `exploratory.rs` module docs).
//! - [`UnblockKey::AttachedRepo`] — **wired via the
//!   [`attached_repo::AttachedRepoBackend`] trait** (§07). The default
//!   [`attached_repo::NotReadyAttachedRepoBackend`] surfaces `BackendNotReady`;
//!   wiring sites with a [`SharedRepoRegistry`](crate::repo_registry::SharedRepoRegistry)
//!   in scope can plug in [`attached_repo::ManifestScopeAttachedRepoBackend`]
//!   (scope-check only) or the production
//!   [`attached_repo::MirrorPushAttachedRepoBackend`] (full T126
//!   `mirror_push_with_approval` + `ApprovalGate` composition).
//! - [`UnblockKey::Composite`] — stubbed; §09 lands the fan-out logic.
//!
//! Capacity control lives in [`spawn_budget::SpawnBudget`] (design §4.2).
//!
//! The three-channel merge-back (§5) is only partially wired: this chunk
//! emits the **event channel** (`goal.subgoal.spawned`,
//! `goal.subgoal.completed`, `goal.subgoal.failed`) and hands back an
//! [`researcher::ArchiveNoteDraft`]. §08 will add the Archive note write
//! and the thread-message pill.
//!
//! [`UnblockKey::ResearchOnly`]: symbiotic_core::types::question_group::UnblockKey::ResearchOnly
//! [`UnblockKey::Exploratory`]: symbiotic_core::types::question_group::UnblockKey::Exploratory
//! [`UnblockKey::AttachedRepo`]: symbiotic_core::types::question_group::UnblockKey::AttachedRepo
//! [`UnblockKey::Composite`]: symbiotic_core::types::question_group::UnblockKey::Composite

pub mod attached_repo;
pub mod dispatcher;
pub mod exploratory;
pub mod researcher;
pub mod spawn_budget;

pub use attached_repo::{
    AttachedRepoBackend, AttachedRepoError, AttachedRepoOutcome, AttachedRepoPushRunner,
    AttachedRepoRequest, ManifestScopeAttachedRepoBackend, MirrorPushAttachedRepoBackend,
    NotReadyAttachedRepoBackend,
};
pub use dispatcher::{
    DispatchEvent, DispatchOutcome, Dispatcher, DispatcherError, UnblockedContext, Verdict,
};
pub use exploratory::{
    ExploratoryBackend, ExploratoryError, ExploratoryOutcome, ExploratoryRequest,
    NotReadyExploratoryBackend,
};
pub use researcher::{
    ArchiveNoteDraft, RecallSnippet, ResearchLlm, ResearchRecall, ResearchRequest, ResearcherAgent,
    ResearcherError,
};
pub use spawn_budget::{BudgetExceeded, BudgetGuard, SpawnBudget, SpawnBudgetConfig};
