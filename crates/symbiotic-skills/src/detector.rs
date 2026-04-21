//! Proactive synthesis detection for the agent's ReAct loop.
//!
//! The [`SynthesisDetector`] tracks problem signatures observed during an
//! agent execution and fires when the same category of problem has been
//! encountered enough times to justify forging a permanent skill.

use std::collections::HashMap;

/// A fingerprint of a problem the agent encountered.
#[derive(Debug, Clone)]
pub struct ProblemSignature {
    /// Category of the problem (e.g., "file-parsing", "api-integration").
    pub category: String,
    /// Free-text description of what the agent was trying to do.
    pub description: String,
    /// Hash of the approach used (to detect repetition of the same workaround).
    pub approach_hash: u64,
    /// Timestamp (epoch seconds) of when this problem was encountered.
    pub timestamp: u64,
}

/// Detects when repeated problems should trigger skill synthesis.
///
/// Runs inside the agent's ReAct observation phase. Each time the agent
/// encounters friction it calls [`observe`](Self::observe) with a
/// [`ProblemSignature`]. When any category hits the repetition threshold,
/// [`should_synthesize`](Self::should_synthesize) returns the category.
pub struct SynthesisDetector {
    /// All observed problem signatures, grouped by category.
    problem_log: HashMap<String, Vec<ProblemSignature>>,
    /// Minimum observations of the same category before suggesting synthesis.
    repetition_threshold: usize,
}

impl SynthesisDetector {
    /// Create a new detector with the given repetition threshold.
    ///
    /// A threshold of 2 means the third occurrence triggers synthesis
    /// (the agent saw the problem, worked around it, saw it again, and
    /// now should forge a tool).
    pub fn new(threshold: usize) -> Self {
        Self {
            problem_log: HashMap::new(),
            repetition_threshold: threshold,
        }
    }

    /// Log a problem observation.
    pub fn observe(&mut self, sig: ProblemSignature) {
        self.problem_log
            .entry(sig.category.clone())
            .or_default()
            .push(sig);
    }

    /// Check if any category has reached the repetition threshold.
    ///
    /// Returns `Some(category)` for the first category that exceeded the
    /// threshold, or `None` if no category qualifies yet.
    pub fn should_synthesize(&self) -> Option<&str> {
        for (category, sigs) in &self.problem_log {
            if sigs.len() >= self.repetition_threshold {
                return Some(category.as_str());
            }
        }
        None
    }

    /// Build a suggestion message for the agent, describing why synthesis
    /// is recommended and what the skill should do.
    pub fn suggestion(&self, category: &str) -> String {
        let sigs = match self.problem_log.get(category) {
            Some(s) => s,
            None => return format!("No observations for category \"{category}\"."),
        };

        let descriptions: Vec<&str> = sigs.iter().map(|s| s.description.as_str()).collect();
        let unique_approaches: Vec<u64> = {
            let mut v: Vec<u64> = sigs.iter().map(|s| s.approach_hash).collect();
            v.sort_unstable();
            v.dedup();
            v
        };

        format!(
            "Repeated friction detected in category \"{category}\" ({count} occurrences, \
             {approaches} distinct approaches). Descriptions:\n{descs}\n\n\
             Consider synthesizing a permanent skill to handle this problem class.",
            count = sigs.len(),
            approaches = unique_approaches.len(),
            descs = descriptions
                .iter()
                .enumerate()
                .map(|(i, d)| format!("  {}. {d}", i + 1))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// Reset the detector, clearing all observations.
    pub fn clear(&mut self) {
        self.problem_log.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(category: &str, desc: &str, hash: u64) -> ProblemSignature {
        ProblemSignature {
            category: category.to_string(),
            description: desc.to_string(),
            approach_hash: hash,
            timestamp: 1000,
        }
    }

    #[test]
    fn no_observations_returns_none() {
        let detector = SynthesisDetector::new(2);
        assert!(detector.should_synthesize().is_none());
    }

    #[test]
    fn below_threshold_returns_none() {
        let mut detector = SynthesisDetector::new(3);
        detector.observe(sig("file-parsing", "parsed CSV manually", 1));
        detector.observe(sig("file-parsing", "parsed CSV again", 2));
        assert!(detector.should_synthesize().is_none());
    }

    #[test]
    fn at_threshold_triggers() {
        let mut detector = SynthesisDetector::new(2);
        detector.observe(sig("file-parsing", "parsed CSV manually", 1));
        detector.observe(sig("file-parsing", "parsed CSV again", 2));
        assert_eq!(detector.should_synthesize(), Some("file-parsing"));
    }

    #[test]
    fn different_categories_dont_cross_trigger() {
        let mut detector = SynthesisDetector::new(2);
        detector.observe(sig("file-parsing", "parsed CSV", 1));
        detector.observe(sig("api-integration", "called REST API", 2));
        assert!(detector.should_synthesize().is_none());
    }

    #[test]
    fn clear_resets_state() {
        let mut detector = SynthesisDetector::new(2);
        detector.observe(sig("file-parsing", "parsed CSV", 1));
        detector.observe(sig("file-parsing", "parsed CSV again", 2));
        assert!(detector.should_synthesize().is_some());
        detector.clear();
        assert!(detector.should_synthesize().is_none());
    }

    #[test]
    fn suggestion_includes_descriptions() {
        let mut detector = SynthesisDetector::new(2);
        detector.observe(sig("json-patching", "patched config.json with sed", 1));
        detector.observe(sig("json-patching", "patched data.json with jq", 2));

        let msg = detector.suggestion("json-patching");
        assert!(msg.contains("json-patching"));
        assert!(msg.contains("2 occurrences"));
        assert!(msg.contains("2 distinct approaches"));
        assert!(msg.contains("patched config.json with sed"));
        assert!(msg.contains("patched data.json with jq"));
        assert!(msg.contains("Consider synthesizing"));
    }

    #[test]
    fn suggestion_unknown_category() {
        let detector = SynthesisDetector::new(2);
        let msg = detector.suggestion("nonexistent");
        assert!(msg.contains("No observations"));
    }

    #[test]
    fn threshold_of_one_triggers_on_first_observation() {
        let mut detector = SynthesisDetector::new(1);
        detector.observe(sig("quick", "one-shot", 1));
        assert_eq!(detector.should_synthesize(), Some("quick"));
    }
}
