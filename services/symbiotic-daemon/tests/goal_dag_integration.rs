//! Integration tests for T130 §04 — Goal DAG + unblock events.
//!
//! Verifies the cross-crate interaction between:
//! - `symbiotic-core::types::question_group` (wire types)
//! - `symbiotic-control-plane::goals::GoalProcess` (DAG extension fields)
//! - `symbiotic_daemon::goal_pipeline::question_resolver` (resolution logic)
//!
//! These tests intentionally stay on the public API surface of each crate —
//! no direct daemon-internal plumbing — so they double as documentation for
//! future integrators.

use std::collections::HashMap;

use symbiotic_core::types::question_group::{
    AnnotatedQuestion, AnswerType, AutoDecision, PlannedSpawn, QuestionGroup, QuestionSeverity,
    ResolutionMode, UnblockKey,
};
use symbiotic_daemon::goal_pipeline::question_resolver::{
    GroupResolutionOutcome, QuestionAnswer, QuestionResolver,
};

const NOW: &str = "2026-04-18T10:42:00Z";

fn q(text: &str, severity: QuestionSeverity) -> AnnotatedQuestion {
    AnnotatedQuestion {
        text: text.to_string(),
        quick_replies: None,
        recommendation: None,
        confidence: 0.85,
        severity,
        expected_answer_type: AnswerType::FreeText,
        resolution_trail: None,
    }
}

#[test]
fn three_questions_three_answers_fires_unblocked_with_correct_answers() {
    let mut resolver = QuestionResolver::new();

    let group = QuestionGroup {
        group_id: "design-phase".to_string(),
        parent_goal_id: "goal-build-frontend".to_string(),
        unblock_key: UnblockKey::Exploratory {
            topic: "frontend-framework-choice".to_string(),
        },
        questions: vec![
            q("framework?", QuestionSeverity::Decision),
            q("ssr?", QuestionSeverity::Decision),
            q("browsers?", QuestionSeverity::Informational),
        ],
        created_at: NOW.to_string(),
        resolved_at: None,
        resolution_mode: ResolutionMode::AllRequired,
    };
    resolver.register(group);

    let answers_in: Vec<(usize, &str)> = vec![(1, "Yes"), (2, "modern"), (0, "Vue")];
    let mut final_outcome = None;
    for (idx, text) in answers_in {
        let outcome = resolver
            .submit_answer(
                QuestionAnswer {
                    group_id: "design-phase".into(),
                    question_index: idx,
                    answer: Some(text.into()),
                },
                NOW,
            )
            .expect("submit should succeed");
        if outcome.is_some() {
            final_outcome = outcome;
        }
    }

    let outcome = final_outcome.expect("third answer should resolve the group");
    match outcome {
        GroupResolutionOutcome::Unblocked {
            group_id,
            parent_goal_id,
            answers,
            auto_records,
        } => {
            assert_eq!(group_id, "design-phase");
            assert_eq!(parent_goal_id, "goal-build-frontend");
            let mut expected: HashMap<usize, String> = HashMap::new();
            expected.insert(0, "Vue".into());
            expected.insert(1, "Yes".into());
            expected.insert(2, "modern".into());
            assert_eq!(answers, expected);
            assert!(auto_records.is_empty(), "no auto records for live answers");
        }
        other => panic!("expected Unblocked, got {other:?}"),
    }
}

#[test]
fn critical_severity_group_always_escalates_on_grace_tick() {
    let mut resolver = QuestionResolver::new();
    let group = QuestionGroup {
        group_id: "g-crit".to_string(),
        parent_goal_id: "goal-x".to_string(),
        unblock_key: UnblockKey::Exploratory {
            topic: "ops".to_string(),
        },
        questions: vec![AnnotatedQuestion {
            text: "format the prod DB?".to_string(),
            quick_replies: None,
            recommendation: Some("Yes".to_string()),
            confidence: 1.0,
            severity: QuestionSeverity::Critical,
            expected_answer_type: AnswerType::Boolean,
            resolution_trail: None,
        }],
        created_at: NOW.to_string(),
        resolved_at: None,
        resolution_mode: ResolutionMode::AllRequired,
    };
    resolver.register(group);

    let outcome = resolver
        .tick_grace("g-crit", &[0], NOW)
        .expect("tick should succeed")
        .expect("grace expiry on Critical must produce Expired");
    match outcome {
        GroupResolutionOutcome::Expired {
            unresolved_indexes,
            auto_records,
            ..
        } => {
            assert_eq!(unresolved_indexes, vec![0]);
            assert_eq!(auto_records.len(), 1);
            assert_eq!(auto_records[0].decision, AutoDecision::EscalatedToOperator);
        }
        other => panic!("expected Expired on Critical, got {other:?}"),
    }
}

// ── Store migration: control-plane GoalProcess with default DAG fields ──

#[test]
fn goal_process_serializes_without_dag_fields_by_default() {
    // A fresh `GoalProcess` with default DAG fields must not serialize the
    // four new optional fields — proves downstream code reading such a
    // JSON blob (legacy on-disk payloads) rebuilds the same struct.
    use symbiotic_control_plane::goals::GoalProcess;
    use symbiotic_control_plane::types::{
        AutonomyLevel, CheckFrequency, GoalConstraints, GoalPhase, GoalState, ProcessConfig,
        ProcessType,
    };
    use symbiotic_control_plane::GoalMetrics;

    let goal = GoalProcess {
        id: "id-slim".into(),
        slug: "slim".into(),
        title: "Slim goal".into(),
        state: GoalState::Active,
        priority: 1,
        autonomy_level: AutonomyLevel::Semi,
        phase: GoalPhase::Research,
        process: ProcessConfig {
            process_type: ProcessType::Periodic,
            check_frequency: CheckFrequency::Daily,
            max_parallel_agents: 2,
        },
        streams: Vec::new(),
        domains: vec!["engineering".into()],
        vault_namespace: "goal-slim".into(),
        constraints: GoalConstraints {
            budget_usd: Some(100.0),
            time_horizon_days: Some(30),
            risk_tolerance: None,
        },
        metrics: GoalMetrics::default(),
        created_at: 1_000,
        last_check_at: 1_000,
        next_check_at: 2_000,
        parent_goal_id: None,
        unblock_key: None,
        blocked_by_groups: Vec::new(),
        spawns_on_unblock: Vec::new(),
    };

    let json = serde_json::to_string(&goal).unwrap();
    assert!(!json.contains("parent_goal_id"));
    assert!(!json.contains("unblock_key"));
    assert!(!json.contains("blocked_by_groups"));
    assert!(!json.contains("spawns_on_unblock"));

    // Populated DAG fields must round-trip.
    let child = GoalProcess {
        parent_goal_id: Some("goal-parent".into()),
        unblock_key: Some(UnblockKey::Exploratory {
            topic: "child-work".into(),
        }),
        blocked_by_groups: vec!["design-phase".into()],
        spawns_on_unblock: vec![PlannedSpawn {
            unblock_key: UnblockKey::ResearchOnly {
                question: "which oauth crate?".into(),
            },
            when_group_resolved: "design-phase".into(),
            initial_prompt: "survey oauth crates".into(),
        }],
        ..goal
    };
    let json = serde_json::to_string(&child).unwrap();
    let parsed: GoalProcess = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.parent_goal_id.as_deref(), Some("goal-parent"));
    assert_eq!(parsed.blocked_by_groups, vec!["design-phase".to_string()]);
    assert_eq!(parsed.spawns_on_unblock.len(), 1);
    assert!(matches!(
        parsed.unblock_key,
        Some(UnblockKey::Exploratory { .. })
    ));
}
