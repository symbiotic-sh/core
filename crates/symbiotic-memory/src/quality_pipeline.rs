//! Quality pipeline for extracted facts.
//!
//! Implements the 7-step quality gate from the memory system design:
//! 1. Exact dedupe (hash match)
//! 2. Semantic dedupe (embedding similarity placeholder)
//! 3. Junk filter (transient chat, greetings, filler)
//! 4. Plausibility check (contradicts high-confidence existing?)
//! 5. Durable-personal bias (prefer personal facts over generic)
//! 6. Conflict detection (queue for superseding)
//! 7. Borderline flagging (low confidence -> Review)

use crate::dedup::normalize_name;
use crate::types::{ExtractedFact, FactType, Memory, MemoryStatus};

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Verdict from the quality pipeline for a single extracted fact.
#[derive(Debug, Clone, PartialEq)]
pub enum QualityVerdict {
    /// Fact passes all checks with a final confidence score.
    Accept(f64),
    /// Fact is a duplicate of an existing memory (contains its ID).
    Deduplicate(String),
    /// Fact supersedes an existing memory (contains its ID).
    Supersede(String),
    /// Fact is rejected with a reason.
    Reject(String),
    /// Fact needs human review with a reason.
    Review(String),
}

/// Configuration knobs for the quality pipeline.
#[derive(Debug, Clone)]
pub struct QualityConfig {
    /// Minimum confidence for automatic acceptance (step 7).
    pub min_accept_confidence: f64,
    /// Confidence below which facts go to review (step 7).
    pub review_threshold: f64,
    /// Minimum durable-personal score to keep (step 5).
    pub min_durable_personal_score: f64,
    /// Semantic similarity threshold for deduplication (step 2).
    pub semantic_dedup_threshold: f64,
}

impl Default for QualityConfig {
    fn default() -> Self {
        Self {
            min_accept_confidence: 0.5,
            review_threshold: 0.3,
            min_durable_personal_score: 0.15,
            semantic_dedup_threshold: 0.92,
        }
    }
}

/// The quality pipeline evaluates extracted facts before they enter the store.
pub struct QualityPipeline {
    config: QualityConfig,
}

impl QualityPipeline {
    pub fn new(config: QualityConfig) -> Self {
        Self { config }
    }

    /// Run all 7 quality checks on an extracted fact against existing memories.
    ///
    /// `existing_memories` are the current active memories for the same entity
    /// (or all memories if doing cross-entity quality checks).
    pub fn evaluate(
        &self,
        fact: &ExtractedFact,
        source_text: &str,
        existing_memories: &[Memory],
    ) -> QualityVerdict {
        // Step 1: Exact dedupe — hash match on normalized fact text
        let fact_hash = hash_fact_text(&fact.fact);
        for mem in existing_memories {
            if mem.status == MemoryStatus::Active && hash_fact_text(&mem.fact) == fact_hash {
                return QualityVerdict::Deduplicate(mem.id.clone());
            }
        }

        // Step 2: Semantic dedupe — placeholder for embedding-based similarity.
        // When embeddings are available, compare against existing facts with
        // cosine similarity > self.config.semantic_dedup_threshold.
        // For now, use token Jaccard + character-level similarity as a rough proxy.
        // Both must be high to avoid false positives from reordered text
        // (e.g., "X over Y" vs "Y over X" share all tokens but differ in meaning).
        for mem in existing_memories {
            if mem.status == MemoryStatus::Active {
                let jaccard = normalized_text_similarity(&fact.fact, &mem.fact);
                if jaccard >= self.config.semantic_dedup_threshold {
                    // Double-check with character-level similarity
                    let char_sim = char_level_similarity(&fact.fact, &mem.fact);
                    if char_sim >= self.config.semantic_dedup_threshold {
                        return QualityVerdict::Deduplicate(mem.id.clone());
                    }
                }
            }
        }

        // Step 3: Junk filter — discard transient chat, greetings, filler
        if is_junk(&fact.fact, source_text) {
            return QualityVerdict::Reject("junk: transient or filler content".to_string());
        }

        // Step 4: Plausibility check — contradicts high-confidence existing?
        for mem in existing_memories {
            if mem.status == MemoryStatus::Active
                && mem.confidence >= 0.8
                && is_direct_contradiction(&fact.fact, &mem.fact)
                && fact.confidence < 0.6
            {
                // Step 6 would handle this as superseding,
                // but if the new fact is low-confidence and the existing is high,
                // flag for review.
                return QualityVerdict::Review(format!(
                    "low-confidence fact contradicts high-confidence memory '{}'",
                    mem.id
                ));
            }
        }

        // Step 5: Durable-personal bias
        let dp_score = durable_personal_score(fact);
        if dp_score < self.config.min_durable_personal_score {
            return QualityVerdict::Reject(format!(
                "durable-personal score too low: {dp_score:.2}"
            ));
        }

        // Step 6: Conflict detection — queue for superseding
        for mem in existing_memories {
            if mem.status == MemoryStatus::Active
                && is_direct_contradiction(&fact.fact, &mem.fact)
                && fact.confidence >= 0.6
            {
                return QualityVerdict::Supersede(mem.id.clone());
            }
        }

        // Step 7: Borderline flagging — low confidence -> Review
        if fact.confidence < self.config.review_threshold {
            return QualityVerdict::Review("confidence below review threshold".to_string());
        }

        if fact.confidence < self.config.min_accept_confidence {
            return QualityVerdict::Review(format!(
                "borderline confidence: {:.2}",
                fact.confidence
            ));
        }

        QualityVerdict::Accept(dp_score)
    }
}

impl Default for QualityPipeline {
    fn default() -> Self {
        Self::new(QualityConfig::default())
    }
}

/// Hash the normalized text of a fact for exact deduplication.
fn hash_fact_text(text: &str) -> u64 {
    let normalized = normalize_name(text);
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// Rough normalized text similarity using token overlap (Jaccard coefficient).
/// Returns 0.0..=1.0.
fn normalized_text_similarity(a: &str, b: &str) -> f64 {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();
    let a_tokens: std::collections::HashSet<&str> = a_lower.split_whitespace().collect();
    let b_tokens: std::collections::HashSet<&str> = b_lower.split_whitespace().collect();

    if a_tokens.is_empty() && b_tokens.is_empty() {
        return 1.0;
    }
    if a_tokens.is_empty() || b_tokens.is_empty() {
        return 0.0;
    }

    let intersection = a_tokens.intersection(&b_tokens).count();
    let union = a_tokens.union(&b_tokens).count();

    intersection as f64 / union as f64
}

/// Character-level similarity using normalized Levenshtein distance.
/// Returns 0.0..=1.0 (1.0 = identical).
fn char_level_similarity(a: &str, b: &str) -> f64 {
    let a_norm = a.trim().to_lowercase();
    let b_norm = b.trim().to_lowercase();

    if a_norm == b_norm {
        return 1.0;
    }

    let a_chars: Vec<char> = a_norm.chars().collect();
    let b_chars: Vec<char> = b_norm.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());

    if max_len == 0 {
        return 1.0;
    }

    let dist = crate::dedup::edit_distance(&a_norm, &b_norm);
    1.0 - (dist as f64 / max_len as f64)
}

/// Check if the extracted fact is transient chat junk.
fn is_junk(fact_text: &str, _source_text: &str) -> bool {
    let lowered = fact_text.trim().to_lowercase();

    // Too short to be meaningful
    if lowered.len() < 5 {
        return true;
    }

    // Common filler/greeting patterns
    const JUNK_PATTERNS: &[&str] = &[
        "ok",
        "okay",
        "thanks",
        "thank you",
        "got it",
        "sounds good",
        "let me check",
        "let me think",
        "i see",
        "sure",
        "yes",
        "no",
        "maybe",
        "hello",
        "hi",
        "hey",
        "bye",
        "goodbye",
        "good morning",
        "good night",
        "step completed",
        "step done",
        "working on it",
        "in progress",
        "noted",
        "acknowledged",
    ];

    for pattern in JUNK_PATTERNS {
        if lowered == *pattern || lowered.starts_with(&format!("{pattern}.")) {
            return true;
        }
    }

    // Status updates that are transient
    lowered.starts_with("step ") && lowered.contains("of ") && lowered.contains("completed")
}

/// Detect if two fact texts are direct contradictions.
///
/// This is a heuristic approach: same entity name tokens + opposing predicate.
/// Full semantic contradiction detection would require embeddings or LLM calls.
fn is_direct_contradiction(new_fact: &str, existing_fact: &str) -> bool {
    let new_lower = new_fact.to_lowercase();
    let existing_lower = existing_fact.to_lowercase();

    // Simple heuristic: check if they share significant tokens but have
    // negation patterns, or describe the same subject with different values.
    let new_tokens: std::collections::HashSet<&str> = new_lower.split_whitespace().collect();
    let existing_tokens: std::collections::HashSet<&str> =
        existing_lower.split_whitespace().collect();

    let intersection = new_tokens.intersection(&existing_tokens).count();
    let min_len = new_tokens.len().min(existing_tokens.len());

    if min_len == 0 {
        return false;
    }

    // High token overlap (same subject area)
    let overlap_ratio = intersection as f64 / min_len as f64;
    if overlap_ratio < 0.5 {
        return false;
    }

    // Check for negation patterns
    let has_negation = |text: &str| -> bool {
        text.contains(" not ")
            || text.contains(" no ")
            || text.contains("n't ")
            || text.contains("never ")
            || text.contains("instead of ")
            || text.contains(" over ")
            || text.contains(" rather than ")
    };

    // One negates but the other doesn't, on the same topic
    if has_negation(&new_lower) != has_negation(&existing_lower) {
        return true;
    }

    // Check for "X over Y" vs "Y over X" patterns
    if new_lower.contains(" over ") && existing_lower.contains(" over ") {
        return true;
    }

    false
}

/// Compute the durable-personal score for a fact.
///
/// Higher scores mean the fact is more likely to be valuable long-term
/// and personally relevant. Based on the fact type hierarchy:
/// Decision > Preference > Methodology > Finding > Entity > Episode
fn durable_personal_score(fact: &ExtractedFact) -> f64 {
    let type_weight = match fact.fact_type {
        Some(FactType::Decision) => 1.0,
        Some(FactType::Preference) => 0.9,
        Some(FactType::Methodology) => 0.8,
        Some(FactType::Finding) => 0.65,
        Some(FactType::Entity) => 0.55,
        Some(FactType::Episode) => 0.3,
        // Unclassified facts get a moderate weight
        None => 0.5,
    };

    // Combine type weight with confidence for final score
    type_weight * fact.confidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        EntityType, ExtractedFact, FactDisposition, FactType, Memory, MemoryStatus, Sensitivity,
    };

    fn make_fact(text: &str, confidence: f64, fact_type: Option<FactType>) -> ExtractedFact {
        ExtractedFact {
            entity_type: EntityType::Person,
            entity_name: "Test".to_string(),
            fact: text.to_string(),
            confidence,
            evidence_quote: "test quote".to_string(),
            temporal_hint: None,
            fact_type,
        }
    }

    fn make_memory(id: &str, fact: &str, confidence: f64) -> Memory {
        Memory {
            id: id.to_string(),
            entity_id: "ent-1".to_string(),
            fact: fact.to_string(),
            confidence,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: "2026-01-01T00:00:00Z".to_string(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        }
    }

    #[test]
    fn exact_dupe_detected() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact("Uses Rust for all projects", 0.9, Some(FactType::Finding));
        let existing = vec![make_memory("mem-1", "Uses Rust for all projects", 0.85)];

        let verdict = pipeline.evaluate(&fact, "source text", &existing);
        assert_eq!(verdict, QualityVerdict::Deduplicate("mem-1".to_string()));
    }

    #[test]
    fn case_insensitive_exact_dupe() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact("uses rust for all projects", 0.9, Some(FactType::Finding));
        let existing = vec![make_memory("mem-1", "Uses Rust For All Projects", 0.85)];

        let verdict = pipeline.evaluate(&fact, "source text", &existing);
        assert_eq!(verdict, QualityVerdict::Deduplicate("mem-1".to_string()));
    }

    #[test]
    fn junk_rejected() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact("ok", 0.9, None);

        let verdict = pipeline.evaluate(&fact, "ok thanks", &[]);
        assert!(matches!(verdict, QualityVerdict::Reject(ref r) if r.contains("junk")));
    }

    #[test]
    fn junk_thanks_rejected() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact("thanks", 0.9, None);

        let verdict = pipeline.evaluate(&fact, "thanks for that", &[]);
        assert!(matches!(verdict, QualityVerdict::Reject(ref r) if r.contains("junk")));
    }

    #[test]
    fn short_text_rejected_as_junk() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact("hi", 0.9, None);

        let verdict = pipeline.evaluate(&fact, "hi there", &[]);
        assert!(matches!(verdict, QualityVerdict::Reject(_)));
    }

    #[test]
    fn low_confidence_flagged_for_review() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact(
            "Might use Python for scripting",
            0.25,
            Some(FactType::Finding),
        );

        let verdict = pipeline.evaluate(&fact, "source text", &[]);
        assert!(matches!(verdict, QualityVerdict::Review(_)));
    }

    #[test]
    fn borderline_confidence_flagged_for_review() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact(
            "Might use Python for scripting",
            0.4,
            Some(FactType::Finding),
        );

        let verdict = pipeline.evaluate(&fact, "source text", &[]);
        assert!(matches!(verdict, QualityVerdict::Review(_)));
    }

    #[test]
    fn decision_fact_accepted() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact(
            "Frontend framework decision: Vue over React",
            0.9,
            Some(FactType::Decision),
        );

        let verdict = pipeline.evaluate(&fact, "source text", &[]);
        assert!(matches!(verdict, QualityVerdict::Accept(_)));
    }

    #[test]
    fn episode_with_low_confidence_rejected() {
        let pipeline = QualityPipeline::default();
        // Episode with low confidence produces low durable-personal score
        let fact = make_fact(
            "API call responded in 200ms today",
            0.3,
            Some(FactType::Episode),
        );

        let verdict = pipeline.evaluate(&fact, "source text", &[]);
        // 0.3 * 0.3 = 0.09, below min_durable_personal_score 0.15
        assert!(matches!(verdict, QualityVerdict::Reject(ref r) if r.contains("durable-personal")));
    }

    #[test]
    fn conflict_triggers_supersede() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact(
            "Frontend framework: Vue over React",
            0.9,
            Some(FactType::Decision),
        );
        let existing = vec![make_memory(
            "mem-old",
            "Frontend framework: React over Vue",
            0.85,
        )];

        let verdict = pipeline.evaluate(&fact, "source text", &existing);
        assert_eq!(verdict, QualityVerdict::Supersede("mem-old".to_string()));
    }

    #[test]
    fn low_confidence_contradiction_goes_to_review() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact(
            "Frontend framework: Vue over React",
            0.4,
            Some(FactType::Finding),
        );
        let existing = vec![make_memory(
            "mem-old",
            "Frontend framework: React over Vue",
            0.9,
        )];

        let verdict = pipeline.evaluate(&fact, "source text", &existing);
        assert!(matches!(verdict, QualityVerdict::Review(_)));
    }

    #[test]
    fn accept_returns_weighted_score() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact(
            "Uses feature branches for all work",
            0.85,
            Some(FactType::Methodology),
        );

        let verdict = pipeline.evaluate(&fact, "source text", &[]);
        match verdict {
            QualityVerdict::Accept(score) => {
                // durable_personal_score = 0.8 (methodology weight) * 0.85 (confidence) = 0.68
                let expected = 0.8 * 0.85;
                assert!(
                    (score - expected).abs() < 0.01,
                    "expected ~{expected}, got {score}"
                );
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn superseded_memories_not_matched_for_dedupe() {
        let pipeline = QualityPipeline::default();
        let fact = make_fact("Uses Vue for frontend", 0.9, Some(FactType::Decision));
        let mut old_mem = make_memory("mem-old", "Uses Vue for frontend", 0.85);
        old_mem.status = MemoryStatus::Superseded;

        let verdict = pipeline.evaluate(&fact, "source text", &[old_mem]);
        assert!(matches!(verdict, QualityVerdict::Accept(_)));
    }

    // --- hash / similarity helpers ---

    #[test]
    fn hash_fact_text_normalizes() {
        assert_eq!(hash_fact_text("  Uses Rust  "), hash_fact_text("uses rust"));
    }

    #[test]
    fn normalized_similarity_identical() {
        let sim =
            normalized_text_similarity("Uses Rust for all projects", "uses rust for all projects");
        assert!((sim - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn normalized_similarity_different() {
        let sim = normalized_text_similarity(
            "Uses Rust for all projects",
            "Python is a scripting language",
        );
        assert!(sim < 0.3, "expected low similarity, got {sim}");
    }

    #[test]
    fn junk_filter_allows_real_facts() {
        assert!(!is_junk("Uses Rust for all backend projects", "source"));
        assert!(!is_junk(
            "Frontend framework decision: Vue over React",
            "source"
        ));
    }

    #[test]
    fn junk_filter_catches_status_updates() {
        assert!(is_junk(
            "step 2 of 4 completed",
            "step 2 of 4 completed successfully"
        ));
    }

    #[test]
    fn durable_personal_score_hierarchy() {
        let decision = make_fact("Choose Vue", 0.9, Some(FactType::Decision));
        let finding = make_fact("Vue is fast", 0.9, Some(FactType::Finding));
        let episode = make_fact("Deployed today", 0.9, Some(FactType::Episode));

        let d = durable_personal_score(&decision);
        let f = durable_personal_score(&finding);
        let e = durable_personal_score(&episode);

        assert!(
            d > f,
            "decision ({d}) should score higher than finding ({f})"
        );
        assert!(
            f > e,
            "finding ({f}) should score higher than episode ({e})"
        );
    }
}
