//! Types backing Grouped Inquisition (T130).
//!
//! These declarative types define the wire + in-memory shape of:
//! - `QuestionGroup` — a batch of independent questions sharing an unblock key
//! - `AnnotatedQuestion` — one question carrying recommendation/severity metadata
//! - `UnblockKey` — routing label deciding which backend executes unblocked work
//! - `ResolutionTrail` — populated as the bottom-up escalation ladder is walked
//! - `LedgerEntry` — append-only preference ledger record (training-ready fields
//!   pre-planted so T131's SFT/DPO dataset builder does not force a schema
//!   migration later)
//!
//! Timestamps are carried as ISO 8601 `String`s (e.g. `2026-04-18T10:42:00Z`)
//! to match the convention already used by `symbiotic-core::temporal` helpers.
//!
//! See `docs/design/grouped-inquisition.md` §2, §3, §7, §14.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// QuestionGroup
// ---------------------------------------------------------------------------

/// A batch of independent questions that share an [`UnblockKey`].
///
/// When every question in the group has a resolved answer (per
/// [`ResolutionMode`]), a `goal.unblocked` event fires and the Sub-Goal
/// Dispatcher consumes the unblock to spawn work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionGroup {
    pub group_id: String,
    pub parent_goal_id: String,
    pub unblock_key: UnblockKey,
    pub questions: Vec<AnnotatedQuestion>,
    /// ISO 8601 UTC timestamp.
    pub created_at: String,
    /// ISO 8601 UTC timestamp, set once `resolution_mode` is satisfied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    pub resolution_mode: ResolutionMode,
}

/// A single question inside a [`QuestionGroup`].
///
/// Carries the Inquisitor's recommendation + confidence so the widget can
/// surface a "control the level of conviction" dial. The `resolution_trail`
/// is `None` when the question is first drafted by the Inquisitor, and
/// populated by the time the operator sees it (every tier attempted appends
/// to the trail).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotatedQuestion {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_replies: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    pub confidence: f32,
    pub severity: QuestionSeverity,
    pub expected_answer_type: AnswerType,
    /// Populated by the bottom-up escalation ladder; `None` at emission time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_trail: Option<ResolutionTrail>,
}

/// Severity drives grace period (§3.3) + `Critical` always-escalate rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum QuestionSeverity {
    /// Default-everything category; ~2min grace.
    Trivial,
    /// Low-impact / easy-to-reverse; ~30min grace.
    Informational,
    /// Normal choices; ~10min grace.
    Decision,
    /// Irreversible or high-impact; no grace — always operator.
    Critical,
}

/// Shape the widget expects for the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum AnswerType {
    FreeText,
    SingleChoice,
    Boolean,
    Scalar,
}

/// How many questions in a group need answers before it resolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum ResolutionMode {
    /// Every question must reach a resolved state.
    AllRequired,
    /// One answer is enough (disjunction).
    AnyOne,
    /// At least `n` answers required (polling-style groups).
    MajoritySignal { n: u32 },
}

// ---------------------------------------------------------------------------
// UnblockKey
// ---------------------------------------------------------------------------

/// Structured routing label — the Sub-Goal Dispatcher uses this to decide
/// which backend executes the unblocked work and what scope the work has.
///
/// Wire shape is `#[serde(tag = "type")]` to match the JSON examples in the
/// design doc (§3.1):
///
/// ```json
/// {"type": "Exploratory", "topic": "frontend-framework-choice"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UnblockKey {
    /// Exploratory / throwaway work — T116 internal git swarm.
    Exploratory { topic: String },
    /// Work targeting a user-attached project repo — T126
    /// `mirror_push_with_approval` + ApprovalGate.
    AttachedRepo {
        repo_id: String,
        branch_hint: String,
        requires_approval: bool,
    },
    /// Archive-only research — sandboxed researcher agent; output lands in
    /// `knowledge-base/semantic/` as a finding note.
    ResearchOnly { question: String },
    /// Composite — spawns multiple sub-goals, one per inner key. Parent
    /// completes only when every child reaches terminal state.
    Composite { children: Vec<UnblockKey> },
}

// ---------------------------------------------------------------------------
// GoalDAG extension fields
// ---------------------------------------------------------------------------

/// Pre-planned sub-goal spawn description (§2.3).
///
/// Lives alongside the parent goal; when the referenced group resolves, the
/// Dispatcher materialises a child goal using `initial_prompt` + the group's
/// answers as seed context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedSpawn {
    pub unblock_key: UnblockKey,
    /// `group_id` that must resolve before this spawn fires.
    pub when_group_resolved: String,
    /// Seed prompt for the sub-goal's Inquisitor.
    pub initial_prompt: String,
}

/// Goal-DAG extension bundle — the additive fields to layer onto
/// `symbiotic-control-plane::goals::GoalProcess`.
///
/// Kept as a separate struct in `symbiotic-core` so every consumer (daemon,
/// control-plane, client) agrees on the schema without needing to depend on
/// the goal lifecycle crate. The concrete `GoalProcess` struct folds these
/// fields in at its own level (§04 lands the integration; §02 just fixes the
/// shape).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GoalDagExtensions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_goal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unblock_key: Option<UnblockKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawns_on_unblock: Vec<PlannedSpawn>,
}

// ---------------------------------------------------------------------------
// Bottom-up escalation — ResolutionTrail + tiers
// ---------------------------------------------------------------------------

/// The four-tier escalation ladder a sub-agent walks before surfacing a
/// question to the operator (§13.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum EscalationTier {
    Recall,
    Peer,
    Council,
    Operator,
}

/// One tier-1 (Recall) attempt — a query into
/// `knowledge-base/methodology/preferences/` + Neural Graph for matching
/// past decisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallAttempt {
    /// The query text used against the Recall Gateway.
    pub query: String,
    /// Top match confidence (0.0–1.0); below threshold → fall through.
    pub match_confidence: f32,
    /// Archive path of the matched preference note, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_preference: Option<String>,
}

/// One tier-2 (Peer) opinion — a cheap read-only peer agent consulted for a
/// one-shot verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerOpinion {
    pub peer_id: String,
    pub answer: String,
    pub confidence: f32,
    pub rationale: String,
}

/// Tier-3 (Council) deliberation outcome — T60 Multi-LLM Council.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CouncilResult {
    pub consensus: String,
    pub confidence: f32,
    pub dissenting: Vec<String>,
}

/// Full reasoning history a question carries by the time it reaches the
/// operator. The UI renders this trail so the operator sees the *most
/// informed* version of the question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolutionTrail {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recall_attempts: Vec<RecallAttempt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_opinions: Vec<PeerOpinion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub council_result: Option<CouncilResult>,
    pub final_tier_reached: EscalationTier,
    pub escalation_reason: String,
}

// ---------------------------------------------------------------------------
// Autonomy dial
// ---------------------------------------------------------------------------

/// Operator-controlled autonomy posture active when a question was answered
/// (logged in the ledger — not the primary knob; see §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum AutonomyTolerance {
    Strict,
    Balanced,
    Autonomous,
}

// ---------------------------------------------------------------------------
// Ledger + auto-resolution records
// ---------------------------------------------------------------------------

/// Terminal auto-decision category for questions that never reached the
/// operator (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum AutoDecision {
    Accepted,
    Failed,
    EscalatedToOperator,
}

/// Log entry for any question auto-resolved without operator input.
///
/// Fed into the operator review surface + the calibration loop that tunes
/// the fail threshold per category.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoResolutionRecord {
    pub question_id: String,
    pub severity: QuestionSeverity,
    pub confidence: f32,
    /// Grace period granted, expressed in seconds (avoids `time::Duration`
    /// dependency while keeping second-precision fidelity).
    pub grace_period_secs: u64,
    pub decision: AutoDecision,
    /// ISO 8601 UTC timestamp.
    pub resolved_at: String,
}

/// Async outcome signal written by the metrics layer after the sub-goal
/// produced by an answered question reaches a terminal state.
///
/// Kept on the ledger entry so T131's training-corpus builder can join
/// preference decisions with their real-world results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeSignal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_goal_success: Option<bool>,
    /// Pointer to a later-superseding preference note, if the operator
    /// retroactively flagged the answer as wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_revision: Option<String>,
    /// ISO 8601 UTC timestamp.
    pub measured_at: String,
}

/// The operator's answer to a question (§2.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorAnswer {
    pub text: String,
    pub overrode_recommendation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text_note: Option<String>,
    pub autonomy_tolerance_active: AutonomyTolerance,
}

/// Append-only preference ledger record.
///
/// T130 writes these; T131 reads them for SFT/DPO dataset construction. The
/// five training-readiness fields at the bottom (`model_version`,
/// `context_pack_hash`, `context_pack_ref`, `reasoning_trace`,
/// `downstream_outcome`) are consumed by T131 only — they exist here so the
/// schema does not have to migrate later. Keep them populated on writes even
/// when T131 is not yet live; `None`/empty is legal and forward-compatible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: String,
    /// ISO 8601 UTC timestamp.
    pub emitted_at: String,
    pub thread_id: String,
    pub goal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_goal_id: Option<String>,
    pub question: AnnotatedQuestion,
    pub resolution_trail: ResolutionTrail,
    pub operator_answer: OperatorAnswer,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    // ------------------------------------------------------------------
    // Training-readiness fields — consumed by T131, not T130 itself.
    // See T130 README "Anticipated by T131" + chunk 02 Step 5.
    // ------------------------------------------------------------------
    /// Base model that drafted the recommendation (e.g. provider tag + rev).
    pub model_version: String,
    /// Deterministic hash of the context pack at recommendation time.
    pub context_pack_hash: String,
    /// Optional durable snapshot pointer (if the pack was archived).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_pack_ref: Option<String>,
    /// LLM chain-of-thought, if the model emitted one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_trace: Option<String>,
    /// Filled asynchronously by the metrics layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downstream_outcome: Option<OutcomeSignal>,
}

// ---------------------------------------------------------------------------
// Tests — round-trip serde for every enum variant + LedgerEntry cases.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn trail_minimal(tier: EscalationTier) -> ResolutionTrail {
        ResolutionTrail {
            recall_attempts: Vec::new(),
            peer_opinions: Vec::new(),
            council_result: None,
            final_tier_reached: tier,
            escalation_reason: "below threshold".to_string(),
        }
    }

    fn annotated_question() -> AnnotatedQuestion {
        AnnotatedQuestion {
            text: "Framework preference?".to_string(),
            quick_replies: Some(vec!["Vue".to_string(), "React".to_string()]),
            recommendation: Some("Vue".to_string()),
            confidence: 0.72,
            severity: QuestionSeverity::Decision,
            expected_answer_type: AnswerType::SingleChoice,
            resolution_trail: None,
        }
    }

    // ----- UnblockKey variants -----

    #[test]
    fn unblock_key_exploratory_roundtrip() {
        let key = UnblockKey::Exploratory {
            topic: "frontend-framework-choice".to_string(),
        };
        let json = serde_json::to_string(&key).expect("serialize");
        assert!(json.contains("\"type\":\"Exploratory\""));
        assert!(json.contains("\"topic\":\"frontend-framework-choice\""));
        let parsed: UnblockKey = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, key);
    }

    #[test]
    fn unblock_key_attached_repo_roundtrip() {
        let key = UnblockKey::AttachedRepo {
            repo_id: "user/saas-api".to_string(),
            branch_hint: "feature/auth".to_string(),
            requires_approval: true,
        };
        let json = serde_json::to_string(&key).expect("serialize");
        assert!(json.contains("\"type\":\"AttachedRepo\""));
        assert!(json.contains("\"requires_approval\":true"));
        let parsed: UnblockKey = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, key);
    }

    #[test]
    fn unblock_key_research_only_roundtrip() {
        let key = UnblockKey::ResearchOnly {
            question: "which oauth crate?".to_string(),
        };
        let json = serde_json::to_string(&key).expect("serialize");
        assert!(json.contains("\"type\":\"ResearchOnly\""));
        let parsed: UnblockKey = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, key);
    }

    #[test]
    fn unblock_key_composite_roundtrip() {
        let key = UnblockKey::Composite {
            children: vec![
                UnblockKey::ResearchOnly {
                    question: "survey".to_string(),
                },
                UnblockKey::Exploratory {
                    topic: "prototype".to_string(),
                },
            ],
        };
        let json = serde_json::to_string(&key).expect("serialize");
        assert!(json.contains("\"type\":\"Composite\""));
        let parsed: UnblockKey = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, key);
    }

    // ----- ResolutionMode variants -----

    #[test]
    fn resolution_mode_all_required_roundtrip() {
        let mode = ResolutionMode::AllRequired;
        let json = serde_json::to_string(&mode).expect("serialize");
        assert!(json.contains("\"mode\":\"AllRequired\""));
        let parsed: ResolutionMode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, mode);
    }

    #[test]
    fn resolution_mode_any_one_roundtrip() {
        let mode = ResolutionMode::AnyOne;
        let json = serde_json::to_string(&mode).expect("serialize");
        assert!(json.contains("\"mode\":\"AnyOne\""));
        let parsed: ResolutionMode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, mode);
    }

    #[test]
    fn resolution_mode_majority_signal_roundtrip() {
        let mode = ResolutionMode::MajoritySignal { n: 3 };
        let json = serde_json::to_string(&mode).expect("serialize");
        assert!(json.contains("\"mode\":\"MajoritySignal\""));
        assert!(json.contains("\"n\":3"));
        let parsed: ResolutionMode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, mode);
    }

    // ----- ResolutionTrail populated vs empty -----

    #[test]
    fn resolution_trail_empty_roundtrip() {
        let trail = trail_minimal(EscalationTier::Recall);
        let json = serde_json::to_string(&trail).expect("serialize");
        let parsed: ResolutionTrail = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, trail);
        assert!(parsed.recall_attempts.is_empty());
        assert!(parsed.peer_opinions.is_empty());
        assert!(parsed.council_result.is_none());
    }

    #[test]
    fn resolution_trail_populated_roundtrip() {
        let trail = ResolutionTrail {
            recall_attempts: vec![RecallAttempt {
                query: "frontend framework".to_string(),
                match_confidence: 0.71,
                matched_preference: Some(
                    "methodology/preferences/frontend-framework-choice.md".to_string(),
                ),
            }],
            peer_opinions: vec![
                PeerOpinion {
                    peer_id: "peer-agent-22".to_string(),
                    answer: "Vue".to_string(),
                    confidence: 0.68,
                    rationale: "cited one article".to_string(),
                },
                PeerOpinion {
                    peer_id: "peer-agent-23".to_string(),
                    answer: "React".to_string(),
                    confidence: 0.55,
                    rationale: "team familiarity".to_string(),
                },
            ],
            council_result: Some(CouncilResult {
                consensus: "Vue".to_string(),
                confidence: 0.72,
                dissenting: vec!["model-b".to_string()],
            }),
            final_tier_reached: EscalationTier::Operator,
            escalation_reason: "council below 0.80 threshold".to_string(),
        };
        let json = serde_json::to_string(&trail).expect("serialize");
        let parsed: ResolutionTrail = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, trail);
        assert_eq!(parsed.peer_opinions.len(), 2);
        assert!(parsed.council_result.is_some());
    }

    // ----- LedgerEntry with training-readiness fields populated + None -----

    fn ledger_entry_base() -> LedgerEntry {
        LedgerEntry {
            id: "pref-2026-04-18-0001".to_string(),
            emitted_at: "2026-04-18T10:42:00Z".to_string(),
            thread_id: "thread-saas".to_string(),
            goal_id: "goal-build-frontend".to_string(),
            sub_goal_id: Some("subgoal-design-phase".to_string()),
            question: annotated_question(),
            resolution_trail: trail_minimal(EscalationTier::Operator),
            operator_answer: OperatorAnswer {
                text: "Vue".to_string(),
                overrode_recommendation: false,
                free_text_note: None,
                autonomy_tolerance_active: AutonomyTolerance::Balanced,
            },
            tags: vec!["decision".to_string(), "frontend".to_string()],
            model_version: String::new(),
            context_pack_hash: String::new(),
            context_pack_ref: None,
            reasoning_trace: None,
            downstream_outcome: None,
        }
    }

    #[test]
    fn ledger_entry_training_fields_all_populated_roundtrip() {
        let mut entry = ledger_entry_base();
        entry.model_version = "qwen3-personal-v3".to_string();
        entry.context_pack_hash = "sha256:abc123".to_string();
        entry.context_pack_ref = Some("archive://context-packs/cp-0001.json".to_string());
        entry.reasoning_trace = Some("Vue matches past SSR preference...".to_string());
        entry.downstream_outcome = Some(OutcomeSignal {
            sub_goal_success: Some(true),
            operator_revision: None,
            measured_at: "2026-04-18T11:00:00Z".to_string(),
        });

        let json = serde_json::to_string(&entry).expect("serialize");
        let parsed: LedgerEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, entry);
        assert!(parsed.downstream_outcome.is_some());
        assert_eq!(parsed.model_version, "qwen3-personal-v3");
    }

    #[test]
    fn ledger_entry_training_fields_all_none_roundtrip() {
        // Required string fields stay empty but present; Option fields absent.
        let entry = ledger_entry_base();
        let json = serde_json::to_string(&entry).expect("serialize");
        // Option fields should be omitted.
        assert!(!json.contains("\"context_pack_ref\""));
        assert!(!json.contains("\"reasoning_trace\""));
        assert!(!json.contains("\"downstream_outcome\""));
        // Required fields should still serialize, even if empty strings.
        assert!(json.contains("\"model_version\""));
        assert!(json.contains("\"context_pack_hash\""));

        let parsed: LedgerEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, entry);
        assert!(parsed.context_pack_ref.is_none());
        assert!(parsed.reasoning_trace.is_none());
        assert!(parsed.downstream_outcome.is_none());
    }

    // ----- Severity + answer type round-trip (sanity) -----

    #[test]
    fn question_severity_all_variants_roundtrip() {
        for severity in [
            QuestionSeverity::Trivial,
            QuestionSeverity::Informational,
            QuestionSeverity::Decision,
            QuestionSeverity::Critical,
        ] {
            let json = serde_json::to_string(&severity).expect("serialize");
            let parsed: QuestionSeverity = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, severity);
        }
    }

    #[test]
    fn answer_type_all_variants_roundtrip() {
        for at in [
            AnswerType::FreeText,
            AnswerType::SingleChoice,
            AnswerType::Boolean,
            AnswerType::Scalar,
        ] {
            let json = serde_json::to_string(&at).expect("serialize");
            let parsed: AnswerType = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, at);
        }
    }

    // ----- Auto-resolution -----

    #[test]
    fn auto_decision_all_variants_roundtrip() {
        for d in [
            AutoDecision::Accepted,
            AutoDecision::Failed,
            AutoDecision::EscalatedToOperator,
        ] {
            let json = serde_json::to_string(&d).expect("serialize");
            let parsed: AutoDecision = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, d);
        }
    }

    #[test]
    fn auto_resolution_record_roundtrip() {
        let record = AutoResolutionRecord {
            question_id: "q-1".to_string(),
            severity: QuestionSeverity::Trivial,
            confidence: 0.91,
            grace_period_secs: 120,
            decision: AutoDecision::Accepted,
            resolved_at: "2026-04-18T10:44:00Z".to_string(),
        };
        let json = serde_json::to_string(&record).expect("serialize");
        let parsed: AutoResolutionRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, record);
    }

    // ----- QuestionGroup full round-trip -----

    #[test]
    fn question_group_full_roundtrip() {
        let group = QuestionGroup {
            group_id: "design-phase".to_string(),
            parent_goal_id: "goal-build-frontend".to_string(),
            unblock_key: UnblockKey::Exploratory {
                topic: "frontend".to_string(),
            },
            questions: vec![annotated_question()],
            created_at: "2026-04-18T10:42:00Z".to_string(),
            resolved_at: None,
            resolution_mode: ResolutionMode::AllRequired,
        };
        let json = serde_json::to_string(&group).expect("serialize");
        let parsed: QuestionGroup = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, group);
    }

    // ----- GoalDagExtensions default roundtrip -----

    #[test]
    fn goal_dag_extensions_default_roundtrip() {
        let ext = GoalDagExtensions::default();
        let json = serde_json::to_string(&ext).expect("serialize");
        // Defaults should omit all fields.
        assert_eq!(json, "{}");
        let parsed: GoalDagExtensions = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, ext);
    }

    #[test]
    fn goal_dag_extensions_populated_roundtrip() {
        let ext = GoalDagExtensions {
            parent_goal_id: Some("goal-parent".to_string()),
            unblock_key: Some(UnblockKey::ResearchOnly {
                question: "survey".to_string(),
            }),
            blocked_by_groups: vec!["design-phase".to_string()],
            spawns_on_unblock: vec![PlannedSpawn {
                unblock_key: UnblockKey::Exploratory {
                    topic: "prototype".to_string(),
                },
                when_group_resolved: "design-phase".to_string(),
                initial_prompt: "scaffold the app".to_string(),
            }],
        };
        let json = serde_json::to_string(&ext).expect("serialize");
        let parsed: GoalDagExtensions = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, ext);
    }
}
