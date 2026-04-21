//! Temporal and emotional (somatic) indexing for the memory graph.
//!
//! Somatic markers tag memories with emotional valence for fast routing.
//! Temporal indexing enables decay-aware retrieval: recent high-valence
//! memories surface faster than old neutral ones.
//!
//! Architecture reference: `docs/design/architecture-2.0-pivot.md`
//! > "claims are extracted and written to the graph with temporal tags
//! > and emotional/impact weighting (somatic markers for fast routing)"

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::graph::{
    GraphRetrievalConfig, GraphRetrievalError, GraphRetrievalResult, GraphRetriever,
};
use crate::Sensitivity;

// ---------------------------------------------------------------------------
// Somatic Marker Types
// ---------------------------------------------------------------------------

/// Emotional valence categories for somatic markers.
///
/// Based on Damasio's somatic marker hypothesis: gut-level emotional tags
/// that accelerate decision making by pre-filtering options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmotionalValence {
    /// Strongly positive (excitement, pride, breakthrough).
    HighPositive,
    /// Mildly positive (satisfaction, interest).
    Positive,
    /// No strong emotional signal.
    Neutral,
    /// Mildly negative (concern, frustration).
    Negative,
    /// Strongly negative (danger, anger, betrayal).
    HighNegative,
}

impl EmotionalValence {
    /// Numeric weight for scoring. High-valence (both positive and negative)
    /// memories are more salient and should surface faster.
    pub fn salience_weight(&self) -> f64 {
        match self {
            Self::HighPositive => 1.5,
            Self::Positive => 1.2,
            Self::Neutral => 1.0,
            Self::Negative => 1.2,
            Self::HighNegative => 1.5,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::HighPositive => "high_positive",
            Self::Positive => "positive",
            Self::Neutral => "neutral",
            Self::Negative => "negative",
            Self::HighNegative => "high_negative",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "high_positive" => Some(Self::HighPositive),
            "positive" => Some(Self::Positive),
            "neutral" => Some(Self::Neutral),
            "negative" => Some(Self::Negative),
            "high_negative" => Some(Self::HighNegative),
            _ => None,
        }
    }
}

/// Impact category: why this memory matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactCategory {
    /// Affects a goal or active plan.
    GoalRelevant,
    /// Relates to identity or self-model.
    Identity,
    /// Relates to a person/relationship.
    Relational,
    /// Methodological insight (how-to, process).
    Procedural,
    /// General knowledge without strong impact.
    Informational,
}

impl ImpactCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GoalRelevant => "goal_relevant",
            Self::Identity => "identity",
            Self::Relational => "relational",
            Self::Procedural => "procedural",
            Self::Informational => "informational",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "goal_relevant" => Some(Self::GoalRelevant),
            "identity" => Some(Self::Identity),
            "relational" => Some(Self::Relational),
            "procedural" => Some(Self::Procedural),
            "informational" => Some(Self::Informational),
            _ => None,
        }
    }
}

/// A somatic marker attached to a memory or entity in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SomaticMarker {
    /// The entity or memory ID this marker is attached to.
    pub target_id: String,
    /// Emotional valence.
    pub valence: EmotionalValence,
    /// Impact category.
    pub impact: ImpactCategory,
    /// Intensity (0.0..=1.0). Higher = more emotionally charged.
    pub intensity: f64,
    /// Unix timestamp when this marker was created.
    pub created_at: u64,
    /// Unix timestamp of the event this memory refers to (for temporal indexing).
    pub event_time: u64,
    /// Access count: how many times this memory has been recalled.
    pub access_count: u32,
    /// Last access timestamp.
    pub last_accessed: u64,
}

// ---------------------------------------------------------------------------
// Temporal Decay
// ---------------------------------------------------------------------------

/// Configuration for temporal decay and somatic boosting.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SomaticConfig {
    /// Half-life in seconds for temporal decay. After this duration,
    /// a memory's temporal freshness score drops to 0.5.
    pub temporal_half_life_secs: u64,
    /// Weight for somatic (emotional) boosting in final score.
    /// Final score = base_score * (1.0 + somatic_weight * somatic_boost).
    pub somatic_weight: f64,
    /// Weight for temporal freshness in final score.
    /// Applied as: score * temporal_factor^(age / half_life).
    pub temporal_weight: f64,
    /// Bonus for frequently accessed memories (logarithmic).
    pub access_frequency_weight: f64,
}

impl Default for SomaticConfig {
    fn default() -> Self {
        Self {
            temporal_half_life_secs: 7 * 86400, // 1 week
            somatic_weight: 0.3,
            temporal_weight: 0.5,
            access_frequency_weight: 0.1,
        }
    }
}

/// Compute temporal freshness factor (0.0..=1.0).
/// Uses exponential decay with configurable half-life.
pub fn temporal_freshness(event_time: u64, now: u64, half_life_secs: u64) -> f64 {
    if half_life_secs == 0 || now <= event_time {
        return 1.0;
    }
    let age = now - event_time;
    // f(t) = 0.5^(age / half_life)
    0.5_f64.powf(age as f64 / half_life_secs as f64)
}

/// Compute access frequency bonus (logarithmic).
pub fn access_frequency_bonus(access_count: u32) -> f64 {
    if access_count == 0 {
        return 0.0;
    }
    (access_count as f64 + 1.0).ln()
}

/// Compute the somatic boost for a marker.
/// Combines valence salience and intensity.
pub fn somatic_boost(marker: &SomaticMarker) -> f64 {
    marker.valence.salience_weight() * marker.intensity
}

/// Compute the full somatic-temporal score adjustment.
/// Returns a multiplier to apply to the base graph score.
pub fn somatic_temporal_multiplier(
    marker: &SomaticMarker,
    now: u64,
    config: &SomaticConfig,
) -> f64 {
    let freshness = temporal_freshness(marker.event_time, now, config.temporal_half_life_secs);
    let boost = somatic_boost(marker);
    let freq = access_frequency_bonus(marker.access_count);

    1.0 + config.somatic_weight * boost
        + config.temporal_weight * (freshness - 0.5) // centered: fresh > 0, stale < 0
        + config.access_frequency_weight * freq
}

// ---------------------------------------------------------------------------
// Somatic Index (in-memory)
// ---------------------------------------------------------------------------

/// Errors from somatic operations.
#[derive(Debug, Error)]
pub enum SomaticError {
    #[error("marker not found for target: {0}")]
    MarkerNotFound(String),
    #[error("invalid intensity: {0} (must be 0.0..=1.0)")]
    InvalidIntensity(f64),
    #[error("graph retrieval error: {0}")]
    GraphError(#[from] GraphRetrievalError),
    #[error("storage error: {0}")]
    StorageError(String),
}

// ---------------------------------------------------------------------------
// SomaticStore trait (persistence interface)
// ---------------------------------------------------------------------------

/// Trait for persisting somatic markers to durable storage.
///
/// Implementations back the in-memory `SomaticIndex` with a persistent store
/// so that markers survive process restarts.
#[async_trait::async_trait]
pub trait SomaticStore: Send + Sync {
    /// Save or update a marker for the given entity.
    async fn save_marker(
        &self,
        entity_id: &str,
        marker: &SomaticMarker,
    ) -> Result<(), SomaticError>;

    /// Load a marker by entity ID.
    async fn load_marker(&self, entity_id: &str) -> Result<Option<SomaticMarker>, SomaticError>;

    /// Load all persisted markers.
    async fn load_all(&self) -> Result<Vec<(String, SomaticMarker)>, SomaticError>;

    /// Record an access for the given entity (increment count + update timestamp).
    async fn record_access(&self, entity_id: &str) -> Result<(), SomaticError>;

    /// Remove markers older than the given number of days. Returns the count removed.
    async fn prune_older_than(&self, days: u64) -> Result<u64, SomaticError>;
}

// ---------------------------------------------------------------------------
// SqliteSomaticStore
// ---------------------------------------------------------------------------

/// SQLite-backed implementation of `SomaticStore`.
///
/// Table schema:
/// ```sql
/// somatic_markers(
///     entity_id TEXT PRIMARY KEY,
///     valence TEXT NOT NULL,
///     intensity REAL NOT NULL,
///     impact_category TEXT NOT NULL,
///     event_time INTEGER NOT NULL,
///     access_count INTEGER NOT NULL DEFAULT 0,
///     last_accessed INTEGER NOT NULL DEFAULT 0,
///     created_at INTEGER NOT NULL
/// )
/// ```
pub struct SqliteSomaticStore {
    conn: std::sync::Mutex<rusqlite::Connection>,
}

impl SqliteSomaticStore {
    /// Open or create a SQLite database at the given path and ensure the
    /// `somatic_markers` table exists.
    pub fn open(path: &std::path::Path) -> Result<Self, SomaticError> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS somatic_markers (
                entity_id TEXT PRIMARY KEY,
                valence TEXT NOT NULL,
                intensity REAL NOT NULL,
                impact_category TEXT NOT NULL,
                event_time INTEGER NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }

    /// Create an in-memory SQLite store (useful for testing).
    pub fn in_memory() -> Result<Self, SomaticError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS somatic_markers (
                entity_id TEXT PRIMARY KEY,
                valence TEXT NOT NULL,
                intensity REAL NOT NULL,
                impact_category TEXT NOT NULL,
                event_time INTEGER NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }

    fn row_to_marker(row: &rusqlite::Row<'_>) -> Result<(String, SomaticMarker), rusqlite::Error> {
        let entity_id: String = row.get(0)?;
        let valence_str: String = row.get(1)?;
        let intensity: f64 = row.get(2)?;
        let impact_str: String = row.get(3)?;
        let event_time: u64 = row.get(4)?;
        let access_count: u32 = row.get(5)?;
        let last_accessed: u64 = row.get(6)?;
        let created_at: u64 = row.get(7)?;

        let valence =
            EmotionalValence::parse_str(&valence_str).unwrap_or(EmotionalValence::Neutral);
        let impact =
            ImpactCategory::parse_str(&impact_str).unwrap_or(ImpactCategory::Informational);

        Ok((
            entity_id.clone(),
            SomaticMarker {
                target_id: entity_id,
                valence,
                impact,
                intensity,
                created_at,
                event_time,
                access_count,
                last_accessed,
            },
        ))
    }
}

#[async_trait::async_trait]
impl SomaticStore for SqliteSomaticStore {
    async fn save_marker(
        &self,
        entity_id: &str,
        marker: &SomaticMarker,
    ) -> Result<(), SomaticError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        conn.execute(
            "INSERT INTO somatic_markers (entity_id, valence, intensity, impact_category, event_time, access_count, last_accessed, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(entity_id) DO UPDATE SET
                valence = excluded.valence,
                intensity = excluded.intensity,
                impact_category = excluded.impact_category,
                event_time = excluded.event_time,
                access_count = excluded.access_count,
                last_accessed = excluded.last_accessed",
            rusqlite::params![
                entity_id,
                marker.valence.as_str(),
                marker.intensity,
                marker.impact.as_str(),
                marker.event_time,
                marker.access_count,
                marker.last_accessed,
                marker.created_at,
            ],
        )
        .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        Ok(())
    }

    async fn load_marker(&self, entity_id: &str) -> Result<Option<SomaticMarker>, SomaticError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT entity_id, valence, intensity, impact_category, event_time, access_count, last_accessed, created_at
                 FROM somatic_markers WHERE entity_id = ?1",
            )
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        let result = stmt
            .query_row(rusqlite::params![entity_id], Self::row_to_marker)
            .optional()
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        Ok(result.map(|(_, m)| m))
    }

    async fn load_all(&self) -> Result<Vec<(String, SomaticMarker)>, SomaticError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT entity_id, valence, intensity, impact_category, event_time, access_count, last_accessed, created_at
                 FROM somatic_markers",
            )
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        let rows = stmt
            .query_map([], Self::row_to_marker)
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| SomaticError::StorageError(e.to_string()))?);
        }
        Ok(result)
    }

    async fn record_access(&self, entity_id: &str) -> Result<(), SomaticError> {
        let now = symbiotic_core::now_unix();
        let conn = self
            .conn
            .lock()
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        let updated = conn
            .execute(
                "UPDATE somatic_markers SET access_count = access_count + 1, last_accessed = ?1 WHERE entity_id = ?2",
                rusqlite::params![now, entity_id],
            )
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        if updated == 0 {
            return Err(SomaticError::MarkerNotFound(entity_id.to_string()));
        }
        Ok(())
    }

    async fn prune_older_than(&self, days: u64) -> Result<u64, SomaticError> {
        let now = symbiotic_core::now_unix();
        let cutoff = now.saturating_sub(days * 86400);
        let conn = self
            .conn
            .lock()
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        let deleted = conn
            .execute(
                "DELETE FROM somatic_markers WHERE last_accessed < ?1 AND event_time < ?1",
                rusqlite::params![cutoff],
            )
            .map_err(|e| SomaticError::StorageError(e.to_string()))?;
        Ok(deleted as u64)
    }
}

// We need the `optional` method on `Result` from rusqlite
use rusqlite::OptionalExtension;

// ---------------------------------------------------------------------------
// Somatic Index (in-memory, optionally store-backed)
// ---------------------------------------------------------------------------

/// In-memory somatic marker index.
/// Maps entity/memory IDs to their somatic markers.
/// Optionally backed by a `SomaticStore` for persistence.
#[derive(Default)]
pub struct SomaticIndex {
    markers: HashMap<String, SomaticMarker>,
    /// Optional persistent store. When present, mutations are written through.
    store: Option<Arc<dyn SomaticStore>>,
}

impl std::fmt::Debug for SomaticIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SomaticIndex")
            .field("markers", &self.markers)
            .field("has_store", &self.store.is_some())
            .finish()
    }
}

impl SomaticIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an index backed by a persistent store.
    ///
    /// Call `load_from_store()` after construction to populate the in-memory
    /// cache from the store.
    pub fn with_store(store: Arc<dyn SomaticStore>) -> Self {
        Self {
            markers: HashMap::new(),
            store: Some(store),
        }
    }

    /// Load all markers from the backing store into the in-memory cache.
    ///
    /// This is a no-op if no store is configured. Should be called once
    /// after construction with `with_store()`.
    pub async fn load_from_store(&mut self) -> Result<usize, SomaticError> {
        if let Some(ref store) = self.store {
            let entries = store.load_all().await?;
            let count = entries.len();
            for (id, marker) in entries {
                self.markers.insert(id, marker);
            }
            Ok(count)
        } else {
            Ok(0)
        }
    }

    /// Add or update a somatic marker.
    ///
    /// When a store is configured, the marker is persisted synchronously
    /// (via the async store trait, but called in a blocking context for
    /// backward compatibility with the sync API). For async callers,
    /// prefer `upsert_async`.
    pub fn upsert(&mut self, marker: SomaticMarker) -> Result<(), SomaticError> {
        if !(0.0..=1.0).contains(&marker.intensity) {
            return Err(SomaticError::InvalidIntensity(marker.intensity));
        }
        self.markers.insert(marker.target_id.clone(), marker);
        Ok(())
    }

    /// Add or update a somatic marker, persisting to the backing store.
    pub async fn upsert_async(&mut self, marker: SomaticMarker) -> Result<(), SomaticError> {
        if !(0.0..=1.0).contains(&marker.intensity) {
            return Err(SomaticError::InvalidIntensity(marker.intensity));
        }
        if let Some(ref store) = self.store {
            store.save_marker(&marker.target_id, &marker).await?;
        }
        self.markers.insert(marker.target_id.clone(), marker);
        Ok(())
    }

    /// Get a marker by target ID.
    pub fn get(&self, target_id: &str) -> Option<&SomaticMarker> {
        self.markers.get(target_id)
    }

    /// Remove a marker.
    pub fn remove(&mut self, target_id: &str) -> bool {
        self.markers.remove(target_id).is_some()
    }

    /// Record an access (recall) for a target, incrementing count and updating timestamp.
    pub fn record_access(&mut self, target_id: &str, now: u64) -> Result<(), SomaticError> {
        let marker = self
            .markers
            .get_mut(target_id)
            .ok_or_else(|| SomaticError::MarkerNotFound(target_id.to_string()))?;
        marker.access_count += 1;
        marker.last_accessed = now;
        Ok(())
    }

    /// Record an access and persist it to the backing store.
    pub async fn record_access_async(
        &mut self,
        target_id: &str,
        now: u64,
    ) -> Result<(), SomaticError> {
        self.record_access(target_id, now)?;
        if let Some(ref store) = self.store {
            store.record_access(target_id).await?;
        }
        Ok(())
    }

    /// Get all markers.
    pub fn markers(&self) -> &HashMap<String, SomaticMarker> {
        &self.markers
    }

    /// Number of markers.
    pub fn len(&self) -> usize {
        self.markers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }

    /// Query markers by valence.
    pub fn by_valence(&self, valence: EmotionalValence) -> Vec<&SomaticMarker> {
        self.markers
            .values()
            .filter(|m| m.valence == valence)
            .collect()
    }

    /// Query markers by impact category.
    pub fn by_impact(&self, impact: ImpactCategory) -> Vec<&SomaticMarker> {
        self.markers
            .values()
            .filter(|m| m.impact == impact)
            .collect()
    }

    /// Get markers sorted by somatic-temporal score (highest first).
    pub fn ranked(&self, now: u64, config: &SomaticConfig) -> Vec<(&str, f64)> {
        let mut ranked: Vec<(&str, f64)> = self
            .markers
            .iter()
            .map(|(id, marker)| {
                let score = somatic_temporal_multiplier(marker, now, config);
                (id.as_str(), score)
            })
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked
    }

    /// Returns a reference to the backing store, if configured.
    pub fn store(&self) -> Option<&Arc<dyn SomaticStore>> {
        self.store.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Somatic-enhanced Graph Retriever
// ---------------------------------------------------------------------------

/// Wraps an existing `GraphRetriever` and applies somatic-temporal boosting
/// to the retrieval results.
pub struct SomaticGraphRetriever<R: GraphRetriever> {
    inner: R,
    index: SomaticIndex,
    config: SomaticConfig,
}

impl<R: GraphRetriever> SomaticGraphRetriever<R> {
    pub fn new(inner: R, index: SomaticIndex, config: SomaticConfig) -> Self {
        Self {
            inner,
            index,
            config,
        }
    }

    /// Get a mutable reference to the somatic index.
    pub fn index_mut(&mut self) -> &mut SomaticIndex {
        &mut self.index
    }

    /// Get a reference to the somatic index.
    pub fn index(&self) -> &SomaticIndex {
        &self.index
    }
}

impl<R: GraphRetriever> GraphRetriever for SomaticGraphRetriever<R> {
    fn retrieve(
        &self,
        query: &str,
        config: &GraphRetrievalConfig,
        sensitivity_max: Sensitivity,
    ) -> Result<GraphRetrievalResult, GraphRetrievalError> {
        let now = symbiotic_core::now_unix();
        let mut result = self.inner.retrieve(query, config, sensitivity_max)?;

        // Apply somatic-temporal boosting to all nodes.
        for node in result.seeds.iter_mut().chain(result.related.iter_mut()) {
            if let Some(marker) = self.index.get(&node.entity_id) {
                let multiplier = somatic_temporal_multiplier(marker, now, &self.config);
                node.score *= multiplier;
            }
        }

        // Re-sort after boosting.
        result.seeds.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        result.related.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{
        BfsGraphRetriever, EntityType, GraphEdge, GraphEntity, InMemoryGraphStore, Memory,
    };

    fn make_marker(target_id: &str, valence: EmotionalValence, event_time: u64) -> SomaticMarker {
        SomaticMarker {
            target_id: target_id.to_string(),
            valence,
            impact: ImpactCategory::Informational,
            intensity: 0.8,
            created_at: event_time,
            event_time,
            access_count: 0,
            last_accessed: 0,
        }
    }

    fn make_entity(id: &str, name: &str) -> GraphEntity {
        GraphEntity {
            id: id.to_string(),
            name: name.to_string(),
            entity_type: EntityType::Concept,
            sensitivity: Sensitivity::Shareable,
            memories: vec![Memory {
                id: format!("{id}-mem"),
                content: format!("Fact about {name}"),
                sensitivity: Sensitivity::Shareable,
                evidence: vec![format!("archive:{id}")],
                updated_at: None,
                archived: false,
                fsrs: None,
            }],
        }
    }

    fn make_edge(source: &str, target: &str) -> GraphEdge {
        GraphEdge {
            source_id: source.to_string(),
            target_id: target.to_string(),
            relationship: "related_to".to_string(),
            strength: 1.0,
            weight: 1.0,
        }
    }

    // --- EmotionalValence tests ---

    #[test]
    fn valence_salience_weights_are_symmetric() {
        assert_eq!(
            EmotionalValence::HighPositive.salience_weight(),
            EmotionalValence::HighNegative.salience_weight()
        );
        assert_eq!(
            EmotionalValence::Positive.salience_weight(),
            EmotionalValence::Negative.salience_weight()
        );
    }

    #[test]
    fn valence_high_has_greater_weight_than_neutral() {
        assert!(
            EmotionalValence::HighPositive.salience_weight()
                > EmotionalValence::Neutral.salience_weight()
        );
        assert!(
            EmotionalValence::HighNegative.salience_weight()
                > EmotionalValence::Neutral.salience_weight()
        );
    }

    #[test]
    fn valence_roundtrip_str() {
        for valence in [
            EmotionalValence::HighPositive,
            EmotionalValence::Positive,
            EmotionalValence::Neutral,
            EmotionalValence::Negative,
            EmotionalValence::HighNegative,
        ] {
            assert_eq!(EmotionalValence::parse_str(valence.as_str()), Some(valence));
        }
        assert_eq!(EmotionalValence::parse_str("unknown"), None);
    }

    #[test]
    fn impact_category_roundtrip_str() {
        for impact in [
            ImpactCategory::GoalRelevant,
            ImpactCategory::Identity,
            ImpactCategory::Relational,
            ImpactCategory::Procedural,
            ImpactCategory::Informational,
        ] {
            assert_eq!(ImpactCategory::parse_str(impact.as_str()), Some(impact));
        }
        assert_eq!(ImpactCategory::parse_str("bogus"), None);
    }

    // --- Temporal freshness tests ---

    #[test]
    fn temporal_freshness_at_event_time_is_one() {
        let now = 1000u64;
        let f = temporal_freshness(now, now, 86400);
        assert!((f - 1.0).abs() < 1e-9);
    }

    #[test]
    fn temporal_freshness_at_half_life_is_half() {
        let half_life = 86400u64; // 1 day
        let f = temporal_freshness(0, half_life, half_life);
        assert!((f - 0.5).abs() < 1e-9);
    }

    #[test]
    fn temporal_freshness_at_two_half_lives_is_quarter() {
        let half_life = 86400u64;
        let f = temporal_freshness(0, 2 * half_life, half_life);
        assert!((f - 0.25).abs() < 1e-9);
    }

    #[test]
    fn temporal_freshness_future_event_is_one() {
        let f = temporal_freshness(2000, 1000, 86400);
        assert!((f - 1.0).abs() < 1e-9);
    }

    #[test]
    fn temporal_freshness_zero_half_life_is_one() {
        let f = temporal_freshness(0, 1000, 0);
        assert!((f - 1.0).abs() < 1e-9);
    }

    // --- Access frequency bonus ---

    #[test]
    fn access_frequency_zero_is_zero() {
        assert!((access_frequency_bonus(0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn access_frequency_increases_with_count() {
        let b1 = access_frequency_bonus(1);
        let b5 = access_frequency_bonus(5);
        let b20 = access_frequency_bonus(20);
        assert!(b1 > 0.0);
        assert!(b5 > b1);
        assert!(b20 > b5);
    }

    // --- Somatic boost ---

    #[test]
    fn somatic_boost_high_valence_high_intensity() {
        let marker = SomaticMarker {
            target_id: "x".to_string(),
            valence: EmotionalValence::HighPositive,
            impact: ImpactCategory::GoalRelevant,
            intensity: 1.0,
            created_at: 0,
            event_time: 0,
            access_count: 0,
            last_accessed: 0,
        };
        let boost = somatic_boost(&marker);
        assert!((boost - 1.5).abs() < 1e-9); // 1.5 * 1.0
    }

    #[test]
    fn somatic_boost_neutral_is_intensity() {
        let marker = SomaticMarker {
            target_id: "x".to_string(),
            valence: EmotionalValence::Neutral,
            impact: ImpactCategory::Informational,
            intensity: 0.5,
            created_at: 0,
            event_time: 0,
            access_count: 0,
            last_accessed: 0,
        };
        let boost = somatic_boost(&marker);
        assert!((boost - 0.5).abs() < 1e-9); // 1.0 * 0.5
    }

    // --- Somatic-temporal multiplier ---

    #[test]
    fn multiplier_fresh_high_valence_boosts_significantly() {
        let config = SomaticConfig::default();
        let now = 1000u64;
        let marker = SomaticMarker {
            target_id: "x".to_string(),
            valence: EmotionalValence::HighPositive,
            impact: ImpactCategory::GoalRelevant,
            intensity: 1.0,
            created_at: now,
            event_time: now, // fresh
            access_count: 5,
            last_accessed: now,
        };
        let multiplier = somatic_temporal_multiplier(&marker, now, &config);
        assert!(
            multiplier > 1.5,
            "fresh high-valence should boost significantly, got {multiplier}"
        );
    }

    #[test]
    fn multiplier_stale_neutral_penalizes() {
        let config = SomaticConfig::default();
        let now = 1_000_000u64;
        let marker = SomaticMarker {
            target_id: "x".to_string(),
            valence: EmotionalValence::Neutral,
            impact: ImpactCategory::Informational,
            intensity: 0.0,
            created_at: 0,
            event_time: 0, // very old
            access_count: 0,
            last_accessed: 0,
        };
        let multiplier = somatic_temporal_multiplier(&marker, now, &config);
        assert!(
            multiplier < 1.0,
            "stale neutral should penalize, got {multiplier}"
        );
    }

    // --- SomaticIndex tests ---

    #[test]
    fn index_upsert_and_get() {
        let mut index = SomaticIndex::new();
        let marker = make_marker("entity-1", EmotionalValence::Positive, 1000);
        index.upsert(marker).unwrap();

        assert_eq!(index.len(), 1);
        let got = index.get("entity-1").unwrap();
        assert_eq!(got.valence, EmotionalValence::Positive);
    }

    #[test]
    fn index_upsert_replaces() {
        let mut index = SomaticIndex::new();
        index
            .upsert(make_marker("e1", EmotionalValence::Neutral, 1000))
            .unwrap();
        index
            .upsert(make_marker("e1", EmotionalValence::HighNegative, 2000))
            .unwrap();

        assert_eq!(index.len(), 1);
        assert_eq!(
            index.get("e1").unwrap().valence,
            EmotionalValence::HighNegative
        );
    }

    #[test]
    fn index_rejects_invalid_intensity() {
        let mut index = SomaticIndex::new();
        let mut marker = make_marker("e1", EmotionalValence::Neutral, 1000);
        marker.intensity = 1.5; // invalid
        assert!(matches!(
            index.upsert(marker),
            Err(SomaticError::InvalidIntensity(_))
        ));

        let mut marker2 = make_marker("e2", EmotionalValence::Neutral, 1000);
        marker2.intensity = -0.1; // invalid
        assert!(matches!(
            index.upsert(marker2),
            Err(SomaticError::InvalidIntensity(_))
        ));
    }

    #[test]
    fn index_remove() {
        let mut index = SomaticIndex::new();
        index
            .upsert(make_marker("e1", EmotionalValence::Positive, 1000))
            .unwrap();
        assert!(index.remove("e1"));
        assert!(!index.remove("e1"));
        assert!(index.is_empty());
    }

    #[test]
    fn index_record_access() {
        let mut index = SomaticIndex::new();
        index
            .upsert(make_marker("e1", EmotionalValence::Positive, 1000))
            .unwrap();

        index.record_access("e1", 2000).unwrap();
        index.record_access("e1", 3000).unwrap();

        let marker = index.get("e1").unwrap();
        assert_eq!(marker.access_count, 2);
        assert_eq!(marker.last_accessed, 3000);
    }

    #[test]
    fn index_record_access_missing_errors() {
        let mut index = SomaticIndex::new();
        assert!(matches!(
            index.record_access("ghost", 1000),
            Err(SomaticError::MarkerNotFound(_))
        ));
    }

    #[test]
    fn index_by_valence() {
        let mut index = SomaticIndex::new();
        index
            .upsert(make_marker("e1", EmotionalValence::Positive, 1000))
            .unwrap();
        index
            .upsert(make_marker("e2", EmotionalValence::Negative, 1000))
            .unwrap();
        index
            .upsert(make_marker("e3", EmotionalValence::Positive, 2000))
            .unwrap();

        let positives = index.by_valence(EmotionalValence::Positive);
        assert_eq!(positives.len(), 2);

        let negatives = index.by_valence(EmotionalValence::Negative);
        assert_eq!(negatives.len(), 1);

        let neutrals = index.by_valence(EmotionalValence::Neutral);
        assert!(neutrals.is_empty());
    }

    #[test]
    fn index_by_impact() {
        let mut index = SomaticIndex::new();
        let mut m1 = make_marker("e1", EmotionalValence::Positive, 1000);
        m1.impact = ImpactCategory::GoalRelevant;
        index.upsert(m1).unwrap();

        let mut m2 = make_marker("e2", EmotionalValence::Negative, 1000);
        m2.impact = ImpactCategory::Identity;
        index.upsert(m2).unwrap();

        assert_eq!(index.by_impact(ImpactCategory::GoalRelevant).len(), 1);
        assert_eq!(index.by_impact(ImpactCategory::Identity).len(), 1);
        assert!(index.by_impact(ImpactCategory::Procedural).is_empty());
    }

    #[test]
    fn index_ranked_orders_by_score() {
        let mut index = SomaticIndex::new();
        let now = 10_000u64;
        let config = SomaticConfig::default();

        // Fresh high-valence.
        let mut m1 = make_marker("fresh-hot", EmotionalValence::HighPositive, now);
        m1.intensity = 1.0;
        m1.access_count = 10;
        index.upsert(m1).unwrap();

        // Old neutral.
        let mut m2 = make_marker("stale-cold", EmotionalValence::Neutral, 0);
        m2.intensity = 0.0;
        m2.access_count = 0;
        index.upsert(m2).unwrap();

        let ranked = index.ranked(now, &config);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0, "fresh-hot");
        assert!(ranked[0].1 > ranked[1].1);
    }

    // --- SomaticGraphRetriever tests ---

    fn graph_store_with_entities() -> InMemoryGraphStore {
        let mut store = InMemoryGraphStore::new();
        store.add_entity(make_entity("a", "Alpha"));
        store.add_entity(make_entity("b", "Beta"));
        store.add_entity(make_entity("c", "Gamma"));
        store.add_edge(make_edge("a", "b"));
        store.add_edge(make_edge("b", "c"));
        store
    }

    #[test]
    fn somatic_retriever_boosts_marked_entities() {
        let store = graph_store_with_entities();
        let inner = BfsGraphRetriever::new(store);

        let mut somatic_index = SomaticIndex::new();
        // Mark "c" (Gamma) as emotionally significant.
        let mut marker = make_marker(
            "c",
            EmotionalValence::HighPositive,
            symbiotic_core::now_unix(),
        );
        marker.intensity = 1.0;
        marker.access_count = 5;
        somatic_index.upsert(marker).unwrap();

        let config = SomaticConfig::default();
        let retriever = SomaticGraphRetriever::new(inner, somatic_index, config);

        let graph_config = GraphRetrievalConfig {
            max_depth: 2,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alpha", &graph_config, Sensitivity::Private)
            .unwrap();

        // Both B and C should be in related.
        assert_eq!(result.related.len(), 2);

        let gamma = result.related.iter().find(|n| n.entity_id == "c").unwrap();
        let beta = result.related.iter().find(|n| n.entity_id == "b").unwrap();

        // Gamma (depth 2, decayed) should be boosted above its normal score.
        // Normal: 0.7^2 = 0.49. With somatic boost it should be > 0.49.
        assert!(
            gamma.score > 0.49,
            "gamma should be boosted, got {}",
            gamma.score
        );

        // Beta has no marker, so score unchanged at 0.7.
        assert!(
            (beta.score - 0.7).abs() < 0.01,
            "beta should be ~0.7, got {}",
            beta.score
        );
    }

    #[test]
    fn somatic_retriever_reorders_by_boosted_score() {
        let store = graph_store_with_entities();
        let inner = BfsGraphRetriever::new(store);

        let mut somatic_index = SomaticIndex::new();
        // Give "c" a huge boost so it outranks "b" despite deeper depth.
        let mut marker = make_marker(
            "c",
            EmotionalValence::HighPositive,
            symbiotic_core::now_unix(),
        );
        marker.intensity = 1.0;
        marker.access_count = 50;
        somatic_index.upsert(marker).unwrap();

        let config = SomaticConfig {
            somatic_weight: 1.0,  // amplify for test
            temporal_weight: 1.0, // amplify for test
            access_frequency_weight: 0.5,
            ..Default::default()
        };

        let retriever = SomaticGraphRetriever::new(inner, somatic_index, config);

        let graph_config = GraphRetrievalConfig {
            max_depth: 2,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alpha", &graph_config, Sensitivity::Private)
            .unwrap();

        // With strong boosting, Gamma should rank above Beta.
        assert_eq!(
            result.related[0].entity_id, "c",
            "boosted gamma should rank first"
        );
    }

    #[test]
    fn somatic_retriever_without_markers_passes_through() {
        let store = graph_store_with_entities();
        let inner = BfsGraphRetriever::new(store);

        let somatic_index = SomaticIndex::new(); // empty
        let config = SomaticConfig::default();
        let retriever = SomaticGraphRetriever::new(inner, somatic_index, config);

        let graph_config = GraphRetrievalConfig {
            max_depth: 2,
            decay_factor: 0.7,
            min_score: 0.0,
            ..Default::default()
        };

        let result = retriever
            .retrieve("Alpha", &graph_config, Sensitivity::Private)
            .unwrap();

        // Scores should be unchanged from base BFS.
        let beta = result.related.iter().find(|n| n.entity_id == "b").unwrap();
        assert!((beta.score - 0.7).abs() < 1e-9);

        let gamma = result.related.iter().find(|n| n.entity_id == "c").unwrap();
        assert!((gamma.score - 0.49).abs() < 1e-9);
    }

    #[test]
    fn somatic_retriever_index_access() {
        let store = graph_store_with_entities();
        let inner = BfsGraphRetriever::new(store);

        let mut somatic_index = SomaticIndex::new();
        somatic_index
            .upsert(make_marker("a", EmotionalValence::Positive, 1000))
            .unwrap();

        let config = SomaticConfig::default();
        let mut retriever = SomaticGraphRetriever::new(inner, somatic_index, config);

        assert_eq!(retriever.index().len(), 1);
        retriever
            .index_mut()
            .upsert(make_marker("b", EmotionalValence::Negative, 2000))
            .unwrap();
        assert_eq!(retriever.index().len(), 2);
    }

    #[test]
    fn somatic_retriever_propagates_no_seeds_error() {
        let store = InMemoryGraphStore::new(); // empty
        let inner = BfsGraphRetriever::new(store);
        let somatic_index = SomaticIndex::new();
        let config = SomaticConfig::default();
        let retriever = SomaticGraphRetriever::new(inner, somatic_index, config);

        let graph_config = GraphRetrievalConfig::default();
        let result = retriever.retrieve("anything", &graph_config, Sensitivity::Private);
        assert!(matches!(result, Err(GraphRetrievalError::NoSeeds)));
    }

    // --- SqliteSomaticStore tests ---

    #[tokio::test]
    async fn sqlite_store_save_load_round_trip() {
        let store = SqliteSomaticStore::in_memory().unwrap();
        let marker = make_marker("entity-1", EmotionalValence::Positive, 1000);

        store.save_marker("entity-1", &marker).await.unwrap();
        let loaded = store.load_marker("entity-1").await.unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.target_id, "entity-1");
        assert_eq!(loaded.valence, EmotionalValence::Positive);
        assert!((loaded.intensity - 0.8).abs() < 1e-9);
        assert_eq!(loaded.event_time, 1000);
    }

    #[tokio::test]
    async fn sqlite_store_upsert_overwrites() {
        let store = SqliteSomaticStore::in_memory().unwrap();
        let m1 = make_marker("e1", EmotionalValence::Neutral, 1000);
        store.save_marker("e1", &m1).await.unwrap();

        let m2 = make_marker("e1", EmotionalValence::HighNegative, 2000);
        store.save_marker("e1", &m2).await.unwrap();

        let loaded = store.load_marker("e1").await.unwrap().unwrap();
        assert_eq!(loaded.valence, EmotionalValence::HighNegative);
        assert_eq!(loaded.event_time, 2000);
    }

    #[tokio::test]
    async fn sqlite_store_load_all() {
        let store = SqliteSomaticStore::in_memory().unwrap();
        store
            .save_marker("e1", &make_marker("e1", EmotionalValence::Positive, 1000))
            .await
            .unwrap();
        store
            .save_marker("e2", &make_marker("e2", EmotionalValence::Negative, 2000))
            .await
            .unwrap();

        let all = store.load_all().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn sqlite_store_record_access_increments() {
        let store = SqliteSomaticStore::in_memory().unwrap();
        let marker = make_marker("e1", EmotionalValence::Positive, 1000);
        store.save_marker("e1", &marker).await.unwrap();

        store.record_access("e1").await.unwrap();
        store.record_access("e1").await.unwrap();

        let loaded = store.load_marker("e1").await.unwrap().unwrap();
        assert_eq!(loaded.access_count, 2);
        assert!(loaded.last_accessed > 0);
    }

    #[tokio::test]
    async fn sqlite_store_record_access_missing_errors() {
        let store = SqliteSomaticStore::in_memory().unwrap();
        let result = store.record_access("ghost").await;
        assert!(matches!(result, Err(SomaticError::MarkerNotFound(_))));
    }

    #[tokio::test]
    async fn sqlite_store_prune_removes_old_entries() {
        let store = SqliteSomaticStore::in_memory().unwrap();

        // Old marker (event_time = 0, last_accessed = 0)
        let old = SomaticMarker {
            target_id: "old".to_string(),
            valence: EmotionalValence::Neutral,
            impact: ImpactCategory::Informational,
            intensity: 0.5,
            created_at: 0,
            event_time: 0,
            access_count: 0,
            last_accessed: 0,
        };
        store.save_marker("old", &old).await.unwrap();

        // Recent marker
        let now = symbiotic_core::now_unix();
        let recent = SomaticMarker {
            target_id: "recent".to_string(),
            valence: EmotionalValence::Positive,
            impact: ImpactCategory::GoalRelevant,
            intensity: 0.9,
            created_at: now,
            event_time: now,
            access_count: 1,
            last_accessed: now,
        };
        store.save_marker("recent", &recent).await.unwrap();

        // Prune entries older than 1 day
        let pruned = store.prune_older_than(1).await.unwrap();
        assert_eq!(pruned, 1);

        // Recent should still exist
        assert!(store.load_marker("recent").await.unwrap().is_some());
        // Old should be gone
        assert!(store.load_marker("old").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlite_store_load_nonexistent_returns_none() {
        let store = SqliteSomaticStore::in_memory().unwrap();
        let result = store.load_marker("does-not-exist").await.unwrap();
        assert!(result.is_none());
    }

    // --- SomaticIndex with store ---

    #[tokio::test]
    async fn index_with_store_upsert_persists() {
        let store = Arc::new(SqliteSomaticStore::in_memory().unwrap());
        let mut index = SomaticIndex::with_store(store.clone());

        let marker = make_marker("e1", EmotionalValence::Positive, 1000);
        index.upsert_async(marker).await.unwrap();

        // Should be in the in-memory cache
        assert_eq!(index.len(), 1);

        // Should also be in the store
        let loaded = store.load_marker("e1").await.unwrap();
        assert!(loaded.is_some());
    }

    #[tokio::test]
    async fn index_with_store_load_from_store() {
        let store = Arc::new(SqliteSomaticStore::in_memory().unwrap());

        // Seed the store directly
        store
            .save_marker("e1", &make_marker("e1", EmotionalValence::Positive, 1000))
            .await
            .unwrap();
        store
            .save_marker("e2", &make_marker("e2", EmotionalValence::Negative, 2000))
            .await
            .unwrap();

        // Create index and load
        let mut index = SomaticIndex::with_store(store);
        let count = index.load_from_store().await.unwrap();
        assert_eq!(count, 2);
        assert_eq!(index.len(), 2);
        assert!(index.get("e1").is_some());
        assert!(index.get("e2").is_some());
    }

    #[tokio::test]
    async fn index_with_store_record_access_persists() {
        let store = Arc::new(SqliteSomaticStore::in_memory().unwrap());
        let mut index = SomaticIndex::with_store(store.clone());

        let marker = make_marker("e1", EmotionalValence::Positive, 1000);
        index.upsert_async(marker).await.unwrap();

        let now = symbiotic_core::now_unix();
        index.record_access_async("e1", now).await.unwrap();

        // In-memory should have updated count
        assert_eq!(index.get("e1").unwrap().access_count, 1);

        // Store should also have updated count
        let loaded = store.load_marker("e1").await.unwrap().unwrap();
        assert_eq!(loaded.access_count, 1);
    }
}
