//! Source-grounding check for extracted facts.
//!
//! Validates that LLM-extracted facts are actually supported by the source text,
//! using fuzzy string matching on the evidence_quote field.

use crate::types::{ConfidenceThresholds, ExtractedFact, ExtractionDisposition, GroundingResult};

/// Configuration for the grounding checker.
pub struct GroundingChecker {
    /// Minimum normalized similarity for quote grounding (0.0 to 1.0).
    pub quote_match_threshold: f64,
}

impl Default for GroundingChecker {
    fn default() -> Self {
        Self {
            quote_match_threshold: 0.6,
        }
    }
}

impl GroundingChecker {
    /// Validate extracted facts against source text.
    ///
    /// For each fact, fuzzy-matches the `evidence_quote` against sliding windows
    /// of the source text. Returns a `GroundingResult` per fact.
    pub fn validate(&self, facts: &[ExtractedFact], source_text: &str) -> Vec<GroundingResult> {
        facts
            .iter()
            .map(|fact| {
                let score = best_window_similarity(&fact.evidence_quote, source_text);
                GroundingResult {
                    fact: fact.clone(),
                    grounded: score >= self.quote_match_threshold,
                    match_score: score,
                }
            })
            .collect()
    }
}

/// Determine the disposition of a fact based on confidence and grounding.
///
/// Implements the confidence/grounding matrix from the design doc:
/// - >= auto_accept + grounded -> AutoStore
/// - >= auto_accept + ungrounded -> demote confidence, ReviewFlag
/// - review..auto_accept + grounded -> ReviewFlag
/// - review..auto_accept + ungrounded -> Drop
/// - < review -> Drop or PendingConfirmation
pub fn disposition(
    confidence: f64,
    grounded: bool,
    thresholds: &ConfidenceThresholds,
) -> ExtractionDisposition {
    if confidence >= thresholds.auto_accept {
        if grounded {
            ExtractionDisposition::AutoStore
        } else {
            ExtractionDisposition::ReviewFlag
        }
    } else if confidence >= thresholds.review {
        if grounded {
            ExtractionDisposition::ReviewFlag
        } else {
            ExtractionDisposition::Drop
        }
    } else {
        ExtractionDisposition::PendingConfirmation
    }
}

/// Compute the best normalized similarity between `quote` and any window of
/// `source` with the same length as `quote`.
///
/// Uses normalized Levenshtein distance: 1.0 - (edit_distance / max_len).
/// Returns 0.0 if either string is empty.
fn best_window_similarity(quote: &str, source: &str) -> f64 {
    if quote.is_empty() || source.is_empty() {
        return 0.0;
    }

    let quote_lower = quote.to_lowercase();
    let source_lower = source.to_lowercase();

    // If the quote is a substring, perfect match
    if source_lower.contains(&quote_lower) {
        return 1.0;
    }

    let quote_chars: Vec<char> = quote_lower.chars().collect();
    let source_chars: Vec<char> = source_lower.chars().collect();

    if quote_chars.is_empty() || source_chars.is_empty() {
        return 0.0;
    }

    // Slide a window of quote_len across source, compute normalized edit distance
    let window_len = quote_chars.len();
    if window_len > source_chars.len() {
        // Quote longer than source — compare full strings
        return normalized_similarity(&quote_chars, &source_chars);
    }

    let mut best = 0.0_f64;
    let step = if source_chars.len() > 500 { 3 } else { 1 };

    for start in (0..=(source_chars.len() - window_len)).step_by(step) {
        let window = &source_chars[start..start + window_len];
        let sim = normalized_similarity(&quote_chars, window);
        if sim > best {
            best = sim;
            if best >= 1.0 {
                return 1.0;
            }
        }
    }

    best
}

/// Normalized similarity between two char slices.
/// Returns 1.0 - (levenshtein_distance / max_len).
fn normalized_similarity(a: &[char], b: &[char]) -> f64 {
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein(a, b);
    1.0 - (dist as f64 / max_len as f64)
}

/// Standard Levenshtein distance between two char slices.
fn levenshtein(a: &[char], b: &[char]) -> usize {
    let n = a.len();
    let m = b.len();

    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }

    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0; m + 1];

    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EntityType;

    fn make_fact(quote: &str, confidence: f64) -> ExtractedFact {
        ExtractedFact {
            entity_type: EntityType::Person,
            entity_name: "Test".to_string(),
            fact: "A test fact".to_string(),
            confidence,
            evidence_quote: quote.to_string(),
            temporal_hint: None,
            fact_type: None,
        }
    }

    // --- Levenshtein tests ---

    #[test]
    fn levenshtein_identical() {
        let a: Vec<char> = "hello".chars().collect();
        assert_eq!(levenshtein(&a, &a), 0);
    }

    #[test]
    fn levenshtein_empty() {
        let a: Vec<char> = "hello".chars().collect();
        let b: Vec<char> = Vec::new();
        assert_eq!(levenshtein(&a, &b), 5);
        assert_eq!(levenshtein(&b, &a), 5);
    }

    #[test]
    fn levenshtein_one_edit() {
        let a: Vec<char> = "kitten".chars().collect();
        let b: Vec<char> = "sitten".chars().collect();
        assert_eq!(levenshtein(&a, &b), 1);
    }

    #[test]
    fn levenshtein_classic() {
        let a: Vec<char> = "kitten".chars().collect();
        let b: Vec<char> = "sitting".chars().collect();
        assert_eq!(levenshtein(&a, &b), 3);
    }

    // --- Window similarity tests ---

    #[test]
    fn exact_substring_returns_one() {
        let score = best_window_similarity(
            "Jane works at Acme",
            "Earlier, Jane works at Acme Corp and loves it.",
        );
        assert_eq!(score, 1.0);
    }

    #[test]
    fn empty_quote_returns_zero() {
        assert_eq!(best_window_similarity("", "some source"), 0.0);
    }

    #[test]
    fn empty_source_returns_zero() {
        assert_eq!(best_window_similarity("some quote", ""), 0.0);
    }

    #[test]
    fn case_insensitive_match() {
        let score = best_window_similarity("JANE WORKS", "jane works at acme");
        assert_eq!(score, 1.0);
    }

    #[test]
    fn near_match_above_threshold() {
        // "Jane works at Acme" vs "Jane worked at Acme" — 1 char diff in an 18-char string
        let score = best_window_similarity("Jane works at Acme", "Jane worked at Acme");
        assert!(score > 0.6, "score was {score}");
    }

    #[test]
    fn completely_different_below_threshold() {
        let score = best_window_similarity(
            "This is completely different",
            "Rust is a systems programming language",
        );
        assert!(score < 0.6, "score was {score}");
    }

    // --- GroundingChecker tests ---

    #[test]
    fn grounded_fact_with_exact_match() {
        let checker = GroundingChecker::default();
        let source = "Jane mentioned she's been designing at Acme for two years.";
        let fact = make_fact("she's been designing at Acme for two years", 0.85);

        let results = checker.validate(&[fact], source);
        assert_eq!(results.len(), 1);
        assert!(results[0].grounded);
        assert!(results[0].match_score >= 0.6);
    }

    #[test]
    fn ungrounded_fact_fabricated_quote() {
        let checker = GroundingChecker::default();
        let source = "The weather today is sunny and warm.";
        let fact = make_fact("Jane is the CEO of TechCorp since 2020", 0.9);

        let results = checker.validate(&[fact], source);
        assert_eq!(results.len(), 1);
        assert!(!results[0].grounded);
    }

    #[test]
    fn empty_facts_returns_empty() {
        let checker = GroundingChecker::default();
        let results = checker.validate(&[], "some source text");
        assert!(results.is_empty());
    }

    #[test]
    fn multiple_facts_validated_independently() {
        let checker = GroundingChecker::default();
        let source = "Alice works at Google. The sky is blue.";
        let grounded = make_fact("Alice works at Google", 0.8);
        let ungrounded = make_fact("Bob is a rocket scientist at NASA", 0.7);

        let results = checker.validate(&[grounded, ungrounded], source);
        assert_eq!(results.len(), 2);
        assert!(results[0].grounded);
        assert!(!results[1].grounded);
    }

    // --- Disposition tests ---

    #[test]
    fn high_confidence_grounded_auto_stores() {
        let t = ConfidenceThresholds::default();
        assert_eq!(
            disposition(0.85, true, &t),
            ExtractionDisposition::AutoStore
        );
    }

    #[test]
    fn high_confidence_ungrounded_review_flags() {
        let t = ConfidenceThresholds::default();
        assert_eq!(
            disposition(0.85, false, &t),
            ExtractionDisposition::ReviewFlag
        );
    }

    #[test]
    fn medium_confidence_grounded_review_flags() {
        let t = ConfidenceThresholds::default();
        assert_eq!(
            disposition(0.55, true, &t),
            ExtractionDisposition::ReviewFlag
        );
    }

    #[test]
    fn medium_confidence_ungrounded_drops() {
        let t = ConfidenceThresholds::default();
        assert_eq!(disposition(0.55, false, &t), ExtractionDisposition::Drop);
    }

    #[test]
    fn low_confidence_pending_confirmation() {
        let t = ConfidenceThresholds::default();
        assert_eq!(
            disposition(0.2, true, &t),
            ExtractionDisposition::PendingConfirmation
        );
        assert_eq!(
            disposition(0.2, false, &t),
            ExtractionDisposition::PendingConfirmation
        );
    }

    #[test]
    fn boundary_at_auto_accept() {
        let t = ConfidenceThresholds::default();
        // Exactly at 0.7 should auto-store when grounded
        assert_eq!(disposition(0.7, true, &t), ExtractionDisposition::AutoStore);
        assert_eq!(
            disposition(0.7, false, &t),
            ExtractionDisposition::ReviewFlag
        );
    }

    #[test]
    fn boundary_at_review() {
        let t = ConfidenceThresholds::default();
        // Exactly at 0.4 should be in the review range
        assert_eq!(
            disposition(0.4, true, &t),
            ExtractionDisposition::ReviewFlag
        );
        assert_eq!(disposition(0.4, false, &t), ExtractionDisposition::Drop);
    }

    #[test]
    fn custom_thresholds() {
        let t = ConfidenceThresholds {
            auto_accept: 0.9,
            review: 0.5,
            drop: 0.5,
        };
        // 0.8 is below custom auto_accept but above review
        assert_eq!(
            disposition(0.8, true, &t),
            ExtractionDisposition::ReviewFlag
        );
        assert_eq!(disposition(0.8, false, &t), ExtractionDisposition::Drop);
    }
}
