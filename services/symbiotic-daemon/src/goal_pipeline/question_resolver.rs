//! `QuestionResolver` — watches active `QuestionGroup`s, detects
//! resolution per [`ResolutionMode`], and emits the `goal.unblocked`
//! event (or `goal.question_group.expired` on grace timeout).
//!
//! This module is the daemon-internal sibling of the Inquisitor: the
//! Inquisitor *produces* `QuestionGroup`s (§3.1); this resolver consumes
//! incoming `goal.answer` events, tracks per-question state, and fires
//! downstream events when a group's `resolution_mode` is satisfied.
//!
//! # Design alignment
//!
//! - §3.2.1 — per-question state machine
//!   (`Unanswered | Drafted | Submitted | Skipped`). Drafted state never
//!   crosses the wire so the resolver only tracks `Submitted | Skipped`
//!   deterministically.
//! - §3.2.2 — multiple groups pending simultaneously; each resolves
//!   independently. This resolver holds an in-memory map keyed by
//!   `group_id` and never cross-blocks.
//! - §3.3 — grace × severity × confidence auto-decision. A dedicated
//!   [`GraceConfig`] bundle captures the operator-configurable defaults.
//!   `Critical` severity never auto-resolves. Confidence < `fail_threshold`
//!   forces [`AutoDecision::EscalatedToOperator`] regardless of severity.
//! - §7 — emits `goal.unblocked` (answers map) and
//!   `goal.question_group.expired` (grace expired without auto-accept).
//!
//! # Where state lives
//!
//! Per the chunk spec: this resolver owns no long-running state beyond the
//! in-flight group map. On daemon restart the map starts empty; callers are
//! expected to re-register active groups from persisted `management_store`
//! data + replay unresolved `goal.answer` events from the thread room. No
//! new persistence store is introduced in §04.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use symbiotic_core::types::question_group::{
    AnnotatedQuestion, AutoDecision, AutoResolutionRecord, QuestionGroup, QuestionSeverity,
    ResolutionMode,
};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Default grace period for `Decision`-severity questions (§3.3).
pub const DEFAULT_GRACE_DECISION_SECS: u64 = 10 * 60;

/// Default grace period for `Informational`-severity questions (§3.3).
pub const DEFAULT_GRACE_INFORMATIONAL_SECS: u64 = 30 * 60;

/// Default grace period for `Trivial`-severity questions (§3.3).
pub const DEFAULT_GRACE_TRIVIAL_SECS: u64 = 2 * 60;

/// Default confidence floor below which auto-accept is disabled and the
/// question escalates immediately (§3.3).
pub const DEFAULT_FAIL_THRESHOLD: f32 = 0.70;

// ---------------------------------------------------------------------------
// Per-question state machine
// ---------------------------------------------------------------------------

/// Wire-visible answer states tracked by the resolver.
///
/// `Unanswered | Drafted` live only on the client (§3.2.1). The daemon sees
/// `Submitted` (with an `answer`) and `Skipped` (with `skipped: true`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AnswerState {
    /// No `goal.answer` has arrived for this index yet.
    Unanswered,
    /// Operator explicitly submitted an answer string.
    Submitted(String),
    /// Operator explicitly skipped (legal for `AnyOne` / `MajoritySignal`).
    Skipped,
    /// Auto-accepted after grace period with the recommendation (§3.3).
    AutoAccepted(String),
}

impl AnswerState {
    /// True when the operator's (or the auto-accept's) verdict is in.
    fn is_resolved(&self) -> bool {
        !matches!(self, Self::Unanswered)
    }

    /// True when the resolution counts towards `ResolutionMode` satisfaction.
    ///
    /// `Skipped` satisfies `AnyOne`/`MajoritySignal` at the group level but
    /// is a legal terminal state per §3.2.1 (so the group as a whole
    /// resolves even if some questions were skipped).
    fn counts_as_answer(&self) -> bool {
        matches!(self, Self::Submitted(_) | Self::AutoAccepted(_))
    }

    /// Return the answer text if resolved with one. `Skipped` → None.
    fn answer_text(&self) -> Option<&str> {
        match self {
            Self::Submitted(s) | Self::AutoAccepted(s) => Some(s.as_str()),
            Self::Unanswered | Self::Skipped => None,
        }
    }
}

// ---------------------------------------------------------------------------
// QuestionAnswer — input event shape
// ---------------------------------------------------------------------------

/// Shape of an incoming `goal.answer` event routed into the resolver.
///
/// Deliberately decoupled from `MatrixEventEnvelope` — callers adapt the
/// wire event into this struct at the router boundary. That keeps the
/// resolver unit-testable without Matrix transport dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionAnswer {
    pub group_id: String,
    pub question_index: usize,
    /// `None` means `skipped: true` on the wire.
    pub answer: Option<String>,
}

// ---------------------------------------------------------------------------
// GroupResolutionOutcome — what a `submit_answer` call produced
// ---------------------------------------------------------------------------

/// The terminal signal emitted when a group's `resolution_mode` is met.
///
/// `Unblocked` fires `goal.unblocked` (§3.2). `Expired` fires
/// `goal.question_group.expired` (§3.3). The caller is responsible for
/// translating these into daemon events on the bus.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupResolutionOutcome {
    /// Group resolved; emit `goal.unblocked`. Carries `question_index →
    /// answer` for every resolved question (skipped questions are omitted
    /// from the map since there's no value to propagate; the payload
    /// consumer can still look up the group's `ResolutionMode` to know
    /// how the group was satisfied).
    Unblocked {
        group_id: String,
        parent_goal_id: String,
        answers: HashMap<usize, String>,
        /// Auto-resolution records for any questions that were
        /// auto-accepted / auto-escalated during the resolution run.
        auto_records: Vec<AutoResolutionRecord>,
    },
    /// Group expired with at least one unresolvable question; emit
    /// `goal.question_group.expired`. Carries the question indexes that
    /// failed to resolve (below threshold confidence OR `Critical`
    /// severity that stayed unanswered).
    Expired {
        group_id: String,
        parent_goal_id: String,
        unresolved_indexes: Vec<usize>,
        auto_records: Vec<AutoResolutionRecord>,
    },
}

// ---------------------------------------------------------------------------
// ResolverError
// ---------------------------------------------------------------------------

/// Errors produced by the resolver.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResolverError {
    #[error("group '{0}' is not registered")]
    UnknownGroup(String),
    #[error("question index {0} out of range for group '{1}'")]
    IndexOutOfRange(usize, String),
    #[error("question index {0} in group '{1}' already resolved (audit integrity)")]
    AlreadyResolved(usize, String),
    #[error(
        "question index {0} in group '{1}' has severity=Critical — Skipped is not a legal state"
    )]
    CannotSkipCritical(usize, String),
    #[error(
        "question index {0} in group '{1}' has mode=AllRequired — Skipped is not a legal state"
    )]
    CannotSkipAllRequired(usize, String),
}

// ---------------------------------------------------------------------------
// GraceConfig — operator-configurable auto-decision knobs
// ---------------------------------------------------------------------------

/// Severity × confidence thresholds driving the auto-accept / auto-fail
/// behaviour in §3.3.
#[derive(Debug, Clone, PartialEq)]
pub struct GraceConfig {
    /// Confidence floor below which auto-accept is never allowed.
    pub fail_threshold: f32,
    pub grace_trivial: Duration,
    pub grace_informational: Duration,
    pub grace_decision: Duration,
    /// Strict-mode extends every grace to infinity (§3.3). When `true`,
    /// [`grace_for`] returns `None` for every non-Critical severity.
    pub strict_mode: bool,
}

impl Default for GraceConfig {
    fn default() -> Self {
        Self {
            fail_threshold: DEFAULT_FAIL_THRESHOLD,
            grace_trivial: Duration::from_secs(DEFAULT_GRACE_TRIVIAL_SECS),
            grace_informational: Duration::from_secs(DEFAULT_GRACE_INFORMATIONAL_SECS),
            grace_decision: Duration::from_secs(DEFAULT_GRACE_DECISION_SECS),
            strict_mode: false,
        }
    }
}

impl GraceConfig {
    /// Grace period for the given severity, or `None` if auto-accept is not
    /// possible (Critical severity; or strict-mode engaged for anything
    /// non-Critical).
    pub fn grace_for(&self, severity: QuestionSeverity) -> Option<Duration> {
        if self.strict_mode {
            return None;
        }
        match severity {
            QuestionSeverity::Critical => None,
            QuestionSeverity::Trivial => Some(self.grace_trivial),
            QuestionSeverity::Informational => Some(self.grace_informational),
            QuestionSeverity::Decision => Some(self.grace_decision),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-group state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct GroupState {
    group: QuestionGroup,
    answers: Vec<AnswerState>,
    /// Already emitted a terminal outcome — further submissions raise
    /// [`ResolverError::AlreadyResolved`].
    terminal: bool,
}

impl GroupState {
    fn new(group: QuestionGroup) -> Self {
        let n = group.questions.len();
        Self {
            group,
            answers: vec![AnswerState::Unanswered; n],
            terminal: false,
        }
    }

    fn question(&self, idx: usize) -> Option<&AnnotatedQuestion> {
        self.group.questions.get(idx)
    }

    /// Return true when the current answer vector satisfies the group's
    /// `ResolutionMode`.
    fn is_satisfied(&self) -> bool {
        let n_resolved = self.answers.iter().filter(|s| s.is_resolved()).count();
        let n_answered = self.answers.iter().filter(|s| s.counts_as_answer()).count();
        match &self.group.resolution_mode {
            ResolutionMode::AllRequired => n_resolved == self.answers.len(),
            ResolutionMode::AnyOne => n_answered >= 1,
            ResolutionMode::MajoritySignal { n } => n_answered >= *n as usize,
        }
    }

    /// Gather the answers map for `Unblocked` events.
    fn answer_map(&self) -> HashMap<usize, String> {
        self.answers
            .iter()
            .enumerate()
            .filter_map(|(i, a)| a.answer_text().map(|text| (i, text.to_string())))
            .collect()
    }

    /// Collect indexes of questions still unresolved (for `Expired` events).
    fn unresolved_indexes(&self) -> Vec<usize> {
        self.answers
            .iter()
            .enumerate()
            .filter_map(|(i, a)| if a.is_resolved() { None } else { Some(i) })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// ResolverSnapshot — read-only view for callers
// ---------------------------------------------------------------------------

/// Debug / observability snapshot of a registered group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolverSnapshot {
    pub group_id: String,
    pub parent_goal_id: String,
    pub total_questions: usize,
    pub resolved_count: usize,
    pub terminal: bool,
}

// ---------------------------------------------------------------------------
// QuestionResolver
// ---------------------------------------------------------------------------

/// Resolves `QuestionGroup`s as answers flow in.
///
/// One resolver per daemon process; groups register on creation and are
/// removed on terminal resolution. Internally a plain `HashMap<group_id,
/// GroupState>` — concurrency is the caller's responsibility (wrap in an
/// `Arc<Mutex<QuestionResolver>>` at the dispatch boundary).
#[derive(Debug, Default)]
pub struct QuestionResolver {
    groups: HashMap<String, GroupState>,
    grace: GraceConfig,
}

impl QuestionResolver {
    /// New resolver with default grace configuration.
    pub fn new() -> Self {
        Self::with_grace(GraceConfig::default())
    }

    /// New resolver with explicit grace configuration.
    pub fn with_grace(grace: GraceConfig) -> Self {
        Self {
            groups: HashMap::new(),
            grace,
        }
    }

    /// Access the grace configuration (read-only).
    pub fn grace_config(&self) -> &GraceConfig {
        &self.grace
    }

    /// Register a group. Idempotent by group id — an existing entry is
    /// overwritten (useful for restart re-registration).
    pub fn register(&mut self, group: QuestionGroup) {
        let id = group.group_id.clone();
        debug!(
            group_id = %id,
            parent_goal_id = %group.parent_goal_id,
            n_questions = group.questions.len(),
            "question_resolver: register group",
        );
        self.groups.insert(id, GroupState::new(group));
    }

    /// True when `group_id` is currently tracked.
    pub fn is_registered(&self, group_id: &str) -> bool {
        self.groups.contains_key(group_id)
    }

    /// Drop a group without emitting events — used on cancellation paths.
    pub fn forget(&mut self, group_id: &str) {
        self.groups.remove(group_id);
    }

    /// Observability snapshots for every active group.
    pub fn snapshot(&self) -> Vec<ResolverSnapshot> {
        self.groups
            .values()
            .map(|state| ResolverSnapshot {
                group_id: state.group.group_id.clone(),
                parent_goal_id: state.group.parent_goal_id.clone(),
                total_questions: state.group.questions.len(),
                resolved_count: state.answers.iter().filter(|a| a.is_resolved()).count(),
                terminal: state.terminal,
            })
            .collect()
    }

    /// Ingest a `goal.answer` event. Returns `Some(outcome)` if the group
    /// resolved (or expired) as a result of this submission; `None` when
    /// the group is still pending more answers.
    ///
    /// `now_iso` is the ISO 8601 timestamp to stamp on any
    /// [`AutoResolutionRecord`] the submission produces (confidence-floor
    /// auto-fails land here).
    pub fn submit_answer(
        &mut self,
        answer: QuestionAnswer,
        now_iso: &str,
    ) -> Result<Option<GroupResolutionOutcome>, ResolverError> {
        let state = self
            .groups
            .get_mut(&answer.group_id)
            .ok_or_else(|| ResolverError::UnknownGroup(answer.group_id.clone()))?;

        if state.terminal {
            return Err(ResolverError::AlreadyResolved(
                answer.question_index,
                answer.group_id.clone(),
            ));
        }

        let question = state.question(answer.question_index).ok_or_else(|| {
            ResolverError::IndexOutOfRange(answer.question_index, answer.group_id.clone())
        })?;
        let severity = question.severity;

        let slot = state
            .answers
            .get_mut(answer.question_index)
            .expect("index checked via question()");
        if slot.is_resolved() {
            return Err(ResolverError::AlreadyResolved(
                answer.question_index,
                answer.group_id.clone(),
            ));
        }

        // Skip-legality checks (§3.2.1).
        match (&answer.answer, &state.group.resolution_mode, severity) {
            (None, ResolutionMode::AllRequired, _) => {
                return Err(ResolverError::CannotSkipAllRequired(
                    answer.question_index,
                    answer.group_id.clone(),
                ));
            }
            (None, _, QuestionSeverity::Critical) => {
                return Err(ResolverError::CannotSkipCritical(
                    answer.question_index,
                    answer.group_id.clone(),
                ));
            }
            _ => {}
        }

        *slot = match answer.answer {
            Some(text) => AnswerState::Submitted(text),
            None => AnswerState::Skipped,
        };

        debug!(
            group_id = %answer.group_id,
            question_index = answer.question_index,
            resolution_mode = ?state.group.resolution_mode,
            "question_resolver: answer submitted",
        );

        if state.is_satisfied() {
            Ok(Some(Self::finalize_unblocked(state, Vec::new(), now_iso)))
        } else {
            Ok(None)
        }
    }

    /// Attempt grace-period auto-resolution for a group. Intended to be
    /// called by a tick loop (or a scheduled task) after each group's grace
    /// window has elapsed — callers pass the list of question indexes that
    /// have aged past their severity-specific grace threshold.
    ///
    /// The per-question decision follows §3.3:
    ///
    /// 1. `Critical` severity — never auto-resolves; returns
    ///    `Expired { unresolved_indexes: [...] }` once all Critical
    ///    questions cross the grace threshold without an operator answer.
    /// 2. confidence `< fail_threshold` — auto-fail → escalate; emits an
    ///    `AutoResolutionRecord { decision: EscalatedToOperator }` and the
    ///    question stays `Unanswered` (next tick will re-evaluate).
    /// 3. otherwise — auto-accept the recommendation (if any) as an
    ///    [`AnswerState::AutoAccepted`]; emits an
    ///    `AutoResolutionRecord { decision: Accepted }`.
    ///
    /// After processing the given indexes, if the group is now satisfied
    /// the returned outcome is `Unblocked`; if at least one question stays
    /// unresolved and its grace already expired without auto-accept
    /// possibility, the outcome is `Expired`.
    pub fn tick_grace(
        &mut self,
        group_id: &str,
        expired_indexes: &[usize],
        now_iso: &str,
    ) -> Result<Option<GroupResolutionOutcome>, ResolverError> {
        let state = self
            .groups
            .get_mut(group_id)
            .ok_or_else(|| ResolverError::UnknownGroup(group_id.to_string()))?;

        if state.terminal {
            return Err(ResolverError::AlreadyResolved(0, group_id.to_string()));
        }

        let fail_threshold = self.grace.fail_threshold;

        let mut auto_records = Vec::new();
        let mut saw_permanent_failure = false;

        for &idx in expired_indexes {
            let Some(question) = state.group.questions.get(idx).cloned() else {
                return Err(ResolverError::IndexOutOfRange(idx, group_id.to_string()));
            };
            let slot = state
                .answers
                .get_mut(idx)
                .ok_or_else(|| ResolverError::IndexOutOfRange(idx, group_id.to_string()))?;

            // Already resolved — grace doesn't un-resolve anything.
            if slot.is_resolved() {
                continue;
            }

            // Critical severity: always escalate; no auto-decision is
            // legal here (§3.3). The resolver marks this as a permanent
            // failure path so downstream code can fire `expired`.
            if question.severity == QuestionSeverity::Critical {
                warn!(
                    group_id,
                    question_index = idx,
                    "question_resolver: Critical severity grace expired — escalating"
                );
                auto_records.push(AutoResolutionRecord {
                    question_id: format!("{group_id}#{idx}"),
                    severity: question.severity,
                    confidence: question.confidence,
                    grace_period_secs: 0,
                    decision: AutoDecision::EscalatedToOperator,
                    resolved_at: now_iso.to_string(),
                });
                saw_permanent_failure = true;
                continue;
            }

            // Confidence-floor check: below threshold → escalate (no
            // auto-accept possible).
            if question.confidence < fail_threshold {
                warn!(
                    group_id,
                    question_index = idx,
                    confidence = question.confidence,
                    fail_threshold,
                    "question_resolver: confidence below threshold — escalating"
                );
                auto_records.push(AutoResolutionRecord {
                    question_id: format!("{group_id}#{idx}"),
                    severity: question.severity,
                    confidence: question.confidence,
                    grace_period_secs: 0,
                    decision: AutoDecision::EscalatedToOperator,
                    resolved_at: now_iso.to_string(),
                });
                saw_permanent_failure = true;
                continue;
            }

            // Strict mode or no grace defined for severity → can't
            // auto-accept. Escalate permanently (same as confidence floor).
            let Some(grace) = self.grace.grace_for(question.severity) else {
                auto_records.push(AutoResolutionRecord {
                    question_id: format!("{group_id}#{idx}"),
                    severity: question.severity,
                    confidence: question.confidence,
                    grace_period_secs: 0,
                    decision: AutoDecision::EscalatedToOperator,
                    resolved_at: now_iso.to_string(),
                });
                saw_permanent_failure = true;
                continue;
            };

            // Auto-accept the recommendation (if any). If there is no
            // recommendation the resolver can't synthesize one — escalate.
            let Some(recommendation) = question.recommendation.clone() else {
                auto_records.push(AutoResolutionRecord {
                    question_id: format!("{group_id}#{idx}"),
                    severity: question.severity,
                    confidence: question.confidence,
                    grace_period_secs: grace.as_secs(),
                    decision: AutoDecision::EscalatedToOperator,
                    resolved_at: now_iso.to_string(),
                });
                saw_permanent_failure = true;
                continue;
            };

            info!(
                group_id,
                question_index = idx,
                grace_secs = grace.as_secs(),
                confidence = question.confidence,
                "question_resolver: auto-accepted recommendation"
            );
            auto_records.push(AutoResolutionRecord {
                question_id: format!("{group_id}#{idx}"),
                severity: question.severity,
                confidence: question.confidence,
                grace_period_secs: grace.as_secs(),
                decision: AutoDecision::Accepted,
                resolved_at: now_iso.to_string(),
            });
            *slot = AnswerState::AutoAccepted(recommendation);
        }

        if state.is_satisfied() {
            return Ok(Some(Self::finalize_unblocked(state, auto_records, now_iso)));
        }

        if saw_permanent_failure {
            return Ok(Some(Self::finalize_expired(state, auto_records, now_iso)));
        }

        Ok(None)
    }

    fn finalize_unblocked(
        state: &mut GroupState,
        auto_records: Vec<AutoResolutionRecord>,
        now_iso: &str,
    ) -> GroupResolutionOutcome {
        state.terminal = true;
        state.group.resolved_at = Some(now_iso.to_string());
        let answers = state.answer_map();
        GroupResolutionOutcome::Unblocked {
            group_id: state.group.group_id.clone(),
            parent_goal_id: state.group.parent_goal_id.clone(),
            answers,
            auto_records,
        }
    }

    fn finalize_expired(
        state: &mut GroupState,
        auto_records: Vec<AutoResolutionRecord>,
        now_iso: &str,
    ) -> GroupResolutionOutcome {
        state.terminal = true;
        state.group.resolved_at = Some(now_iso.to_string());
        let unresolved_indexes = state.unresolved_indexes();
        GroupResolutionOutcome::Expired {
            group_id: state.group.group_id.clone(),
            parent_goal_id: state.group.parent_goal_id.clone(),
            unresolved_indexes,
            auto_records,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use symbiotic_core::types::question_group::{
        AnnotatedQuestion, AnswerType, QuestionGroup, ResolutionMode, UnblockKey,
    };

    const NOW: &str = "2026-04-18T10:42:00Z";

    fn q(
        text: &str,
        confidence: f32,
        severity: QuestionSeverity,
        recommendation: Option<&str>,
    ) -> AnnotatedQuestion {
        AnnotatedQuestion {
            text: text.to_string(),
            quick_replies: None,
            recommendation: recommendation.map(|s| s.to_string()),
            confidence,
            severity,
            expected_answer_type: AnswerType::FreeText,
            resolution_trail: None,
        }
    }

    fn group(id: &str, mode: ResolutionMode, questions: Vec<AnnotatedQuestion>) -> QuestionGroup {
        QuestionGroup {
            group_id: id.to_string(),
            parent_goal_id: "goal-parent".to_string(),
            unblock_key: UnblockKey::Exploratory {
                topic: "test".to_string(),
            },
            questions,
            created_at: NOW.to_string(),
            resolved_at: None,
            resolution_mode: mode,
        }
    }

    // ── ResolutionMode variants ──────────────────────────────────────────

    #[test]
    fn all_required_triggers_only_after_every_question_resolved() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-all",
            ResolutionMode::AllRequired,
            vec![
                q("q0", 0.9, QuestionSeverity::Decision, Some("a")),
                q("q1", 0.9, QuestionSeverity::Decision, Some("b")),
                q("q2", 0.9, QuestionSeverity::Decision, Some("c")),
            ],
        ));

        let o0 = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-all".into(),
                    question_index: 0,
                    answer: Some("one".into()),
                },
                NOW,
            )
            .unwrap();
        assert!(o0.is_none(), "two questions still pending");

        let o1 = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-all".into(),
                    question_index: 1,
                    answer: Some("two".into()),
                },
                NOW,
            )
            .unwrap();
        assert!(o1.is_none(), "one question still pending");

        let o2 = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-all".into(),
                    question_index: 2,
                    answer: Some("three".into()),
                },
                NOW,
            )
            .unwrap();
        match o2 {
            Some(GroupResolutionOutcome::Unblocked {
                group_id,
                parent_goal_id,
                answers,
                auto_records,
            }) => {
                assert_eq!(group_id, "g-all");
                assert_eq!(parent_goal_id, "goal-parent");
                assert_eq!(answers.get(&0).map(String::as_str), Some("one"));
                assert_eq!(answers.get(&1).map(String::as_str), Some("two"));
                assert_eq!(answers.get(&2).map(String::as_str), Some("three"));
                assert!(auto_records.is_empty());
            }
            other => panic!("expected Unblocked, got {other:?}"),
        }
    }

    #[test]
    fn any_one_triggers_on_first_answer() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-any",
            ResolutionMode::AnyOne,
            vec![
                q("q0", 0.9, QuestionSeverity::Decision, None),
                q("q1", 0.9, QuestionSeverity::Decision, None),
            ],
        ));

        let outcome = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-any".into(),
                    question_index: 0,
                    answer: Some("only one".into()),
                },
                NOW,
            )
            .unwrap();

        match outcome {
            Some(GroupResolutionOutcome::Unblocked { answers, .. }) => {
                assert_eq!(answers.len(), 1);
                assert_eq!(answers.get(&0).map(String::as_str), Some("only one"));
            }
            other => panic!("expected Unblocked on first AnyOne answer, got {other:?}"),
        }
    }

    #[test]
    fn majority_signal_waits_for_n_answers() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-maj",
            ResolutionMode::MajoritySignal { n: 2 },
            vec![
                q("q0", 0.9, QuestionSeverity::Decision, None),
                q("q1", 0.9, QuestionSeverity::Decision, None),
                q("q2", 0.9, QuestionSeverity::Decision, None),
            ],
        ));

        let o0 = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-maj".into(),
                    question_index: 0,
                    answer: Some("a".into()),
                },
                NOW,
            )
            .unwrap();
        assert!(o0.is_none(), "one answer, need two");

        let o1 = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-maj".into(),
                    question_index: 1,
                    answer: Some("b".into()),
                },
                NOW,
            )
            .unwrap();
        match o1 {
            Some(GroupResolutionOutcome::Unblocked { answers, .. }) => {
                assert_eq!(answers.len(), 2);
            }
            other => panic!("expected Unblocked, got {other:?}"),
        }
    }

    // ── Per-severity grace periods ───────────────────────────────────────

    #[test]
    fn grace_config_returns_none_for_critical() {
        let cfg = GraceConfig::default();
        assert!(cfg.grace_for(QuestionSeverity::Critical).is_none());
    }

    #[test]
    fn grace_config_returns_severity_specific_durations() {
        let cfg = GraceConfig::default();
        assert_eq!(
            cfg.grace_for(QuestionSeverity::Trivial),
            Some(Duration::from_secs(DEFAULT_GRACE_TRIVIAL_SECS))
        );
        assert_eq!(
            cfg.grace_for(QuestionSeverity::Informational),
            Some(Duration::from_secs(DEFAULT_GRACE_INFORMATIONAL_SECS))
        );
        assert_eq!(
            cfg.grace_for(QuestionSeverity::Decision),
            Some(Duration::from_secs(DEFAULT_GRACE_DECISION_SECS))
        );
    }

    #[test]
    fn strict_mode_disables_every_grace() {
        let cfg = GraceConfig {
            strict_mode: true,
            ..GraceConfig::default()
        };
        assert!(cfg.grace_for(QuestionSeverity::Trivial).is_none());
        assert!(cfg.grace_for(QuestionSeverity::Decision).is_none());
        assert!(cfg.grace_for(QuestionSeverity::Informational).is_none());
        assert!(cfg.grace_for(QuestionSeverity::Critical).is_none());
    }

    // ── Auto-decision semantics (§3.3) ──────────────────────────────────

    #[test]
    fn tick_grace_auto_accepts_above_threshold_with_recommendation() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-auto",
            ResolutionMode::AllRequired,
            vec![q(
                "q0",
                0.95,
                QuestionSeverity::Decision,
                Some("default-answer"),
            )],
        ));

        let outcome = resolver.tick_grace("g-auto", &[0], NOW).unwrap();
        match outcome {
            Some(GroupResolutionOutcome::Unblocked {
                answers,
                auto_records,
                ..
            }) => {
                assert_eq!(answers.get(&0).map(String::as_str), Some("default-answer"));
                assert_eq!(auto_records.len(), 1);
                assert_eq!(auto_records[0].decision, AutoDecision::Accepted);
            }
            other => panic!("expected Unblocked with auto-accept, got {other:?}"),
        }
    }

    #[test]
    fn low_confidence_auto_fails_and_escalates_regardless_of_severity() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-low-conf",
            ResolutionMode::AllRequired,
            vec![q(
                "q0",
                0.50, // below DEFAULT_FAIL_THRESHOLD = 0.70
                QuestionSeverity::Trivial,
                Some("default-but-unsure"),
            )],
        ));

        let outcome = resolver.tick_grace("g-low-conf", &[0], NOW).unwrap();
        match outcome {
            Some(GroupResolutionOutcome::Expired {
                unresolved_indexes,
                auto_records,
                ..
            }) => {
                assert_eq!(unresolved_indexes, vec![0]);
                assert_eq!(auto_records.len(), 1);
                assert_eq!(
                    auto_records[0].decision,
                    AutoDecision::EscalatedToOperator,
                    "<0.70 confidence must auto-fail"
                );
            }
            other => panic!("expected Expired on low confidence, got {other:?}"),
        }
    }

    #[test]
    fn critical_severity_never_auto_resolves_regardless_of_confidence() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-crit",
            ResolutionMode::AllRequired,
            vec![q(
                "q0",
                0.99, // even very high confidence
                QuestionSeverity::Critical,
                Some("default-crit"),
            )],
        ));

        let outcome = resolver.tick_grace("g-crit", &[0], NOW).unwrap();
        match outcome {
            Some(GroupResolutionOutcome::Expired {
                unresolved_indexes,
                auto_records,
                ..
            }) => {
                assert_eq!(unresolved_indexes, vec![0]);
                assert_eq!(auto_records.len(), 1);
                assert_eq!(
                    auto_records[0].decision,
                    AutoDecision::EscalatedToOperator,
                    "Critical severity always escalates"
                );
            }
            other => panic!("expected Expired on Critical severity, got {other:?}"),
        }
    }

    #[test]
    fn critical_severity_still_escalates_in_autonomous_strict_mode_off() {
        // Even with wide-open thresholds, Critical is hard-coded to escalate.
        let grace = GraceConfig {
            fail_threshold: 0.0,
            strict_mode: false,
            ..GraceConfig::default()
        };
        let mut resolver = QuestionResolver::with_grace(grace);
        resolver.register(group(
            "g-crit-open",
            ResolutionMode::AllRequired,
            vec![q("q0", 1.0, QuestionSeverity::Critical, Some("high-conf"))],
        ));

        let outcome = resolver.tick_grace("g-crit-open", &[0], NOW).unwrap();
        match outcome {
            Some(GroupResolutionOutcome::Expired { auto_records, .. }) => {
                assert_eq!(auto_records[0].decision, AutoDecision::EscalatedToOperator);
            }
            other => panic!("Critical must escalate, got {other:?}"),
        }
    }

    #[test]
    fn missing_recommendation_escalates_rather_than_guessing() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-no-rec",
            ResolutionMode::AllRequired,
            vec![q("q0", 0.95, QuestionSeverity::Decision, None)],
        ));

        let outcome = resolver.tick_grace("g-no-rec", &[0], NOW).unwrap();
        match outcome {
            Some(GroupResolutionOutcome::Expired { auto_records, .. }) => {
                assert_eq!(auto_records[0].decision, AutoDecision::EscalatedToOperator);
            }
            other => panic!("expected Expired when no recommendation, got {other:?}"),
        }
    }

    // ── Error paths ──────────────────────────────────────────────────────

    #[test]
    fn submit_to_unknown_group_errors() {
        let mut resolver = QuestionResolver::new();
        let err = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "nope".into(),
                    question_index: 0,
                    answer: Some("x".into()),
                },
                NOW,
            )
            .unwrap_err();
        assert!(matches!(err, ResolverError::UnknownGroup(_)));
    }

    #[test]
    fn double_submit_rejected_for_audit_integrity() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-dbl",
            ResolutionMode::AnyOne,
            vec![q("q0", 0.9, QuestionSeverity::Decision, None)],
        ));

        let _ = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-dbl".into(),
                    question_index: 0,
                    answer: Some("first".into()),
                },
                NOW,
            )
            .unwrap();

        // Second submission: the group is already terminal.
        let err = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-dbl".into(),
                    question_index: 0,
                    answer: Some("second".into()),
                },
                NOW,
            )
            .unwrap_err();
        assert!(matches!(err, ResolverError::AlreadyResolved(_, _)));
    }

    #[test]
    fn skipping_under_all_required_errors() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-skip-all",
            ResolutionMode::AllRequired,
            vec![q("q0", 0.9, QuestionSeverity::Decision, None)],
        ));
        let err = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-skip-all".into(),
                    question_index: 0,
                    answer: None,
                },
                NOW,
            )
            .unwrap_err();
        assert!(matches!(err, ResolverError::CannotSkipAllRequired(_, _)));
    }

    #[test]
    fn skipping_critical_severity_errors() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-skip-crit",
            ResolutionMode::AnyOne,
            vec![q("q0", 0.9, QuestionSeverity::Critical, Some("x"))],
        ));
        let err = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-skip-crit".into(),
                    question_index: 0,
                    answer: None,
                },
                NOW,
            )
            .unwrap_err();
        assert!(matches!(err, ResolverError::CannotSkipCritical(_, _)));
    }

    #[test]
    fn snapshot_reflects_progress() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-snap",
            ResolutionMode::AllRequired,
            vec![
                q("q0", 0.9, QuestionSeverity::Decision, None),
                q("q1", 0.9, QuestionSeverity::Decision, None),
            ],
        ));

        let initial = resolver.snapshot();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].total_questions, 2);
        assert_eq!(initial[0].resolved_count, 0);
        assert!(!initial[0].terminal);

        let _ = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "g-snap".into(),
                    question_index: 0,
                    answer: Some("a".into()),
                },
                NOW,
            )
            .unwrap();

        let mid = resolver.snapshot();
        assert_eq!(mid[0].resolved_count, 1);
        assert!(!mid[0].terminal);
    }

    // ── Integration: 3 questions × 3 answers → answers map ──────────────

    #[test]
    fn integration_three_questions_three_answers_produces_answers_map() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "design-phase",
            ResolutionMode::AllRequired,
            vec![
                q("framework?", 0.72, QuestionSeverity::Decision, Some("Vue")),
                q("ssr?", 0.85, QuestionSeverity::Decision, Some("Yes")),
                q(
                    "browsers?",
                    0.60,
                    QuestionSeverity::Informational,
                    Some("modern"),
                ),
            ],
        ));

        // Simulate operator submitting all three in arbitrary order
        // (§3.2.1 — free-order answering).
        for (idx, answer) in [(2, "last 2 versions"), (0, "Vue"), (1, "Yes")] {
            let outcome = resolver
                .submit_answer(
                    QuestionAnswer {
                        group_id: "design-phase".into(),
                        question_index: idx,
                        answer: Some(answer.into()),
                    },
                    NOW,
                )
                .unwrap();
            if idx != 1 {
                assert!(outcome.is_none(), "not yet satisfied");
            } else {
                match outcome {
                    Some(GroupResolutionOutcome::Unblocked {
                        group_id,
                        answers,
                        auto_records,
                        ..
                    }) => {
                        assert_eq!(group_id, "design-phase");
                        assert_eq!(answers.len(), 3);
                        assert_eq!(answers.get(&0).map(String::as_str), Some("Vue"));
                        assert_eq!(answers.get(&1).map(String::as_str), Some("Yes"));
                        assert_eq!(answers.get(&2).map(String::as_str), Some("last 2 versions"));
                        assert!(
                            auto_records.is_empty(),
                            "no auto-records for direct answers"
                        );
                    }
                    other => panic!("expected Unblocked, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn register_overwrites_prior_entry() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-over",
            ResolutionMode::AllRequired,
            vec![q("v1", 0.9, QuestionSeverity::Decision, None)],
        ));
        assert!(resolver.is_registered("g-over"));

        resolver.register(group(
            "g-over",
            ResolutionMode::AllRequired,
            vec![q("v2", 0.9, QuestionSeverity::Decision, None)],
        ));
        let snaps = resolver.snapshot();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].total_questions, 1);
    }

    #[test]
    fn forget_drops_group_silently() {
        let mut resolver = QuestionResolver::new();
        resolver.register(group(
            "g-forget",
            ResolutionMode::AllRequired,
            vec![q("q0", 0.9, QuestionSeverity::Decision, None)],
        ));
        resolver.forget("g-forget");
        assert!(!resolver.is_registered("g-forget"));
    }
}
