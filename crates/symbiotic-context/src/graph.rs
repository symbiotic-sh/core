//! BFS context graph retrieval with decay scoring.
//!
//! Provides graph-based context retrieval that complements keyword and vector
//! search. Entities and relationships form a graph; BFS traversal from seed
//! entities discovers related context with score decay per hop.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Sensitivity;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Entity classification in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Person,
    Project,
    Concept,
    Tool,
    Task,
    Organization,
}

/// FSRS (Free Spaced Repetition Scheduler) parameters for adaptive memory decay.
///
/// When present on a [`Memory`], the FSRS retention formula is used instead of
/// the legacy half-life exponential decay. This gives each memory its own
/// adaptive forgetting curve that evolves with access patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsrsParams {
    /// How long (in days) until retention drops to ~90%.
    /// Higher values = more durable memories. Default: 30.0
    pub stability: f64,
    /// How hard this memory is to recall. Range \[0.0, 1.0\].
    /// Higher = harder. Default: 0.3
    pub difficulty: f64,
    /// Last time this memory was accessed (epoch seconds).
    /// Used to compute age for FSRS decay.
    pub last_access: u64,
}

impl Default for FsrsParams {
    fn default() -> Self {
        Self {
            stability: 30.0,
            difficulty: 0.3,
            last_access: 0,
        }
    }
}

/// A memory fact attached to an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub sensitivity: Sensitivity,
    /// Archive article IDs that provide evidence for this memory.
    pub evidence: Vec<String>,
    /// Unix epoch seconds when this memory was last updated.
    /// Used for temporal decay scoring. When `None`, no temporal decay is applied.
    #[serde(default)]
    pub updated_at: Option<u64>,
    /// Whether this memory has been archived (soft-deleted).
    /// Archived memories are excluded from standard context retrieval
    /// but preserved for historical auditing.
    #[serde(default)]
    pub archived: bool,
    /// FSRS scheduling parameters for this memory.
    /// When `None`, falls back to config-level half-life decay.
    #[serde(default)]
    pub fsrs: Option<FsrsParams>,
}

/// A typed, directed edge between two entities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source_id: String,
    pub target_id: String,
    pub relationship: String,
    /// Strength of the relationship (0.0..=1.0).
    pub strength: f64,
    /// Dynamic traversal reinforcement multiplier.
    pub weight: f64,
}

/// An entity node in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEntity {
    pub id: String,
    pub name: String,
    pub entity_type: EntityType,
    pub sensitivity: Sensitivity,
    pub memories: Vec<Memory>,
}

/// A node in a retrieval result, annotated with traversal metadata.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: EntityType,
    /// Score after accumulated path scoring and temporal decay.
    pub score: f64,
    /// Hops from the nearest seed entity.
    pub depth: usize,
    /// Relationship types traversed from seed to this node.
    pub path: Vec<String>,
    /// Active memories for this entity.
    pub memories: Vec<Memory>,
}

/// Result of a graph retrieval operation.
#[derive(Debug, Clone)]
pub struct GraphRetrievalResult {
    /// Seed entities that matched the query directly.
    pub seeds: Vec<GraphNode>,
    /// Related entities discovered via BFS traversal.
    pub related: Vec<GraphNode>,
    /// Total entities considered before filtering.
    pub total_considered: usize,
}

#[derive(Debug, Clone)]
struct VisitedState {
    score: f64,
    depth: usize,
    path: Vec<String>,
    edge_path: Vec<GraphEdge>,
}

/// Configuration for graph retrieval.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GraphRetrievalConfig {
    /// Maximum BFS traversal depth from seed entities.
    pub max_depth: usize,
    /// Score decay factor per hop. Each traversed edge multiplies the
    /// accumulated score by this factor.
    pub decay_factor: f64,
    /// Maximum number of entities to return.
    pub max_entities: usize,
    /// Maximum number of memories per entity to include.
    pub max_memories_per_entity: usize,
    /// Minimum score threshold; entities below this are excluded.
    pub min_score: f64,
    /// Half-life in days for temporal decay of memories.
    ///
    /// A memory that is `half_life_days` old scores 0.5× its base score.
    /// Set to `0.0` to disable temporal decay.
    #[serde(default)]
    pub half_life_days: f64,
    /// Current timestamp (epoch seconds) for temporal decay calculation.
    /// When `0`, uses the system clock.
    #[serde(default)]
    pub now_epoch_secs: u64,
}

impl Default for GraphRetrievalConfig {
    fn default() -> Self {
        Self {
            max_depth: 3,
            decay_factor: 0.7,
            max_entities: 20,
            max_memories_per_entity: 5,
            min_score: 0.1,
            half_life_days: 90.0, // 3 months — findings/general facts
            now_epoch_secs: 0,    // 0 = use system clock
        }
    }
}

/// Errors from graph retrieval.
#[derive(Debug, Error)]
pub enum GraphRetrievalError {
    #[error("no seed entities found for query")]
    NoSeeds,
    #[error("store error: {0}")]
    StoreError(String),
}

/// Trait for providing graph data to the retriever.
///
/// Implementations may back this with SQLite, in-memory stores, etc.
pub trait GraphStore: Send + Sync {
    /// Find entities whose names match the query terms.
    fn find_seed_entities(&self, query: &str) -> Result<Vec<GraphEntity>, GraphRetrievalError>;

    /// Get outgoing edges from an entity.
    fn get_edges(&self, entity_id: &str) -> Result<Vec<GraphEdge>, GraphRetrievalError>;

    /// Get an entity by ID.
    fn get_entity(&self, entity_id: &str) -> Result<Option<GraphEntity>, GraphRetrievalError>;

    /// Persist updated FSRS parameters for a recalled memory.
    ///
    /// Lightweight stores may ignore this by using the default no-op.
    fn update_memory_fsrs(
        &self,
        _memory_id: &str,
        _fsrs: &FsrsParams,
    ) -> Result<(), GraphRetrievalError> {
        Ok(())
    }

    /// Reinforce successful traversal paths.
    fn reinforce_edges(&self, _edges: &[GraphEdge]) -> Result<(), GraphRetrievalError> {
        Ok(())
    }

    /// Return a bounded derived multiplier for structurally important nodes.
    ///
    /// Implementations may compute this from betweenness centrality or any
    /// other rebuildable graph metric. The default is neutral.
    fn structural_boost(&self, _entity_id: &str) -> Result<f64, GraphRetrievalError> {
        Ok(1.0)
    }
}

/// Trait for graph-based context retrieval.
pub trait GraphRetriever: Send + Sync {
    /// Retrieve context graph for a query.
    fn retrieve(
        &self,
        query: &str,
        config: &GraphRetrievalConfig,
        sensitivity_max: Sensitivity,
    ) -> Result<GraphRetrievalResult, GraphRetrievalError>;
}

// ---------------------------------------------------------------------------
// FSRS retention functions
// ---------------------------------------------------------------------------

/// FSRS retention formula: R(t) = e^(-t / (9 * S))
/// where t = days since last access, S = stability.
/// Returns a value in (0.0, 1.0].
pub fn fsrs_retention(age_days: f64, stability: f64) -> f64 {
    if stability <= 0.0 {
        return 0.0;
    }
    (-age_days / (9.0 * stability)).exp()
}

/// Update stability after a successful recall.
/// Stability increases — the memory becomes more durable.
pub fn fsrs_update_stability(current: f64, difficulty: f64) -> f64 {
    // Simplified FSRS-4: growth inversely proportional to difficulty.
    let growth = 1.0 + (1.0 - difficulty) * 0.5;
    (current * growth).min(365.0 * 10.0) // cap at 10 years
}

/// Update difficulty after a recall attempt.
/// quality: 0.0 = complete failure, 1.0 = perfect recall
pub fn fsrs_update_difficulty(current: f64, quality: f64) -> f64 {
    // Move difficulty toward quality-based target.
    let target = 1.0 - quality;
    let new_diff = current + 0.1 * (target - current);
    new_diff.clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Temporal decay
// ---------------------------------------------------------------------------

/// Compute temporal decay factor for a memory based on its age.
///
/// When `fsrs` is `Some`, uses the FSRS retention formula:
/// `R(t) = e^(-t / (9 * stability))`
///
/// When `fsrs` is `None`, falls back to the legacy half-life formula:
/// `factor = 0.5^(age_days / half_life_days)`
///
/// Returns 1.0 (no decay) when temporal decay is disabled or no timestamp is available.
pub fn temporal_decay(
    updated_at: Option<u64>,
    now_secs: u64,
    half_life_days: f64,
    fsrs: Option<&FsrsParams>,
) -> f64 {
    // If FSRS params are available, use FSRS retention formula.
    if let Some(params) = fsrs {
        let effective_ts = if params.last_access > 0 {
            params.last_access
        } else {
            updated_at.unwrap_or(0)
        };
        if effective_ts == 0 {
            return 1.0;
        }
        let age_secs = now_secs.saturating_sub(effective_ts) as f64;
        let age_days = age_secs / 86400.0;
        return fsrs_retention(age_days, params.stability);
    }

    // Fallback: legacy half-life decay.
    if half_life_days <= 0.0 || half_life_days.is_infinite() {
        return 1.0;
    }
    let Some(ts) = updated_at else {
        return 1.0;
    };
    let age_secs = now_secs.saturating_sub(ts) as f64;
    let age_days = age_secs / 86400.0;
    0.5_f64.powf(age_days / half_life_days)
}

/// Compute the temporal decay factor for an entity based on its memories.
///
/// If any memory has FSRS params, uses the most-recently-accessed one.
/// Otherwise falls back to legacy half-life decay on the most recent `updated_at`.
fn entity_temporal_decay(entity: &GraphEntity, now_secs: u64, half_life_days: f64) -> f64 {
    // Try to find the most recently accessed FSRS-enabled memory.
    let best_fsrs = entity
        .memories
        .iter()
        .filter(|m| !m.archived)
        .filter_map(|m| m.fsrs.as_ref())
        .max_by_key(|p| p.last_access);

    if let Some(params) = best_fsrs {
        return temporal_decay(None, now_secs, half_life_days, Some(params));
    }

    // Legacy path: use most recent updated_at timestamp.
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let most_recent = entity.memories.iter().filter_map(|m| m.updated_at).max();
    temporal_decay(most_recent, now_secs, half_life_days, None)
}

/// Resolve the "now" timestamp from config (0 = system clock).
fn resolve_now(config: &GraphRetrievalConfig) -> u64 {
    if config.now_epoch_secs > 0 {
        config.now_epoch_secs
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// BFS Implementation
// ---------------------------------------------------------------------------

/// BFS graph retriever backed by a `GraphStore`.
pub struct BfsGraphRetriever<S: GraphStore> {
    store: S,
}

impl<S: GraphStore> BfsGraphRetriever<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

impl<S: GraphStore> GraphRetriever for BfsGraphRetriever<S> {
    fn retrieve(
        &self,
        query: &str,
        config: &GraphRetrievalConfig,
        sensitivity_max: Sensitivity,
    ) -> Result<GraphRetrievalResult, GraphRetrievalError> {
        let seed_entities = self.store.find_seed_entities(query)?;

        if seed_entities.is_empty() {
            return Err(GraphRetrievalError::NoSeeds);
        }

        let mut visited: HashMap<String, VisitedState> = HashMap::new();
        // BFS queue: (entity_id, depth, accumulated_path)
        let mut queue: VecDeque<(String, usize, Vec<String>)> = VecDeque::new();
        let mut total_considered: usize = 0;

        // Enqueue seed entities at depth 0 with base score 1.0
        for entity in &seed_entities {
            let score = 1.0; // Seeds get base score 1.0
            visited.insert(
                entity.id.clone(),
                VisitedState {
                    score,
                    depth: 0,
                    path: vec![],
                    edge_path: vec![],
                },
            );
            queue.push_back((entity.id.clone(), 0, vec![]));
            total_considered += 1;
        }

        // BFS traversal
        while let Some((entity_id, depth, path)) = queue.pop_front() {
            if depth >= config.max_depth {
                continue;
            }

            let current_score = visited
                .get(&entity_id)
                .map(|state| state.score)
                .unwrap_or(1.0);
            let edges = self.store.get_edges(&entity_id)?;

            for edge in edges {
                let target_id = if edge.source_id == entity_id {
                    &edge.target_id
                } else {
                    &edge.source_id
                };

                let next_depth = depth + 1;
                let decayed_score =
                    current_score * edge.strength * edge.weight * config.decay_factor;

                if decayed_score < config.min_score {
                    continue;
                }

                total_considered += 1;

                let mut next_path = path.clone();
                next_path.push(edge.relationship.clone());
                let mut next_edge_path = visited
                    .get(&entity_id)
                    .map(|prev| prev.edge_path.clone())
                    .unwrap_or_default();
                next_edge_path.push(edge.clone());

                let should_enqueue = !matches!(
                    visited.get(target_id.as_str()),
                    Some(prev) if prev.score >= decayed_score
                );

                if should_enqueue {
                    visited.insert(
                        target_id.clone(),
                        VisitedState {
                            score: decayed_score,
                            depth: next_depth,
                            path: next_path.clone(),
                            edge_path: next_edge_path.clone(),
                        },
                    );
                    queue.push_back((target_id.clone(), next_depth, next_path));
                }
            }
        }

        // Collect seed IDs for partitioning
        let seed_ids: HashSet<&str> = seed_entities.iter().map(|e| e.id.as_str()).collect();
        let now_secs = resolve_now(config);

        // Build result nodes
        let mut seeds: Vec<GraphNode> = Vec::new();
        let mut related: Vec<GraphNode> = Vec::new();

        for (entity_id, visit) in &visited {
            // Fetch entity to check sensitivity
            let entity = match self.store.get_entity(entity_id)? {
                Some(e) => e,
                None => continue,
            };

            if entity.sensitivity > sensitivity_max {
                continue;
            }

            // Apply temporal decay based on entity's most recent memory.
            let time_factor = entity_temporal_decay(&entity, now_secs, config.half_life_days);
            let structural_boost = if seed_ids.contains(entity_id.as_str()) {
                1.0
            } else {
                self.store.structural_boost(entity_id)?
            };
            let decayed_score = visit.score * time_factor * structural_boost;

            // Skip entities whose score falls below threshold after temporal decay.
            if decayed_score < config.min_score && !seed_ids.contains(entity_id.as_str()) {
                continue;
            }

            // Filter memories: exclude archived and respect sensitivity
            let memories: Vec<Memory> = entity
                .memories
                .into_iter()
                .filter(|m| !m.archived && m.sensitivity <= sensitivity_max)
                .take(config.max_memories_per_entity)
                .collect();

            let node = GraphNode {
                entity_id: entity.id,
                entity_name: entity.name,
                entity_type: entity.entity_type,
                score: decayed_score,
                depth: visit.depth,
                path: visit.path.clone(),
                memories,
            };

            if seed_ids.contains(entity_id.as_str()) {
                seeds.push(node);
            } else {
                related.push(node);
            }
        }

        // Sort by score descending, then depth ascending
        seeds.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.depth.cmp(&b.depth))
        });
        related.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.depth.cmp(&b.depth))
        });

        // Enforce max_entities (seeds + related combined)
        let total_allowed = config.max_entities;
        let seed_count = seeds.len().min(total_allowed);
        seeds.truncate(seed_count);
        let remaining = total_allowed.saturating_sub(seed_count);
        related.truncate(remaining);

        reinforce_returned_paths(&self.store, &visited, seeds.iter().chain(related.iter()))?;

        persist_recall_adaptation(
            &self.store,
            seeds.iter_mut().chain(related.iter_mut()),
            now_secs,
        )?;

        Ok(GraphRetrievalResult {
            seeds,
            related,
            total_considered,
        })
    }
}

fn reinforce_returned_paths<'a>(
    store: &impl GraphStore,
    visited: &HashMap<String, VisitedState>,
    nodes: impl Iterator<Item = &'a GraphNode>,
) -> Result<(), GraphRetrievalError> {
    for node in nodes {
        if let Some(visit) = visited.get(&node.entity_id) {
            store.reinforce_edges(&visit.edge_path)?;
        }
    }
    Ok(())
}

fn persist_recall_adaptation<'a>(
    store: &impl GraphStore,
    nodes: impl Iterator<Item = &'a mut GraphNode>,
    now_secs: u64,
) -> Result<(), GraphRetrievalError> {
    for node in nodes {
        let quality = recall_quality(node.depth);
        for memory in &mut node.memories {
            let mut fsrs = memory.fsrs.clone().unwrap_or_else(|| FsrsParams {
                stability: 30.0,
                difficulty: 0.3,
                last_access: memory.updated_at.unwrap_or(now_secs),
            });
            fsrs.stability = fsrs_update_stability(fsrs.stability, fsrs.difficulty);
            fsrs.difficulty = fsrs_update_difficulty(fsrs.difficulty, quality);
            fsrs.last_access = now_secs;
            store.update_memory_fsrs(&memory.id, &fsrs)?;
            memory.fsrs = Some(fsrs);
        }
    }

    Ok(())
}

fn recall_quality(depth: usize) -> f64 {
    match depth {
        0 => 0.95,
        1 => 0.85,
        2 => 0.75,
        _ => 0.65,
    }
}

/// Compute unweighted betweenness centrality over the current graph topology.
///
/// Relationship direction is intentionally ignored here: the metric is used as
/// a structural "bridge across clusters" signal rather than an execution
/// semantics signal. Returned scores are raw betweenness values; callers may
/// normalize or bound them for retrieval use.
pub fn betweenness_centrality(entity_ids: &[String], edges: &[GraphEdge]) -> HashMap<String, f64> {
    let mut adjacency: HashMap<String, HashSet<String>> = entity_ids
        .iter()
        .cloned()
        .map(|id| (id, HashSet::new()))
        .collect();

    for edge in edges {
        adjacency
            .entry(edge.source_id.clone())
            .or_default()
            .insert(edge.target_id.clone());
        adjacency
            .entry(edge.target_id.clone())
            .or_default()
            .insert(edge.source_id.clone());
    }

    let nodes: Vec<String> = adjacency.keys().cloned().collect();
    let mut centrality: HashMap<String, f64> = nodes.iter().cloned().map(|id| (id, 0.0)).collect();

    for source in &nodes {
        let mut stack = Vec::new();
        let mut predecessors: HashMap<String, Vec<String>> =
            nodes.iter().cloned().map(|id| (id, Vec::new())).collect();
        let mut sigma: HashMap<String, f64> = nodes.iter().cloned().map(|id| (id, 0.0)).collect();
        let mut distance: HashMap<String, i32> = nodes.iter().cloned().map(|id| (id, -1)).collect();

        sigma.insert(source.clone(), 1.0);
        distance.insert(source.clone(), 0);

        let mut queue = VecDeque::new();
        queue.push_back(source.clone());

        while let Some(node) = queue.pop_front() {
            stack.push(node.clone());
            let node_distance = distance.get(&node).copied().unwrap_or(-1);
            let node_sigma = sigma.get(&node).copied().unwrap_or(0.0);

            if let Some(neighbors) = adjacency.get(&node) {
                for neighbor in neighbors {
                    if distance.get(neighbor).copied().unwrap_or(-1) < 0 {
                        queue.push_back(neighbor.clone());
                        distance.insert(neighbor.clone(), node_distance + 1);
                    }
                    if distance.get(neighbor).copied().unwrap_or(-1) == node_distance + 1 {
                        let next_sigma = sigma.get(neighbor).copied().unwrap_or(0.0) + node_sigma;
                        sigma.insert(neighbor.clone(), next_sigma);
                        predecessors
                            .entry(neighbor.clone())
                            .or_default()
                            .push(node.clone());
                    }
                }
            }
        }

        let mut dependency: HashMap<String, f64> =
            nodes.iter().cloned().map(|id| (id, 0.0)).collect();

        while let Some(node) = stack.pop() {
            let dep_value = dependency.get(&node).copied().unwrap_or(0.0);
            if let Some(preds) = predecessors.get(&node) {
                let sigma_node = sigma.get(&node).copied().unwrap_or(1.0);
                for predecessor in preds {
                    let sigma_pred = sigma.get(predecessor).copied().unwrap_or(0.0);
                    if sigma_node > 0.0 {
                        let contribution = (sigma_pred / sigma_node) * (1.0 + dep_value);
                        *dependency.entry(predecessor.clone()).or_insert(0.0) += contribution;
                    }
                }
            }
            if node != *source {
                *centrality.entry(node).or_insert(0.0) += dep_value;
            }
        }
    }

    for value in centrality.values_mut() {
        *value /= 2.0;
    }

    centrality
}

// ---------------------------------------------------------------------------
// In-memory graph store for testing
// ---------------------------------------------------------------------------

/// Simple in-memory graph store.
#[derive(Debug, Default)]
pub struct InMemoryGraphStore {
    pub entities: Vec<GraphEntity>,
    pub edges: Vec<GraphEdge>,
}

impl InMemoryGraphStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_entity(&mut self, entity: GraphEntity) {
        self.entities.push(entity);
    }

    pub fn add_edge(&mut self, edge: GraphEdge) {
        self.edges.push(edge);
    }
}

impl GraphStore for InMemoryGraphStore {
    fn find_seed_entities(&self, query: &str) -> Result<Vec<GraphEntity>, GraphRetrievalError> {
        let query_lower = query.to_ascii_lowercase();
        let terms: Vec<&str> = query_lower.split_whitespace().collect();

        let matches: Vec<GraphEntity> = self
            .entities
            .iter()
            .filter(|e| {
                let name_lower = e.name.to_ascii_lowercase();
                terms.iter().any(|term| name_lower.contains(term))
            })
            .cloned()
            .collect();

        Ok(matches)
    }

    fn get_edges(&self, entity_id: &str) -> Result<Vec<GraphEdge>, GraphRetrievalError> {
        let edges: Vec<GraphEdge> = self
            .edges
            .iter()
            .filter(|e| e.source_id == entity_id || e.target_id == entity_id)
            .cloned()
            .collect();
        Ok(edges)
    }

    fn get_entity(&self, entity_id: &str) -> Result<Option<GraphEntity>, GraphRetrievalError> {
        Ok(self.entities.iter().find(|e| e.id == entity_id).cloned())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entity(id: &str, name: &str, entity_type: EntityType) -> GraphEntity {
        GraphEntity {
            id: id.to_string(),
            name: name.to_string(),
            entity_type,
            sensitivity: Sensitivity::Shareable,
            memories: vec![Memory {
                id: format!("{id}-mem1"),
                content: format!("Fact about {name}"),
                sensitivity: Sensitivity::Shareable,
                evidence: vec![format!("archive:{id}")],
                updated_at: None,
                archived: false,
                fsrs: None,
            }],
        }
    }

    fn make_edge(source: &str, target: &str, rel: &str, strength: f64) -> GraphEdge {
        GraphEdge {
            source_id: source.to_string(),
            target_id: target.to_string(),
            relationship: rel.to_string(),
            strength,
            weight: 1.0,
        }
    }

    /// Build a simple chain: A -> B -> C -> D
    fn chain_store() -> InMemoryGraphStore {
        let mut store = InMemoryGraphStore::new();
        store.add_entity(make_entity("a", "Alice", EntityType::Person));
        store.add_entity(make_entity("b", "Bob", EntityType::Person));
        store.add_entity(make_entity("c", "Charlie", EntityType::Person));
        store.add_entity(make_entity("d", "David", EntityType::Person));

        store.add_edge(make_edge("a", "b", "works_with", 1.0));
        store.add_edge(make_edge("b", "c", "works_with", 1.0));
        store.add_edge(make_edge("c", "d", "works_with", 1.0));

        store
    }

    #[test]
    fn bfs_depth_1_returns_direct_neighbors() {
        let store = chain_store();
        let retriever = BfsGraphRetriever::new(store);

        let config = GraphRetrievalConfig {
            max_depth: 1,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        assert_eq!(result.seeds.len(), 1);
        assert_eq!(result.seeds[0].entity_id, "a");
        assert!((result.seeds[0].score - 1.0).abs() < 1e-9);

        assert_eq!(result.related.len(), 1);
        assert_eq!(result.related[0].entity_id, "b");
        assert!((result.related[0].score - 0.7).abs() < 1e-9);
        assert_eq!(result.related[0].depth, 1);
    }

    #[test]
    fn bfs_depth_2_applies_double_decay() {
        let store = chain_store();
        let retriever = BfsGraphRetriever::new(store);

        let config = GraphRetrievalConfig {
            max_depth: 2,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        assert_eq!(result.seeds.len(), 1);
        assert_eq!(result.related.len(), 2);

        let bob = result.related.iter().find(|n| n.entity_id == "b").unwrap();
        assert!((bob.score - 0.7).abs() < 1e-9);
        assert_eq!(bob.depth, 1);

        let charlie = result.related.iter().find(|n| n.entity_id == "c").unwrap();
        assert!((charlie.score - 0.49).abs() < 1e-9);
        assert_eq!(charlie.depth, 2);
    }

    #[test]
    fn bfs_depth_3_applies_triple_decay() {
        let store = chain_store();
        let retriever = BfsGraphRetriever::new(store);

        let config = GraphRetrievalConfig {
            max_depth: 3,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        assert_eq!(result.seeds.len(), 1);
        assert_eq!(result.related.len(), 3);

        let david = result.related.iter().find(|n| n.entity_id == "d").unwrap();
        // 1.0 * 0.7^3 = 0.343
        assert!((david.score - 0.343).abs() < 1e-9);
        assert_eq!(david.depth, 3);
    }

    #[test]
    fn max_depth_enforcement_stops_traversal() {
        let store = chain_store();
        let retriever = BfsGraphRetriever::new(store);

        let config = GraphRetrievalConfig {
            max_depth: 1,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        // Should only see A (seed) and B (depth 1), not C or D
        let all_ids: Vec<&str> = result
            .seeds
            .iter()
            .chain(result.related.iter())
            .map(|n| n.entity_id.as_str())
            .collect();

        assert!(all_ids.contains(&"a"));
        assert!(all_ids.contains(&"b"));
        assert!(!all_ids.contains(&"c"));
        assert!(!all_ids.contains(&"d"));
    }

    #[test]
    fn min_score_filters_low_scoring_entities() {
        let store = chain_store();
        let retriever = BfsGraphRetriever::new(store);

        let config = GraphRetrievalConfig {
            max_depth: 3,
            decay_factor: 0.7,
            min_score: 0.5,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        // depth 0: score 1.0 (seed, always included)
        // depth 1: score 0.7 (above 0.5)
        // depth 2: score 0.49 (below 0.5, filtered)
        // depth 3: score 0.343 (below 0.5, filtered)
        let all_ids: Vec<&str> = result
            .seeds
            .iter()
            .chain(result.related.iter())
            .map(|n| n.entity_id.as_str())
            .collect();

        assert!(all_ids.contains(&"a"));
        assert!(all_ids.contains(&"b"));
        assert!(!all_ids.contains(&"c"));
        assert!(!all_ids.contains(&"d"));
    }

    #[test]
    fn empty_graph_returns_no_seeds_error() {
        let store = InMemoryGraphStore::new();
        let retriever = BfsGraphRetriever::new(store);

        let config = GraphRetrievalConfig::default();
        let result = retriever.retrieve("anything", &config, Sensitivity::Private);

        assert!(matches!(result, Err(GraphRetrievalError::NoSeeds)));
    }

    #[test]
    fn no_matching_query_returns_no_seeds_error() {
        let store = chain_store();
        let retriever = BfsGraphRetriever::new(store);

        let config = GraphRetrievalConfig::default();
        let result = retriever.retrieve("zzzzz_nonexistent", &config, Sensitivity::Private);

        assert!(matches!(result, Err(GraphRetrievalError::NoSeeds)));
    }

    #[test]
    fn cycle_handling_does_not_loop_forever() {
        // Build a cycle: A -> B -> C -> A
        let mut store = InMemoryGraphStore::new();
        store.add_entity(make_entity("a", "Alpha", EntityType::Concept));
        store.add_entity(make_entity("b", "Beta", EntityType::Concept));
        store.add_entity(make_entity("c", "Gamma", EntityType::Concept));

        store.add_edge(make_edge("a", "b", "related_to", 1.0));
        store.add_edge(make_edge("b", "c", "related_to", 1.0));
        store.add_edge(make_edge("c", "a", "related_to", 1.0));

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 3,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        // Should complete without infinite loop
        let result = retriever
            .retrieve("Alpha", &config, Sensitivity::Private)
            .unwrap();

        assert_eq!(result.seeds.len(), 1);
        assert_eq!(result.seeds[0].entity_id, "a");
        // B and C should appear as related
        assert_eq!(result.related.len(), 2);
    }

    #[test]
    fn deduplication_keeps_highest_score() {
        // Diamond: A -> B, A -> C, B -> D, C -> D
        // D reachable via a stronger and a weaker route. Retrieval should keep
        // the higher-scoring accumulated path.
        let mut store = InMemoryGraphStore::new();
        store.add_entity(make_entity("a", "Alpha", EntityType::Concept));
        store.add_entity(make_entity("b", "Beta", EntityType::Concept));
        store.add_entity(make_entity("c", "Charlie", EntityType::Concept));
        store.add_entity(make_entity("d", "Delta", EntityType::Concept));

        // A -> B with strength 1.0, A -> C with strength 0.5
        store.add_edge(make_edge("a", "b", "stronger_route", 1.0));
        store.add_edge(make_edge("a", "c", "weaker_route", 0.5));
        // B -> D with strength 1.0, C -> D with strength 1.0
        store.add_edge(make_edge("b", "d", "uses", 1.0));
        store.add_edge(make_edge("c", "d", "uses", 1.0));

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 2,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alpha", &config, Sensitivity::Private)
            .unwrap();

        let d_node = result.related.iter().find(|n| n.entity_id == "d").unwrap();
        // Via B: 1.0 * 1.0 * 0.7 * 1.0 * 1.0 * 0.7 = 0.49
        // Via C: 1.0 * 0.5 * 0.7 * 1.0 * 1.0 * 0.7 = 0.245
        assert!((d_node.score - 0.49).abs() < 1e-9);
        assert_eq!(
            d_node.path,
            vec!["stronger_route".to_string(), "uses".to_string()]
        );
    }

    #[test]
    fn sensitivity_filters_private_entities() {
        let mut store = InMemoryGraphStore::new();

        let mut private_entity = make_entity("a", "Alice", EntityType::Person);
        private_entity.sensitivity = Sensitivity::Shareable;
        store.add_entity(private_entity);

        let mut restricted = make_entity("b", "Bob Secret", EntityType::Person);
        restricted.sensitivity = Sensitivity::Private;
        store.add_entity(restricted);

        store.add_edge(make_edge("a", "b", "knows", 1.0));

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 1,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Shareable)
            .unwrap();

        // Bob should be filtered out because sensitivity=Private > Shareable
        assert_eq!(result.seeds.len(), 1);
        assert!(result.related.is_empty());
    }

    #[test]
    fn max_entities_limits_result_count() {
        let mut store = InMemoryGraphStore::new();
        store.add_entity(make_entity("seed", "Seed", EntityType::Concept));

        for i in 0..10 {
            let id = format!("r{i}");
            store.add_entity(make_entity(
                &id,
                &format!("Related{i}"),
                EntityType::Concept,
            ));
            store.add_edge(make_edge("seed", &id, "related_to", 1.0));
        }

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 1,
            decay_factor: 0.7,
            min_score: 0.0,
            max_entities: 5,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Seed", &config, Sensitivity::Private)
            .unwrap();

        let total = result.seeds.len() + result.related.len();
        assert!(total <= 5);
    }

    #[test]
    fn memories_filtered_by_sensitivity() {
        let mut store = InMemoryGraphStore::new();

        let entity = GraphEntity {
            id: "a".to_string(),
            name: "Alice".to_string(),
            entity_type: EntityType::Person,
            sensitivity: Sensitivity::Shareable,
            memories: vec![
                Memory {
                    id: "m1".to_string(),
                    content: "Public fact".to_string(),
                    sensitivity: Sensitivity::Shareable,
                    evidence: vec!["archive:a".to_string()],
                    updated_at: None,
                    archived: false,
                    fsrs: None,
                },
                Memory {
                    id: "m2".to_string(),
                    content: "Private fact".to_string(),
                    sensitivity: Sensitivity::Private,
                    evidence: vec!["archive:a".to_string()],
                    updated_at: None,
                    archived: false,
                    fsrs: None,
                },
            ],
        };
        store.add_entity(entity);

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 0,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Shareable)
            .unwrap();

        assert_eq!(result.seeds.len(), 1);
        assert_eq!(result.seeds[0].memories.len(), 1);
        assert_eq!(result.seeds[0].memories[0].id, "m1");
    }

    #[test]
    fn max_memories_per_entity_enforced() {
        let mut store = InMemoryGraphStore::new();

        let memories: Vec<Memory> = (0..10)
            .map(|i| Memory {
                id: format!("m{i}"),
                content: format!("Fact {i}"),
                sensitivity: Sensitivity::Shareable,
                evidence: vec!["archive:a".to_string()],
                updated_at: None,
                archived: false,
                fsrs: None,
            })
            .collect();

        let entity = GraphEntity {
            id: "a".to_string(),
            name: "Alice".to_string(),
            entity_type: EntityType::Person,
            sensitivity: Sensitivity::Shareable,
            memories,
        };
        store.add_entity(entity);

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 0,
            decay_factor: 0.7,
            min_score: 0.0,
            max_memories_per_entity: 3,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        assert_eq!(result.seeds.len(), 1);
        assert_eq!(result.seeds[0].memories.len(), 3);
    }

    #[test]
    fn path_tracks_relationship_types() {
        let store = chain_store();
        let retriever = BfsGraphRetriever::new(store);

        let config = GraphRetrievalConfig {
            max_depth: 3,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        let david = result.related.iter().find(|n| n.entity_id == "d").unwrap();
        assert_eq!(david.path.len(), 3);
        assert!(david.path.iter().all(|r| r == "works_with"));
    }

    #[test]
    fn default_config_values() {
        let config = GraphRetrievalConfig::default();
        assert_eq!(config.max_depth, 3);
        assert!((config.decay_factor - 0.7).abs() < 1e-9);
        assert_eq!(config.max_entities, 20);
        assert_eq!(config.max_memories_per_entity, 5);
        assert!((config.min_score - 0.1).abs() < 1e-9);
        assert!((config.half_life_days - 90.0).abs() < 1e-9);
    }

    #[test]
    fn decay_scoring_accuracy() {
        let config = GraphRetrievalConfig::default();

        // Verify decay formula: base * factor^depth
        let base = 1.0_f64;
        assert!((base * config.decay_factor.powi(0) - 1.0).abs() < 1e-9);
        assert!((base * config.decay_factor.powi(1) - 0.7).abs() < 1e-9);
        assert!((base * config.decay_factor.powi(2) - 0.49).abs() < 1e-9);
        assert!((base * config.decay_factor.powi(3) - 0.343).abs() < 1e-9);
    }

    // --- Temporal decay tests ---

    #[test]
    fn temporal_decay_function_basic() {
        // At exactly 1 half-life, score should be 0.5
        let factor = temporal_decay(Some(0), 86400 * 90, 90.0, None);
        assert!((factor - 0.5).abs() < 1e-6);

        // At 0 age, score should be 1.0
        let factor = temporal_decay(Some(1000), 1000, 90.0, None);
        assert!((factor - 1.0).abs() < 1e-6);

        // At 2 half-lives, score should be 0.25
        let factor = temporal_decay(Some(0), 86400 * 180, 90.0, None);
        assert!((factor - 0.25).abs() < 1e-6);
    }

    #[test]
    fn temporal_decay_disabled_when_zero_half_life() {
        let factor = temporal_decay(Some(0), 86400 * 365, 0.0, None);
        assert!((factor - 1.0).abs() < 1e-6);
    }

    #[test]
    fn temporal_decay_no_timestamp_returns_one() {
        let factor = temporal_decay(None, 86400 * 365, 90.0, None);
        assert!((factor - 1.0).abs() < 1e-6);
    }

    #[test]
    fn temporal_decay_applied_during_bfs_retrieval() {
        let now = 86400 * 200; // 200 days from epoch

        let mut store = InMemoryGraphStore::new();

        // Entity with a recent memory (10 days old)
        let mut recent = make_entity("recent", "RecentEntity", EntityType::Concept);
        recent.memories[0].updated_at = Some(now - 86400 * 10);
        store.add_entity(recent);

        // Entity with an old memory (180 days old = 2 half-lives at 90d)
        let mut old = make_entity("old", "OldEntity", EntityType::Concept);
        old.memories[0].updated_at = Some(now - 86400 * 180);
        store.add_entity(old);

        // Seed entity links to both
        let mut seed = make_entity("seed", "SeedEntity", EntityType::Concept);
        seed.memories[0].updated_at = Some(now);
        store.add_entity(seed);

        store.add_edge(make_edge("seed", "recent", "related_to", 1.0));
        store.add_edge(make_edge("seed", "old", "related_to", 1.0));

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 1,
            decay_factor: 1.0, // no hop decay — isolate temporal decay
            min_score: 0.0,
            half_life_days: 90.0,
            now_epoch_secs: now,
            ..Default::default()
        };

        let result = retriever
            .retrieve("SeedEntity", &config, Sensitivity::Private)
            .unwrap();

        let recent_node = result
            .related
            .iter()
            .find(|n| n.entity_id == "recent")
            .unwrap();
        let old_node = result
            .related
            .iter()
            .find(|n| n.entity_id == "old")
            .unwrap();

        // Recent entity (10 days old) should score much higher than old (180 days)
        assert!(
            recent_node.score > old_node.score,
            "recent ({}) should score higher than old ({})",
            recent_node.score,
            old_node.score
        );

        // Old entity at 2 half-lives should be ~0.25
        assert!(
            (old_node.score - 0.25).abs() < 0.05,
            "old entity score should be ~0.25, got {}",
            old_node.score
        );
    }

    #[test]
    fn temporal_decay_disabled_preserves_original_scores() {
        let now = 86400 * 200;

        let mut store = InMemoryGraphStore::new();
        let mut seed = make_entity("a", "Alice", EntityType::Person);
        seed.memories[0].updated_at = Some(now - 86400 * 180); // very old
        store.add_entity(seed);

        let mut related = make_entity("b", "Bob", EntityType::Person);
        related.memories[0].updated_at = Some(now - 86400 * 180);
        store.add_entity(related);
        store.add_edge(make_edge("a", "b", "works_with", 1.0));

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 1,
            decay_factor: 0.7,
            min_score: 0.0,
            half_life_days: 0.0, // disabled
            now_epoch_secs: now,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        let bob = result.related.iter().find(|n| n.entity_id == "b").unwrap();
        // Without temporal decay, Bob's score should be pure hop decay: 1.0 * 0.7 = 0.7
        assert!(
            (bob.score - 0.7).abs() < 1e-9,
            "score should be 0.7 without temporal decay, got {}",
            bob.score
        );
    }

    // --- Archived memory filtering tests ---

    #[test]
    fn archived_memories_excluded_from_results() {
        let mut store = InMemoryGraphStore::new();

        let entity = GraphEntity {
            id: "a".to_string(),
            name: "Alice".to_string(),
            entity_type: EntityType::Person,
            sensitivity: Sensitivity::Shareable,
            memories: vec![
                Memory {
                    id: "active-mem".to_string(),
                    content: "Active fact about Alice".to_string(),
                    sensitivity: Sensitivity::Shareable,
                    evidence: vec!["archive:a".to_string()],
                    updated_at: None,
                    archived: false,
                    fsrs: None,
                },
                Memory {
                    id: "archived-mem".to_string(),
                    content: "Outdated fact about Alice".to_string(),
                    sensitivity: Sensitivity::Shareable,
                    evidence: vec!["archive:a".to_string()],
                    updated_at: None,
                    archived: true,
                    fsrs: None,
                },
            ],
        };
        store.add_entity(entity);

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 0,
            half_life_days: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        assert_eq!(result.seeds.len(), 1);
        assert_eq!(
            result.seeds[0].memories.len(),
            1,
            "archived memory should be filtered out"
        );
        assert_eq!(result.seeds[0].memories[0].id, "active-mem");
    }

    #[test]
    fn fully_archived_entity_has_empty_memories() {
        let mut store = InMemoryGraphStore::new();

        let entity = GraphEntity {
            id: "a".to_string(),
            name: "Alice".to_string(),
            entity_type: EntityType::Person,
            sensitivity: Sensitivity::Shareable,
            memories: vec![Memory {
                id: "archived-only".to_string(),
                content: "Old fact".to_string(),
                sensitivity: Sensitivity::Shareable,
                evidence: vec!["archive:a".to_string()],
                updated_at: None,
                archived: true,
                fsrs: None,
            }],
        };
        store.add_entity(entity);

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 0,
            half_life_days: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alice", &config, Sensitivity::Private)
            .unwrap();

        assert_eq!(result.seeds.len(), 1);
        assert!(
            result.seeds[0].memories.is_empty(),
            "all memories are archived — should be empty"
        );
    }

    // --- FSRS retention tests ---

    #[test]
    fn fsrs_retention_at_zero_age() {
        // At zero age, retention should be ~1.0 regardless of stability.
        let r = fsrs_retention(0.0, 30.0);
        assert!(
            (r - 1.0).abs() < 1e-9,
            "retention at t=0 should be 1.0, got {r}"
        );
    }

    #[test]
    fn fsrs_retention_decays_over_time() {
        let r_1day = fsrs_retention(1.0, 30.0);
        let r_30day = fsrs_retention(30.0, 30.0);
        let r_90day = fsrs_retention(90.0, 30.0);

        assert!(r_1day > r_30day, "1 day should retain more than 30 days");
        assert!(r_30day > r_90day, "30 days should retain more than 90 days");
        assert!(
            r_1day < 1.0,
            "retention should be < 1.0 after any time passes"
        );
        assert!(r_90day > 0.0, "retention should never reach exactly 0.0");
    }

    #[test]
    fn fsrs_retention_high_stability_decays_slower() {
        let age = 30.0; // 30 days
        let r_low_stability = fsrs_retention(age, 10.0);
        let r_high_stability = fsrs_retention(age, 100.0);

        assert!(
            r_high_stability > r_low_stability,
            "high stability ({r_high_stability}) should retain more than low ({r_low_stability})"
        );
    }

    #[test]
    fn fsrs_retention_zero_stability_returns_zero() {
        let r = fsrs_retention(10.0, 0.0);
        assert!(r.abs() < 1e-9, "zero stability should yield 0.0, got {r}");
    }

    #[test]
    fn fsrs_update_stability_increases_on_recall() {
        let initial = 30.0;
        let difficulty = 0.3;
        let updated = fsrs_update_stability(initial, difficulty);

        assert!(
            updated > initial,
            "stability should increase after recall: {updated} > {initial}"
        );
    }

    #[test]
    fn fsrs_update_stability_capped_at_ten_years() {
        let huge = 5000.0;
        let updated = fsrs_update_stability(huge, 0.0);
        assert!(
            updated <= 365.0 * 10.0,
            "stability should be capped at 3650 days, got {updated}"
        );
    }

    #[test]
    fn fsrs_update_stability_harder_difficulty_grows_less() {
        let initial = 30.0;
        let easy = fsrs_update_stability(initial, 0.1);
        let hard = fsrs_update_stability(initial, 0.9);

        assert!(
            easy > hard,
            "easy memories should grow stability faster: {easy} > {hard}"
        );
    }

    #[test]
    fn fsrs_update_difficulty_adjusts_toward_quality() {
        let current = 0.5;

        // Perfect recall should lower difficulty.
        let after_perfect = fsrs_update_difficulty(current, 1.0);
        assert!(
            after_perfect < current,
            "perfect recall should lower difficulty: {after_perfect} < {current}"
        );

        // Complete failure should raise difficulty.
        let after_fail = fsrs_update_difficulty(current, 0.0);
        assert!(
            after_fail > current,
            "complete failure should raise difficulty: {after_fail} > {current}"
        );
    }

    #[test]
    fn fsrs_update_difficulty_clamps_to_valid_range() {
        // Even with extreme inputs, difficulty stays in [0.0, 1.0].
        let low = fsrs_update_difficulty(0.01, 1.0);
        assert!(low >= 0.0, "difficulty should not go below 0.0, got {low}");

        let high = fsrs_update_difficulty(0.99, 0.0);
        assert!(high <= 1.0, "difficulty should not exceed 1.0, got {high}");
    }

    #[test]
    fn legacy_half_life_still_works_with_fsrs_none() {
        // When fsrs=None, temporal_decay should produce the same legacy results.
        let factor = temporal_decay(Some(0), 86400 * 90, 90.0, None);
        assert!(
            (factor - 0.5).abs() < 1e-6,
            "legacy half-life at 1 half-life should be 0.5, got {factor}"
        );
    }

    #[test]
    fn temporal_decay_uses_fsrs_when_provided() {
        let params = FsrsParams {
            stability: 30.0,
            difficulty: 0.3,
            last_access: 86400 * 100, // 100 days from epoch
        };
        let now = 86400 * 130; // 30 days after last_access

        let factor = temporal_decay(None, now, 90.0, Some(&params));
        // FSRS: e^(-30 / (9*30)) = e^(-30/270) = e^(-1/9) ~ 0.8948
        let expected = (-30.0_f64 / (9.0 * 30.0)).exp();
        assert!(
            (factor - expected).abs() < 1e-6,
            "FSRS decay should be ~{expected}, got {factor}"
        );
    }

    #[test]
    fn temporal_decay_fsrs_falls_back_to_updated_at() {
        // When last_access is 0, FSRS should use updated_at instead.
        let params = FsrsParams {
            stability: 30.0,
            difficulty: 0.3,
            last_access: 0,
        };
        let updated_at = 86400 * 100;
        let now = 86400 * 130; // 30 days after updated_at

        let factor = temporal_decay(Some(updated_at), now, 90.0, Some(&params));
        let expected = (-30.0_f64 / (9.0 * 30.0)).exp();
        assert!(
            (factor - expected).abs() < 1e-6,
            "FSRS with last_access=0 should use updated_at, got {factor}"
        );
    }

    #[test]
    fn bfs_with_fsrs_memories_score_differently() {
        let now = 86400 * 200;

        let mut store = InMemoryGraphStore::new();

        // Seed entity
        let mut seed = make_entity("seed", "SeedEntity", EntityType::Concept);
        seed.memories[0].updated_at = Some(now);
        store.add_entity(seed);

        // Entity with FSRS-enabled memory (high stability = slow decay)
        let fsrs_entity = GraphEntity {
            id: "fsrs-high".to_string(),
            name: "FsrsHigh".to_string(),
            entity_type: EntityType::Concept,
            sensitivity: Sensitivity::Shareable,
            memories: vec![Memory {
                id: "fh-mem".to_string(),
                content: "FSRS high stability memory".to_string(),
                sensitivity: Sensitivity::Shareable,
                evidence: vec!["archive:fh".to_string()],
                updated_at: Some(now - 86400 * 60),
                archived: false,
                fsrs: Some(FsrsParams {
                    stability: 100.0, // very stable
                    difficulty: 0.3,
                    last_access: now - 86400 * 60, // 60 days ago
                }),
            }],
        };
        store.add_entity(fsrs_entity);

        // Entity with legacy memory (same age, uses half-life)
        let legacy_entity = GraphEntity {
            id: "legacy".to_string(),
            name: "LegacyEntity".to_string(),
            entity_type: EntityType::Concept,
            sensitivity: Sensitivity::Shareable,
            memories: vec![Memory {
                id: "l-mem".to_string(),
                content: "Legacy memory".to_string(),
                sensitivity: Sensitivity::Shareable,
                evidence: vec!["archive:l".to_string()],
                updated_at: Some(now - 86400 * 60), // 60 days ago
                archived: false,
                fsrs: None,
            }],
        };
        store.add_entity(legacy_entity);

        store.add_edge(make_edge("seed", "fsrs-high", "related_to", 1.0));
        store.add_edge(make_edge("seed", "legacy", "related_to", 1.0));

        let retriever = BfsGraphRetriever::new(store);
        let config = GraphRetrievalConfig {
            max_depth: 1,
            decay_factor: 1.0, // no hop decay
            min_score: 0.0,
            half_life_days: 90.0,
            now_epoch_secs: now,
            ..Default::default()
        };

        let result = retriever
            .retrieve("SeedEntity", &config, Sensitivity::Private)
            .unwrap();

        let fsrs_node = result
            .related
            .iter()
            .find(|n| n.entity_id == "fsrs-high")
            .unwrap();
        let legacy_node = result
            .related
            .iter()
            .find(|n| n.entity_id == "legacy")
            .unwrap();

        // FSRS with stability=100: e^(-60/(9*100)) = e^(-60/900) ~ 0.9355
        // Legacy with half_life=90: 0.5^(60/90) ~ 0.6300
        assert!(
            fsrs_node.score > legacy_node.score,
            "FSRS high-stability ({}) should score higher than legacy ({})",
            fsrs_node.score,
            legacy_node.score
        );

        // Verify the FSRS score is approximately correct.
        let expected_fsrs = (-60.0_f64 / (9.0 * 100.0)).exp();
        assert!(
            (fsrs_node.score - expected_fsrs).abs() < 0.01,
            "FSRS score should be ~{expected_fsrs}, got {}",
            fsrs_node.score
        );
    }

    #[test]
    fn betweenness_centrality_favors_bridge_nodes() {
        let entity_ids = vec!["a", "b", "c", "d"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let edges = vec![
            make_edge("a", "b", "related_to", 1.0),
            make_edge("b", "c", "related_to", 1.0),
            make_edge("b", "d", "related_to", 1.0),
        ];

        let centrality = betweenness_centrality(&entity_ids, &edges);
        let bridge = centrality.get("b").copied().unwrap_or_default();
        let leaf = centrality.get("a").copied().unwrap_or_default();

        assert!(bridge > leaf, "bridge node should outrank leaves");
        assert!(bridge > 0.0, "bridge node should have non-zero centrality");
        assert_eq!(leaf, 0.0, "leaf node should not be structurally central");
    }

    #[test]
    fn betweenness_centrality_handles_disconnected_nodes() {
        let entity_ids = vec!["a", "b", "c"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let edges = vec![make_edge("a", "b", "related_to", 1.0)];

        let centrality = betweenness_centrality(&entity_ids, &edges);
        assert_eq!(centrality.get("c").copied().unwrap_or_default(), 0.0);
    }
}
