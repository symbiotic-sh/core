//! Per-daemon concurrency caps for sub-goal spawning (T130 §05).
//!
//! The [`SpawnBudget`] enforces the capacity control described in design §4.2:
//!
//! - `max_concurrent_subgoals` — daemon-wide ceiling across every backend.
//! - `max_concurrent_per_attached_repo` — serialises pushes against any one
//!   attached repo (default `1`).
//! - `max_research_only` — separate, higher cap for the cheap read-only
//!   research backend (default `8`).
//!
//! Slot acquisition returns a [`BudgetGuard`]; dropping the guard releases
//! the slot. The dispatcher calls [`SpawnBudget::try_reserve`] before
//! dispatching to any backend; if the reservation fails the sub-goal is
//! queued and retried on the next child-completion event (§4.2).
//!
//! The budget is intentionally lock-free: every counter is an atomic
//! `usize`, so the dispatcher can reserve slots from any thread without
//! contention. Only the `AttachedRepo` backend needs per-key accounting;
//! that map is held behind a `Mutex` because repo ids are dynamic. The
//! `Mutex` is never held across `await` points.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use symbiotic_core::types::question_group::UnblockKey;
use thiserror::Error;

/// Default daemon-wide cap (design §4.2).
pub const DEFAULT_MAX_CONCURRENT_SUBGOALS: usize = 4;
/// Default per-attached-repo serialisation cap (design §4.2).
pub const DEFAULT_MAX_CONCURRENT_PER_ATTACHED_REPO: usize = 1;
/// Default ResearchOnly cap — cheap, bumped up (design §4.2).
pub const DEFAULT_MAX_RESEARCH_ONLY: usize = 8;

/// Configuration for a [`SpawnBudget`]. Field names mirror the design doc
/// so operator overrides feed in 1:1.
#[derive(Debug, Clone)]
pub struct SpawnBudgetConfig {
    pub max_concurrent_subgoals: usize,
    pub max_concurrent_per_attached_repo: usize,
    pub max_research_only: usize,
}

impl Default for SpawnBudgetConfig {
    fn default() -> Self {
        Self {
            max_concurrent_subgoals: DEFAULT_MAX_CONCURRENT_SUBGOALS,
            max_concurrent_per_attached_repo: DEFAULT_MAX_CONCURRENT_PER_ATTACHED_REPO,
            max_research_only: DEFAULT_MAX_RESEARCH_ONLY,
        }
    }
}

/// Error returned when a budget reservation fails.
///
/// Each variant names the specific cap that was exhausted so the
/// dispatcher's `queued` status carries a useful reason.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum BudgetExceeded {
    #[error("daemon-wide concurrent sub-goal cap reached ({cap})")]
    GlobalCap { cap: usize },
    #[error("research-only cap reached ({cap})")]
    ResearchCap { cap: usize },
    #[error("attached-repo '{repo_id}' cap reached ({cap})")]
    AttachedRepoCap { repo_id: String, cap: usize },
}

/// Per-daemon budget tracker. Cheap to clone — the state lives behind
/// `Arc`s internally.
#[derive(Clone)]
pub struct SpawnBudget {
    inner: Arc<BudgetInner>,
}

#[derive(Debug)]
struct BudgetInner {
    config: SpawnBudgetConfig,
    active_total: AtomicUsize,
    active_research: AtomicUsize,
    active_per_repo: Mutex<HashMap<String, usize>>,
}

impl SpawnBudget {
    /// Construct a new budget from explicit config. Use [`SpawnBudget::default`]
    /// for the design §4.2 defaults.
    pub fn new(config: SpawnBudgetConfig) -> Self {
        Self {
            inner: Arc::new(BudgetInner {
                config,
                active_total: AtomicUsize::new(0),
                active_research: AtomicUsize::new(0),
                active_per_repo: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Current daemon-wide active count (observability helper).
    pub fn active_total(&self) -> usize {
        self.inner.active_total.load(Ordering::Acquire)
    }

    /// Current ResearchOnly active count (observability helper).
    pub fn active_research(&self) -> usize {
        self.inner.active_research.load(Ordering::Acquire)
    }

    /// Current active count against a specific attached repo.
    pub fn active_attached_repo(&self, repo_id: &str) -> usize {
        self.inner
            .active_per_repo
            .lock()
            .map(|m| m.get(repo_id).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Attempt to reserve a slot for the given [`UnblockKey`].
    ///
    /// On success returns a [`BudgetGuard`] that releases the slot on drop.
    /// On failure returns [`BudgetExceeded`] naming the exhausted cap.
    ///
    /// `Composite` is treated as a single reservation at the parent level;
    /// the dispatcher recurses on the children separately, each taking its
    /// own slot. This matches §4.1's "fan out" note — Composite does not
    /// "consume" the budget on its own, it just gates entry.
    pub fn try_reserve(&self, key: &UnblockKey) -> Result<BudgetGuard, BudgetExceeded> {
        // Global cap first — applies to every backend.
        let cfg = &self.inner.config;
        let prev_total = self.inner.active_total.fetch_add(1, Ordering::AcqRel);
        if prev_total >= cfg.max_concurrent_subgoals {
            self.inner.active_total.fetch_sub(1, Ordering::AcqRel);
            return Err(BudgetExceeded::GlobalCap {
                cap: cfg.max_concurrent_subgoals,
            });
        }

        // Per-variant sub-cap.
        let scope = match key {
            UnblockKey::ResearchOnly { .. } => {
                let prev = self.inner.active_research.fetch_add(1, Ordering::AcqRel);
                if prev >= cfg.max_research_only {
                    self.inner.active_research.fetch_sub(1, Ordering::AcqRel);
                    self.inner.active_total.fetch_sub(1, Ordering::AcqRel);
                    return Err(BudgetExceeded::ResearchCap {
                        cap: cfg.max_research_only,
                    });
                }
                BudgetScope::Research
            }
            UnblockKey::AttachedRepo { repo_id, .. } => {
                let mut map = self.inner.active_per_repo.lock().expect("budget lock");
                let entry = map.entry(repo_id.clone()).or_insert(0);
                if *entry >= cfg.max_concurrent_per_attached_repo {
                    self.inner.active_total.fetch_sub(1, Ordering::AcqRel);
                    return Err(BudgetExceeded::AttachedRepoCap {
                        repo_id: repo_id.clone(),
                        cap: cfg.max_concurrent_per_attached_repo,
                    });
                }
                *entry += 1;
                BudgetScope::AttachedRepo(repo_id.clone())
            }
            UnblockKey::Exploratory { .. } | UnblockKey::Composite { .. } => BudgetScope::Shared,
        };

        Ok(BudgetGuard {
            inner: Arc::clone(&self.inner),
            scope,
            released: false,
        })
    }
}

impl Default for SpawnBudget {
    fn default() -> Self {
        Self::new(SpawnBudgetConfig::default())
    }
}

/// Which per-variant counter a guard belongs to, so drop releases the
/// right one.
#[derive(Debug, Clone)]
enum BudgetScope {
    Shared,
    Research,
    AttachedRepo(String),
}

/// RAII guard: releases the reserved slot when dropped.
#[derive(Debug)]
pub struct BudgetGuard {
    inner: Arc<BudgetInner>,
    scope: BudgetScope,
    released: bool,
}

impl BudgetGuard {
    /// Release the slot eagerly (before drop). Idempotent.
    pub fn release(mut self) {
        self.release_internal();
    }

    fn release_internal(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.inner.active_total.fetch_sub(1, Ordering::AcqRel);
        match &self.scope {
            BudgetScope::Research => {
                self.inner.active_research.fetch_sub(1, Ordering::AcqRel);
            }
            BudgetScope::AttachedRepo(repo_id) => {
                if let Ok(mut map) = self.inner.active_per_repo.lock() {
                    if let Some(entry) = map.get_mut(repo_id) {
                        *entry = entry.saturating_sub(1);
                        if *entry == 0 {
                            map.remove(repo_id);
                        }
                    }
                }
            }
            BudgetScope::Shared => {}
        }
    }
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        self.release_internal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn research(q: &str) -> UnblockKey {
        UnblockKey::ResearchOnly {
            question: q.to_string(),
        }
    }

    fn attached(repo_id: &str) -> UnblockKey {
        UnblockKey::AttachedRepo {
            repo_id: repo_id.to_string(),
            branch_hint: "main".to_string(),
            requires_approval: true,
        }
    }

    fn exploratory(topic: &str) -> UnblockKey {
        UnblockKey::Exploratory {
            topic: topic.to_string(),
        }
    }

    #[test]
    fn defaults_match_design_section_4_2() {
        let cfg = SpawnBudgetConfig::default();
        assert_eq!(cfg.max_concurrent_subgoals, 4);
        assert_eq!(cfg.max_concurrent_per_attached_repo, 1);
        assert_eq!(cfg.max_research_only, 8);
    }

    #[test]
    fn reserve_and_drop_round_trip_releases_slot() {
        let budget = SpawnBudget::default();
        let guard = budget.try_reserve(&research("q1")).expect("room for 1");
        assert_eq!(budget.active_total(), 1);
        assert_eq!(budget.active_research(), 1);
        drop(guard);
        assert_eq!(budget.active_total(), 0);
        assert_eq!(budget.active_research(), 0);
    }

    #[test]
    fn explicit_release_is_idempotent_with_drop() {
        let budget = SpawnBudget::default();
        let guard = budget.try_reserve(&exploratory("swarm")).expect("ok");
        guard.release();
        assert_eq!(budget.active_total(), 0);
    }

    #[test]
    fn global_cap_rejects_past_total() {
        let budget = SpawnBudget::new(SpawnBudgetConfig {
            max_concurrent_subgoals: 2,
            max_concurrent_per_attached_repo: 1,
            max_research_only: 8,
        });

        let _g1 = budget.try_reserve(&research("q1")).unwrap();
        let _g2 = budget.try_reserve(&research("q2")).unwrap();
        let err = budget.try_reserve(&research("q3")).unwrap_err();
        assert_eq!(err, BudgetExceeded::GlobalCap { cap: 2 });
        assert_eq!(budget.active_total(), 2, "failed reservation must not leak");
    }

    #[test]
    fn research_cap_enforced_below_global_cap() {
        let budget = SpawnBudget::new(SpawnBudgetConfig {
            max_concurrent_subgoals: 8,
            max_concurrent_per_attached_repo: 1,
            max_research_only: 2,
        });

        let _g1 = budget.try_reserve(&research("q1")).unwrap();
        let _g2 = budget.try_reserve(&research("q2")).unwrap();
        let err = budget.try_reserve(&research("q3")).unwrap_err();
        assert_eq!(err, BudgetExceeded::ResearchCap { cap: 2 });
        assert_eq!(budget.active_research(), 2);
        assert_eq!(budget.active_total(), 2, "failed reservation must not leak");
    }

    #[test]
    fn attached_repo_cap_is_per_key() {
        let budget = SpawnBudget::new(SpawnBudgetConfig {
            max_concurrent_subgoals: 8,
            max_concurrent_per_attached_repo: 1,
            max_research_only: 8,
        });

        let _g1 = budget.try_reserve(&attached("repo-a")).unwrap();
        // Different repo — should succeed.
        let _g2 = budget.try_reserve(&attached("repo-b")).unwrap();
        // Same repo as g1 — must hit the per-repo cap.
        let err = budget.try_reserve(&attached("repo-a")).unwrap_err();
        match err {
            BudgetExceeded::AttachedRepoCap { repo_id, cap } => {
                assert_eq!(repo_id, "repo-a");
                assert_eq!(cap, 1);
            }
            other => panic!("expected AttachedRepoCap, got {other:?}"),
        }
        assert_eq!(budget.active_attached_repo("repo-a"), 1);
        assert_eq!(budget.active_attached_repo("repo-b"), 1);
    }

    #[test]
    fn releasing_attached_repo_slot_lets_new_reservation_through() {
        let budget = SpawnBudget::new(SpawnBudgetConfig {
            max_concurrent_subgoals: 4,
            max_concurrent_per_attached_repo: 1,
            max_research_only: 8,
        });

        let g1 = budget.try_reserve(&attached("repo-a")).unwrap();
        assert!(budget.try_reserve(&attached("repo-a")).is_err());
        drop(g1);
        // Now room for another.
        let _g2 = budget.try_reserve(&attached("repo-a")).unwrap();
        assert_eq!(budget.active_attached_repo("repo-a"), 1);
    }
}
