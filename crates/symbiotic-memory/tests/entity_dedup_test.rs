//! Integration tests for entity deduplication.

use symbiotic_core::MemorySpace;
use symbiotic_memory::entity_dedup::{DedupMatchReason, EntityDeduplicator};
use symbiotic_memory::types::{AllowedModels, Entity, EntityStatus, EntityType, Sensitivity};

fn make_entity(
    id: &str,
    name: &str,
    etype: EntityType,
    created_at: &str,
    attributes: serde_json::Value,
) -> Entity {
    Entity {
        id: id.to_string(),
        entity_type: etype,
        name: name.to_string(),
        attributes,
        sensitivity: Sensitivity::Shareable,
        allowed_models: AllowedModels::Any,
        space: MemorySpace::Knowledge,
        status: EntityStatus::Active,
        merged_into: None,
        created_at: created_at.to_string(),
        updated_at: created_at.to_string(),
    }
}

#[test]
fn exact_normalized_match_john_smith() {
    let dedup = EntityDeduplicator::default();
    let entities = vec![
        make_entity(
            "e1",
            "John Smith",
            EntityType::Person,
            "2026-01-01",
            serde_json::json!({}),
        ),
        make_entity(
            "e2",
            "john smith",
            EntityType::Person,
            "2026-01-02",
            serde_json::json!({}),
        ),
    ];

    let candidates = dedup.find_candidates(&entities);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].confidence, 1.0);
    assert_eq!(
        candidates[0].match_reason,
        DedupMatchReason::ExactNormalized
    );
}

#[test]
fn fuzzy_match_kubernetes_typo() {
    let dedup = EntityDeduplicator::default();
    let entities = vec![
        make_entity(
            "e1",
            "Kubernetes",
            EntityType::Tool,
            "2026-01-01",
            serde_json::json!({}),
        ),
        make_entity(
            "e2",
            "Kubernets",
            EntityType::Tool,
            "2026-01-02",
            serde_json::json!({}),
        ),
    ];

    let candidates = dedup.find_candidates(&entities);
    assert_eq!(candidates.len(), 1);
    match &candidates[0].match_reason {
        DedupMatchReason::FuzzyName { distance } => {
            assert!(*distance <= 2, "expected distance <= 2, got {distance}");
        }
        other => panic!("expected FuzzyName, got {other:?}"),
    }
    // Confidence should be high for a 2-char distance in a 10-char word.
    assert!(candidates[0].confidence > 0.7);
}

#[test]
fn different_types_do_not_match() {
    let dedup = EntityDeduplicator::default();
    let entities = vec![
        make_entity(
            "e1",
            "Rust",
            EntityType::Person,
            "2026-01-01",
            serde_json::json!({}),
        ),
        make_entity(
            "e2",
            "Rust",
            EntityType::Tool,
            "2026-01-02",
            serde_json::json!({}),
        ),
    ];

    let candidates = dedup.find_candidates(&entities);
    assert!(
        candidates.is_empty(),
        "Person 'Rust' and Tool 'Rust' should not match"
    );
}

#[test]
fn already_merged_entities_are_skipped() {
    let dedup = EntityDeduplicator::default();
    let mut merged = make_entity(
        "e1",
        "John Smith",
        EntityType::Person,
        "2026-01-01",
        serde_json::json!({}),
    );
    merged.status = EntityStatus::Merged;
    merged.merged_into = Some("e0".to_string());

    let active = make_entity(
        "e2",
        "john smith",
        EntityType::Person,
        "2026-01-02",
        serde_json::json!({}),
    );

    let candidates = dedup.find_candidates(&[merged, active]);
    assert!(
        candidates.is_empty(),
        "merged entity should be excluded from matching"
    );
}

#[test]
fn alias_matching_works() {
    let dedup = EntityDeduplicator::default();
    let entities = vec![
        make_entity(
            "e1",
            "React",
            EntityType::Tool,
            "2026-01-01",
            serde_json::json!({"aliases": ["ReactJS", "React.js"]}),
        ),
        make_entity(
            "e2",
            "ReactJS",
            EntityType::Tool,
            "2026-01-02",
            serde_json::json!({}),
        ),
    ];

    let candidates = dedup.find_candidates(&entities);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].confidence, 0.95);
    assert_eq!(candidates[0].match_reason, DedupMatchReason::AliasMatch);
}

#[test]
fn candidates_sorted_by_confidence() {
    let dedup = EntityDeduplicator::default();
    let entities = vec![
        // Exact match pair (confidence 1.0)
        make_entity(
            "e1",
            "Alice",
            EntityType::Person,
            "2026-01-01",
            serde_json::json!({}),
        ),
        make_entity(
            "e2",
            "alice",
            EntityType::Person,
            "2026-01-02",
            serde_json::json!({}),
        ),
        // Fuzzy match pair (confidence < 1.0)
        make_entity(
            "e3",
            "Docker",
            EntityType::Tool,
            "2026-01-01",
            serde_json::json!({}),
        ),
        make_entity(
            "e4",
            "Dockre",
            EntityType::Tool,
            "2026-01-02",
            serde_json::json!({}),
        ),
    ];

    let candidates = dedup.find_candidates(&entities);
    assert_eq!(candidates.len(), 2);

    // Verify descending confidence order.
    for i in 0..candidates.len() - 1 {
        assert!(
            candidates[i].confidence >= candidates[i + 1].confidence,
            "candidates not sorted by confidence: {} < {}",
            candidates[i].confidence,
            candidates[i + 1].confidence,
        );
    }
}

#[test]
fn reverse_alias_match_works() {
    // Entity B has the alias matching entity A's name.
    let dedup = EntityDeduplicator::default();
    let entities = vec![
        make_entity(
            "e1",
            "Vue.js",
            EntityType::Tool,
            "2026-01-01",
            serde_json::json!({}),
        ),
        make_entity(
            "e2",
            "Vue",
            EntityType::Tool,
            "2026-01-02",
            serde_json::json!({"aliases": ["Vue.js", "VueJS"]}),
        ),
    ];

    let candidates = dedup.find_candidates(&entities);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].match_reason, DedupMatchReason::AliasMatch);
    assert_eq!(candidates[0].confidence, 0.95);
}

#[test]
fn no_false_positives_for_short_distant_names() {
    let dedup = EntityDeduplicator::default();
    let entities = vec![
        make_entity(
            "e1",
            "Go",
            EntityType::Tool,
            "2026-01-01",
            serde_json::json!({}),
        ),
        make_entity(
            "e2",
            "Zig",
            EntityType::Tool,
            "2026-01-02",
            serde_json::json!({}),
        ),
    ];

    let candidates = dedup.find_candidates(&entities);
    // "go" vs "zig" has edit distance 3, exceeding the threshold of 2.
    assert!(candidates.is_empty());
}
