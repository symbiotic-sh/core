//! Sub-Goal Dispatcher (T130 §05 + §06 + §07).
//!
//! Consumes `goal.unblocked` signals and routes them to the appropriate
//! backend per design §4.1:
//!
//! ```text
//! switch unblock_key:
//!   Exploratory  → T116 swarm via ExploratoryBackend trait (§06 — LIVE; default backend reports BackendNotReady until T116 authoring role lands)
//!   AttachedRepo → T126 mirror_push_with_approval via AttachedRepoBackend trait (§07 — LIVE; default backend reports BackendNotReady until full wiring is plumbed)
//!   ResearchOnly → sandboxed research agent        (§05 — LIVE)
//!   Composite    → fan out per child          (§09 pending — stub here)
//! ```
//!
//! For ResearchOnly the dispatcher:
//!
//! 1. Budget-checks via [`SpawnBudget`] (§4.2).
//! 2. Mints a child [`GoalProcess`] via [`GoalProcess::new_child`].
//! 3. Emits `goal.subgoal.spawned` on the event channel.
//! 4. Invokes the [`ResearcherAgent`]; on success emits
//!    `goal.subgoal.completed` and surfaces the
//!    [`ArchiveNoteDraft`] via the returned [`DispatchOutcome`].
//!
//! **Merge-back stub:** The design's three-channel merge-back (§5) requires
//! both an Archive write and a thread-message pill in addition to the event.
//! This chunk emits only the event channel and returns the draft — §08
//! lands the Archive write and thread pill.
//!
//! The dispatcher does not own a Matrix transport directly. It produces
//! [`DispatchEvent`]s (effectively pre-formed [`MatrixEventEnvelope`]s paired
//! with their target room ids) that the caller forwards to the outbound
//! channel. This keeps the dispatcher unit-testable and avoids deepening
//! the lifetime web around the existing transport plumbing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use symbiotic_control_plane::goals::{GoalProcess, GoalProcessManager};
use symbiotic_control_plane::types::GoalPhase;
use symbiotic_core::types::question_group::UnblockKey;
use symbiotic_matrix::events::MatrixEventEnvelope;
use thiserror::Error;
use tracing::{info, warn};

use super::attached_repo::{
    AttachedRepoBackend, AttachedRepoError, AttachedRepoOutcome, AttachedRepoRequest,
    NotReadyAttachedRepoBackend,
};
use super::exploratory::{
    ExploratoryBackend, ExploratoryError, ExploratoryRequest, NotReadyExploratoryBackend,
};
use super::researcher::{ArchiveNoteDraft, ResearchRequest, ResearcherAgent, ResearcherError};
use super::spawn_budget::{BudgetGuard, SpawnBudget};

// ---------------------------------------------------------------------------
// Dispatcher input + output types
// ---------------------------------------------------------------------------

/// What the dispatcher needs to act on a `goal.unblocked` signal.
///
/// The caller (the `goal.answer` handler in `commands.rs`) builds this from
/// the `GroupResolutionOutcome::Unblocked` payload plus the [`UnblockKey`]
/// the dispatcher previously remembered via [`Dispatcher::remember_group`].
#[derive(Debug, Clone)]
pub struct UnblockedContext {
    pub parent_goal_id: String,
    pub group_id: String,
    pub thread_id: Option<String>,
    /// Target room id for the emitted `goal.subgoal.*` envelopes. Typically
    /// the parent goal's thread room.
    pub room_id: String,
    /// Unix timestamp for event stamping.
    pub now: u64,
    /// Operator answers from the resolved group (question_index → text).
    pub answers: HashMap<usize, String>,
}

/// One (room_id, envelope) pair the dispatcher wants the caller to send.
///
/// Mirrors [`crate::MatrixOutboundMessage`] without importing lib.rs, so
/// this module stays testable in isolation.
#[derive(Debug, Clone)]
pub struct DispatchEvent {
    pub room_id: String,
    pub envelope: MatrixEventEnvelope,
}

/// Terminal outcome of a single `on_unblocked` routing decision.
///
/// The dispatcher returns one of these per call; the caller is responsible
/// for forwarding `events` to the outbound Matrix channel, persisting the
/// `child_goal_slug` (if any) to the control plane, and ultimately handing
/// the `archive_note_draft` off to §08's merge-back write path.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    pub verdict: Verdict,
    /// Slug of the child `GoalProcess` the dispatcher created (if any).
    pub child_goal_slug: Option<String>,
    /// Matrix events to forward (`goal.subgoal.spawned`, `goal.subgoal.completed`,
    /// or `goal.subgoal.failed`).
    pub events: Vec<DispatchEvent>,
    /// On ResearchOnly success, the note draft the researcher produced.
    /// §08 picks this up and writes it to Archive.
    pub archive_note_draft: Option<ArchiveNoteDraft>,
}

/// What the dispatcher decided for the unblock.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Backend ran successfully end-to-end.
    Success,
    /// Backend is not wired yet — `goal.subgoal.failed` was emitted with an
    /// `UnsupportedBackend` outcome (§07/§09 still in this state).
    UnsupportedBackend,
    /// The Exploratory backend's underlying T116 primitives haven't shipped
    /// yet (sub-goal authoring runner role missing). The dispatcher emits a
    /// `goal.subgoal.failed` event with `outcome=BackendNotReady` and a
    /// reason that names the missing primitive. Distinct from
    /// `UnsupportedBackend` because the wiring *exists* — only the
    /// underlying primitive is pending.
    BackendNotReady,
    /// Budget was exceeded; the sub-goal is `queued` and the caller should
    /// retry on the next child-completion event (§4.2).
    Queued,
    /// Backend started but failed mid-execution (e.g. LLM/Recall error).
    Failed,
}

#[derive(Debug, Error)]
pub enum DispatcherError {
    #[error("unknown group '{0}' — no UnblockKey was remembered; did you call remember_group?")]
    UnknownGroup(String),
    #[error("child goal '{slug}' conflicts with an existing goal: {source}")]
    ChildGoalConflict {
        slug: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("goal process manager lock poisoned")]
    ManagerLockPoisoned,
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Per-daemon dispatcher. Holds the [`SpawnBudget`], the researcher, and a
/// side-map of `group_id -> UnblockKey` populated at group registration
/// time (so `goal.unblocked` events can route without needing to re-parse
/// the group JSON).
///
/// The dispatcher is `Clone` via inner `Arc`s; share a single instance
/// across async call sites.
#[derive(Clone)]
pub struct Dispatcher {
    inner: Arc<DispatcherInner>,
}

struct DispatcherInner {
    budget: SpawnBudget,
    researcher: Arc<ResearcherAgent>,
    /// Backend for `UnblockKey::Exploratory`. Defaults to
    /// [`NotReadyExploratoryBackend`] (which surfaces `BackendNotReady`)
    /// when the dispatcher is constructed without an explicit backend; the
    /// production wiring swaps in a `SwarmExploratoryBackend` once the T116
    /// authoring role lands. See `exploratory.rs` module docs.
    exploratory: Arc<dyn ExploratoryBackend>,
    /// Backend for `UnblockKey::AttachedRepo`. Defaults to
    /// [`NotReadyAttachedRepoBackend`] (which surfaces `BackendNotReady`)
    /// when the dispatcher is constructed without an explicit backend. The
    /// production wiring at `main.rs` swaps in a
    /// `MirrorPushAttachedRepoBackend` (composing T126's
    /// `mirror_push_with_approval` + `ApprovalGate` + `GitPushSession`) once
    /// the operator-approval room id, MatrixPoster, PushProvider, and
    /// GoalScopedVault are all in scope. See `attached_repo.rs` module docs.
    attached_repo: Arc<dyn AttachedRepoBackend>,
    goal_manager: Arc<Mutex<GoalProcessManager>>,
    group_keys: Mutex<HashMap<String, UnblockKey>>,
}

impl Dispatcher {
    /// Construct a dispatcher with the default Exploratory + AttachedRepo
    /// backends (both [`NotReadyExploratoryBackend`] /
    /// [`NotReadyAttachedRepoBackend`] fallbacks). Call
    /// [`Dispatcher::new_with_exploratory`] / [`Dispatcher::new_with_attached_repo`]
    /// to plug in the production wiring once the underlying primitives are
    /// reachable.
    pub fn new(
        budget: SpawnBudget,
        researcher: Arc<ResearcherAgent>,
        goal_manager: Arc<Mutex<GoalProcessManager>>,
    ) -> Self {
        Self::new_with_backends(
            budget,
            researcher,
            Arc::new(NotReadyExploratoryBackend::new()),
            Arc::new(NotReadyAttachedRepoBackend::new()),
            goal_manager,
        )
    }

    /// Construct a dispatcher with an explicit Exploratory backend.
    ///
    /// In production this is the seam where `main.rs` plugs in the future
    /// `SwarmExploratoryBackend` (composing `daemon.create_swarm_repo` +
    /// the sub-goal authoring sandbox + the existing T116 PR / distillery
    /// pipeline) once the missing primitives ship.
    pub fn new_with_exploratory(
        budget: SpawnBudget,
        researcher: Arc<ResearcherAgent>,
        exploratory: Arc<dyn ExploratoryBackend>,
        goal_manager: Arc<Mutex<GoalProcessManager>>,
    ) -> Self {
        Self::new_with_backends(
            budget,
            researcher,
            exploratory,
            Arc::new(NotReadyAttachedRepoBackend::new()),
            goal_manager,
        )
    }

    /// Construct a dispatcher with an explicit AttachedRepo backend.
    ///
    /// In production this is the seam where `main.rs` plugs in the
    /// [`MirrorPushAttachedRepoBackend`](super::attached_repo::MirrorPushAttachedRepoBackend)
    /// composing T126's `mirror_push_with_approval` + `ApprovalGate` +
    /// `GitPushSession` once the operator approval room id, the
    /// `MatrixPoster`, the `PushProvider`, and the `GoalScopedVault` are all
    /// reachable from the construction site.
    pub fn new_with_attached_repo(
        budget: SpawnBudget,
        researcher: Arc<ResearcherAgent>,
        attached_repo: Arc<dyn AttachedRepoBackend>,
        goal_manager: Arc<Mutex<GoalProcessManager>>,
    ) -> Self {
        Self::new_with_backends(
            budget,
            researcher,
            Arc::new(NotReadyExploratoryBackend::new()),
            attached_repo,
            goal_manager,
        )
    }

    /// Construct a dispatcher with explicit Exploratory **and** AttachedRepo
    /// backends. Useful for production wiring that has both backends ready,
    /// and for integration tests that need to drive both pipelines.
    pub fn new_with_backends(
        budget: SpawnBudget,
        researcher: Arc<ResearcherAgent>,
        exploratory: Arc<dyn ExploratoryBackend>,
        attached_repo: Arc<dyn AttachedRepoBackend>,
        goal_manager: Arc<Mutex<GoalProcessManager>>,
    ) -> Self {
        Self {
            inner: Arc::new(DispatcherInner {
                budget,
                researcher,
                exploratory,
                attached_repo,
                goal_manager,
                group_keys: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Observability helper.
    pub fn budget(&self) -> &SpawnBudget {
        &self.inner.budget
    }

    /// Record the [`UnblockKey`] associated with a group so a later
    /// `on_unblocked` call can route correctly. Call this at the same
    /// point the resolver's `register()` is invoked.
    pub fn remember_group(&self, group_id: impl Into<String>, key: UnblockKey) {
        if let Ok(mut map) = self.inner.group_keys.lock() {
            map.insert(group_id.into(), key);
        }
    }

    /// Forget a group — useful on cancellation paths. Idempotent.
    pub fn forget_group(&self, group_id: &str) {
        if let Ok(mut map) = self.inner.group_keys.lock() {
            map.remove(group_id);
        }
    }

    /// Look up the remembered unblock key. Returns `None` if the group is
    /// unknown (e.g. the daemon restarted between registration and
    /// resolution — handling that case is a §04a-level concern).
    pub fn lookup_group(&self, group_id: &str) -> Option<UnblockKey> {
        self.inner
            .group_keys
            .lock()
            .ok()
            .and_then(|m| m.get(group_id).cloned())
    }

    /// Main entry point: a `goal.unblocked` event arrived; decide what to do.
    pub async fn on_unblocked(
        &self,
        ctx: UnblockedContext,
    ) -> Result<DispatchOutcome, DispatcherError> {
        let key = self
            .lookup_group(&ctx.group_id)
            .ok_or_else(|| DispatcherError::UnknownGroup(ctx.group_id.clone()))?;

        self.on_unblocked_with_key(ctx, key).await
    }

    /// Variant that takes the [`UnblockKey`] directly — exposed so the
    /// wiring site can pass a key it already has in scope and skip the
    /// side-map lookup (useful in hot paths and in unit tests).
    pub async fn on_unblocked_with_key(
        &self,
        ctx: UnblockedContext,
        key: UnblockKey,
    ) -> Result<DispatchOutcome, DispatcherError> {
        // Budget-check before doing any real work (§4.2).
        let guard = match self.inner.budget.try_reserve(&key) {
            Ok(g) => g,
            Err(exceeded) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    group = %ctx.group_id,
                    "dispatcher: budget exceeded, sub-goal queued: {exceeded}"
                );
                let event = emit_subgoal_failed(
                    &ctx,
                    /* sub_goal_slug */ None,
                    SubgoalFailureReason::Queued(exceeded.to_string()),
                );
                return Ok(DispatchOutcome {
                    verdict: Verdict::Queued,
                    child_goal_slug: None,
                    events: vec![event],
                    archive_note_draft: None,
                });
            }
        };

        // After the guard is in hand, route.
        let outcome = match &key {
            UnblockKey::ResearchOnly { question } => {
                self.run_research_only(&ctx, &key, question, guard).await
            }
            UnblockKey::Exploratory { topic } => {
                self.run_exploratory(&ctx, &key, topic, guard).await
            }
            UnblockKey::AttachedRepo {
                repo_id,
                branch_hint,
                requires_approval,
            } => {
                self.run_attached_repo(&ctx, &key, repo_id, branch_hint, *requires_approval, guard)
                    .await
            }
            UnblockKey::Composite { .. } => {
                guard.release();
                let event = emit_subgoal_failed(
                    &ctx,
                    None,
                    SubgoalFailureReason::Unsupported {
                        backend: "Composite",
                        reason: "§09 pending",
                    },
                );
                DispatchOutcome {
                    verdict: Verdict::UnsupportedBackend,
                    child_goal_slug: None,
                    events: vec![event],
                    archive_note_draft: None,
                }
            }
        };

        // ResearchOnly branch drops the guard internally after emitting
        // goal.subgoal.completed. The unsupported branches drop it above.
        // Drop the remembered mapping so the side-map doesn't leak.
        self.forget_group(&ctx.group_id);

        Ok(outcome)
    }

    async fn run_research_only(
        &self,
        ctx: &UnblockedContext,
        key: &UnblockKey,
        question: &str,
        guard: BudgetGuard,
    ) -> DispatchOutcome {
        let sub_goal_slug = mint_sub_goal_slug(&ctx.parent_goal_id, &ctx.group_id);

        // Create the child goal. Failure here is structural (slug clash
        // etc.) — we log, mark Failed, and continue so the caller still
        // sees a terminal event on the event channel (§9 failure matrix).
        let child_created = match create_child_goal(
            &self.inner.goal_manager,
            &sub_goal_slug,
            &ctx.parent_goal_id,
            key.clone(),
            question,
        ) {
            Ok(()) => true,
            Err(err) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    slug = %sub_goal_slug,
                    "dispatcher: failed to persist child goal: {err}"
                );
                false
            }
        };

        let mut events = Vec::new();
        events.push(emit_subgoal_spawned(ctx, &sub_goal_slug, key));

        let request = ResearchRequest::new(sub_goal_slug.clone(), question)
            .with_answers(answers_to_vec(&ctx.answers));

        let research_result = self.inner.researcher.execute(request).await;

        // Release budget now that the async work is done.
        guard.release();

        match research_result {
            Ok(draft) => {
                info!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    sources = draft.sources.len(),
                    "dispatcher: research sub-goal completed"
                );
                events.push(emit_subgoal_completed(
                    ctx,
                    &sub_goal_slug,
                    key,
                    &draft.summary,
                ));
                DispatchOutcome {
                    verdict: Verdict::Success,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    archive_note_draft: Some(draft),
                }
            }
            Err(err) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    "dispatcher: research sub-goal failed: {err}"
                );
                events.push(emit_subgoal_failed(
                    ctx,
                    Some(&sub_goal_slug),
                    SubgoalFailureReason::ResearcherError(describe_researcher_error(&err)),
                ));
                DispatchOutcome {
                    verdict: Verdict::Failed,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    archive_note_draft: None,
                }
            }
        }
    }

    async fn run_exploratory(
        &self,
        ctx: &UnblockedContext,
        key: &UnblockKey,
        topic: &str,
        guard: BudgetGuard,
    ) -> DispatchOutcome {
        let sub_goal_slug = mint_sub_goal_slug(&ctx.parent_goal_id, &ctx.group_id);

        // Mint the child goal up-front (matches ResearchOnly path) so the
        // event stream + control-plane store are consistent regardless of
        // whether the backend is the production swarm or the
        // BackendNotReady fallback.
        let child_created = match create_child_goal(
            &self.inner.goal_manager,
            &sub_goal_slug,
            &ctx.parent_goal_id,
            key.clone(),
            topic,
        ) {
            Ok(()) => true,
            Err(err) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    slug = %sub_goal_slug,
                    "dispatcher: failed to persist child goal: {err}"
                );
                false
            }
        };

        let mut events = Vec::new();
        events.push(emit_subgoal_spawned(ctx, &sub_goal_slug, key));

        let request = ExploratoryRequest::new(sub_goal_slug.clone(), topic)
            .with_answers(answers_to_vec(&ctx.answers));

        let backend_result = self.inner.exploratory.execute(request).await;

        // Release budget now that the async work is done.
        guard.release();

        match backend_result {
            Ok(outcome) => {
                info!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    artifacts = outcome.artifact_refs.len(),
                    "dispatcher: exploratory sub-goal completed"
                );
                events.push(emit_subgoal_completed(
                    ctx,
                    &sub_goal_slug,
                    key,
                    &outcome.summary,
                ));
                DispatchOutcome {
                    verdict: Verdict::Success,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    // TODO(T130 §08): exploratory backend produces Archive
                    // artifact refs (post-merge distillery output) rather
                    // than a single inline draft. The §08 merge-back will
                    // surface those refs through the same channel as the
                    // ResearchOnly draft (likely a shared enum). For now we
                    // emit the event channel only (matches §05's stub).
                    archive_note_draft: None,
                }
            }
            Err(ExploratoryError::BackendNotReady { reason }) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    %reason,
                    "dispatcher: exploratory backend not ready (T116 wiring pending)"
                );
                events.push(emit_subgoal_failed(
                    ctx,
                    Some(&sub_goal_slug),
                    SubgoalFailureReason::BackendNotReady(reason),
                ));
                DispatchOutcome {
                    verdict: Verdict::BackendNotReady,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    archive_note_draft: None,
                }
            }
            Err(err) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    "dispatcher: exploratory sub-goal failed: {err}"
                );
                events.push(emit_subgoal_failed(
                    ctx,
                    Some(&sub_goal_slug),
                    SubgoalFailureReason::ExploratoryError(describe_exploratory_error(&err)),
                ));
                DispatchOutcome {
                    verdict: Verdict::Failed,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    archive_note_draft: None,
                }
            }
        }
    }

    async fn run_attached_repo(
        &self,
        ctx: &UnblockedContext,
        key: &UnblockKey,
        repo_id: &str,
        branch_hint: &str,
        requires_approval_hint: bool,
        guard: BudgetGuard,
    ) -> DispatchOutcome {
        let sub_goal_slug = mint_sub_goal_slug(&ctx.parent_goal_id, &ctx.group_id);

        // Mint the child goal up-front (matches ResearchOnly + Exploratory
        // paths) so the event stream + control-plane store stay consistent
        // regardless of whether the backend ultimately Succeeds, lands a
        // Partial outcome, or Fails (scope-denied / timeout / not-ready).
        let seed_title = format!("attached-repo:{repo_id}:{branch_hint}");
        let child_created = match create_child_goal(
            &self.inner.goal_manager,
            &sub_goal_slug,
            &ctx.parent_goal_id,
            key.clone(),
            &seed_title,
        ) {
            Ok(()) => true,
            Err(err) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    slug = %sub_goal_slug,
                    "dispatcher: failed to persist child goal: {err}"
                );
                false
            }
        };

        let mut events = Vec::new();
        events.push(emit_subgoal_spawned(ctx, &sub_goal_slug, key));

        let request = AttachedRepoRequest::new(
            sub_goal_slug.clone(),
            repo_id,
            branch_hint,
            requires_approval_hint,
        )
        .with_answers(answers_to_vec(&ctx.answers));

        let backend_result = self.inner.attached_repo.execute(request).await;

        // Release budget now that the async work is done. Per design §4.2 the
        // per-attached-repo cap is 1 (serializes pushes per repo); releasing
        // here lets the next queued sub-goal for the same repo proceed.
        guard.release();

        match backend_result {
            Ok(AttachedRepoOutcome::Success {
                summary,
                branch_ref,
                ..
            }) => {
                info!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    %branch_ref,
                    "dispatcher: attached_repo sub-goal completed (push approved)"
                );
                events.push(emit_attached_repo_completed(
                    ctx,
                    &sub_goal_slug,
                    key,
                    &summary,
                    Some(&branch_ref),
                    "Success",
                    None,
                ));
                DispatchOutcome {
                    verdict: Verdict::Success,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    // TODO(T130 §08): three-channel merge-back. Archive note
                    // captures the approved push (commit ref + diff stats);
                    // thread pill renders the success in the parent's
                    // mission-control surface. For this chunk only the event
                    // channel fires (matches §05/§06).
                    archive_note_draft: None,
                }
            }
            Ok(AttachedRepoOutcome::Partial {
                summary,
                denial_note,
                ..
            }) => {
                info!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    "dispatcher: attached_repo sub-goal partial (operator denied push)"
                );
                events.push(emit_attached_repo_completed(
                    ctx,
                    &sub_goal_slug,
                    key,
                    &summary,
                    None,
                    "Partial",
                    Some(&denial_note),
                ));
                DispatchOutcome {
                    verdict: Verdict::Success,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    // TODO(T130 §08): on Partial we need to surface the
                    // denial_note as both an Archive note placeholder
                    // ("operator denied push: <reason>") and a thread pill
                    // ("⚠ subgoal-X: push denied"). Stubbed here; merge-back
                    // chunk will pick this up.
                    archive_note_draft: None,
                }
            }
            Err(AttachedRepoError::ScopeDenied { repo_id }) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    %repo_id,
                    "dispatcher: attached_repo scope-denied (push_external not granted)"
                );
                events.push(emit_subgoal_failed(
                    ctx,
                    Some(&sub_goal_slug),
                    SubgoalFailureReason::AttachedRepoScopeDenied { repo_id },
                ));
                DispatchOutcome {
                    verdict: Verdict::Failed,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    archive_note_draft: None,
                }
            }
            Err(AttachedRepoError::ApprovalTimeout { ticket_id }) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    %ticket_id,
                    "dispatcher: attached_repo ApprovalGate timeout"
                );
                events.push(emit_subgoal_failed(
                    ctx,
                    Some(&sub_goal_slug),
                    SubgoalFailureReason::AttachedRepoApprovalTimeout { ticket_id },
                ));
                DispatchOutcome {
                    verdict: Verdict::Failed,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    archive_note_draft: None,
                }
            }
            Err(AttachedRepoError::BackendNotReady { reason }) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    %reason,
                    "dispatcher: attached_repo backend not ready (T126 wiring pending)"
                );
                events.push(emit_subgoal_failed(
                    ctx,
                    Some(&sub_goal_slug),
                    SubgoalFailureReason::BackendNotReady(reason),
                ));
                DispatchOutcome {
                    verdict: Verdict::BackendNotReady,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    archive_note_draft: None,
                }
            }
            Err(err) => {
                warn!(
                    parent = %ctx.parent_goal_id,
                    sub_goal = %sub_goal_slug,
                    "dispatcher: attached_repo sub-goal failed: {err}"
                );
                events.push(emit_subgoal_failed(
                    ctx,
                    Some(&sub_goal_slug),
                    SubgoalFailureReason::AttachedRepoError(describe_attached_repo_error(&err)),
                ));
                DispatchOutcome {
                    verdict: Verdict::Failed,
                    child_goal_slug: if child_created {
                        Some(sub_goal_slug)
                    } else {
                        None
                    },
                    events,
                    archive_note_draft: None,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Child goal creation
// ---------------------------------------------------------------------------

fn create_child_goal(
    manager: &Arc<Mutex<GoalProcessManager>>,
    slug: &str,
    parent_goal_id: &str,
    key: UnblockKey,
    seed_title: &str,
) -> Result<(), DispatcherError> {
    let mut guard = manager
        .lock()
        .map_err(|_| DispatcherError::ManagerLockPoisoned)?;
    let child = GoalProcess::new_child(slug, parent_goal_id, key, seed_title);
    guard
        .insert_child(child)
        .map_err(|source| DispatcherError::ChildGoalConflict {
            slug: slug.to_string(),
            source,
        })?;
    Ok(())
}

fn mint_sub_goal_slug(parent_goal_id: &str, group_id: &str) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let short = &suffix[..8];
    format!("sg-{parent_goal_id}-{group_id}-{short}")
}

fn answers_to_vec(map: &HashMap<usize, String>) -> Vec<(usize, String)> {
    let mut v: Vec<(usize, String)> = map.iter().map(|(k, v)| (*k, v.clone())).collect();
    v.sort_by_key(|(k, _)| *k);
    v
}

// ---------------------------------------------------------------------------
// Event builders (event channel only — merge-back Archive + thread pill in §08)
// ---------------------------------------------------------------------------

fn emit_subgoal_spawned(
    ctx: &UnblockedContext,
    sub_goal_slug: &str,
    key: &UnblockKey,
) -> DispatchEvent {
    let body = format!("Sub-goal {sub_goal_slug} spawned");
    let envelope = MatrixEventEnvelope::state("goal.subgoal.spawned", ctx.now, &body)
        .with_detail_field("parent_goal_id", ctx.parent_goal_id.as_str())
        .with_detail_field("sub_goal_id", sub_goal_slug)
        .with_detail_field("group_id", ctx.group_id.as_str())
        .with_detail_field(
            "unblock_key",
            serde_json::to_value(key).unwrap_or(serde_json::Value::Null),
        );
    let envelope = if let Some(thread) = &ctx.thread_id {
        envelope.with_thread(thread.clone())
    } else {
        envelope.with_thread(ctx.parent_goal_id.clone())
    };
    DispatchEvent {
        room_id: ctx.room_id.clone(),
        envelope,
    }
}

fn emit_subgoal_completed(
    ctx: &UnblockedContext,
    sub_goal_slug: &str,
    key: &UnblockKey,
    summary: &str,
) -> DispatchEvent {
    // TODO(T130 §08): full three-channel merge-back — in addition to this
    // event channel, §08 will write the ArchiveNoteDraft to
    // `knowledge-base/episodic/subgoals/<sub_goal_id>/result.md` and emit
    // a `subgoal.completed` thread-message pill to the parent thread. For
    // this chunk we emit the event channel only.
    let body = format!("Sub-goal {sub_goal_slug} completed");
    let envelope = MatrixEventEnvelope::state("goal.subgoal.completed", ctx.now, &body)
        .with_detail_field("parent_goal_id", ctx.parent_goal_id.as_str())
        .with_detail_field("sub_goal_id", sub_goal_slug)
        .with_detail_field("group_id", ctx.group_id.as_str())
        .with_detail_field(
            "unblock_key",
            serde_json::to_value(key).unwrap_or(serde_json::Value::Null),
        )
        .with_detail_field("outcome", "Success")
        .with_detail_field("summary", summary);
    let envelope = if let Some(thread) = &ctx.thread_id {
        envelope.with_thread(thread.clone())
    } else {
        envelope.with_thread(ctx.parent_goal_id.clone())
    };
    DispatchEvent {
        room_id: ctx.room_id.clone(),
        envelope,
    }
}

enum SubgoalFailureReason {
    Unsupported {
        backend: &'static str,
        reason: &'static str,
    },
    /// Exploratory / AttachedRepo backend's underlying primitive isn't ready
    /// yet — surfaced as a distinct outcome so observers can tell "wiring
    /// exists, underlying primitive pending" from "wiring not yet shipped".
    BackendNotReady(String),
    Queued(String),
    ResearcherError(String),
    ExploratoryError(String),
    AttachedRepoError(String),
    /// The sub-goal's RepoManifest does not grant `agent_scopes.push_external`
    /// for this agent role. Surfaced before any session is opened so no
    /// credential or git surface is touched.
    AttachedRepoScopeDenied {
        repo_id: String,
    },
    /// `ApprovalGate` ticket TTL elapsed before the operator approved/denied.
    AttachedRepoApprovalTimeout {
        ticket_id: String,
    },
}

fn emit_subgoal_failed(
    ctx: &UnblockedContext,
    sub_goal_slug: Option<&str>,
    reason: SubgoalFailureReason,
) -> DispatchEvent {
    let (outcome, detail) = match &reason {
        SubgoalFailureReason::Unsupported { backend, reason } => {
            ("UnsupportedBackend", format!("{backend}: {reason}"))
        }
        SubgoalFailureReason::BackendNotReady(msg) => ("BackendNotReady", msg.clone()),
        SubgoalFailureReason::Queued(msg) => ("Queued", msg.clone()),
        SubgoalFailureReason::ResearcherError(msg) => ("ResearcherError", msg.clone()),
        SubgoalFailureReason::ExploratoryError(msg) => ("ExploratoryError", msg.clone()),
        SubgoalFailureReason::AttachedRepoError(msg) => ("AttachedRepoError", msg.clone()),
        SubgoalFailureReason::AttachedRepoScopeDenied { repo_id } => (
            "Failed",
            format!("scope-denied: push_external not in agent_scopes for {repo_id}"),
        ),
        SubgoalFailureReason::AttachedRepoApprovalTimeout { ticket_id } => (
            "Failed",
            format!("ApprovalGate timeout (ticket {ticket_id})"),
        ),
    };
    let body = format!("Sub-goal failed ({outcome})");
    let mut envelope = MatrixEventEnvelope::state("goal.subgoal.failed", ctx.now, &body)
        .with_detail_field("parent_goal_id", ctx.parent_goal_id.as_str())
        .with_detail_field("group_id", ctx.group_id.as_str())
        .with_detail_field("outcome", outcome)
        .with_detail_field("reason", detail);
    if let Some(slug) = sub_goal_slug {
        envelope = envelope.with_detail_field("sub_goal_id", slug);
    }
    let envelope = if let Some(thread) = &ctx.thread_id {
        envelope.with_thread(thread.clone())
    } else {
        envelope.with_thread(ctx.parent_goal_id.clone())
    };
    DispatchEvent {
        room_id: ctx.room_id.clone(),
        envelope,
    }
}

fn describe_researcher_error(err: &ResearcherError) -> String {
    match err {
        ResearcherError::Recall(e) => format!("recall error: {e}"),
        ResearcherError::Llm(e) => format!("llm error: {e}"),
        ResearcherError::EmptyLlmResponse => "empty llm response".to_string(),
    }
}

fn describe_exploratory_error(err: &ExploratoryError) -> String {
    match err {
        // BackendNotReady is routed through its own variant in the dispatcher
        // before reaching this helper; included here for completeness in the
        // event a future caller surfaces it through the generic error path.
        ExploratoryError::BackendNotReady { reason } => format!("backend not ready: {reason}"),
        ExploratoryError::SwarmRepoCreation(e) => format!("swarm repo creation error: {e}"),
        ExploratoryError::SandboxSpawn(e) => format!("sandbox spawn error: {e}"),
        ExploratoryError::AuthoringFailed(msg) => format!("authoring failed: {msg}"),
        ExploratoryError::DistilleryRejected(msg) => format!("distillery rejected: {msg}"),
    }
}

fn describe_attached_repo_error(err: &AttachedRepoError) -> String {
    match err {
        // ScopeDenied / ApprovalTimeout / BackendNotReady are routed through
        // their own variants in the dispatcher before reaching this helper;
        // included here for completeness so a future caller that surfaces them
        // through the generic error path still produces a useful reason.
        AttachedRepoError::ScopeDenied { repo_id } => {
            format!("scope-denied: push_external not in agent_scopes for {repo_id}")
        }
        AttachedRepoError::ManifestNotFound { repo_id } => {
            format!("manifest not found: {repo_id}")
        }
        AttachedRepoError::ManifestNotActive { repo_id, state } => {
            format!("manifest not active: {repo_id} (state {state:?})")
        }
        AttachedRepoError::ApprovalTimeout { ticket_id } => {
            format!("ApprovalGate timeout (ticket {ticket_id})")
        }
        AttachedRepoError::SessionOpenFailed(msg) => format!("session open failed: {msg}"),
        AttachedRepoError::AuthoringFailed(msg) => format!("authoring failed: {msg}"),
        AttachedRepoError::PushFailed(msg) => format!("push failed: {msg}"),
        AttachedRepoError::BackendNotReady { reason } => format!("backend not ready: {reason}"),
    }
}

/// Emit the AttachedRepo `goal.subgoal.completed` envelope. Carries the
/// `outcome` discriminator (`Success` for approved push, `Partial` for
/// operator-denied push) plus optional `branch_ref` (Success) or `denial_note`
/// (Partial). Three-channel merge-back (Archive note + thread pill) is §08.
fn emit_attached_repo_completed(
    ctx: &UnblockedContext,
    sub_goal_slug: &str,
    key: &UnblockKey,
    summary: &str,
    branch_ref: Option<&str>,
    outcome: &'static str,
    denial_note: Option<&str>,
) -> DispatchEvent {
    let body = format!("Sub-goal {sub_goal_slug} completed ({outcome})");
    let mut envelope = MatrixEventEnvelope::state("goal.subgoal.completed", ctx.now, &body)
        .with_detail_field("parent_goal_id", ctx.parent_goal_id.as_str())
        .with_detail_field("sub_goal_id", sub_goal_slug)
        .with_detail_field("group_id", ctx.group_id.as_str())
        .with_detail_field(
            "unblock_key",
            serde_json::to_value(key).unwrap_or(serde_json::Value::Null),
        )
        .with_detail_field("outcome", outcome)
        .with_detail_field("summary", summary);
    if let Some(bref) = branch_ref {
        envelope = envelope.with_detail_field("branch_ref", bref);
    }
    if let Some(note) = denial_note {
        envelope = envelope.with_detail_field("denial_note", note);
    }
    let envelope = if let Some(thread) = &ctx.thread_id {
        envelope.with_thread(thread.clone())
    } else {
        envelope.with_thread(ctx.parent_goal_id.clone())
    };
    DispatchEvent {
        room_id: ctx.room_id.clone(),
        envelope,
    }
}

// ---------------------------------------------------------------------------
// Child sub-goal phase
// ---------------------------------------------------------------------------

/// Phase assigned to a freshly-spawned child sub-goal.
///
/// The design calls for `AgentExecute`; the control-plane crate's
/// [`GoalPhase`] enum doesn't have that variant yet, so we map to
/// [`GoalPhase::Implementation`] as the closest-in-spirit existing variant.
/// A future chunk can add an explicit `AgentExecute` variant if desired.
pub const CHILD_SUB_GOAL_PHASE: GoalPhase = GoalPhase::Implementation;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subgoal::researcher::{RecallSnippet, ResearchRecall};
    use async_trait::async_trait;
    use symbiotic_core::protocol::{ChatMessage, LlmClient};
    use symbiotic_core::types::question_group::UnblockKey;
    use tempfile::TempDir;

    struct StubRecall;
    #[async_trait]
    impl ResearchRecall for StubRecall {
        async fn query(&self, _topic: &str, _top_k: usize) -> anyhow::Result<Vec<RecallSnippet>> {
            Ok(vec![RecallSnippet {
                source: "archive://test/fixture.md".into(),
                content: "prior art snippet".into(),
                score: 0.9,
            }])
        }
    }

    struct StubLlm {
        reply: String,
    }
    #[async_trait]
    impl LlmClient for StubLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _json_mode: bool,
        ) -> anyhow::Result<String> {
            Ok(self.reply.clone())
        }
    }

    fn build_dispatcher() -> (Dispatcher, TempDir, Arc<Mutex<GoalProcessManager>>) {
        let tmp = TempDir::new().unwrap();
        let mgr = Arc::new(Mutex::new(GoalProcessManager::new(
            tmp.path().to_path_buf(),
        )));
        let recall = Arc::new(StubRecall) as Arc<dyn ResearchRecall>;
        let llm = Arc::new(StubLlm {
            reply: "TL;DR: use the `oauth2` crate.\n\nDetails: it's idiomatic.".into(),
        }) as Arc<dyn LlmClient>;
        let researcher = Arc::new(ResearcherAgent::new(recall, llm));
        let disp = Dispatcher::new(SpawnBudget::default(), researcher, Arc::clone(&mgr));
        (disp, tmp, mgr)
    }

    fn ctx(parent: &str, group: &str) -> UnblockedContext {
        let mut answers = HashMap::new();
        answers.insert(0, "yes".to_string());
        UnblockedContext {
            parent_goal_id: parent.to_string(),
            group_id: group.to_string(),
            thread_id: Some("thread-parent".into()),
            room_id: "!parent-room:example".into(),
            now: 1_700_000_000,
            answers,
        }
    }

    #[tokio::test]
    async fn research_only_variant_routes_to_researcher_and_emits_events() {
        let (disp, _tmp, mgr) = build_dispatcher();
        let key = UnblockKey::ResearchOnly {
            question: "which OAuth crate?".into(),
        };

        let outcome = disp
            .on_unblocked_with_key(ctx("goal-parent", "grp-1"), key)
            .await
            .expect("routing succeeds");

        assert_eq!(outcome.verdict, Verdict::Success);
        assert!(outcome.child_goal_slug.is_some());
        assert!(outcome.archive_note_draft.is_some());

        // Two events: spawned + completed.
        assert_eq!(outcome.events.len(), 2);
        let kinds: Vec<_> = outcome
            .events
            .iter()
            .map(|e| e.envelope.sym.a.clone().unwrap_or_default())
            .collect();
        assert!(kinds.contains(&"goal.subgoal.spawned".to_string()));
        assert!(kinds.contains(&"goal.subgoal.completed".to_string()));

        // Child goal persisted with parent_goal_id set.
        let slug = outcome.child_goal_slug.unwrap();
        let guard = mgr.lock().unwrap();
        let child = guard.get(&slug).expect("child goal persisted");
        assert_eq!(child.parent_goal_id.as_deref(), Some("goal-parent"));
        assert!(matches!(
            child.unblock_key,
            Some(UnblockKey::ResearchOnly { .. })
        ));
    }

    #[tokio::test]
    async fn exploratory_variant_with_default_backend_emits_backend_not_ready() {
        // The default Exploratory backend is `NotReadyExploratoryBackend`,
        // which surfaces `BackendNotReady` until the T116 sub-goal authoring
        // role is wired (see `subgoal/exploratory.rs` module docs).
        let (disp, _tmp, mgr) = build_dispatcher();
        let key = UnblockKey::Exploratory {
            topic: "frontend".into(),
        };
        let outcome = disp
            .on_unblocked_with_key(ctx("p", "g"), key)
            .await
            .expect("route");
        assert_eq!(outcome.verdict, Verdict::BackendNotReady);

        // Exploratory creates the child goal up-front (matches ResearchOnly)
        // so the event stream + control plane stay consistent regardless of
        // backend readiness.
        let slug = outcome.child_goal_slug.expect("child slug minted");
        {
            let guard = mgr.lock().unwrap();
            let child = guard.get(&slug).expect("child persisted even on not-ready");
            assert_eq!(child.parent_goal_id.as_deref(), Some("p"));
        }

        // Two events: spawned + failed(BackendNotReady).
        assert_eq!(outcome.events.len(), 2);
        let failed = outcome
            .events
            .iter()
            .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.failed"))
            .expect("failure event present");
        let detail = failed.envelope.sym.d.as_ref().unwrap();
        assert_eq!(
            detail.get("outcome").and_then(|v| v.as_str()),
            Some("BackendNotReady")
        );
        let reason = detail.get("reason").and_then(|v| v.as_str()).unwrap();
        assert!(
            reason.contains("T116 primitives pending"),
            "unexpected reason: {reason}"
        );
    }

    #[tokio::test]
    async fn exploratory_variant_with_success_backend_emits_completed() {
        // Prove the production wire-in path: when a real
        // `ExploratoryBackend` is plugged in, the dispatcher emits the
        // standard spawned + completed pair (event channel of the
        // three-channel merge-back; Archive write + thread pill are §08).
        use super::super::exploratory::{
            ExploratoryBackend, ExploratoryError, ExploratoryOutcome, ExploratoryRequest,
        };

        struct StubSwarmBackend;
        #[async_trait]
        impl ExploratoryBackend for StubSwarmBackend {
            async fn execute(
                &self,
                request: ExploratoryRequest,
            ) -> Result<ExploratoryOutcome, ExploratoryError> {
                Ok(ExploratoryOutcome {
                    sub_goal_id: request.sub_goal_id,
                    summary: format!("explored topic '{}'", request.topic),
                    artifact_refs: vec!["archive://episodic/subgoals/sg-x/result.md".to_string()],
                })
            }
        }

        let tmp = TempDir::new().unwrap();
        let mgr = Arc::new(Mutex::new(GoalProcessManager::new(
            tmp.path().to_path_buf(),
        )));
        let recall = Arc::new(StubRecall) as Arc<dyn ResearchRecall>;
        let llm = Arc::new(StubLlm { reply: "ok".into() }) as Arc<dyn LlmClient>;
        let researcher = Arc::new(ResearcherAgent::new(recall, llm));
        let exploratory: Arc<dyn ExploratoryBackend> = Arc::new(StubSwarmBackend);
        let disp = Dispatcher::new_with_exploratory(
            SpawnBudget::default(),
            researcher,
            exploratory,
            Arc::clone(&mgr),
        );

        let key = UnblockKey::Exploratory {
            topic: "frontend-framework".into(),
        };
        let outcome = disp
            .on_unblocked_with_key(ctx("p", "g"), key)
            .await
            .expect("routing succeeds");
        assert_eq!(outcome.verdict, Verdict::Success);
        let slug = outcome.child_goal_slug.expect("child goal minted");
        assert!(slug.starts_with("sg-"));

        // spawned + completed
        assert_eq!(outcome.events.len(), 2);
        let completed = outcome
            .events
            .iter()
            .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.completed"))
            .expect("completion event present");
        let detail = completed.envelope.sym.d.as_ref().unwrap();
        assert_eq!(
            detail.get("outcome").and_then(|v| v.as_str()),
            Some("Success")
        );
        assert!(detail
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("frontend-framework"));
    }

    #[tokio::test]
    async fn attached_repo_variant_with_default_backend_emits_backend_not_ready() {
        // §07 wired the AttachedRepo branch via the AttachedRepoBackend trait.
        // The default backend is `NotReadyAttachedRepoBackend`, which surfaces
        // `BackendNotReady` until the wiring site plugs in a
        // `MirrorPushAttachedRepoBackend` (which composes T126's
        // `mirror_push_with_approval` + `ApprovalGate` + `GitPushSession`).
        let (disp, _tmp, mgr) = build_dispatcher();
        let key = UnblockKey::AttachedRepo {
            repo_id: "repo-a".into(),
            branch_hint: "main".into(),
            requires_approval: true,
        };
        let outcome = disp
            .on_unblocked_with_key(ctx("p", "g"), key)
            .await
            .expect("route");
        assert_eq!(outcome.verdict, Verdict::BackendNotReady);

        // Child goal is minted up-front (matches the ResearchOnly +
        // Exploratory paths) so the event stream + control plane stay
        // consistent regardless of backend readiness.
        let slug = outcome.child_goal_slug.expect("child slug minted");
        {
            let guard = mgr.lock().unwrap();
            let child = guard.get(&slug).expect("child persisted even on not-ready");
            assert_eq!(child.parent_goal_id.as_deref(), Some("p"));
            assert!(matches!(
                child.unblock_key,
                Some(UnblockKey::AttachedRepo { .. })
            ));
        }

        // Two events: spawned + failed(BackendNotReady).
        assert_eq!(outcome.events.len(), 2);
        let failed = outcome
            .events
            .iter()
            .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.failed"))
            .expect("failure event present");
        let detail = failed.envelope.sym.d.as_ref().unwrap();
        assert_eq!(
            detail.get("outcome").and_then(|v| v.as_str()),
            Some("BackendNotReady")
        );
        let reason = detail
            .get("reason")
            .and_then(|v| v.as_str())
            .expect("reason populated");
        assert!(
            reason.contains("MirrorPushAttachedRepoBackend"),
            "expected reason to name the production backend, got: {reason}"
        );
    }

    #[tokio::test]
    async fn attached_repo_variant_with_explicit_backend_emits_completed_with_branch_ref() {
        // Prove the §07 production wire-in path: when a real
        // `AttachedRepoBackend` is plugged in via `Dispatcher::new_with_attached_repo`,
        // the dispatcher emits the standard spawned + completed pair with a
        // Success outcome that carries the `branch_ref` we got back from the
        // backend.
        use super::super::attached_repo::{
            AttachedRepoBackend, AttachedRepoError, AttachedRepoOutcome, AttachedRepoRequest,
        };

        struct StubAttachedRepoBackend;
        #[async_trait]
        impl AttachedRepoBackend for StubAttachedRepoBackend {
            async fn execute(
                &self,
                request: AttachedRepoRequest,
            ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
                Ok(AttachedRepoOutcome::Success {
                    sub_goal_id: request.sub_goal_id,
                    summary: format!("pushed {}", request.branch_hint),
                    branch_ref: format!("refs/heads/{}", request.branch_hint),
                })
            }
        }

        let tmp = TempDir::new().unwrap();
        let mgr = Arc::new(Mutex::new(GoalProcessManager::new(
            tmp.path().to_path_buf(),
        )));
        let recall = Arc::new(StubRecall) as Arc<dyn ResearchRecall>;
        let llm = Arc::new(StubLlm { reply: "ok".into() }) as Arc<dyn LlmClient>;
        let researcher = Arc::new(ResearcherAgent::new(recall, llm));
        let backend: Arc<dyn AttachedRepoBackend> = Arc::new(StubAttachedRepoBackend);
        let disp = Dispatcher::new_with_attached_repo(
            SpawnBudget::default(),
            researcher,
            backend,
            Arc::clone(&mgr),
        );

        let key = UnblockKey::AttachedRepo {
            repo_id: "repo:saas".into(),
            branch_hint: "feature/auth".into(),
            requires_approval: true,
        };
        let outcome = disp
            .on_unblocked_with_key(ctx("p", "g"), key)
            .await
            .expect("routing succeeds");
        assert_eq!(outcome.verdict, Verdict::Success);
        let slug = outcome.child_goal_slug.expect("child goal minted");
        assert!(slug.starts_with("sg-"));

        // spawned + completed
        assert_eq!(outcome.events.len(), 2);
        let completed = outcome
            .events
            .iter()
            .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.completed"))
            .expect("completion event present");
        let detail = completed.envelope.sym.d.as_ref().unwrap();
        assert_eq!(
            detail.get("outcome").and_then(|v| v.as_str()),
            Some("Success")
        );
        assert_eq!(
            detail.get("branch_ref").and_then(|v| v.as_str()),
            Some("refs/heads/feature/auth")
        );
        assert!(detail
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("feature/auth"));
    }

    #[tokio::test]
    async fn attached_repo_variant_scope_denied_short_circuits_to_failed() {
        // Hard security contract from the §07 chunk spec: when the backend
        // surfaces ScopeDenied, the dispatcher emits a failed event with the
        // typed scope-denied reason — distinct from BackendNotReady (which
        // means "wiring isn't done") and from AttachedRepoError (which is
        // catch-all).
        use super::super::attached_repo::{
            AttachedRepoBackend, AttachedRepoError, AttachedRepoOutcome, AttachedRepoRequest,
        };

        struct ScopeDeniedBackend;
        #[async_trait]
        impl AttachedRepoBackend for ScopeDeniedBackend {
            async fn execute(
                &self,
                request: AttachedRepoRequest,
            ) -> Result<AttachedRepoOutcome, AttachedRepoError> {
                Err(AttachedRepoError::ScopeDenied {
                    repo_id: request.repo_id,
                })
            }
        }

        let tmp = TempDir::new().unwrap();
        let mgr = Arc::new(Mutex::new(GoalProcessManager::new(
            tmp.path().to_path_buf(),
        )));
        let recall = Arc::new(StubRecall) as Arc<dyn ResearchRecall>;
        let llm = Arc::new(StubLlm { reply: "ok".into() }) as Arc<dyn LlmClient>;
        let researcher = Arc::new(ResearcherAgent::new(recall, llm));
        let disp = Dispatcher::new_with_attached_repo(
            SpawnBudget::default(),
            researcher,
            Arc::new(ScopeDeniedBackend),
            Arc::clone(&mgr),
        );

        let key = UnblockKey::AttachedRepo {
            repo_id: "repo:locked".into(),
            branch_hint: "main".into(),
            requires_approval: true,
        };
        let outcome = disp
            .on_unblocked_with_key(ctx("p", "g"), key)
            .await
            .expect("routes");
        assert_eq!(outcome.verdict, Verdict::Failed);
        let failed = outcome
            .events
            .iter()
            .find(|e| e.envelope.sym.a.as_deref() == Some("goal.subgoal.failed"))
            .expect("failure event");
        let reason = failed
            .envelope
            .sym
            .d
            .as_ref()
            .unwrap()
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        assert!(
            reason.contains("scope-denied"),
            "expected scope-denied reason, got: {reason}"
        );
        assert!(
            reason.contains("repo:locked"),
            "expected repo_id in reason, got: {reason}"
        );
    }

    #[tokio::test]
    async fn composite_variant_emits_unsupported_backend_failure() {
        let (disp, _tmp, _mgr) = build_dispatcher();
        let key = UnblockKey::Composite {
            children: vec![UnblockKey::ResearchOnly {
                question: "q".into(),
            }],
        };
        let outcome = disp
            .on_unblocked_with_key(ctx("p", "g"), key)
            .await
            .expect("route");
        assert_eq!(outcome.verdict, Verdict::UnsupportedBackend);
        let reason = outcome.events[0]
            .envelope
            .sym
            .d
            .as_ref()
            .unwrap()
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        assert!(reason.contains("Composite"));
        assert!(reason.contains("§09"));
    }

    #[tokio::test]
    async fn budget_exceeded_marks_sub_goal_queued() {
        let tmp = TempDir::new().unwrap();
        let mgr = Arc::new(Mutex::new(GoalProcessManager::new(
            tmp.path().to_path_buf(),
        )));
        let recall = Arc::new(StubRecall) as Arc<dyn ResearchRecall>;
        let llm = Arc::new(StubLlm { reply: "ok".into() }) as Arc<dyn LlmClient>;
        let researcher = Arc::new(ResearcherAgent::new(recall, llm));
        let budget = SpawnBudget::new(super::super::spawn_budget::SpawnBudgetConfig {
            max_concurrent_subgoals: 0, // block everything
            max_concurrent_per_attached_repo: 1,
            max_research_only: 0,
        });
        let disp = Dispatcher::new(budget, researcher, mgr);

        let key = UnblockKey::ResearchOnly {
            question: "x".into(),
        };
        let outcome = disp
            .on_unblocked_with_key(ctx("p", "g"), key)
            .await
            .expect("routing should succeed even when queued");

        assert_eq!(outcome.verdict, Verdict::Queued);
        assert!(outcome.child_goal_slug.is_none());
        assert_eq!(outcome.events.len(), 1);
        let env = &outcome.events[0].envelope;
        assert_eq!(env.sym.a.as_deref(), Some("goal.subgoal.failed"));
        assert_eq!(
            env.sym
                .d
                .as_ref()
                .unwrap()
                .get("outcome")
                .and_then(|v| v.as_str()),
            Some("Queued")
        );
    }

    #[tokio::test]
    async fn remember_and_lookup_round_trip() {
        let (disp, _tmp, _mgr) = build_dispatcher();
        let key = UnblockKey::ResearchOnly {
            question: "x".into(),
        };
        disp.remember_group("grp-42", key.clone());
        assert_eq!(disp.lookup_group("grp-42"), Some(key));
        disp.forget_group("grp-42");
        assert!(disp.lookup_group("grp-42").is_none());
    }

    #[tokio::test]
    async fn on_unblocked_without_remembered_group_errors() {
        let (disp, _tmp, _mgr) = build_dispatcher();
        let err = disp
            .on_unblocked(ctx("p", "missing-group"))
            .await
            .unwrap_err();
        assert!(matches!(err, DispatcherError::UnknownGroup(_)));
    }

    #[tokio::test]
    async fn research_failure_emits_failed_event() {
        struct FailingLlm;
        #[async_trait]
        impl LlmClient for FailingLlm {
            async fn chat(
                &self,
                _messages: &[ChatMessage],
                _json_mode: bool,
            ) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("llm exploded"))
            }
        }

        let tmp = TempDir::new().unwrap();
        let mgr = Arc::new(Mutex::new(GoalProcessManager::new(
            tmp.path().to_path_buf(),
        )));
        let recall = Arc::new(StubRecall) as Arc<dyn ResearchRecall>;
        let llm = Arc::new(FailingLlm) as Arc<dyn LlmClient>;
        let researcher = Arc::new(ResearcherAgent::new(recall, llm));
        let disp = Dispatcher::new(SpawnBudget::default(), researcher, mgr);

        let outcome = disp
            .on_unblocked_with_key(
                ctx("p", "g"),
                UnblockKey::ResearchOnly {
                    question: "q".into(),
                },
            )
            .await
            .expect("routing");
        assert_eq!(outcome.verdict, Verdict::Failed);
        assert!(outcome.archive_note_draft.is_none());
        // spawned + failed
        assert_eq!(outcome.events.len(), 2);
        let last = &outcome.events[1].envelope;
        assert_eq!(last.sym.a.as_deref(), Some("goal.subgoal.failed"));
    }
}
