//! Self-improving graph: friction detection and structural proposals.
//!
//! The daemon monitors the Neural Graph for friction signals — structural
//! problems that degrade quality or usability — and generates human-readable
//! proposals for the user to approve.
//!
//! Signals detected:
//! - Overlapping threads (same topic across 3+ threads)
//! - Divergent topics (thread spans 5+ distinct topics)
//! - Repeated query misses (searched X, found nothing, 3+ times)
//! - Contradictions (fact A vs fact B)
//! - Idle threads with open questions (30+ days idle)

use crate::types::{EntityType, Memory};

use std::collections::{HashMap, HashSet, VecDeque};

// ── Friction signals ──────────────────────────────────────────────────

/// A detected friction signal in the memory graph.
#[derive(Debug, Clone, PartialEq)]
pub enum FrictionSignal {
    /// The same topic appears across multiple threads — consider merging.
    OverlappingThreads {
        topic: String,
        thread_ids: Vec<String>,
    },
    /// A single thread covers too many distinct topics — consider splitting.
    DivergentTopics {
        thread_id: String,
        topics: Vec<String>,
    },
    /// An agent searched for something multiple times and found nothing.
    RepeatedQueryMiss { query: String, miss_count: u32 },
    /// Two active memories contradict each other.
    Contradiction {
        memory_a: String,
        memory_b: String,
        description: String,
    },
    /// A thread has been idle for a long time with unresolved questions.
    IdleThreadWithOpenQuestions {
        thread_id: String,
        idle_days: u64,
        open_question_count: usize,
    },
    /// An entity is referenced in many threads but has no profile note.
    EntityWithoutProfile {
        entity_name: String,
        reference_count: usize,
    },
    /// A decision is based on data from a stale finding.
    DecisionOnStaleData {
        decision_id: String,
        stale_finding_id: String,
        finding_age_days: u64,
    },
    /// A memory-backed entity is structurally isolated and has likely
    /// reconnection candidates worth human review.
    OrphanedNode {
        entity_id: String,
        entity_name: String,
        degree: usize,
        component_size: usize,
        memory_count: usize,
        candidate_reconnections: Vec<ReconnectionCandidate>,
    },
}

/// A graph-maintenance snapshot used for derived structural analysis.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphMaintenanceSnapshot {
    pub nodes: Vec<GraphMaintenanceNode>,
    pub edges: Vec<GraphMaintenanceEdge>,
}

/// A node summary used by graph maintenance heuristics.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphMaintenanceNode {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: EntityType,
    pub age_days: u64,
    pub memory_count: usize,
    pub keywords: Vec<String>,
}

/// A lightweight graph edge used for structural analysis.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphMaintenanceEdge {
    pub source_entity_id: String,
    pub target_entity_id: String,
}

/// A likely reconnection target for an orphaned node.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconnectionCandidate {
    pub entity_id: String,
    pub entity_name: String,
    pub score: f64,
    pub shared_keywords: Vec<String>,
}

/// A human-readable proposal generated from a friction signal.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    /// The friction signal that triggered this proposal.
    pub signal: FrictionSignal,
    /// Human-readable description of what's wrong.
    pub description: String,
    /// Suggested action.
    pub suggestion: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContradictionEvidenceSummary {
    pub source_label: String,
    pub source_url: Option<String>,
    pub evidence_quote: Option<String>,
}

/// Derived contradiction summary for operator-facing integrity surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContradictionSummary {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: EntityType,
    pub memory_a_id: String,
    pub memory_a_fact: String,
    pub memory_b_id: String,
    pub memory_b_fact: String,
    pub description: String,
    pub suggestion: String,
    pub needs_review: bool,
    pub resolution_confidence_percent: u8,
    pub preferred_memory_id: Option<String>,
    pub preferred_fact: Option<String>,
    pub preferred_reason: Option<String>,
    pub investigation_summary: String,
    pub memory_a_evidence: Vec<ContradictionEvidenceSummary>,
    pub memory_b_evidence: Vec<ContradictionEvidenceSummary>,
}

/// Read-only integrity snapshot built from active memory state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIntegritySnapshot {
    pub tracked_entity_count: usize,
    pub entities_with_contradictions: usize,
    pub contradiction_count: usize,
    pub review_count: usize,
    pub contradictions: Vec<ContradictionSummary>,
}

// ── Query miss tracker ────────────────────────────────────────────────

/// Tracks queries that return empty results to detect knowledge gaps.
#[derive(Debug, Clone, Default)]
pub struct QueryMissTracker {
    /// query -> count of empty results.
    misses: HashMap<String, u32>,
    /// Minimum number of misses before surfacing a signal.
    pub threshold: u32,
}

impl QueryMissTracker {
    pub fn new(threshold: u32) -> Self {
        Self {
            misses: HashMap::new(),
            threshold,
        }
    }

    /// Record a query miss. Returns a friction signal if the threshold is met.
    pub fn record_miss(&mut self, query: &str) -> Option<FrictionSignal> {
        let normalized = query.trim().to_lowercase();
        if normalized.is_empty() {
            return None;
        }

        let count = self.misses.entry(normalized.clone()).or_insert(0);
        *count += 1;

        if *count >= self.threshold {
            Some(FrictionSignal::RepeatedQueryMiss {
                query: normalized,
                miss_count: *count,
            })
        } else {
            None
        }
    }

    /// Reset the miss count for a query (e.g., after the user provides the info).
    pub fn clear_query(&mut self, query: &str) {
        let normalized = query.trim().to_lowercase();
        self.misses.remove(&normalized);
    }

    /// Get all queries above the threshold.
    pub fn signals(&self) -> Vec<FrictionSignal> {
        self.misses
            .iter()
            .filter(|(_, count)| **count >= self.threshold)
            .map(|(query, count)| FrictionSignal::RepeatedQueryMiss {
                query: query.clone(),
                miss_count: *count,
            })
            .collect()
    }
}

// ── Friction detector ─────────────────────────────────────────────────

/// Detects friction signals across the memory graph.
pub struct FrictionDetector {
    /// Threshold for overlapping threads (same topic in N+ threads).
    pub overlap_threshold: usize,
    /// Threshold for divergent topics (N+ topics in one thread).
    pub divergence_threshold: usize,
    /// Days of inactivity before a thread is considered idle.
    pub idle_days_threshold: u64,
    /// Minimum references for "entity without profile" signal.
    pub entity_reference_threshold: usize,
    /// Minimum age before a weakly connected node is considered for orphan repair.
    pub orphan_min_age_days: u64,
    /// Minimum active memories before a node is considered meaningful enough
    /// for orphan repair proposals.
    pub orphan_min_memory_count: usize,
    /// Maximum degree still considered weakly connected for repair review.
    pub orphan_max_degree: usize,
    /// Maximum connected-component size still considered orphan-like when the
    /// node only has a single graph connection.
    pub orphan_max_component_size: usize,
    /// Maximum number of reconnection candidates surfaced in a proposal.
    pub orphan_candidate_limit: usize,
}

impl Default for FrictionDetector {
    fn default() -> Self {
        Self {
            overlap_threshold: 3,
            divergence_threshold: 5,
            idle_days_threshold: 30,
            entity_reference_threshold: 5,
            orphan_min_age_days: 7,
            orphan_min_memory_count: 1,
            orphan_max_degree: 1,
            orphan_max_component_size: 3,
            orphan_candidate_limit: 3,
        }
    }
}

impl FrictionDetector {
    /// Detect overlapping threads: same topic appears in multiple threads.
    ///
    /// `thread_topics` maps thread_id -> list of topic keywords/tags.
    pub fn detect_overlapping_threads(
        &self,
        thread_topics: &HashMap<String, Vec<String>>,
    ) -> Vec<FrictionSignal> {
        // Invert: topic -> threads that mention it
        let mut topic_threads: HashMap<&str, Vec<&str>> = HashMap::new();
        for (thread_id, topics) in thread_topics {
            for topic in topics {
                let lower = topic.as_str();
                topic_threads
                    .entry(lower)
                    .or_default()
                    .push(thread_id.as_str());
            }
        }

        topic_threads
            .into_iter()
            .filter(|(_, threads)| threads.len() >= self.overlap_threshold)
            .map(|(topic, threads)| FrictionSignal::OverlappingThreads {
                topic: topic.to_string(),
                thread_ids: threads.into_iter().map(|s| s.to_string()).collect(),
            })
            .collect()
    }

    /// Detect divergent threads: a single thread spans too many topics.
    ///
    /// `thread_topics` maps thread_id -> list of distinct topic tags.
    pub fn detect_divergent_threads(
        &self,
        thread_topics: &HashMap<String, Vec<String>>,
    ) -> Vec<FrictionSignal> {
        thread_topics
            .iter()
            .filter(|(_, topics)| topics.len() >= self.divergence_threshold)
            .map(|(thread_id, topics)| FrictionSignal::DivergentTopics {
                thread_id: thread_id.clone(),
                topics: topics.clone(),
            })
            .collect()
    }

    /// Detect idle threads with open questions.
    ///
    /// `threads` maps thread_id -> (idle_days, open_question_count).
    pub fn detect_idle_threads(
        &self,
        threads: &HashMap<String, (u64, usize)>,
    ) -> Vec<FrictionSignal> {
        threads
            .iter()
            .filter(|(_, (idle_days, questions))| {
                *idle_days >= self.idle_days_threshold && *questions > 0
            })
            .map(|(thread_id, (idle_days, questions))| {
                FrictionSignal::IdleThreadWithOpenQuestions {
                    thread_id: thread_id.clone(),
                    idle_days: *idle_days,
                    open_question_count: *questions,
                }
            })
            .collect()
    }

    /// Detect entities referenced in many threads but without a profile.
    ///
    /// `entity_refs` maps entity_name -> reference count.
    /// `profiled_entities` is the set of entities that already have profile notes.
    pub fn detect_missing_profiles(
        &self,
        entity_refs: &HashMap<String, usize>,
        profiled_entities: &std::collections::HashSet<String>,
    ) -> Vec<FrictionSignal> {
        entity_refs
            .iter()
            .filter(|(name, count)| {
                **count >= self.entity_reference_threshold && !profiled_entities.contains(*name)
            })
            .map(|(name, count)| FrictionSignal::EntityWithoutProfile {
                entity_name: name.clone(),
                reference_count: *count,
            })
            .collect()
    }

    /// Detect contradictions between active memories for the same entity.
    ///
    /// Uses a simple heuristic: same entity, both active, high token overlap
    /// but opposing content (negation patterns).
    pub fn detect_contradictions(&self, memories: &[Memory]) -> Vec<FrictionSignal> {
        let mut signals = Vec::new();

        // Group memories by entity
        let mut by_entity: HashMap<&str, Vec<&Memory>> = HashMap::new();
        for mem in memories {
            if mem.status == crate::types::MemoryStatus::Active {
                by_entity.entry(&mem.entity_id).or_default().push(mem);
            }
        }

        for entity_mems in by_entity.values() {
            for i in 0..entity_mems.len() {
                for j in (i + 1)..entity_mems.len() {
                    let a = entity_mems[i];
                    let b = entity_mems[j];
                    if looks_contradictory(&a.fact, &b.fact) {
                        signals.push(FrictionSignal::Contradiction {
                            memory_a: a.id.clone(),
                            memory_b: b.id.clone(),
                            description: format!(
                                "'{}' vs '{}'",
                                truncate(&a.fact, 60),
                                truncate(&b.fact, 60)
                            ),
                        });
                    }
                }
            }
        }

        signals
    }

    /// Detect structurally isolated, memory-backed entities and propose
    /// candidate reconnections for human review.
    pub fn detect_orphaned_nodes(
        &self,
        snapshot: &GraphMaintenanceSnapshot,
    ) -> Vec<FrictionSignal> {
        if snapshot.nodes.is_empty() {
            return Vec::new();
        }

        let mut adjacency: HashMap<&str, HashSet<&str>> = HashMap::new();
        for node in &snapshot.nodes {
            adjacency.entry(node.entity_id.as_str()).or_default();
        }
        for edge in &snapshot.edges {
            if edge.source_entity_id == edge.target_entity_id {
                continue;
            }
            adjacency
                .entry(edge.source_entity_id.as_str())
                .or_default()
                .insert(edge.target_entity_id.as_str());
            adjacency
                .entry(edge.target_entity_id.as_str())
                .or_default()
                .insert(edge.source_entity_id.as_str());
        }

        let component_sizes = connected_component_sizes(&adjacency);
        let by_id: HashMap<&str, &GraphMaintenanceNode> = snapshot
            .nodes
            .iter()
            .map(|node| (node.entity_id.as_str(), node))
            .collect();

        snapshot
            .nodes
            .iter()
            .filter_map(|node| {
                let degree = adjacency
                    .get(node.entity_id.as_str())
                    .map(|neighbors| neighbors.len())
                    .unwrap_or(0);
                let component_size = component_sizes
                    .get(node.entity_id.as_str())
                    .copied()
                    .unwrap_or(1);

                if node.age_days < self.orphan_min_age_days
                    || node.memory_count < self.orphan_min_memory_count
                    || degree > self.orphan_max_degree
                    || (degree == 1 && component_size > self.orphan_max_component_size)
                {
                    return None;
                }

                let existing_neighbors = adjacency
                    .get(node.entity_id.as_str())
                    .cloned()
                    .unwrap_or_default();
                let candidates = reconnection_candidates(
                    node,
                    &existing_neighbors,
                    &by_id,
                    self.orphan_candidate_limit,
                );

                if candidates.is_empty() {
                    return None;
                }

                Some(FrictionSignal::OrphanedNode {
                    entity_id: node.entity_id.clone(),
                    entity_name: node.entity_name.clone(),
                    degree,
                    component_size,
                    memory_count: node.memory_count,
                    candidate_reconnections: candidates,
                })
            })
            .collect()
    }

    /// Generate a human-readable proposal from a friction signal.
    pub fn propose(&self, signal: &FrictionSignal) -> Proposal {
        let (description, suggestion) = match signal {
            FrictionSignal::OverlappingThreads { topic, thread_ids } => (
                format!(
                    "The topic '{}' appears across {} threads: {}",
                    topic,
                    thread_ids.len(),
                    thread_ids.join(", ")
                ),
                "Consider merging these threads or creating a dedicated thread for this topic."
                    .to_string(),
            ),
            FrictionSignal::DivergentTopics { thread_id, topics } => (
                format!(
                    "Thread '{}' spans {} distinct topics: {}",
                    thread_id,
                    topics.len(),
                    topics.join(", ")
                ),
                "Consider splitting this thread into focused sub-threads.".to_string(),
            ),
            FrictionSignal::RepeatedQueryMiss { query, miss_count } => (
                format!(
                    "Agents have searched for '{}' {} times but found nothing.",
                    query, miss_count
                ),
                format!("No information about '{}'. Want to add it?", query),
            ),
            FrictionSignal::Contradiction {
                memory_a,
                memory_b,
                description: desc,
            } => (
                format!(
                    "Conflict between memories {} and {}: {}",
                    memory_a, memory_b, desc
                ),
                "Which fact is current? Resolve the contradiction.".to_string(),
            ),
            FrictionSignal::IdleThreadWithOpenQuestions {
                thread_id,
                idle_days,
                open_question_count,
            } => (
                format!(
                    "Thread '{}' has been idle for {} days with {} unresolved question(s).",
                    thread_id, idle_days, open_question_count
                ),
                "Revisit or archive this thread.".to_string(),
            ),
            FrictionSignal::EntityWithoutProfile {
                entity_name,
                reference_count,
            } => (
                format!(
                    "'{}' is referenced in {} places but has no entity profile.",
                    entity_name, reference_count
                ),
                format!("Create an entity note for '{}'?", entity_name),
            ),
            FrictionSignal::DecisionOnStaleData {
                decision_id,
                stale_finding_id,
                finding_age_days,
            } => (
                format!(
                    "Decision '{}' is based on finding '{}' which is {} days old.",
                    decision_id, stale_finding_id, finding_age_days
                ),
                "Re-validate this decision — the underlying data may be outdated.".to_string(),
            ),
            FrictionSignal::OrphanedNode {
                entity_name,
                degree,
                component_size,
                memory_count,
                candidate_reconnections,
                ..
            } => {
                let candidate_names = candidate_reconnections
                    .iter()
                    .map(|candidate| {
                        if candidate.shared_keywords.is_empty() {
                            candidate.entity_name.clone()
                        } else {
                            format!(
                                "{} [{}]",
                                candidate.entity_name,
                                candidate.shared_keywords.join(", ")
                            )
                        }
                    })
                    .collect::<Vec<_>>();
                (
                    format!(
                        "'{}' has {} active memorie(s) but only {} live graph connection(s) inside a component of size {}. Suggested reconnection targets: {}.",
                        entity_name,
                        memory_count,
                        degree,
                        component_size,
                        candidate_names.join(", ")
                    ),
                    "Review whether this entity should connect to the suggested neighbors or remain intentionally standalone.".to_string(),
                )
            }
        };

        Proposal {
            signal: signal.clone(),
            description,
            suggestion,
        }
    }
}

fn connected_component_sizes(adjacency: &HashMap<&str, HashSet<&str>>) -> HashMap<String, usize> {
    let mut visited = HashSet::new();
    let mut result = HashMap::new();

    for &start in adjacency.keys() {
        if visited.contains(start) {
            continue;
        }

        let mut queue = VecDeque::from([start]);
        let mut component = Vec::new();
        visited.insert(start);

        while let Some(node) = queue.pop_front() {
            component.push(node);
            if let Some(neighbors) = adjacency.get(node) {
                for &neighbor in neighbors {
                    if visited.insert(neighbor) {
                        queue.push_back(neighbor);
                    }
                }
            }
        }

        let size = component.len();
        for node in component {
            result.insert(node.to_string(), size);
        }
    }

    result
}

fn reconnection_candidates(
    node: &GraphMaintenanceNode,
    existing_neighbors: &HashSet<&str>,
    by_id: &HashMap<&str, &GraphMaintenanceNode>,
    limit: usize,
) -> Vec<ReconnectionCandidate> {
    let subject_keywords = node
        .keywords
        .iter()
        .map(|keyword| keyword.as_str())
        .collect::<HashSet<_>>();
    if subject_keywords.is_empty() {
        return Vec::new();
    }

    let mut candidates = by_id
        .values()
        .filter_map(|candidate| {
            if candidate.entity_id == node.entity_id
                || existing_neighbors.contains(candidate.entity_id.as_str())
                || candidate.memory_count == 0
            {
                return None;
            }

            let candidate_keywords = candidate
                .keywords
                .iter()
                .map(|keyword| keyword.as_str())
                .collect::<HashSet<_>>();
            let shared = subject_keywords
                .intersection(&candidate_keywords)
                .copied()
                .map(str::to_string)
                .collect::<Vec<_>>();
            if shared.is_empty() {
                return None;
            }

            let union = subject_keywords.union(&candidate_keywords).count().max(1);
            let mut score = shared.len() as f64 / union as f64;
            if candidate.entity_type == node.entity_type {
                score += 0.15;
            }
            if candidate.memory_count > 0 {
                score += 0.05;
            }

            if score < 0.2 {
                return None;
            }

            let mut shared_keywords = shared;
            shared_keywords.sort();
            shared_keywords.truncate(4);

            Some(ReconnectionCandidate {
                entity_id: candidate.entity_id.clone(),
                entity_name: candidate.entity_name.clone(),
                score,
                shared_keywords,
            })
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.entity_name.cmp(&b.entity_name))
    });
    candidates.truncate(limit);
    candidates
}

/// Simple heuristic for contradiction detection.
/// Same approach as quality_pipeline: shared tokens + negation patterns.
fn looks_contradictory(a: &str, b: &str) -> bool {
    let a_lower = a.to_lowercase();
    let b_lower = b.to_lowercase();

    let a_tokens: std::collections::HashSet<&str> = a_lower.split_whitespace().collect();
    let b_tokens: std::collections::HashSet<&str> = b_lower.split_whitespace().collect();

    let intersection = a_tokens.intersection(&b_tokens).count();
    let min_len = a_tokens.len().min(b_tokens.len());

    if min_len < 3 {
        return false;
    }

    let overlap = intersection as f64 / min_len as f64;
    if overlap < 0.5 {
        return false;
    }

    // Check for negation / opposition patterns
    let has_negation = |text: &str| -> bool {
        text.contains(" not ")
            || text.contains("n't ")
            || text.contains(" never ")
            || text.contains(" instead of ")
            || text.contains(" over ")
            || text.contains(" rather than ")
            || text.contains(" no ")
    };

    // Different negation state on the same topic
    if has_negation(&a_lower) != has_negation(&b_lower) {
        return true;
    }

    // Both have "over" patterns (e.g., "X over Y" vs "Y over X")
    if a_lower.contains(" over ") && b_lower.contains(" over ") {
        return true;
    }

    false
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FactDisposition, Memory, MemoryStatus, Sensitivity};
    use std::collections::HashSet;

    fn make_memory(id: &str, entity_id: &str, fact: &str) -> Memory {
        Memory {
            id: id.to_string(),
            entity_id: entity_id.to_string(),
            fact: fact.to_string(),
            confidence: 0.9,
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

    // --- QueryMissTracker tests ---

    #[test]
    fn query_miss_under_threshold_returns_none() {
        let mut tracker = QueryMissTracker::new(3);
        assert!(tracker.record_miss("pricing strategy").is_none());
        assert!(tracker.record_miss("pricing strategy").is_none());
    }

    #[test]
    fn query_miss_at_threshold_returns_signal() {
        let mut tracker = QueryMissTracker::new(3);
        tracker.record_miss("pricing strategy");
        tracker.record_miss("pricing strategy");
        let signal = tracker.record_miss("pricing strategy");
        assert!(signal.is_some());

        match signal.unwrap() {
            FrictionSignal::RepeatedQueryMiss { query, miss_count } => {
                assert_eq!(query, "pricing strategy");
                assert_eq!(miss_count, 3);
            }
            other => panic!("expected RepeatedQueryMiss, got {other:?}"),
        }
    }

    #[test]
    fn query_miss_normalizes_case() {
        let mut tracker = QueryMissTracker::new(2);
        tracker.record_miss("Pricing Strategy");
        let signal = tracker.record_miss("pricing strategy");
        assert!(signal.is_some());
    }

    #[test]
    fn clear_query_resets_count() {
        let mut tracker = QueryMissTracker::new(2);
        tracker.record_miss("pricing strategy");
        tracker.clear_query("pricing strategy");
        assert!(tracker.record_miss("pricing strategy").is_none());
    }

    #[test]
    fn empty_query_ignored() {
        let mut tracker = QueryMissTracker::new(1);
        assert!(tracker.record_miss("").is_none());
        assert!(tracker.record_miss("  ").is_none());
    }

    // --- FrictionDetector tests ---

    #[test]
    fn detect_overlapping_threads() {
        let detector = FrictionDetector::default();
        let mut topics: HashMap<String, Vec<String>> = HashMap::new();
        topics.insert("thread-a".into(), vec!["pricing".into(), "saas".into()]);
        topics.insert(
            "thread-b".into(),
            vec!["pricing".into(), "marketing".into()],
        );
        topics.insert("thread-c".into(), vec!["pricing".into(), "revenue".into()]);

        let signals = detector.detect_overlapping_threads(&topics);
        assert_eq!(signals.len(), 1);
        match &signals[0] {
            FrictionSignal::OverlappingThreads { topic, thread_ids } => {
                assert_eq!(topic, "pricing");
                assert_eq!(thread_ids.len(), 3);
            }
            other => panic!("expected OverlappingThreads, got {other:?}"),
        }
    }

    #[test]
    fn no_overlap_below_threshold() {
        let detector = FrictionDetector::default();
        let mut topics: HashMap<String, Vec<String>> = HashMap::new();
        topics.insert("thread-a".into(), vec!["pricing".into()]);
        topics.insert("thread-b".into(), vec!["pricing".into()]);

        let signals = detector.detect_overlapping_threads(&topics);
        assert!(signals.is_empty()); // threshold is 3
    }

    #[test]
    fn detect_divergent_thread() {
        let detector = FrictionDetector::default();
        let mut topics: HashMap<String, Vec<String>> = HashMap::new();
        topics.insert(
            "thread-x".into(),
            vec![
                "pricing".into(),
                "marketing".into(),
                "tech-stack".into(),
                "hiring".into(),
                "legal".into(),
            ],
        );

        let signals = detector.detect_divergent_threads(&topics);
        assert_eq!(signals.len(), 1);
        match &signals[0] {
            FrictionSignal::DivergentTopics { thread_id, topics } => {
                assert_eq!(thread_id, "thread-x");
                assert_eq!(topics.len(), 5);
            }
            other => panic!("expected DivergentTopics, got {other:?}"),
        }
    }

    #[test]
    fn detect_idle_thread_with_questions() {
        let detector = FrictionDetector::default();
        let mut threads: HashMap<String, (u64, usize)> = HashMap::new();
        threads.insert("thread-idle".into(), (45, 2));
        threads.insert("thread-active".into(), (5, 1));
        threads.insert("thread-idle-no-q".into(), (45, 0));

        let signals = detector.detect_idle_threads(&threads);
        assert_eq!(signals.len(), 1);
        assert!(matches!(
            &signals[0],
            FrictionSignal::IdleThreadWithOpenQuestions { thread_id, .. }
            if thread_id == "thread-idle"
        ));
    }

    #[test]
    fn detect_missing_profiles() {
        let detector = FrictionDetector::default();
        let mut refs: HashMap<String, usize> = HashMap::new();
        refs.insert("Stripe".into(), 7);
        refs.insert("React".into(), 2);
        refs.insert("Vue".into(), 6);

        let profiled: HashSet<String> = ["Stripe".to_string()].into_iter().collect();

        let signals = detector.detect_missing_profiles(&refs, &profiled);
        assert_eq!(signals.len(), 1);
        match &signals[0] {
            FrictionSignal::EntityWithoutProfile {
                entity_name,
                reference_count,
            } => {
                assert_eq!(entity_name, "Vue");
                assert_eq!(*reference_count, 6);
            }
            other => panic!("expected EntityWithoutProfile, got {other:?}"),
        }
    }

    #[test]
    fn detect_contradiction_between_memories() {
        let detector = FrictionDetector::default();
        let a = make_memory("m-1", "ent-frontend", "Frontend framework: React over Vue");
        let b = make_memory("m-2", "ent-frontend", "Frontend framework: Vue over React");

        let signals = detector.detect_contradictions(&[a, b]);
        assert_eq!(signals.len(), 1);
        assert!(matches!(
            &signals[0],
            FrictionSignal::Contradiction { memory_a, memory_b, .. }
            if memory_a == "m-1" && memory_b == "m-2"
        ));
    }

    #[test]
    fn no_contradiction_different_entities() {
        let detector = FrictionDetector::default();
        let a = make_memory("m-1", "ent-a", "Frontend framework: React over Vue");
        let b = make_memory("m-2", "ent-b", "Frontend framework: Vue over React");

        let signals = detector.detect_contradictions(&[a, b]);
        assert!(signals.is_empty());
    }

    #[test]
    fn no_contradiction_different_topics() {
        let detector = FrictionDetector::default();
        let a = make_memory("m-1", "ent-1", "Uses Rust for backend development");
        let b = make_memory("m-2", "ent-1", "Prefers dark mode in all editors");

        let signals = detector.detect_contradictions(&[a, b]);
        assert!(signals.is_empty());
    }

    #[test]
    fn superseded_memories_excluded_from_contradiction() {
        let detector = FrictionDetector::default();
        let a = make_memory("m-1", "ent-1", "Frontend framework: React over Vue");
        let mut b = make_memory("m-2", "ent-1", "Frontend framework: Vue over React");
        b.status = MemoryStatus::Superseded;

        let signals = detector.detect_contradictions(&[a, b]);
        assert!(signals.is_empty());
    }

    // --- Proposal generation tests ---

    #[test]
    fn proposal_for_query_miss() {
        let detector = FrictionDetector::default();
        let signal = FrictionSignal::RepeatedQueryMiss {
            query: "pricing strategy".into(),
            miss_count: 3,
        };

        let proposal = detector.propose(&signal);
        assert!(proposal.description.contains("pricing strategy"));
        assert!(proposal.description.contains("3 times"));
        assert!(proposal.suggestion.contains("pricing strategy"));
    }

    #[test]
    fn proposal_for_contradiction() {
        let detector = FrictionDetector::default();
        let signal = FrictionSignal::Contradiction {
            memory_a: "m-1".into(),
            memory_b: "m-2".into(),
            description: "'React over Vue' vs 'Vue over React'".into(),
        };

        let proposal = detector.propose(&signal);
        assert!(proposal.description.contains("m-1"));
        assert!(proposal.description.contains("m-2"));
        assert!(proposal.suggestion.contains("Resolve"));
    }

    #[test]
    fn proposal_for_decision_on_stale_data() {
        let detector = FrictionDetector::default();
        let signal = FrictionSignal::DecisionOnStaleData {
            decision_id: "dec-1".into(),
            stale_finding_id: "find-1".into(),
            finding_age_days: 120,
        };

        let proposal = detector.propose(&signal);
        assert!(proposal.description.contains("120 days"));
        assert!(proposal.suggestion.contains("Re-validate"));
    }

    #[test]
    fn detect_orphaned_nodes_surfaces_reconnection_candidates() {
        let detector = FrictionDetector::default();
        let snapshot = GraphMaintenanceSnapshot {
            nodes: vec![
                GraphMaintenanceNode {
                    entity_id: "vault".to_string(),
                    entity_name: "Vault Access Broker".to_string(),
                    entity_type: EntityType::Tool,
                    age_days: 21,
                    memory_count: 2,
                    keywords: vec![
                        "vault".to_string(),
                        "access".to_string(),
                        "broker".to_string(),
                        "credential".to_string(),
                    ],
                },
                GraphMaintenanceNode {
                    entity_id: "cred".to_string(),
                    entity_name: "Credential Gateway".to_string(),
                    entity_type: EntityType::Tool,
                    age_days: 18,
                    memory_count: 3,
                    keywords: vec![
                        "credential".to_string(),
                        "gateway".to_string(),
                        "broker".to_string(),
                        "session".to_string(),
                    ],
                },
                GraphMaintenanceNode {
                    entity_id: "auth".to_string(),
                    entity_name: "Auth Sandbox".to_string(),
                    entity_type: EntityType::Tool,
                    age_days: 14,
                    memory_count: 2,
                    keywords: vec![
                        "auth".to_string(),
                        "credential".to_string(),
                        "session".to_string(),
                        "sandbox".to_string(),
                    ],
                },
            ],
            edges: vec![],
        };

        let signals = detector.detect_orphaned_nodes(&snapshot);
        assert_eq!(signals.len(), 3);
        let orphan = signals
            .iter()
            .find(|signal| {
                matches!(
                    signal,
                    FrictionSignal::OrphanedNode { entity_id, .. } if entity_id == "vault"
                )
            })
            .expect("vault orphan signal");
        match orphan {
            FrictionSignal::OrphanedNode {
                candidate_reconnections,
                degree,
                ..
            } => {
                assert_eq!(*degree, 0);
                assert!(
                    candidate_reconnections
                        .iter()
                        .any(|candidate| candidate.entity_id == "cred"),
                    "expected credential gateway as reconnection candidate"
                );
            }
            other => panic!("expected orphan signal, got {other:?}"),
        }
    }

    #[test]
    fn detect_orphaned_nodes_skips_fresh_or_stub_entities() {
        let detector = FrictionDetector::default();
        let snapshot = GraphMaintenanceSnapshot {
            nodes: vec![
                GraphMaintenanceNode {
                    entity_id: "fresh".to_string(),
                    entity_name: "Fresh Idea".to_string(),
                    entity_type: EntityType::Concept,
                    age_days: 1,
                    memory_count: 2,
                    keywords: vec!["fresh".to_string(), "idea".to_string()],
                },
                GraphMaintenanceNode {
                    entity_id: "stub".to_string(),
                    entity_name: "Stub Concept".to_string(),
                    entity_type: EntityType::Concept,
                    age_days: 30,
                    memory_count: 0,
                    keywords: vec!["stub".to_string(), "concept".to_string()],
                },
                GraphMaintenanceNode {
                    entity_id: "anchor".to_string(),
                    entity_name: "Anchor Concept".to_string(),
                    entity_type: EntityType::Concept,
                    age_days: 30,
                    memory_count: 2,
                    keywords: vec!["anchor".to_string(), "concept".to_string()],
                },
            ],
            edges: vec![],
        };

        let signals = detector.detect_orphaned_nodes(&snapshot);
        assert!(
            signals.is_empty(),
            "fresh and stub entities should not generate orphan repair noise"
        );
    }

    #[test]
    fn proposal_for_orphaned_node_includes_candidates() {
        let detector = FrictionDetector::default();
        let proposal = detector.propose(&FrictionSignal::OrphanedNode {
            entity_id: "vault".to_string(),
            entity_name: "Vault Access Broker".to_string(),
            degree: 0,
            component_size: 1,
            memory_count: 2,
            candidate_reconnections: vec![ReconnectionCandidate {
                entity_id: "cred".to_string(),
                entity_name: "Credential Gateway".to_string(),
                score: 0.72,
                shared_keywords: vec!["broker".to_string(), "credential".to_string()],
            }],
        });

        assert!(proposal.description.contains("Vault Access Broker"));
        assert!(proposal.description.contains("Credential Gateway"));
        assert!(proposal.suggestion.contains("standalone"));
    }
}
