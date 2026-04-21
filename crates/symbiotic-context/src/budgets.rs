//! Class budgets and progressive disclosure for the Recall Gateway.
//!
//! Class budgets prevent any single fact type from dominating the context window.
//! Progressive disclosure allows agents to start cheap (titles only) and go deeper
//! only when needed.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Fact Classes
// ---------------------------------------------------------------------------

/// Classification of a fact in the knowledge graph.
///
/// Maps to the typed facts extracted by the Distillery pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactClass {
    Decision,
    Finding,
    Entity,
    Preference,
    Methodology,
    Episode,
}

impl FactClass {
    /// Returns all known fact classes.
    pub fn all() -> &'static [FactClass] {
        &[
            FactClass::Decision,
            FactClass::Finding,
            FactClass::Entity,
            FactClass::Preference,
            FactClass::Methodology,
            FactClass::Episode,
        ]
    }
}

// ---------------------------------------------------------------------------
// Class Budget
// ---------------------------------------------------------------------------

/// Token budget allocation per fact class.
///
/// Each percentage represents the fraction of the total token budget that
/// a given fact class may consume. The percentages should sum to 1.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassBudget {
    pub decision_pct: f32,
    pub finding_pct: f32,
    pub entity_pct: f32,
    pub preference_pct: f32,
    pub methodology_pct: f32,
    pub episode_pct: f32,
}

impl Default for ClassBudget {
    fn default() -> Self {
        Self {
            decision_pct: 0.30,
            finding_pct: 0.25,
            entity_pct: 0.15,
            preference_pct: 0.10,
            methodology_pct: 0.10,
            episode_pct: 0.10,
        }
    }
}

impl ClassBudget {
    /// Returns the budget percentage for a given fact class.
    pub fn percentage_for(&self, class: FactClass) -> f32 {
        match class {
            FactClass::Decision => self.decision_pct,
            FactClass::Finding => self.finding_pct,
            FactClass::Entity => self.entity_pct,
            FactClass::Preference => self.preference_pct,
            FactClass::Methodology => self.methodology_pct,
            FactClass::Episode => self.episode_pct,
        }
    }

    /// Computes the absolute token limit for a class given a total budget.
    pub fn tokens_for(&self, class: FactClass, total_budget: usize) -> usize {
        (self.percentage_for(class) * total_budget as f32).floor() as usize
    }

    /// Returns a map of class -> token limit for a given total budget.
    pub fn allocate(&self, total_budget: usize) -> Vec<(FactClass, usize)> {
        FactClass::all()
            .iter()
            .map(|&class| (class, self.tokens_for(class, total_budget)))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Progressive Disclosure
// ---------------------------------------------------------------------------

/// Progressive disclosure tiers for context retrieval.
///
/// Agents start cheap (Tier 1: titles only) and go deeper when needed.
/// Each tier includes everything from the previous tier plus more detail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisclosureTier {
    /// Just entity/fact titles (cheapest).
    Titles = 0,
    /// One-line summaries.
    Summary = 1,
    /// Complete fact text.
    #[default]
    Full = 2,
    /// Include source conversation excerpts.
    Messages = 3,
    /// Include decision history and superseding chains.
    History = 4,
}

impl DisclosureTier {
    /// Truncates content based on the disclosure tier.
    ///
    /// - `Titles`: returns empty string (title is in the ContextItem.title field)
    /// - `Summary`: returns first line only
    /// - `Full` / `Messages` / `History`: returns full content
    ///
    /// `Messages` and `History` tiers would include additional data beyond the
    /// base content, but that augmentation happens at the retrieval layer. Here
    /// we only handle content truncation.
    pub fn truncate_content(&self, content: &str) -> String {
        match self {
            DisclosureTier::Titles => String::new(),
            DisclosureTier::Summary => {
                // Take only the first line, up to 120 chars
                let first_line = content.lines().next().unwrap_or("");
                if first_line.len() > 120 {
                    format!("{}...", &first_line[..117])
                } else {
                    first_line.to_string()
                }
            }
            DisclosureTier::Full | DisclosureTier::Messages | DisclosureTier::History => {
                content.to_string()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_class_budget_sums_to_one() {
        let budget = ClassBudget::default();
        let total = budget.decision_pct
            + budget.finding_pct
            + budget.entity_pct
            + budget.preference_pct
            + budget.methodology_pct
            + budget.episode_pct;
        assert!(
            (total - 1.0).abs() < 1e-6,
            "budget percentages should sum to 1.0, got {total}"
        );
    }

    #[test]
    fn default_budget_percentages() {
        let budget = ClassBudget::default();
        assert!((budget.decision_pct - 0.30).abs() < 1e-6);
        assert!((budget.finding_pct - 0.25).abs() < 1e-6);
        assert!((budget.entity_pct - 0.15).abs() < 1e-6);
        assert!((budget.preference_pct - 0.10).abs() < 1e-6);
        assert!((budget.methodology_pct - 0.10).abs() < 1e-6);
        assert!((budget.episode_pct - 0.10).abs() < 1e-6);
    }

    #[test]
    fn tokens_for_computes_correctly() {
        let budget = ClassBudget::default();
        // 30% of 1000 = 300
        assert_eq!(budget.tokens_for(FactClass::Decision, 1000), 300);
        // 25% of 1000 = 250
        assert_eq!(budget.tokens_for(FactClass::Finding, 1000), 250);
        // 15% of 1000 = 150
        assert_eq!(budget.tokens_for(FactClass::Entity, 1000), 150);
        // 10% of 1000 = 100
        assert_eq!(budget.tokens_for(FactClass::Preference, 1000), 100);
        assert_eq!(budget.tokens_for(FactClass::Methodology, 1000), 100);
        assert_eq!(budget.tokens_for(FactClass::Episode, 1000), 100);
    }

    #[test]
    fn allocate_returns_all_classes() {
        let budget = ClassBudget::default();
        let alloc = budget.allocate(1000);
        assert_eq!(alloc.len(), 6);

        let total_allocated: usize = alloc.iter().map(|(_, t)| t).sum();
        assert_eq!(total_allocated, 1000);
    }

    #[test]
    fn allocate_with_small_budget() {
        let budget = ClassBudget::default();
        let alloc = budget.allocate(10);
        let total_allocated: usize = alloc.iter().map(|(_, t)| t).sum();
        // With rounding (floor), small budgets may not fully allocate
        assert!(total_allocated <= 10);
    }

    #[test]
    fn disclosure_tier_ordering() {
        assert!(DisclosureTier::Titles < DisclosureTier::Summary);
        assert!(DisclosureTier::Summary < DisclosureTier::Full);
        assert!(DisclosureTier::Full < DisclosureTier::Messages);
        assert!(DisclosureTier::Messages < DisclosureTier::History);
    }

    #[test]
    fn disclosure_titles_returns_empty() {
        let content = "This is a detailed fact about something important.\nWith more lines.";
        let truncated = DisclosureTier::Titles.truncate_content(content);
        assert!(truncated.is_empty());
    }

    #[test]
    fn disclosure_summary_returns_first_line() {
        let content = "First line summary.\nSecond line with details.\nThird line.";
        let truncated = DisclosureTier::Summary.truncate_content(content);
        assert_eq!(truncated, "First line summary.");
    }

    #[test]
    fn disclosure_summary_truncates_long_lines() {
        let long_line = "x".repeat(200);
        let truncated = DisclosureTier::Summary.truncate_content(&long_line);
        assert_eq!(truncated.len(), 120); // 117 chars + "..."
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn disclosure_full_returns_everything() {
        let content = "Full content.\nWith all details.\nIncluding everything.";
        let truncated = DisclosureTier::Full.truncate_content(content);
        assert_eq!(truncated, content);
    }

    #[test]
    fn disclosure_messages_returns_everything() {
        let content = "Content with messages.";
        let truncated = DisclosureTier::Messages.truncate_content(content);
        assert_eq!(truncated, content);
    }

    #[test]
    fn disclosure_history_returns_everything() {
        let content = "Content with history.";
        let truncated = DisclosureTier::History.truncate_content(content);
        assert_eq!(truncated, content);
    }

    #[test]
    fn percentage_for_all_classes() {
        let budget = ClassBudget::default();
        for &class in FactClass::all() {
            let pct = budget.percentage_for(class);
            assert!(pct > 0.0, "class {class:?} should have non-zero budget");
            assert!(pct <= 1.0, "class {class:?} should be <= 1.0");
        }
    }
}
