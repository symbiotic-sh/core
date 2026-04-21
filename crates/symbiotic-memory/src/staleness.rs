//! Simple staleness detection for memory facts.
//!
//! Checks whether a fact's age exceeds its expected freshness cadence.
//! Temporal decay and relevance scoring are handled at read-time by the
//! graph retriever in `symbiotic-context` — this module only provides
//! the basic "is it stale?" check for display warnings.

use crate::types::{FactType, Memory, MemoryStatus};

use std::collections::HashSet;

// ── Result types ──────────────────────────────────────────────────────

/// Result of checking a single memory for staleness.
#[derive(Debug, Clone, PartialEq)]
pub enum StalenessResult {
    /// The memory is fresh.
    Fresh,
    /// The memory is stale — age exceeds the expected cadence.
    Stale { reason: String },
}

// ── Configuration ─────────────────────────────────────────────────────

/// Staleness cadence configuration per fact type.
///
/// Each fact type has an expected "freshness window" in days.
/// Beyond this window, the fact is considered potentially stale.
#[derive(Debug, Clone)]
pub struct StalenessConfig {
    /// Maximum age in days before a Decision is considered stale.
    pub decision_cadence_days: u64,
    /// Maximum age in days before a Finding is considered stale.
    pub finding_cadence_days: u64,
    /// Maximum age in days before a Preference is considered stale.
    pub preference_cadence_days: u64,
    /// Maximum age in days before an Entity reference is considered stale.
    pub entity_cadence_days: u64,
    /// Maximum age in days before an Episode is considered stale.
    pub episode_cadence_days: u64,
    /// Maximum age in days before a Methodology is considered stale.
    pub methodology_cadence_days: u64,
    /// Default cadence for untyped facts.
    pub default_cadence_days: u64,
}

impl Default for StalenessConfig {
    fn default() -> Self {
        Self {
            decision_cadence_days: 180,    // 6 months — decisions are durable
            finding_cadence_days: 90,      // 3 months — findings can go stale
            preference_cadence_days: 365,  // 1 year — preferences are sticky
            entity_cadence_days: 365,      // 1 year — entity refs are structural
            episode_cadence_days: 30,      // 1 month — episodes are temporal
            methodology_cadence_days: 180, // 6 months — methods change slowly
            default_cadence_days: 90,      // 3 months fallback
        }
    }
}

impl StalenessConfig {
    /// Get the cadence in days for a given fact type.
    pub fn cadence_days(&self, fact_type: Option<&FactType>) -> u64 {
        match fact_type {
            Some(FactType::Decision) => self.decision_cadence_days,
            Some(FactType::Finding) => self.finding_cadence_days,
            Some(FactType::Preference) => self.preference_cadence_days,
            Some(FactType::Entity) => self.entity_cadence_days,
            Some(FactType::Episode) => self.episode_cadence_days,
            Some(FactType::Methodology) => self.methodology_cadence_days,
            None => self.default_cadence_days,
        }
    }
}

// ── Checker ───────────────────────────────────────────────────────────

/// Checks memories for staleness based on age and fact type cadence.
///
/// This is a simple age check — no cascading propagation. Temporal decay
/// and relevance scoring happen at read-time in the graph retriever.
pub struct StalenessChecker {
    config: StalenessConfig,
}

impl StalenessChecker {
    pub fn new(config: StalenessConfig) -> Self {
        Self { config }
    }

    /// Check if a single memory is stale based on its age and type.
    ///
    /// `age_days` is the number of days since the memory was created
    /// (caller computes this from `created_at` or `valid_from`).
    pub fn check_staleness(&self, memory: &Memory, age_days: u64) -> StalenessResult {
        // Already-superseded memories are inherently stale
        if memory.status == MemoryStatus::Superseded {
            return StalenessResult::Stale {
                reason: "superseded by a newer fact".to_string(),
            };
        }

        // Expired memories are stale
        if memory.status == MemoryStatus::Expired {
            return StalenessResult::Stale {
                reason: "past valid_to date".to_string(),
            };
        }

        let cadence = self.config.cadence_days(memory.fact_type.as_ref());
        if age_days > cadence {
            StalenessResult::Stale {
                reason: format!(
                    "age ({age_days} days) exceeds {} cadence ({cadence} days)",
                    memory
                        .fact_type
                        .as_ref()
                        .map(|ft| ft.as_str())
                        .unwrap_or("default")
                ),
            }
        } else {
            StalenessResult::Fresh
        }
    }

    /// Run a staleness scan over a set of memories.
    ///
    /// Returns the set of stale memory IDs.
    /// `age_fn` computes the age in days for each memory.
    pub fn scan<F>(&self, memories: &[Memory], age_fn: F) -> HashSet<String>
    where
        F: Fn(&Memory) -> u64,
    {
        let mut stale_ids = HashSet::new();

        for mem in memories {
            if mem.status != MemoryStatus::Active {
                continue;
            }
            let age = age_fn(mem);
            if let StalenessResult::Stale { .. } = self.check_staleness(mem, age) {
                stale_ids.insert(mem.id.clone());
            }
        }

        stale_ids
    }
}

impl Default for StalenessChecker {
    fn default() -> Self {
        Self::new(StalenessConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FactDisposition, FactType, Memory, MemoryStatus, Sensitivity};

    fn make_memory(id: &str, fact_type: Option<FactType>) -> Memory {
        Memory {
            id: id.to_string(),
            entity_id: "ent-1".to_string(),
            fact: format!("fact for {id}"),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: "2026-01-01T00:00:00Z".to_string(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            fact_type,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        }
    }

    #[test]
    fn fresh_finding_within_cadence() {
        let checker = StalenessChecker::default();
        let mem = make_memory("f-1", Some(FactType::Finding));
        let result = checker.check_staleness(&mem, 30); // 30 days, cadence is 90
        assert_eq!(result, StalenessResult::Fresh);
    }

    #[test]
    fn stale_finding_past_cadence() {
        let checker = StalenessChecker::default();
        let mem = make_memory("f-1", Some(FactType::Finding));
        let result = checker.check_staleness(&mem, 100); // 100 days, cadence is 90
        assert!(matches!(result, StalenessResult::Stale { .. }));
    }

    #[test]
    fn decision_stays_fresh_longer() {
        let checker = StalenessChecker::default();
        let mem = make_memory("d-1", Some(FactType::Decision));
        let result = checker.check_staleness(&mem, 100);
        assert_eq!(result, StalenessResult::Fresh);
    }

    #[test]
    fn episode_goes_stale_quickly() {
        let checker = StalenessChecker::default();
        let mem = make_memory("e-1", Some(FactType::Episode));
        let result = checker.check_staleness(&mem, 35); // 35 days, cadence is 30
        assert!(matches!(result, StalenessResult::Stale { .. }));
    }

    #[test]
    fn superseded_memory_is_stale() {
        let checker = StalenessChecker::default();
        let mut mem = make_memory("s-1", Some(FactType::Decision));
        mem.status = MemoryStatus::Superseded;
        let result = checker.check_staleness(&mem, 1);
        assert!(matches!(result, StalenessResult::Stale { .. }));
    }

    #[test]
    fn untyped_fact_uses_default_cadence() {
        let checker = StalenessChecker::default();
        let mem = make_memory("u-1", None);
        assert_eq!(checker.check_staleness(&mem, 89), StalenessResult::Fresh);
        assert!(matches!(
            checker.check_staleness(&mem, 91),
            StalenessResult::Stale { .. }
        ));
    }

    #[test]
    fn scan_finds_stale_facts() {
        let checker = StalenessChecker::default();

        let old_finding = make_memory("old-finding", Some(FactType::Finding));
        let fresh_pref = make_memory("fresh-pref", Some(FactType::Preference));

        let memories = vec![old_finding, fresh_pref];

        let stale = checker.scan(
            &memories,
            |mem| {
                if mem.id == "old-finding" {
                    100
                } else {
                    10
                }
            },
        );

        assert!(stale.contains("old-finding"));
        assert!(!stale.contains("fresh-pref"));
    }
}
