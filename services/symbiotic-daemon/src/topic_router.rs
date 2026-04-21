//! Routes messages to existing threads based on topic similarity.
//!
//! Phase 1: keyword/title matching (rule-based, no embeddings).
//! Phase 2: embedding similarity (requires vector search — future).
//!
//! The TopicRouter never auto-routes. It produces `RoutingSuggestion`s that
//! the UX layer presents as tap-to-confirm cards.

/// Routes messages to existing threads based on topic similarity.
///
/// Phase 1: keyword/title matching.
/// Phase 2: embedding similarity (requires vector search).
pub struct TopicRouter;

/// A routing suggestion — never auto-routes, always suggests.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingSuggestion {
    /// Suggested target thread_id.
    pub thread_id: String,
    /// Thread title for display.
    pub thread_title: String,
    /// Confidence score (0.0-1.0).
    pub confidence: f32,
    /// Why this thread was suggested.
    pub reason: String,
}

/// Words too short or too common to be meaningful for matching.
const MIN_WORD_LEN: usize = 4;

impl TopicRouter {
    /// Find threads that a message might belong to.
    /// Returns suggestions sorted by confidence (highest first).
    /// Empty vec = no match, message stays in #stream.
    pub fn find_matches(
        message: &str,
        active_threads: &[(String, String)], // (thread_id, title) pairs
    ) -> Vec<RoutingSuggestion> {
        let msg_lower = message.to_lowercase();
        let msg_words: Vec<&str> = msg_lower.split_whitespace().collect();

        let mut suggestions = Vec::new();

        for (thread_id, title) in active_threads {
            let title_lower = title.to_lowercase();
            let title_words: Vec<&str> = title_lower.split_whitespace().collect();

            // Score based on word overlap (skip short/common words)
            let significant_words: Vec<&&str> = msg_words
                .iter()
                .filter(|w| w.len() >= MIN_WORD_LEN)
                .collect();

            let overlap = significant_words
                .iter()
                .filter(|w| {
                    title_words
                        .iter()
                        .any(|tw| tw.contains(***w) || w.contains(tw))
                })
                .count();

            if overlap > 0 {
                let denominator = significant_words.len().max(1) as f32;
                let confidence = (overlap as f32 / denominator).min(0.95);

                if confidence >= 0.2 {
                    suggestions.push(RoutingSuggestion {
                        thread_id: thread_id.clone(),
                        thread_title: title.clone(),
                        confidence,
                        reason: format!("{} keyword matches", overlap),
                    });
                }
            }
        }

        suggestions.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        suggestions.truncate(3); // Max 3 suggestions
        suggestions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn threads(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(id, title)| (id.to_string(), title.to_string()))
            .collect()
    }

    #[test]
    fn exact_title_word_match() {
        let active = threads(&[("thread-deploy", "Deployment Pipeline Setup")]);
        let suggestions = TopicRouter::find_matches("deployment pipeline is broken", &active);

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].thread_id, "thread-deploy");
        assert!(suggestions[0].confidence > 0.3);
        assert!(suggestions[0].reason.contains("keyword matches"));
    }

    #[test]
    fn partial_word_match_via_contains() {
        // "deploy" is contained in "deployment"
        let active = threads(&[("thread-deploy", "Deployment Pipeline")]);
        let suggestions = TopicRouter::find_matches("need to deploy the service", &active);

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].thread_id, "thread-deploy");
    }

    #[test]
    fn no_match_returns_empty() {
        let active = threads(&[("thread-deploy", "Deployment Pipeline")]);
        let suggestions = TopicRouter::find_matches("what is the weather today", &active);

        assert!(suggestions.is_empty());
    }

    #[test]
    fn multiple_matches_sorted_by_confidence() {
        let active = threads(&[
            ("thread-rust", "Rust Compiler Optimization"),
            ("thread-web", "Website Redesign"),
            ("thread-perf", "Rust Performance Benchmarks"),
        ]);
        let suggestions =
            TopicRouter::find_matches("rust performance optimization benchmark", &active);

        // Should match thread-perf and thread-rust; thread-perf should rank higher
        assert!(suggestions.len() >= 2);
        // First suggestion should have highest confidence
        assert!(suggestions[0].confidence >= suggestions[1].confidence);
    }

    #[test]
    fn short_words_are_ignored() {
        // Words like "is", "the", "a" (len < 4) should be skipped
        let active = threads(&[("thread-1", "The Big Plan")]);
        // "is the a" — all words < 4 chars, no significant overlap
        let suggestions = TopicRouter::find_matches("is the a", &active);

        assert!(suggestions.is_empty());
    }

    #[test]
    fn empty_message_returns_empty() {
        let active = threads(&[("thread-1", "Some Thread")]);
        let suggestions = TopicRouter::find_matches("", &active);

        assert!(suggestions.is_empty());
    }

    #[test]
    fn empty_active_threads_returns_empty() {
        let suggestions = TopicRouter::find_matches("deploy the service", &[]);

        assert!(suggestions.is_empty());
    }

    #[test]
    fn max_three_suggestions() {
        let active = threads(&[
            ("thread-1", "Rust Async Runtime"),
            ("thread-2", "Rust Error Handling"),
            ("thread-3", "Rust Memory Safety"),
            ("thread-4", "Rust Type System"),
        ]);
        let suggestions = TopicRouter::find_matches("rust programming patterns", &active);

        assert!(suggestions.len() <= 3);
    }

    #[test]
    fn confidence_capped_at_095() {
        // Single significant word that matches perfectly — confidence = 1/1 = 1.0
        // but should be capped at 0.95
        let active = threads(&[("thread-1", "Deployment")]);
        let suggestions = TopicRouter::find_matches("deployment", &active);

        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].confidence <= 0.95);
    }

    #[test]
    fn case_insensitive_matching() {
        let active = threads(&[("thread-1", "DATABASE Migration")]);
        let suggestions = TopicRouter::find_matches("database migration plan", &active);

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].thread_id, "thread-1");
    }

    #[test]
    fn suggestion_contains_thread_title() {
        let active = threads(&[("thread-x", "OAuth Integration")]);
        let suggestions = TopicRouter::find_matches("oauth integration failing", &active);

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].thread_title, "OAuth Integration");
    }
}
