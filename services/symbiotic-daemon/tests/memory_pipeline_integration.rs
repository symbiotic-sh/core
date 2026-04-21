//! Cross-crate integration tests for the memory pipeline.
//!
//! Exercises the full memory pipeline: extraction → quality gate → doc generation →
//! staleness → entity dedup → auto-promotion pre-filter.
//!
//! These tests use actual code paths without needing Docker/Matrix.

use std::collections::HashMap;
use std::fs;

use symbiotic_core::MemorySpace;
use symbiotic_daemon::auto_promotion::{
    pre_filter, pre_filter_with_cooldown, PreFilterResult, PromotionCooldownTracker,
};
use symbiotic_daemon::memory_docs::{
    format_staleness_warnings_at, DecisionHistoryEntry, GoalSummary, ThreadMemoryDocGenerator,
};
use symbiotic_memory::cost::{estimate_extraction_cost, estimate_thread_batch_cost};
use symbiotic_memory::entity_dedup::{DedupMatchReason, EntityDeduplicator};
use symbiotic_memory::staleness::{StalenessChecker, StalenessConfig};
use symbiotic_memory::types::{
    AllowedModels, Entity, EntityStatus, EntityType, FactDisposition, FactType, Memory,
    MemoryStatus, Sensitivity,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_entity_with_attrs(
    id: &str,
    name: &str,
    entity_type: EntityType,
    attrs: serde_json::Value,
) -> Entity {
    Entity {
        id: id.to_string(),
        entity_type,
        name: name.to_string(),
        attributes: attrs,
        sensitivity: Sensitivity::Shareable,
        allowed_models: AllowedModels::Any,
        space: MemorySpace::Knowledge,
        status: EntityStatus::Active,
        merged_into: None,
        created_at: "2026-03-10T00:00:00Z".to_string(),
        updated_at: "2026-03-10T00:00:00Z".to_string(),
    }
}

fn make_entity_created_at(
    id: &str,
    name: &str,
    entity_type: EntityType,
    created_at: &str,
) -> Entity {
    Entity {
        id: id.to_string(),
        entity_type,
        name: name.to_string(),
        attributes: serde_json::json!({}),
        sensitivity: Sensitivity::Shareable,
        allowed_models: AllowedModels::Any,
        space: MemorySpace::Knowledge,
        status: EntityStatus::Active,
        merged_into: None,
        created_at: created_at.to_string(),
        updated_at: created_at.to_string(),
    }
}

fn make_typed_memory(
    id: &str,
    entity_id: &str,
    fact: &str,
    date: &str,
    fact_type: FactType,
) -> Memory {
    Memory {
        id: id.to_string(),
        entity_id: entity_id.to_string(),
        fact: fact.to_string(),
        confidence: 0.9,
        disposition: FactDisposition::AutoStored,
        sensitivity: Sensitivity::Shareable,
        valid_from: date.to_string(),
        valid_to: None,
        status: MemoryStatus::Active,
        superseded_by: None,
        created_at: date.to_string(),
        updated_at: date.to_string(),
        fact_type: Some(fact_type),
        authored_by: None,
        supersedes: None,
        depends_on: vec![],
        fsrs: None,
    }
}

fn make_memory_with_deps(
    id: &str,
    entity_id: &str,
    fact: &str,
    date: &str,
    fact_type: FactType,
    depends_on: Vec<String>,
) -> Memory {
    Memory {
        id: id.to_string(),
        entity_id: entity_id.to_string(),
        fact: fact.to_string(),
        confidence: 0.9,
        disposition: FactDisposition::AutoStored,
        sensitivity: Sensitivity::Shareable,
        valid_from: date.to_string(),
        valid_to: None,
        status: MemoryStatus::Active,
        superseded_by: None,
        created_at: date.to_string(),
        updated_at: date.to_string(),
        fact_type: Some(fact_type),
        authored_by: None,
        supersedes: None,
        depends_on,
        fsrs: None,
    }
}

// ---------------------------------------------------------------------------
// Test 1: full_extraction_to_doc_pipeline
// ---------------------------------------------------------------------------

#[test]
fn full_extraction_to_doc_pipeline() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let gen = ThreadMemoryDocGenerator::new(dir.path());

    // Entities: a Person, a Tool, and a Project
    let entities = vec![
        make_entity_with_attrs(
            "ent-alice",
            "Alice Chen",
            EntityType::Person,
            serde_json::json!({"role": "Lead Architect"}),
        ),
        make_entity_with_attrs(
            "ent-rust",
            "Rust",
            EntityType::Tool,
            serde_json::json!({"description": "Systems programming language"}),
        ),
        make_entity_with_attrs(
            "ent-symbiotic",
            "Symbiotic",
            EntityType::Project,
            serde_json::json!({"description": "AI personal operating system"}),
        ),
    ];

    // Decisions
    let decisions = vec![
        make_typed_memory(
            "dec-1",
            "ent-rust",
            "Use Rust for all backend services instead of Go",
            "2026-03-14T10:00:00Z",
            FactType::Decision,
        ),
        make_typed_memory(
            "dec-2",
            "ent-symbiotic",
            "Adopt Matrix protocol for transport layer",
            "2026-03-15T09:30:00Z",
            FactType::Decision,
        ),
    ];

    // Findings
    let findings = vec![
        make_typed_memory(
            "find-1",
            "ent-rust",
            "Rust compile times average 45s for incremental builds",
            "2026-03-13T14:00:00Z",
            FactType::Finding,
        ),
        make_typed_memory(
            "find-2",
            "ent-alice",
            "Alice has deep experience with distributed systems",
            "2026-03-12T11:00:00Z",
            FactType::Finding,
        ),
        make_typed_memory(
            "find-3",
            "ent-symbiotic",
            "Matrix E2EE adds ~200ms latency per message",
            "2026-03-16T16:00:00Z",
            FactType::Finding,
        ),
    ];

    // Goals
    let goals = vec![GoalSummary {
        title: "Define system architecture".to_string(),
        status: "active".to_string(),
        progress: 65,
        updated_at: "2026-03-16".to_string(),
    }];

    // Decision history
    let decision_history = vec![
        DecisionHistoryEntry {
            date: "2026-03-14".to_string(),
            decision: "Use Rust for backend".to_string(),
            supersedes: Some("Go for backend".to_string()),
        },
        DecisionHistoryEntry {
            date: "2026-03-15".to_string(),
            decision: "Matrix protocol for transport".to_string(),
            supersedes: None,
        },
    ];

    let result = gen.generate(
        "test-thread-001",
        "Project Architecture Discussion",
        "The team discussed the architecture for Symbiotic, focusing on language \
         choice, transport protocol, and team roles. Key decisions were made about \
         using Rust and Matrix.",
        &entities,
        &decisions,
        &findings,
        &goals,
        &decision_history,
    )?;

    // Verify the output file exists
    assert!(
        result.path.exists(),
        "generated doc should exist at {:?}",
        result.path
    );

    let content = fs::read_to_string(&result.path)?;

    // Verify expected sections
    assert!(
        content.contains("# Project Architecture Discussion"),
        "missing title"
    );
    assert!(content.contains("## Summary"), "missing Summary section");
    assert!(
        content.contains("## Key Decisions"),
        "missing Key Decisions section"
    );
    assert!(content.contains("## Findings"), "missing Findings section");
    assert!(content.contains("## Entities"), "missing Entities section");
    assert!(
        content.contains("## Active Goals"),
        "missing Active Goals section"
    );
    assert!(
        content.contains("## Decision History"),
        "missing Decision History section"
    );

    // Verify content_hash is present in frontmatter
    assert!(
        content.contains("content_hash:"),
        "missing content_hash in frontmatter"
    );
    assert!(
        content.contains(&format!("content_hash: {}", result.content_hash)),
        "content_hash value mismatch"
    );
    assert_eq!(
        result.content_hash.len(),
        64,
        "SHA-256 hex hash should be 64 chars"
    );

    // Verify wikilinks for entities
    assert!(
        content.contains("[[Alice Chen]]"),
        "missing wikilink for Alice Chen"
    );
    assert!(content.contains("[[Rust]]"), "missing wikilink for Rust");
    assert!(
        content.contains("[[Symbiotic]]"),
        "missing wikilink for Symbiotic"
    );

    // Verify entity descriptions from attributes
    assert!(
        content.contains("Lead Architect"),
        "missing entity role description"
    );
    assert!(
        content.contains("Systems programming language"),
        "missing entity description"
    );

    // Verify decision facts in content
    assert!(
        content.contains("Use Rust for all backend services instead of Go"),
        "missing decision text"
    );
    assert!(
        content.contains("Adopt Matrix protocol for transport layer"),
        "missing decision text"
    );

    // Verify findings in content
    assert!(
        content.contains("Rust compile times average 45s"),
        "missing finding text"
    );

    // Verify goals
    assert!(
        content.contains("Define system architecture (active, 65%)"),
        "missing goal text"
    );

    // Verify frontmatter
    assert!(
        content.contains("type: thread-memory"),
        "missing type in frontmatter"
    );
    assert!(
        content.contains("thread_id: test-thread-001"),
        "missing thread_id in frontmatter"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2: staleness_warnings_in_doc
// ---------------------------------------------------------------------------

#[test]
fn staleness_warnings_in_doc() -> anyhow::Result<()> {
    // Use a checker with known cadences for predictable staleness.
    let checker = StalenessChecker::new(StalenessConfig {
        decision_cadence_days: 180,
        finding_cadence_days: 90,
        ..StalenessConfig::default()
    });

    // Memory A: an old finding (> 90 days old when evaluated at "now")
    let mem_a = make_typed_memory(
        "stale-finding",
        "ent-1",
        "Competitor pricing is $10/month",
        "2025-06-01T00:00:00Z",
        FactType::Finding,
    );

    // Memory B: a decision that depends on the old finding
    let mem_b = make_memory_with_deps(
        "dependent-decision",
        "ent-1",
        "Price our product at $8/month",
        "2025-12-01T00:00:00Z",
        FactType::Decision,
        vec!["stale-finding".to_string()],
    );

    // Memory C: a fresh finding (recent date)
    let mem_c = make_typed_memory(
        "fresh-finding",
        "ent-2",
        "Server costs are $50/month",
        "2026-03-20T00:00:00Z",
        FactType::Finding,
    );

    let all_memories = vec![mem_a, mem_b, mem_c];

    // "Now" = 2026-03-23 00:00:00 UTC
    // stale-finding: created 2025-06-01, age ≈ 296 days > 90d finding cadence => STALE
    // dependent-decision: created 2025-12-01, age ≈ 112 days < 180d decision cadence => FRESH by age
    //   but depends on stale-finding => SUSPECT (cascading)
    // fresh-finding: created 2026-03-20, age ≈ 3 days < 90d => FRESH
    let now_secs = 1774243200_u64; // approx 2026-03-21T00:00:00Z

    let result = format_staleness_warnings_at(&all_memories, &checker, now_secs);

    assert!(
        result.is_some(),
        "expected staleness warnings for old facts"
    );
    let section = result.expect("should have warnings");

    // Verify section header
    assert!(
        section.contains("## Staleness Warnings"),
        "missing section header"
    );

    // Verify the stale finding is flagged
    assert!(
        section.contains("Competitor pricing is $10/month"),
        "missing stale finding text"
    );
    assert!(
        section.contains("Stale since"),
        "missing 'Stale since' label"
    );

    // Verify NO cascading staleness: dependent-decision should NOT be suspect
    assert!(
        !section.contains("Price our product at $8/month"),
        "fresh dependent decision should NOT appear in staleness warnings (no cascading)"
    );

    // Verify the fresh finding is NOT flagged
    assert!(
        !section.contains("Server costs are $50/month"),
        "fresh finding should NOT appear in staleness warnings"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Test 3: entity_dedup_finds_duplicates
// ---------------------------------------------------------------------------

#[test]
fn entity_dedup_finds_duplicates() -> anyhow::Result<()> {
    let dedup = EntityDeduplicator::default();

    let entities = vec![
        // Exact match pair (case difference)
        make_entity_created_at(
            "e1",
            "John Smith",
            EntityType::Person,
            "2026-01-01T00:00:00Z",
        ),
        make_entity_created_at(
            "e2",
            "john smith",
            EntityType::Person,
            "2026-01-02T00:00:00Z",
        ),
        // Fuzzy match pair (typo: "Kubernets" vs "Kubernetes")
        make_entity_created_at("e3", "Kubernetes", EntityType::Tool, "2026-01-01T00:00:00Z"),
        make_entity_created_at("e4", "Kubernets", EntityType::Tool, "2026-01-02T00:00:00Z"),
        // Unique entity (no match)
        make_entity_created_at("e5", "Rust", EntityType::Tool, "2026-01-03T00:00:00Z"),
    ];

    let candidates = dedup.find_candidates(&entities);

    // Should find exactly 2 duplicate pairs
    assert_eq!(
        candidates.len(),
        2,
        "expected 2 dedup candidates, got {}",
        candidates.len()
    );

    // Find the John Smith pair
    let john_match = candidates
        .iter()
        .find(|c| c.source_name.to_lowercase().contains("john"))
        .expect("should find John Smith match");

    assert_eq!(
        john_match.match_reason,
        DedupMatchReason::ExactNormalized,
        "John Smith pair should be an exact normalized match"
    );
    assert!(
        (john_match.confidence - 1.0).abs() < f64::EPSILON,
        "exact match should have confidence 1.0"
    );
    // Newer entity (e2) is the source, older (e1) is the target
    assert_eq!(john_match.source_id, "e2");
    assert_eq!(john_match.target_id, "e1");

    // Find the Kubernetes pair
    let k8s_match = candidates
        .iter()
        .find(|c| {
            c.source_name.to_lowercase().contains("kubern")
                || c.target_name.to_lowercase().contains("kubern")
        })
        .expect("should find Kubernetes match");

    match &k8s_match.match_reason {
        DedupMatchReason::FuzzyName { distance } => {
            assert!(
                *distance <= 2,
                "Kubernetes/Kubernets distance should be <= 2, got {}",
                distance
            );
        }
        other => panic!("expected FuzzyName for Kubernetes pair, got {:?}", other),
    }
    assert!(
        k8s_match.confidence > 0.8,
        "fuzzy match confidence should be > 0.8, got {}",
        k8s_match.confidence
    );
    assert!(
        k8s_match.confidence < 1.0,
        "fuzzy match confidence should be < 1.0"
    );

    // Verify no false match for standalone "Rust"
    let rust_match = candidates
        .iter()
        .find(|c| c.source_name == "Rust" || c.target_name == "Rust");
    assert!(
        rust_match.is_none(),
        "Rust should not match anything (only one entity with that name)"
    );

    // Verify results are sorted by confidence descending
    for i in 1..candidates.len() {
        assert!(
            candidates[i - 1].confidence >= candidates[i].confidence,
            "candidates should be sorted by confidence descending"
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Test 4: auto_promotion_pre_filter
// ---------------------------------------------------------------------------

#[test]
fn auto_promotion_pre_filter() -> anyhow::Result<()> {
    // Case 1: < 3 messages => TooFewMessages
    let claims_with_decisions = HashMap::from([(MemorySpace::Operations, 1usize)]);
    assert_eq!(
        pre_filter(2, &claims_with_decisions, false),
        PreFilterResult::TooFewMessages,
        "fewer than 3 messages should fail"
    );

    // Case 2: 5 messages but no Decision/Methodology facts => NoActionableFacts
    let empty_claims: HashMap<MemorySpace, usize> = HashMap::new();
    assert_eq!(
        pre_filter(5, &empty_claims, false),
        PreFilterResult::NoActionableFacts,
        "no actionable facts should fail"
    );

    // Also: only 1 knowledge claim is not enough
    let one_knowledge = HashMap::from([(MemorySpace::Knowledge, 1)]);
    assert_eq!(
        pre_filter(5, &one_knowledge, false),
        PreFilterResult::NoActionableFacts,
        "single knowledge claim should not be enough"
    );

    // Case 3: 5 messages with Decision/Methodology facts => Pass
    let methodology_claims = HashMap::from([(MemorySpace::Operations, 1)]);
    assert_eq!(
        pre_filter(5, &methodology_claims, false),
        PreFilterResult::Pass,
        "methodology claims + enough messages should pass"
    );

    // Also test with sufficient Knowledge claims (>= 2)
    let knowledge_claims = HashMap::from([(MemorySpace::Knowledge, 2)]);
    assert_eq!(
        pre_filter(5, &knowledge_claims, false),
        PreFilterResult::Pass,
        "2+ knowledge claims should also pass"
    );

    // Case 4: Already has an active goal => AlreadyHasGoal
    assert_eq!(
        pre_filter(10, &methodology_claims, true),
        PreFilterResult::AlreadyHasGoal,
        "thread with active goal should fail"
    );

    // Case 5: Cooldown active => OnCooldown
    let mut tracker = PromotionCooldownTracker::with_cooldown(3600); // 1 hour cooldown
    let now = 100_000_u64;
    tracker.record_dismissal("thread-test", now);

    let result = pre_filter_with_cooldown(
        "thread-test",
        5,
        &methodology_claims,
        false,
        Some(&tracker),
        now + 1800, // 30 minutes later, still on cooldown
    );
    assert_eq!(
        result,
        PreFilterResult::OnCooldown,
        "dismissed thread should be on cooldown"
    );

    // Case 6: Cooldown expired => should pass normally
    let result_after_expiry = pre_filter_with_cooldown(
        "thread-test",
        5,
        &methodology_claims,
        false,
        Some(&tracker),
        now + 3600, // exactly at expiry
    );
    assert_eq!(
        result_after_expiry,
        PreFilterResult::Pass,
        "should pass after cooldown expires"
    );

    // Case 7: No tracker provided => no cooldown check
    let result_no_tracker =
        pre_filter_with_cooldown("thread-test", 5, &methodology_claims, false, None, now);
    assert_eq!(
        result_no_tracker,
        PreFilterResult::Pass,
        "should pass when no tracker is provided"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Test 5: cost_estimation_pipeline
// ---------------------------------------------------------------------------

#[test]
fn cost_estimation_pipeline() -> anyhow::Result<()> {
    // Create representative messages: 20 messages, ~100 chars each
    let messages: Vec<(String, String, String)> = (0..20)
        .map(|i| {
            (
                format!("user{}", i % 3),
                format!(
                    "This is message number {} discussing the architecture of the system \
                     and various design decisions that need to be made soon.",
                    i
                ),
                format!("2026-03-23T{:02}:00:00Z", i),
            )
        })
        .collect();

    // Verify each message body is roughly ~100 chars
    for (_, body, _) in &messages {
        assert!(
            body.len() > 80 && body.len() < 150,
            "message body should be ~100 chars, got {}",
            body.len()
        );
    }

    // Single extraction cost for Haiku should be < $0.01
    let single_cost =
        estimate_extraction_cost(&messages, "haiku").expect("haiku pricing should be found");

    assert!(
        single_cost.estimated_cost_usd < 0.01,
        "single extraction with 20 messages should cost < $0.01, got ${:.6}",
        single_cost.estimated_cost_usd
    );
    assert!(
        single_cost.input_tokens > 0,
        "should have positive input tokens"
    );
    assert!(
        single_cost.output_tokens > 0,
        "should have positive output tokens"
    );
    assert_eq!(single_cost.model, "haiku");

    // Batch cost for 50 threads with 20 messages each should be < $25/month
    let batch_cost =
        estimate_thread_batch_cost(50, 20, "haiku").expect("haiku pricing should be found");

    // Daily batch for 50 threads; assume 30 runs/month => monthly cost
    let monthly_cost = batch_cost.estimated_cost_usd * 30.0;
    assert!(
        monthly_cost < 25.0,
        "monthly cost for 50 threads (daily) should be < $25, got ${:.2}",
        monthly_cost
    );

    // Verify the batch has more tokens than a single call
    assert!(
        batch_cost.input_tokens > single_cost.input_tokens,
        "batch should have more input tokens than single call"
    );

    // Sonnet should be significantly more expensive than Haiku
    let sonnet_cost =
        estimate_extraction_cost(&messages, "sonnet").expect("sonnet pricing should be found");
    assert!(
        sonnet_cost.estimated_cost_usd > single_cost.estimated_cost_usd * 5.0,
        "sonnet should cost at least 5x more than haiku"
    );

    // Unknown model should return None
    assert!(
        estimate_extraction_cost(&messages, "unknown-model-xyz").is_none(),
        "unknown model should return None"
    );
    assert!(
        estimate_thread_batch_cost(50, 20, "unknown-model-xyz").is_none(),
        "unknown model batch should return None"
    );

    Ok(())
}
