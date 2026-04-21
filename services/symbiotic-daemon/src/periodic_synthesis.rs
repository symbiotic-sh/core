//! Periodic synthesis — weekly cross-thread knowledge consolidation.
//!
//! Runs as a background process (configurable frequency, default weekly) to:
//! 1. Merge overlapping facts across threads into standalone knowledge notes
//! 2. Refresh entity profiles from all active facts
//! 3. Extract methodology patterns from user corrections
//! 4. Decay somatic scores for stale facts
//! 5. Compile decision trace registers
//!
//! See `docs/design/memory-system.md` Layer 5 — Periodic Synthesis.

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// Re-exported domain types from symbiotic-memory.
// These will resolve once the crate dependency is wired in Cargo.toml.
use symbiotic_memory::entity_dedup::{EntityDeduplicator, MergeReport};
use symbiotic_memory::self_improvement::{FrictionDetector, GraphMaintenanceSnapshot, Proposal};
use symbiotic_memory::store::MemoryStore;
use symbiotic_memory::types::{Entity, EntityType, Memory, MemoryStatus, Relationship};

use crate::entity_profiles::EntityProfileGenerator;
use crate::thread_manager::ThreadManager;

/// An action produced by the cross-thread merge process.
///
/// Each action represents a set of overlapping facts across multiple threads
/// that should be consolidated into a single knowledge note.
#[derive(Debug, Clone)]
pub struct MergeAction {
    /// Thread IDs where the overlapping facts were found.
    pub source_threads: Vec<String>,
    /// The merged fact to create (consolidated from the sources).
    pub merged_fact: MergedFact,
    /// IDs of facts that are superseded by the merge.
    pub superseded_ids: Vec<String>,
}

/// A fact produced by merging overlapping memories across threads.
#[derive(Debug, Clone)]
pub struct MergedFact {
    /// The entity this fact belongs to.
    pub entity_id: String,
    /// The consolidated fact text.
    pub fact: String,
    /// Highest confidence among the source facts.
    pub confidence: f64,
    /// Earliest valid_from among the source facts.
    pub valid_from: String,
}

/// A thread's fact set for cross-thread analysis.
#[derive(Debug, Clone)]
pub struct ThreadFacts {
    /// Thread identifier.
    pub thread_id: String,
    /// Active facts in this thread.
    pub facts: Vec<Memory>,
}

/// A methodology pattern extracted from user corrections.
#[derive(Debug, Clone)]
pub struct MethodologyPattern {
    /// The pattern title.
    pub title: String,
    /// The pattern description (rule/process).
    pub description: String,
    /// How many corrections/preferences support this pattern.
    pub supporting_count: usize,
    /// The correction IDs that support this pattern.
    pub source_ids: Vec<String>,
}

/// Result of a stale fact decay pass.
#[derive(Debug, Clone)]
pub struct DecayResult {
    /// Number of facts that had their somatic scores zeroed.
    pub decayed_count: usize,
    /// IDs of facts that were decayed.
    pub decayed_ids: Vec<String>,
}

/// A structural proposal routed either to a specific thread or to the
/// graph-global stream when it is not thread-scoped.
#[derive(Debug, Clone)]
pub struct RoutedProposal {
    pub thread_id: Option<String>,
    pub proposal: Proposal,
}

/// Periodic synthesis engine.
///
/// Runs deep consolidation passes over the entire knowledge graph.
pub struct PeriodicSynthesizer {
    kb_root: PathBuf,
}

impl PeriodicSynthesizer {
    /// Create a new synthesizer targeting the given knowledge-base root.
    pub fn new(kb_root: impl AsRef<Path>) -> Self {
        Self {
            kb_root: kb_root.as_ref().to_path_buf(),
        }
    }

    /// Weekly cross-thread knowledge merge.
    ///
    /// Finds overlapping facts across threads (same entity, similar fact text)
    /// and produces `MergeAction`s to consolidate them into standalone knowledge
    /// notes. The caller is responsible for executing the actions (creating new
    /// memories, superseding old ones, writing knowledge notes).
    ///
    /// # Algorithm
    /// 1. Group all facts by entity_id across all threads
    /// 2. Within each entity group, find facts that appear in 2+ threads
    /// 3. For facts with high text similarity, produce a MergeAction
    pub fn cross_thread_merge(&self, thread_facts: &[ThreadFacts]) -> Vec<MergeAction> {
        // Group facts by entity_id, tracking which thread each came from.
        let mut entity_facts: HashMap<String, Vec<(String, &Memory)>> = HashMap::new();
        for tf in thread_facts {
            for fact in &tf.facts {
                entity_facts
                    .entry(fact.entity_id.clone())
                    .or_default()
                    .push((tf.thread_id.clone(), fact));
            }
        }

        let mut actions = Vec::new();

        for facts_with_threads in entity_facts.values() {
            // Only consider entities that appear in 2+ threads.
            let unique_threads: Vec<&str> = {
                let mut ts: Vec<&str> =
                    facts_with_threads.iter().map(|(t, _)| t.as_str()).collect();
                ts.sort();
                ts.dedup();
                ts
            };
            if unique_threads.len() < 2 {
                continue;
            }

            // Find similar facts across threads using pairwise comparison.
            let mut merged_groups: Vec<Vec<(String, &Memory)>> = Vec::new();

            for (thread_id, fact) in facts_with_threads {
                let mut found_group = false;
                for group in &mut merged_groups {
                    // Check if this fact is similar to any fact in the group
                    // AND comes from a different thread.
                    let group_threads: Vec<&str> = group.iter().map(|(t, _)| t.as_str()).collect();
                    if group_threads.contains(&thread_id.as_str()) {
                        continue; // Same thread, skip.
                    }
                    let is_similar = group
                        .iter()
                        .any(|(_, gf)| text_similarity(&fact.fact, &gf.fact) > 0.6);
                    if is_similar {
                        group.push((thread_id.clone(), fact));
                        found_group = true;
                        break;
                    }
                }
                if !found_group {
                    merged_groups.push(vec![(thread_id.clone(), fact)]);
                }
            }

            // Only produce MergeActions for groups spanning 2+ threads.
            for group in merged_groups {
                let mut thread_set: Vec<String> = group.iter().map(|(t, _)| t.clone()).collect();
                thread_set.sort();
                thread_set.dedup();

                if thread_set.len() < 2 {
                    continue;
                }

                // Pick the highest confidence fact as the representative.
                let best = group
                    .iter()
                    .max_by(|a, b| a.1.confidence.partial_cmp(&b.1.confidence).unwrap())
                    .unwrap();

                // Earliest valid_from.
                let earliest_from = group
                    .iter()
                    .map(|(_, f)| f.valid_from.as_str())
                    .min()
                    .unwrap_or(&best.1.valid_from);

                let superseded: Vec<String> = group.iter().map(|(_, f)| f.id.clone()).collect();

                actions.push(MergeAction {
                    source_threads: thread_set,
                    merged_fact: MergedFact {
                        entity_id: best.1.entity_id.clone(),
                        fact: best.1.fact.clone(),
                        confidence: best.1.confidence,
                        valid_from: earliest_from.to_string(),
                    },
                    superseded_ids: superseded,
                });
            }
        }

        actions
    }

    /// Refresh entity profiles from all active facts.
    ///
    /// Iterates over all provided entities and regenerates their profile
    /// documents using the `EntityProfileGenerator`.
    ///
    /// Returns the number of profiles that were successfully refreshed.
    pub fn refresh_entity_profiles(
        &self,
        entities: &[Entity],
        entity_facts: &HashMap<String, Vec<Memory>>,
        entity_relationships: &HashMap<String, Vec<Relationship>>,
        entity_thread_refs: &HashMap<String, Vec<String>>,
        generator: &EntityProfileGenerator,
    ) -> usize {
        let mut refreshed = 0;

        for entity in entities {
            let facts = entity_facts
                .get(&entity.id)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let rels = entity_relationships
                .get(&entity.id)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let refs = entity_thread_refs
                .get(&entity.id)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            match generator.generate(entity, facts, rels, refs) {
                Ok(_) => refreshed += 1,
                Err(e) => {
                    tracing::warn!(
                        entity_id = %entity.id,
                        error = %e,
                        "failed to refresh entity profile"
                    );
                }
            }
        }

        refreshed
    }

    /// Extract methodology patterns from user corrections and preferences.
    ///
    /// Looks for repeated correction patterns (facts with high confidence
    /// that have superseded other facts) and preferences that suggest
    /// reusable processes or rules.
    ///
    /// # Algorithm
    /// 1. Find all corrections (facts that superseded another fact)
    /// 2. Group corrections by entity
    /// 3. If an entity has 2+ corrections with similar themes, extract a pattern
    pub fn extract_methodology(&self, corrections: &[Memory]) -> Vec<MethodologyPattern> {
        // Group corrections by entity_id.
        let mut by_entity: HashMap<String, Vec<&Memory>> = HashMap::new();
        for correction in corrections {
            by_entity
                .entry(correction.entity_id.clone())
                .or_default()
                .push(correction);
        }

        let mut patterns = Vec::new();

        for entity_corrections in by_entity.values() {
            if entity_corrections.len() < 2 {
                continue;
            }

            // Look for clusters of similar corrections.
            let mut clusters: Vec<Vec<&Memory>> = Vec::new();
            for correction in entity_corrections {
                let mut found = false;
                for cluster in &mut clusters {
                    let is_similar = cluster
                        .iter()
                        .any(|c| text_similarity(&correction.fact, &c.fact) >= 0.4);
                    if is_similar {
                        cluster.push(correction);
                        found = true;
                        break;
                    }
                }
                if !found {
                    clusters.push(vec![correction]);
                }
            }

            for cluster in clusters {
                if cluster.len() < 2 {
                    continue;
                }

                // Use the highest-confidence correction as the representative.
                let best = cluster
                    .iter()
                    .max_by(|a, b| a.confidence.partial_cmp(&b.confidence).unwrap())
                    .unwrap();

                let source_ids: Vec<String> = cluster.iter().map(|c| c.id.clone()).collect();

                patterns.push(MethodologyPattern {
                    title: format!("Pattern: {}", truncate_fact(&best.fact, 60)),
                    description: best.fact.clone(),
                    supporting_count: cluster.len(),
                    source_ids,
                });
            }
        }

        patterns
    }

    /// Decay stale facts by identifying facts older than the threshold.
    ///
    /// Returns a `DecayResult` with the IDs of facts whose somatic scores
    /// should be zeroed. The caller is responsible for actually updating
    /// the somatic index.
    ///
    /// # Arguments
    /// * `facts` — all active facts to consider
    /// * `now_epoch` — current Unix timestamp
    /// * `decay_threshold_days` — facts older than this many days are decayed
    pub fn decay_stale_facts(
        &self,
        facts: &[Memory],
        now_epoch: u64,
        decay_threshold_days: u32,
    ) -> DecayResult {
        let threshold_secs = u64::from(decay_threshold_days) * 86400;
        let mut decayed_ids = Vec::new();

        for fact in facts {
            if fact.status != MemoryStatus::Active {
                continue;
            }

            // Parse updated_at as a Unix timestamp or ISO-8601 date.
            let fact_epoch = parse_epoch(&fact.updated_at);
            if let Some(epoch) = fact_epoch {
                if now_epoch.saturating_sub(epoch) > threshold_secs {
                    decayed_ids.push(fact.id.clone());
                }
            }
        }

        let decayed_count = decayed_ids.len();
        DecayResult {
            decayed_count,
            decayed_ids,
        }
    }

    /// Write a consolidated knowledge note to `knowledge-base/knowledge/`.
    ///
    /// Used by cross-thread merge to persist merged facts as standalone notes.
    pub fn write_knowledge_note(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        source_threads: &[String],
    ) -> Result<PathBuf> {
        let knowledge_dir = self.kb_root.join("knowledge");
        fs::create_dir_all(&knowledge_dir).with_context(|| {
            format!(
                "failed to create knowledge directory {}",
                knowledge_dir.display()
            )
        })?;

        let mut doc = String::with_capacity(1024);
        writeln!(doc, "---").unwrap();
        writeln!(doc, "type: knowledge").unwrap();
        writeln!(doc, "source: periodic-synthesis").unwrap();
        writeln!(doc, "---").unwrap();
        writeln!(doc).unwrap();
        writeln!(doc, "# {title}").unwrap();
        writeln!(doc).unwrap();
        writeln!(doc, "{content}").unwrap();
        writeln!(doc).unwrap();

        if !source_threads.is_empty() {
            writeln!(doc, "## Sources").unwrap();
            writeln!(doc).unwrap();
            for thread in source_threads {
                let thread_slug = thread.strip_prefix("thread-").unwrap_or(thread);
                writeln!(doc, "- [[Thread: {thread_slug}]]").unwrap();
            }
            writeln!(doc).unwrap();
        }

        let path = knowledge_dir.join(format!("{slug}.md"));
        fs::write(&path, &doc)
            .with_context(|| format!("failed to write knowledge note {}", path.display()))?;

        Ok(path)
    }

    /// Write methodology patterns to `knowledge-base/operations/`.
    pub fn write_methodology_note(
        &self,
        slug: &str,
        pattern: &MethodologyPattern,
    ) -> Result<PathBuf> {
        let methodology_dir = self.kb_root.join("operations");
        fs::create_dir_all(&methodology_dir).with_context(|| {
            format!(
                "failed to create operations directory {}",
                methodology_dir.display()
            )
        })?;

        let mut doc = String::with_capacity(512);
        writeln!(doc, "---").unwrap();
        writeln!(doc, "type: methodology").unwrap();
        writeln!(doc, "source: periodic-synthesis").unwrap();
        writeln!(doc, "supporting_corrections: {}", pattern.supporting_count).unwrap();
        writeln!(doc, "---").unwrap();
        writeln!(doc).unwrap();
        writeln!(doc, "# {}", pattern.title).unwrap();
        writeln!(doc).unwrap();
        writeln!(doc, "{}", pattern.description).unwrap();
        writeln!(doc).unwrap();

        let path = methodology_dir.join(format!("{slug}.md"));
        fs::write(&path, &doc)
            .with_context(|| format!("failed to write operations note {}", path.display()))?;

        Ok(path)
    }

    /// Run weekly entity deduplication pass.
    ///
    /// Loads all active entities from the store, finds duplicate candidates,
    /// and auto-merges those above the deduplicator's confidence threshold.
    ///
    /// Returns a summary of all merges performed.
    pub async fn run_entity_dedup(&self, store: &dyn MemoryStore) -> Result<Vec<MergeReport>> {
        let dedup = EntityDeduplicator::default();

        // Load all active entities across all types.
        let mut all_entities = Vec::new();
        for etype in &EntityType::ALL {
            match store.find_entities_by_type(*etype, 10_000).await {
                Ok(entities) => all_entities.extend(entities),
                Err(e) => {
                    tracing::warn!(
                        entity_type = %etype,
                        error = %e,
                        "failed to load entities for dedup"
                    );
                }
            }
        }

        if all_entities.is_empty() {
            tracing::info!("entity dedup: no active entities found, skipping");
            return Ok(Vec::new());
        }

        tracing::info!(
            entity_count = all_entities.len(),
            "entity dedup: scanning for duplicates"
        );

        let reports = dedup
            .find_and_auto_merge(&all_entities, store)
            .await
            .map_err(|e| anyhow::anyhow!("entity dedup merge failed: {e}"))?;

        if reports.is_empty() {
            tracing::info!("entity dedup: no duplicates found");
        } else {
            tracing::info!(
                merge_count = reports.len(),
                "entity dedup: completed merges"
            );
            for report in &reports {
                tracing::info!(
                    source = %report.source,
                    target = %report.target,
                    "entity dedup: merged"
                );
            }
        }

        Ok(reports)
    }

    /// Run friction detection: scan threads for structural problems
    /// (divergent topics, overlapping threads) and produce proposals.
    ///
    /// Called from the serve loop on a ~24 hour cadence alongside entity dedup.
    /// Returns routed proposals so the caller can emit events either to the
    /// correct thread room or to the graph-global stream.
    pub fn run_friction_detection(
        &self,
        thread_manager: &ThreadManager,
        graph_snapshot: Option<&GraphMaintenanceSnapshot>,
    ) -> Vec<RoutedProposal> {
        let detector = FrictionDetector::default();

        // Build thread_topics from active thread titles.
        // Each title is tokenized into lowercase keyword topics,
        // filtering common stopwords. This is a v1 heuristic — when
        // real entity-per-thread tracking is available, swap this out.
        let thread_topics = Self::gather_thread_topics(thread_manager);

        if thread_topics.is_empty() && graph_snapshot.is_none() {
            tracing::debug!("friction_detection: no active threads or graph snapshot, skipping");
            return Vec::new();
        }

        tracing::info!(
            thread_count = thread_topics.len(),
            graph_snapshot = graph_snapshot.is_some(),
            "friction_detection: scanning for structural issues"
        );

        let mut proposals = Vec::new();

        // 1. Detect divergent threads (5+ distinct topics in one thread).
        let divergent = detector.detect_divergent_threads(&thread_topics);
        for signal in &divergent {
            let proposal = detector.propose(signal);
            if let symbiotic_memory::self_improvement::FrictionSignal::DivergentTopics {
                thread_id,
                ..
            } = signal
            {
                tracing::info!(
                    thread_id = %thread_id,
                    topic_count = proposal.description.len(),
                    "friction_detection: divergent topics detected"
                );
                proposals.push(RoutedProposal {
                    thread_id: Some(thread_id.clone()),
                    proposal,
                });
            }
        }

        // 2. Detect overlapping threads (same topic in 3+ threads).
        let overlapping = detector.detect_overlapping_threads(&thread_topics);
        for signal in &overlapping {
            let proposal = detector.propose(signal);
            if let symbiotic_memory::self_improvement::FrictionSignal::OverlappingThreads {
                thread_ids,
                ..
            } = signal
            {
                tracing::info!(
                    thread_count = thread_ids.len(),
                    "friction_detection: overlapping threads detected"
                );
                // Emit proposal to the first thread in the overlap set.
                if let Some(first_thread) = thread_ids.first() {
                    proposals.push(RoutedProposal {
                        thread_id: Some(first_thread.clone()),
                        proposal,
                    });
                }
            }
        }

        // 3. Detect orphaned graph nodes and emit graph-global proposals.
        if let Some(graph_snapshot) = graph_snapshot {
            let orphaned = detector.detect_orphaned_nodes(graph_snapshot);
            for signal in &orphaned {
                let proposal = detector.propose(signal);
                if let symbiotic_memory::self_improvement::FrictionSignal::OrphanedNode {
                    entity_id,
                    candidate_reconnections,
                    ..
                } = signal
                {
                    tracing::info!(
                        entity_id = %entity_id,
                        candidate_count = candidate_reconnections.len(),
                        "friction_detection: orphaned node detected"
                    );
                }
                proposals.push(RoutedProposal {
                    thread_id: None,
                    proposal,
                });
            }
        }

        if proposals.is_empty() {
            tracing::info!("friction_detection: no structural issues found");
        } else {
            tracing::info!(
                proposal_count = proposals.len(),
                "friction_detection: generated proposals"
            );
        }

        proposals
    }

    /// Build a `thread_id -> topics` map from active thread titles.
    ///
    /// Tokenizes each title into lowercase keywords, filtering common English
    /// stopwords. This provides a baseline for friction detection until real
    /// entity-per-thread tracking is available.
    pub fn gather_thread_topics(thread_manager: &ThreadManager) -> HashMap<String, Vec<String>> {
        const STOPWORDS: &[&str] = &[
            "a", "an", "the", "and", "or", "but", "is", "are", "was", "were", "be", "been",
            "being", "have", "has", "had", "do", "does", "did", "will", "would", "could", "should",
            "may", "might", "can", "shall", "to", "of", "in", "for", "on", "with", "at", "by",
            "from", "as", "into", "about", "up", "out", "that", "this", "it", "its", "my", "your",
            "our", "their", "his", "her", "i", "me", "we", "they", "you", "he", "she", "not", "no",
            "so", "if", "how", "what", "when", "where", "which", "who",
        ];

        let active_threads = thread_manager.active_threads();
        let mut thread_topics = HashMap::new();

        for entry in &active_threads {
            let topics: Vec<String> = entry
                .title
                .to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| word.len() > 2 && !STOPWORDS.contains(word))
                .map(String::from)
                .collect();

            if !topics.is_empty() {
                thread_topics.insert(entry.thread_id.clone(), topics);
            }
        }

        thread_topics
    }
}

// --- Helpers ---

/// Simple word-overlap text similarity (Jaccard coefficient on word sets).
/// Returns 0.0 (no overlap) to 1.0 (identical word sets).
fn text_similarity(a: &str, b: &str) -> f64 {
    let a_set: std::collections::HashSet<String> = a
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    let b_set: std::collections::HashSet<String> = b
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();

    if a_set.is_empty() && b_set.is_empty() {
        return 1.0;
    }
    if a_set.is_empty() || b_set.is_empty() {
        return 0.0;
    }

    let intersection = a_set.intersection(&b_set).count();
    let union = a_set.union(&b_set).count();

    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Truncate a fact string to the given max length, appending "..." if needed.
fn truncate_fact(fact: &str, max_len: usize) -> String {
    if fact.len() <= max_len {
        fact.to_string()
    } else {
        // Truncate at word boundary.
        let truncated: String = fact.chars().take(max_len).collect();
        match truncated.rfind(' ') {
            Some(pos) if pos > max_len / 2 => format!("{}...", &truncated[..pos]),
            _ => format!("{truncated}..."),
        }
    }
}

/// Parse a date string as a Unix epoch.
///
/// Supports:
/// - Raw numeric epoch (e.g. "1710000000")
/// - ISO-8601 YYYY-MM-DD (approximated as days since epoch)
fn parse_epoch(date_str: &str) -> Option<u64> {
    // Try parsing as a raw numeric epoch first.
    if let Ok(epoch) = date_str.parse::<u64>() {
        return Some(epoch);
    }

    // Try ISO-8601 date: YYYY-MM-DD.
    // Approximate: count days from a reference point (2020-01-01 = 1577836800).
    let parts: Vec<&str> = date_str.split(&['-', 'T'][..]).collect();
    if parts.len() >= 3 {
        let year: u64 = parts[0].parse().ok()?;
        let month: u64 = parts[1].parse().ok()?;
        let day: u64 = parts[2].parse().ok()?;

        // Rough epoch calculation (not leap-year accurate, but sufficient
        // for staleness threshold comparison).
        let days_since_epoch = (year - 1970) * 365 + (month - 1) * 30 + day;
        return Some(days_since_epoch * 86400);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use symbiotic_memory::types::{
        AllowedModels, EntityStatus, FactDisposition, MemoryStatus, Sensitivity,
    };

    fn make_memory(id: &str, entity_id: &str, fact: &str, date: &str, confidence: f64) -> Memory {
        Memory {
            id: id.to_string(),
            entity_id: entity_id.to_string(),
            fact: fact.to_string(),
            confidence,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Shareable,
            valid_from: date.to_string(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: date.to_string(),
            updated_at: date.to_string(),
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: vec![],
            fsrs: None,
        }
    }

    fn make_entity(id: &str, name: &str, etype: EntityType) -> Entity {
        Entity {
            id: id.to_string(),
            entity_type: etype,
            name: name.to_string(),
            attributes: serde_json::json!({}),
            sensitivity: Sensitivity::Shareable,
            allowed_models: AllowedModels::Any,
            space: symbiotic_core::MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: "2026-03-10T00:00:00Z".to_string(),
            updated_at: "2026-03-10T00:00:00Z".to_string(),
        }
    }

    // --- text_similarity tests ---

    #[test]
    fn text_similarity_identical() {
        assert!((text_similarity("hello world", "hello world") - 1.0).abs() < 0.01);
    }

    #[test]
    fn text_similarity_no_overlap() {
        assert!((text_similarity("hello world", "foo bar")).abs() < 0.01);
    }

    #[test]
    fn text_similarity_partial_overlap() {
        let sim = text_similarity("vue is great for ssr", "vue is good for rendering");
        assert!(sim > 0.2);
        assert!(sim < 0.8);
    }

    #[test]
    fn text_similarity_empty() {
        assert!((text_similarity("", "") - 1.0).abs() < 0.01);
        assert!((text_similarity("hello", "")).abs() < 0.01);
    }

    // --- cross_thread_merge tests ---

    #[test]
    fn cross_thread_merge_finds_overlapping_facts() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let thread_facts = vec![
            ThreadFacts {
                thread_id: "thread-saas".to_string(),
                facts: vec![make_memory(
                    "m1",
                    "vue-001",
                    "Vue chosen for frontend framework",
                    "2026-03-14",
                    0.9,
                )],
            },
            ThreadFacts {
                thread_id: "thread-competitor".to_string(),
                facts: vec![make_memory(
                    "m2",
                    "vue-001",
                    "Vue chosen for frontend framework development",
                    "2026-03-15",
                    0.85,
                )],
            },
        ];

        let actions = synth.cross_thread_merge(&thread_facts);
        assert_eq!(actions.len(), 1);

        let action = &actions[0];
        assert!(action.source_threads.contains(&"thread-saas".to_string()));
        assert!(action
            .source_threads
            .contains(&"thread-competitor".to_string()));
        assert_eq!(action.superseded_ids.len(), 2);
        assert!(action.merged_fact.confidence >= 0.85);
    }

    #[test]
    fn cross_thread_merge_ignores_single_thread_facts() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let thread_facts = vec![
            ThreadFacts {
                thread_id: "thread-a".to_string(),
                facts: vec![make_memory(
                    "m1",
                    "vue-001",
                    "Vue is great",
                    "2026-03-14",
                    0.9,
                )],
            },
            ThreadFacts {
                thread_id: "thread-b".to_string(),
                facts: vec![make_memory(
                    "m2",
                    "react-001", // Different entity
                    "React is popular",
                    "2026-03-15",
                    0.85,
                )],
            },
        ];

        let actions = synth.cross_thread_merge(&thread_facts);
        assert!(actions.is_empty());
    }

    #[test]
    fn cross_thread_merge_no_merge_for_dissimilar_facts() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let thread_facts = vec![
            ThreadFacts {
                thread_id: "thread-a".to_string(),
                facts: vec![make_memory(
                    "m1",
                    "vue-001",
                    "Vue has great SSR support",
                    "2026-03-14",
                    0.9,
                )],
            },
            ThreadFacts {
                thread_id: "thread-b".to_string(),
                facts: vec![make_memory(
                    "m2",
                    "vue-001",
                    "License is MIT and commercially permissive",
                    "2026-03-15",
                    0.85,
                )],
            },
        ];

        let actions = synth.cross_thread_merge(&thread_facts);
        assert!(actions.is_empty());
    }

    // --- extract_methodology tests ---

    #[test]
    fn extract_methodology_finds_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let corrections = vec![
            make_memory(
                "c1",
                "pref-001",
                "Always use dark mode for code editors",
                "2026-03-10",
                0.9,
            ),
            make_memory(
                "c2",
                "pref-001",
                "Always prefer dark mode for development tools",
                "2026-03-12",
                0.95,
            ),
            make_memory(
                "c3",
                "pref-001",
                "Dark mode preferred for all IDE configurations",
                "2026-03-14",
                0.88,
            ),
        ];

        let patterns = synth.extract_methodology(&corrections);
        assert!(!patterns.is_empty());
        assert!(patterns[0].supporting_count >= 2);
    }

    #[test]
    fn extract_methodology_ignores_single_corrections() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let corrections = vec![make_memory(
            "c1",
            "pref-001",
            "Use dark mode",
            "2026-03-10",
            0.9,
        )];

        let patterns = synth.extract_methodology(&corrections);
        assert!(patterns.is_empty());
    }

    // --- decay_stale_facts tests ---

    #[test]
    fn decay_stale_facts_identifies_old_facts() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        // A fact updated at epoch 1000 and current time is 1000 + 91 days.
        let facts = vec![make_memory("m1", "e1", "Old fact", "1000", 0.9)];

        let now = 1000 + 91 * 86400; // 91 days later
        let result = synth.decay_stale_facts(&facts, now, 90);

        assert_eq!(result.decayed_count, 1);
        assert_eq!(result.decayed_ids, vec!["m1"]);
    }

    #[test]
    fn decay_stale_facts_keeps_fresh_facts() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let facts = vec![make_memory("m1", "e1", "Fresh fact", "1000", 0.9)];

        let now = 1000 + 30 * 86400; // 30 days later
        let result = synth.decay_stale_facts(&facts, now, 90);

        assert_eq!(result.decayed_count, 0);
    }

    #[test]
    fn decay_stale_facts_ignores_non_active() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let mut fact = make_memory("m1", "e1", "Old superseded", "1000", 0.9);
        fact.status = MemoryStatus::Superseded;

        let now = 1000 + 200 * 86400;
        let result = synth.decay_stale_facts(&[fact], now, 90);

        assert_eq!(result.decayed_count, 0);
    }

    // --- write_knowledge_note tests ---

    #[test]
    fn write_knowledge_note_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let path = synth
            .write_knowledge_note(
                "vue-vs-react",
                "Vue outperforms React for SSR",
                "Based on analysis across multiple threads, Vue + Nuxt provides better SSR.",
                &["thread-saas".to_string(), "thread-research".to_string()],
            )
            .unwrap();

        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Vue outperforms React for SSR"));
        assert!(content.contains("type: knowledge"));
        assert!(content.contains("source: periodic-synthesis"));
        assert!(content.contains("[[Thread: saas]]"));
        assert!(content.contains("[[Thread: research]]"));
    }

    // --- write_methodology_note tests ---

    #[test]
    fn write_methodology_note_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());

        let pattern = MethodologyPattern {
            title: "Pattern: Always use dark mode".to_string(),
            description: "Always prefer dark mode for development tools".to_string(),
            supporting_count: 3,
            source_ids: vec!["c1".to_string(), "c2".to_string(), "c3".to_string()],
        };

        let path = synth
            .write_methodology_note("dark-mode-preference", &pattern)
            .unwrap();

        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Pattern: Always use dark mode"));
        assert!(content.contains("type: methodology"));
        assert!(content.contains("supporting_corrections: 3"));
    }

    // --- parse_epoch tests ---

    #[test]
    fn parse_epoch_numeric() {
        assert_eq!(parse_epoch("1710000000"), Some(1710000000));
    }

    #[test]
    fn parse_epoch_iso_date() {
        let epoch = parse_epoch("2026-03-15T10:00:00Z");
        assert!(epoch.is_some());
        // Should be roughly correct (within a few days of actual value).
        let days = epoch.unwrap() / 86400;
        assert!(days > 20000); // Sanity check: > 2024
    }

    #[test]
    fn parse_epoch_garbage() {
        assert_eq!(parse_epoch("not-a-date"), None);
    }

    // --- truncate_fact tests ---

    #[test]
    fn truncate_fact_short() {
        assert_eq!(truncate_fact("short", 60), "short");
    }

    #[test]
    fn truncate_fact_long() {
        let long =
            "This is a very long fact that exceeds the maximum length allowed for truncation";
        let result = truncate_fact(long, 40);
        assert!(result.len() <= 44); // 40 + "..."
        assert!(result.ends_with("..."));
    }

    // --- refresh_entity_profiles tests ---

    #[test]
    fn refresh_entity_profiles_counts_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());
        let gen = EntityProfileGenerator::new(dir.path());

        let entities = vec![
            make_entity("vue-001", "Vue", EntityType::Tool),
            make_entity("react-001", "React", EntityType::Tool),
        ];

        let mut entity_facts = HashMap::new();
        entity_facts.insert(
            "vue-001".to_string(),
            vec![make_memory("m1", "vue-001", "Good SSR", "2026-03-14", 0.9)],
        );

        let entity_rels = HashMap::new();
        let entity_refs = HashMap::new();

        let count = synth.refresh_entity_profiles(
            &entities,
            &entity_facts,
            &entity_rels,
            &entity_refs,
            &gen,
        );

        assert_eq!(count, 2);

        // Verify files were created.
        assert!(dir.path().join("ledger/tools/vue/vue.brief.md").exists());
        assert!(dir
            .path()
            .join("ledger/tools/react/react.brief.md")
            .exists());
    }

    // --- friction detection tests ---

    fn make_thread_manager_with_threads(
        dir: &tempfile::TempDir,
        threads: &[(&str, &str)],
    ) -> ThreadManager {
        use crate::thread_registry::ThreadRegistry;
        let registry = ThreadRegistry::new(dir.path());
        let mut mgr = ThreadManager::new(registry);
        for (slug, title) in threads {
            mgr.create_thread(slug, title, &format!("!room-{slug}:ex.com"), 1000);
        }
        mgr
    }

    #[test]
    fn gather_thread_topics_extracts_keywords() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = make_thread_manager_with_threads(
            &dir,
            &[
                ("pricing", "SaaS Pricing Strategy Discussion"),
                ("tech", "Rust Backend Architecture"),
            ],
        );

        let topics = PeriodicSynthesizer::gather_thread_topics(&mgr);
        assert_eq!(topics.len(), 2);

        let pricing_topics = &topics["thread-pricing"];
        assert!(pricing_topics.contains(&"saas".to_string()));
        assert!(pricing_topics.contains(&"pricing".to_string()));
        assert!(pricing_topics.contains(&"strategy".to_string()));
        assert!(pricing_topics.contains(&"discussion".to_string()));
        // Stopwords should be filtered
        assert!(!pricing_topics.contains(&"the".to_string()));

        let tech_topics = &topics["thread-tech"];
        assert!(tech_topics.contains(&"rust".to_string()));
        assert!(tech_topics.contains(&"backend".to_string()));
        assert!(tech_topics.contains(&"architecture".to_string()));
    }

    #[test]
    fn gather_thread_topics_filters_stopwords_and_short_words() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = make_thread_manager_with_threads(&dir, &[("simple", "A is the to")]);

        // All words are stopwords or too short, so no topics.
        let topics = PeriodicSynthesizer::gather_thread_topics(&mgr);
        assert!(topics.is_empty());
    }

    #[test]
    fn friction_detection_finds_divergent_thread() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());
        // A thread with 5+ distinct topic keywords in its title triggers DivergentTopics.
        let mgr = make_thread_manager_with_threads(
            &dir,
            &[(
                "megathread",
                "Pricing Marketing TechStack Hiring Legal Compliance",
            )],
        );

        let proposals = synth.run_friction_detection(&mgr, None);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].thread_id.as_deref(), Some("thread-megathread"));
        assert!(proposals[0].proposal.suggestion.contains("splitting"));
    }

    #[test]
    fn friction_detection_no_divergent_when_few_topics() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());
        let mgr = make_thread_manager_with_threads(&dir, &[("focused", "Pricing Strategy")]);

        let proposals = synth.run_friction_detection(&mgr, None);
        assert!(proposals.is_empty());
    }

    #[test]
    fn friction_detection_finds_overlapping_threads() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());
        // "pricing" keyword appears in 3+ thread titles -> overlap detected.
        let mgr = make_thread_manager_with_threads(
            &dir,
            &[
                ("a", "Pricing Analysis Report"),
                ("b", "Pricing Model Review"),
                ("c", "Pricing Strategy Update"),
            ],
        );

        let proposals = synth.run_friction_detection(&mgr, None);
        assert!(!proposals.is_empty());
        // At least one proposal should mention merging.
        assert!(proposals
            .iter()
            .any(|proposal| proposal.proposal.suggestion.contains("merging")));
    }

    #[test]
    fn friction_detection_routes_orphan_proposals_to_stream() {
        let dir = tempfile::tempdir().unwrap();
        let synth = PeriodicSynthesizer::new(dir.path());
        let mgr = ThreadManager::new(crate::thread_registry::ThreadRegistry::new(dir.path()));
        let snapshot = GraphMaintenanceSnapshot {
            nodes: vec![
                symbiotic_memory::self_improvement::GraphMaintenanceNode {
                    entity_id: "vault".to_string(),
                    entity_name: "Vault Access Broker".to_string(),
                    entity_type: EntityType::Tool,
                    age_days: 21,
                    memory_count: 2,
                    keywords: vec![
                        "vault".to_string(),
                        "broker".to_string(),
                        "credential".to_string(),
                    ],
                },
                symbiotic_memory::self_improvement::GraphMaintenanceNode {
                    entity_id: "cred".to_string(),
                    entity_name: "Credential Gateway".to_string(),
                    entity_type: EntityType::Tool,
                    age_days: 21,
                    memory_count: 2,
                    keywords: vec![
                        "credential".to_string(),
                        "gateway".to_string(),
                        "broker".to_string(),
                    ],
                },
            ],
            edges: vec![],
        };

        let proposals = synth.run_friction_detection(&mgr, Some(&snapshot));
        assert_eq!(proposals.len(), 2);
        assert!(proposals
            .iter()
            .all(|proposal| proposal.thread_id.is_none()));
        assert!(proposals.iter().all(|proposal| {
            proposal
                .proposal
                .description
                .contains("Suggested reconnection targets")
        }));
    }

    #[test]
    fn proposal_text_is_human_readable() {
        let detector = symbiotic_memory::self_improvement::FrictionDetector::default();
        let signal = symbiotic_memory::self_improvement::FrictionSignal::DivergentTopics {
            thread_id: "thread-mega".to_string(),
            topics: vec![
                "pricing".into(),
                "marketing".into(),
                "tech".into(),
                "hiring".into(),
                "legal".into(),
            ],
        };
        let proposal = detector.propose(&signal);
        assert!(proposal.description.contains("thread-mega"));
        assert!(proposal.description.contains("5 distinct topics"));
        assert!(proposal.suggestion.contains("splitting"));
    }
}
