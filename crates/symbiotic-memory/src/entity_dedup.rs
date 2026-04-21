//! Weekly entity deduplication — detects and merges duplicate entities
//! created separately across threads.
//!
//! Uses `dedup::normalize_name` for exact normalized matching and
//! `dedup::edit_distance` for fuzzy matching. Candidates are grouped by
//! `EntityType` so that a Person "Rust" and a Tool "Rust" are never compared.

use crate::dedup::{edit_distance, normalize_name};
use crate::store::MemoryStore;
use crate::types::{Entity, EntityStatus, MemoryStoreError};

/// Why two entities were flagged as duplicates.
#[derive(Debug, Clone, PartialEq)]
pub enum DedupMatchReason {
    /// Names are identical after normalization (lowercase, trim, collapse WS).
    ExactNormalized,
    /// Names are within `distance` Levenshtein edits of each other.
    FuzzyName { distance: usize },
    /// One entity's alias matches the other entity's normalized name.
    AliasMatch,
}

/// A pair of entities that appear to be duplicates.
#[derive(Debug, Clone)]
pub struct DedupCandidate {
    /// Entity to merge away (will be marked `Merged`).
    pub source_id: String,
    /// Entity to keep (target of the merge).
    pub target_id: String,
    /// Original name of the source entity.
    pub source_name: String,
    /// Original name of the target entity.
    pub target_name: String,
    /// Match confidence in 0.0–1.0.
    pub confidence: f64,
    /// How the match was determined.
    pub match_reason: DedupMatchReason,
}

/// Summary returned after executing a single merge.
#[derive(Debug, Clone)]
pub struct MergeReport {
    /// ID of the entity that was merged away.
    pub source: String,
    /// ID of the entity that was kept.
    pub target: String,
}

/// Configurable entity deduplicator.
pub struct EntityDeduplicator {
    /// Maximum Levenshtein distance to consider a fuzzy match (default: 2).
    pub fuzzy_threshold: usize,
    /// Minimum confidence to auto-merge without human review (default: 0.9).
    pub min_confidence: f64,
}

impl Default for EntityDeduplicator {
    fn default() -> Self {
        Self {
            fuzzy_threshold: 2,
            min_confidence: 0.9,
        }
    }
}

impl EntityDeduplicator {
    /// Create a deduplicator with custom thresholds.
    pub fn new(fuzzy_threshold: usize, min_confidence: f64) -> Self {
        Self {
            fuzzy_threshold,
            min_confidence,
        }
    }

    /// Scan a set of entities and return candidate duplicate pairs.
    ///
    /// Only entities of the **same type** are compared. Entities with
    /// `status == Merged` are skipped entirely. Results are sorted by
    /// confidence descending.
    pub fn find_candidates(&self, entities: &[Entity]) -> Vec<DedupCandidate> {
        use std::collections::HashMap;

        // Group active entities by type.
        let mut by_type: HashMap<String, Vec<&Entity>> = HashMap::new();
        for entity in entities {
            if entity.status == EntityStatus::Merged {
                continue;
            }
            by_type
                .entry(entity.entity_type.as_str().to_string())
                .or_default()
                .push(entity);
        }

        let mut candidates = Vec::new();

        for group in by_type.values() {
            for i in 0..group.len() {
                for j in (i + 1)..group.len() {
                    let a = group[i];
                    let b = group[j];

                    if let Some(candidate) = self.compare_pair(a, b) {
                        candidates.push(candidate);
                    }
                }
            }
        }

        // Sort by confidence descending (highest first).
        candidates.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        candidates
    }

    /// Compare a single pair and return a candidate if they match.
    fn compare_pair(&self, a: &Entity, b: &Entity) -> Option<DedupCandidate> {
        let norm_a = normalize_name(&a.name);
        let norm_b = normalize_name(&b.name);

        // 1. Exact normalized match.
        if norm_a == norm_b {
            return Some(self.make_candidate(a, b, 1.0, DedupMatchReason::ExactNormalized));
        }

        // 2. Alias match — check if A's name appears in B's aliases or vice versa.
        if let Some(candidate) = self.check_alias_match(a, b, &norm_a, &norm_b) {
            return Some(candidate);
        }

        // 3. Fuzzy name match via Levenshtein.
        let distance = edit_distance(&norm_a, &norm_b);
        if distance <= self.fuzzy_threshold && distance > 0 {
            let max_len = norm_a.len().max(norm_b.len());
            if max_len == 0 {
                return None;
            }
            let confidence = 1.0 - (distance as f64 / max_len as f64);
            return Some(self.make_candidate(
                a,
                b,
                confidence,
                DedupMatchReason::FuzzyName { distance },
            ));
        }

        None
    }

    /// Check if either entity has an alias matching the other's normalized name.
    fn check_alias_match(
        &self,
        a: &Entity,
        b: &Entity,
        norm_a: &str,
        norm_b: &str,
    ) -> Option<DedupCandidate> {
        // Check B's aliases against A's name.
        if let Some(aliases) = b.attributes.get("aliases").and_then(|v| v.as_array()) {
            for alias in aliases {
                if let Some(alias_str) = alias.as_str() {
                    if normalize_name(alias_str) == norm_a {
                        return Some(self.make_candidate(a, b, 0.95, DedupMatchReason::AliasMatch));
                    }
                }
            }
        }

        // Check A's aliases against B's name.
        if let Some(aliases) = a.attributes.get("aliases").and_then(|v| v.as_array()) {
            for alias in aliases {
                if let Some(alias_str) = alias.as_str() {
                    if normalize_name(alias_str) == norm_b {
                        return Some(self.make_candidate(b, a, 0.95, DedupMatchReason::AliasMatch));
                    }
                }
            }
        }

        None
    }

    /// Build a `DedupCandidate`. The entity created later (by `created_at`)
    /// is the source (merged away); the older one is the target (kept).
    fn make_candidate(
        &self,
        a: &Entity,
        b: &Entity,
        confidence: f64,
        reason: DedupMatchReason,
    ) -> DedupCandidate {
        // Keep the older entity as target — merge the newer one into it.
        let (source, target) = if a.created_at <= b.created_at {
            (b, a)
        } else {
            (a, b)
        };

        DedupCandidate {
            source_id: source.id.clone(),
            target_id: target.id.clone(),
            source_name: source.name.clone(),
            target_name: target.name.clone(),
            confidence,
            match_reason: reason,
        }
    }

    /// Execute a single merge via the store.
    ///
    /// Calls `store.merge_entities()` which marks the source as `Merged`,
    /// sets `merged_into`, and reassigns memories/relationships.
    pub async fn execute_merge(
        &self,
        candidate: &DedupCandidate,
        store: &dyn MemoryStore,
    ) -> Result<MergeReport, MemoryStoreError> {
        store
            .merge_entities(&candidate.source_id, &candidate.target_id)
            .await?;

        Ok(MergeReport {
            source: candidate.source_id.clone(),
            target: candidate.target_id.clone(),
        })
    }

    /// Convenience: find candidates and auto-merge those above `min_confidence`.
    ///
    /// Returns all merge reports for merges that were executed.
    pub async fn find_and_auto_merge(
        &self,
        entities: &[Entity],
        store: &dyn MemoryStore,
    ) -> Result<Vec<MergeReport>, MemoryStoreError> {
        let candidates = self.find_candidates(entities);
        let mut reports = Vec::new();

        for candidate in &candidates {
            if candidate.confidence >= self.min_confidence {
                let report = self.execute_merge(candidate, store).await?;
                reports.push(report);
            }
        }

        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AllowedModels, EntityType, Sensitivity};
    use symbiotic_core::MemorySpace;

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
    fn exact_normalized_match() {
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
        // Newer entity (e2) is the source, older (e1) is the target.
        assert_eq!(candidates[0].source_id, "e2");
        assert_eq!(candidates[0].target_id, "e1");
    }

    #[test]
    fn fuzzy_name_match_typo() {
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
        assert!(candidates[0].confidence > 0.8);
        assert!(candidates[0].confidence < 1.0);
        match &candidates[0].match_reason {
            DedupMatchReason::FuzzyName { distance } => {
                assert!(*distance <= 2);
            }
            other => panic!("expected FuzzyName, got {other:?}"),
        }
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
    fn merged_entities_are_skipped() {
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

        let entities = vec![
            merged,
            make_entity(
                "e2",
                "john smith",
                EntityType::Person,
                "2026-01-02",
                serde_json::json!({}),
            ),
        ];

        let candidates = dedup.find_candidates(&entities);
        assert!(
            candidates.is_empty(),
            "merged entity should be skipped entirely"
        );
    }

    #[test]
    fn alias_match_works() {
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
    fn candidates_sorted_by_confidence_descending() {
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
        // First candidate should have higher confidence.
        assert!(candidates[0].confidence >= candidates[1].confidence);
        assert_eq!(candidates[0].confidence, 1.0); // exact
        assert!(candidates[1].confidence < 1.0); // fuzzy
    }

    #[test]
    fn no_match_for_distant_names() {
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
                "Docker",
                EntityType::Tool,
                "2026-01-02",
                serde_json::json!({}),
            ),
        ];

        let candidates = dedup.find_candidates(&entities);
        assert!(candidates.is_empty());
    }

    #[test]
    fn whitespace_normalization_exact_match() {
        let dedup = EntityDeduplicator::default();
        let entities = vec![
            make_entity(
                "e1",
                "  John   Smith  ",
                EntityType::Person,
                "2026-01-01",
                serde_json::json!({}),
            ),
            make_entity(
                "e2",
                "John Smith",
                EntityType::Person,
                "2026-01-02",
                serde_json::json!({}),
            ),
        ];

        let candidates = dedup.find_candidates(&entities);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].match_reason,
            DedupMatchReason::ExactNormalized
        );
    }
}
