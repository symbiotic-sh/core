//! Temporal modeling for memory facts.
//!
//! Provides recency decay scoring, conflict detection, staleness detection,
//! and temporal filtering. See `docs/design/temporal-modeling.md` for the
//! full design specification.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for temporal scoring and staleness detection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TemporalConfig {
    /// Minimum days_ago value (prevents ln(0) and division by zero).
    pub min_days: f64,
    /// Weight multiplier for superseded facts (0.0 to 1.0).
    pub superseded_weight: f64,
    /// Weight multiplier for expired facts.
    pub expired_weight: f64,
    /// Number of days after which facts without valid_to are considered stale.
    pub staleness_threshold_days: u64,
}

impl Default for TemporalConfig {
    fn default() -> Self {
        Self {
            min_days: 1.0,
            superseded_weight: 0.3,
            expired_weight: 0.0,
            staleness_threshold_days: 365,
        }
    }
}

// ---------------------------------------------------------------------------
// Memory status
// ---------------------------------------------------------------------------

/// Lifecycle status of a memory fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Superseded,
    Expired,
    Stale,
    Archived,
}

// ---------------------------------------------------------------------------
// Scoring functions
// ---------------------------------------------------------------------------

/// Compute the recency boost for a fact.
///
/// Formula: `1.0 / (1.0 + ln(days_ago))` where `days_ago` is clamped to
/// at least `config.min_days` to avoid `ln(0)`.
pub fn recency_boost(days_ago: f64, config: &TemporalConfig) -> f64 {
    let clamped = days_ago.max(config.min_days);
    1.0 / (1.0 + clamped.ln())
}

/// Compute the status weight for a memory.
pub fn status_weight(status: MemoryStatus, config: &TemporalConfig) -> f64 {
    match status {
        MemoryStatus::Active => 1.0,
        MemoryStatus::Superseded => config.superseded_weight,
        MemoryStatus::Expired => config.expired_weight,
        MemoryStatus::Stale => config.superseded_weight, // reduced weight, same as superseded
        MemoryStatus::Archived => 0.0,
    }
}

/// Compute the combined retrieval score.
///
/// `final_score = relevance * recency_boost * confidence * status_weight`
pub fn temporal_score(
    relevance: f64,
    days_ago: f64,
    confidence: f64,
    status: MemoryStatus,
    config: &TemporalConfig,
) -> f64 {
    relevance * recency_boost(days_ago, config) * confidence * status_weight(status, config)
}

// ---------------------------------------------------------------------------
// Conflict detection
// ---------------------------------------------------------------------------

/// Resolution outcome for a temporal conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    /// New fact supersedes old (higher confidence).
    NewSupersedes,
    /// Old fact retained; new fact flagged for review (lower confidence).
    NewFlaggedForReview,
    /// Both flagged for user resolution (similar confidence).
    BothFlaggedForReview,
    /// User manually resolved.
    UserResolved,
}

/// A detected temporal conflict between two memory facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalConflict {
    pub entity_id: String,
    pub existing_memory_id: String,
    pub new_memory_id: String,
    pub existing_fact: String,
    pub new_fact: String,
    /// ISO 8601 start of the overlapping period.
    pub overlap_start: String,
    /// ISO 8601 end of the overlapping period (None if open-ended).
    pub overlap_end: Option<String>,
    pub resolution: ConflictResolution,
}

/// Confidence threshold within which two facts are considered "similar".
const CONFIDENCE_SIMILARITY_THRESHOLD: f64 = 0.1;

/// Determine the conflict resolution given two confidence values.
///
/// - If `new_confidence` exceeds `existing_confidence` by more than the
///   threshold, the new fact supersedes.
/// - If `existing_confidence` exceeds `new_confidence` by more than the
///   threshold, the new fact is flagged for review.
/// - Otherwise both are flagged for user resolution.
pub fn resolve_conflict(existing_confidence: f64, new_confidence: f64) -> ConflictResolution {
    let diff = new_confidence - existing_confidence;
    if diff > CONFIDENCE_SIMILARITY_THRESHOLD {
        ConflictResolution::NewSupersedes
    } else if diff < -CONFIDENCE_SIMILARITY_THRESHOLD {
        ConflictResolution::NewFlaggedForReview
    } else {
        ConflictResolution::BothFlaggedForReview
    }
}

/// Check whether two time ranges overlap.
///
/// A `None` end means "still active" (open-ended). Two ranges overlap when
/// `start_a < end_b AND start_b < end_a`, treating `None` as infinity.
pub fn ranges_overlap(
    start_a: &str,
    end_a: Option<&str>,
    start_b: &str,
    end_b: Option<&str>,
) -> bool {
    // start_a < end_b (if end_b is None, this is always true)
    let a_before_end_b = match end_b {
        Some(eb) => start_a < eb,
        None => true,
    };
    // start_b < end_a (if end_a is None, this is always true)
    let b_before_end_a = match end_a {
        Some(ea) => start_b < ea,
        None => true,
    };
    a_before_end_b && b_before_end_a
}

// ---------------------------------------------------------------------------
// Staleness detection
// ---------------------------------------------------------------------------

/// Check if a fact is stale based on the number of days since last observation.
///
/// A fact is stale if it has no `valid_to` and the days since `observed_at`
/// exceed `config.staleness_threshold_days`.
pub fn is_stale(days_since_observed: u64, has_valid_to: bool, config: &TemporalConfig) -> bool {
    !has_valid_to && days_since_observed > config.staleness_threshold_days
}

// ---------------------------------------------------------------------------
// Temporal embedding metadata
// ---------------------------------------------------------------------------

/// Temporal metadata stored alongside vector embeddings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalEmbeddingMetadata {
    /// When this fact became valid (ISO 8601).
    pub valid_from: String,
    /// When this fact expired (None = still active).
    pub valid_to: Option<String>,
    /// When this fact was observed/extracted (ISO 8601).
    pub observed_at: String,
    /// Current memory status.
    pub status: MemoryStatus,
}

/// Temporal filter applied before or after vector search.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemporalFilter {
    /// Only include facts valid at or after this time (ISO 8601).
    pub valid_after: Option<String>,
    /// Only include facts valid at or before this time (ISO 8601).
    pub valid_before: Option<String>,
    /// Only include facts observed within the last N days.
    pub observed_within_days: Option<u64>,
    /// Include superseded/expired facts (default: false).
    pub include_inactive: bool,
}

impl TemporalFilter {
    /// Check if a piece of temporal metadata passes this filter.
    pub fn matches(&self, meta: &TemporalEmbeddingMetadata) -> bool {
        // Status filter
        if !self.include_inactive
            && !matches!(meta.status, MemoryStatus::Active | MemoryStatus::Stale)
        {
            return false;
        }

        // valid_after: the fact's validity must extend past this point
        if let Some(ref after) = self.valid_after {
            // If valid_to exists and is before our cutoff, exclude
            if let Some(ref vt) = meta.valid_to {
                if vt.as_str() <= after.as_str() {
                    return false;
                }
            }
        }

        // valid_before: the fact must have started before this point
        if let Some(ref before) = self.valid_before {
            if meta.valid_from.as_str() > before.as_str() {
                return false;
            }
        }

        // observed_within_days: the fact must have been observed within N days of now
        if let Some(days) = self.observed_within_days {
            if let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) {
                let cutoff_secs = elapsed.as_secs().saturating_sub(days * 86400);
                let cutoff_iso = unix_to_iso8601(cutoff_secs);
                if meta.observed_at.as_str() < cutoff_iso.as_str() {
                    return false;
                }
            }
        }

        true
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a Unix timestamp (seconds since epoch) to an ISO 8601 string.
pub fn unix_to_iso8601(secs: u64) -> String {
    const SECS_PER_DAY: u64 = 86400;
    const DAYS_PER_400Y: u64 = 146097;

    let mut days = secs / SECS_PER_DAY;
    let day_secs = secs % SECS_PER_DAY;
    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let second = day_secs % 60;

    // Shift epoch from 1970-01-01 to 0000-03-01 for easier leap year math
    days += 719468;
    let era = days / DAYS_PER_400Y;
    let doe = days - era * DAYS_PER_400Y;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during temporal operations.
#[derive(Debug, Error)]
pub enum TemporalError {
    #[error("invalid time range: valid_from {from} is after valid_to {to}")]
    InvalidRange { from: String, to: String },
    #[error("conflict detected: {0}")]
    ConflictDetected(String),
    #[error("missing timestamp: {field} is required")]
    MissingTimestamp { field: String },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> TemporalConfig {
        TemporalConfig::default()
    }

    // -- Recency boost tests -----------------------------------------------

    #[test]
    fn recency_boost_at_one_day() {
        let config = default_config();
        let boost = recency_boost(1.0, &config);
        // 1/(1+ln(1)) = 1/(1+0) = 1.0
        assert!((boost - 1.0).abs() < 1e-6, "boost at 1 day: {boost}");
    }

    #[test]
    fn recency_boost_at_two_days() {
        let config = default_config();
        let boost = recency_boost(2.0, &config);
        // 1/(1+ln(2)) = 1/(1+0.6931) ≈ 0.5906
        assert!((boost - 0.591).abs() < 0.01, "boost at 2 days: {boost}");
    }

    #[test]
    fn recency_boost_at_seven_days() {
        let config = default_config();
        let boost = recency_boost(7.0, &config);
        // 1/(1+ln(7)) ≈ 1/(1+1.9459) ≈ 0.339
        assert!((boost - 0.339).abs() < 0.01, "boost at 7 days: {boost}");
    }

    #[test]
    fn recency_boost_at_thirty_days() {
        let config = default_config();
        let boost = recency_boost(30.0, &config);
        // 1/(1+ln(30)) ≈ 1/(1+3.4012) ≈ 0.227
        assert!((boost - 0.227).abs() < 0.01, "boost at 30 days: {boost}");
    }

    #[test]
    fn recency_boost_at_ninety_days() {
        let config = default_config();
        let boost = recency_boost(90.0, &config);
        // 1/(1+ln(90)) ≈ 1/(1+4.4998) ≈ 0.182
        assert!((boost - 0.182).abs() < 0.01, "boost at 90 days: {boost}");
    }

    #[test]
    fn recency_boost_at_365_days() {
        let config = default_config();
        let boost = recency_boost(365.0, &config);
        // 1/(1+ln(365)) ≈ 1/(1+5.8998) ≈ 0.145
        assert!((boost - 0.145).abs() < 0.01, "boost at 365 days: {boost}");
    }

    #[test]
    fn recency_boost_clamps_zero_days_to_min() {
        let config = default_config();
        // 0 days should be clamped to min_days (1.0)
        let boost = recency_boost(0.0, &config);
        assert!((boost - 1.0).abs() < 1e-6, "boost at 0 days: {boost}");
    }

    #[test]
    fn recency_boost_negative_days_clamped() {
        let config = default_config();
        let boost = recency_boost(-5.0, &config);
        assert!((boost - 1.0).abs() < 1e-6, "boost at -5 days: {boost}");
    }

    #[test]
    fn recency_boost_very_large_days() {
        let config = default_config();
        let boost = recency_boost(10_000.0, &config);
        // Should still be positive and small
        assert!(boost > 0.0);
        assert!(boost < 0.15);
    }

    // -- Status weight tests -----------------------------------------------

    #[test]
    fn status_weight_active() {
        let config = default_config();
        assert!((status_weight(MemoryStatus::Active, &config) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn status_weight_superseded() {
        let config = default_config();
        assert!((status_weight(MemoryStatus::Superseded, &config) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn status_weight_expired() {
        let config = default_config();
        assert!((status_weight(MemoryStatus::Expired, &config) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn status_weight_archived() {
        let config = default_config();
        assert!((status_weight(MemoryStatus::Archived, &config) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn status_weight_stale() {
        let config = default_config();
        // Stale uses same weight as superseded
        assert!((status_weight(MemoryStatus::Stale, &config) - 0.3).abs() < 1e-6);
    }

    // -- Combined score tests ----------------------------------------------

    #[test]
    fn temporal_score_active_recent_high_confidence() {
        let config = default_config();
        let score = temporal_score(0.9, 1.0, 0.95, MemoryStatus::Active, &config);
        // 0.9 * 1.0 * 0.95 * 1.0 = 0.855
        assert!((score - 0.855).abs() < 1e-6, "score: {score}");
    }

    #[test]
    fn temporal_score_superseded_old() {
        let config = default_config();
        let score = temporal_score(0.8, 365.0, 0.7, MemoryStatus::Superseded, &config);
        // 0.8 * recency_boost(365) * 0.7 * 0.3
        let expected = 0.8 * recency_boost(365.0, &config) * 0.7 * 0.3;
        assert!((score - expected).abs() < 1e-6, "score: {score}");
    }

    #[test]
    fn temporal_score_expired_is_zero() {
        let config = default_config();
        let score = temporal_score(1.0, 1.0, 1.0, MemoryStatus::Expired, &config);
        assert!((score - 0.0).abs() < 1e-6, "score: {score}");
    }

    #[test]
    fn temporal_score_archived_is_zero() {
        let config = default_config();
        let score = temporal_score(1.0, 1.0, 1.0, MemoryStatus::Archived, &config);
        assert!((score - 0.0).abs() < 1e-6, "score: {score}");
    }

    #[test]
    fn temporal_score_zero_relevance() {
        let config = default_config();
        let score = temporal_score(0.0, 1.0, 1.0, MemoryStatus::Active, &config);
        assert!((score - 0.0).abs() < 1e-6);
    }

    #[test]
    fn temporal_score_zero_confidence() {
        let config = default_config();
        let score = temporal_score(1.0, 1.0, 0.0, MemoryStatus::Active, &config);
        assert!((score - 0.0).abs() < 1e-6);
    }

    // -- Conflict resolution tests -----------------------------------------

    #[test]
    fn resolve_conflict_new_higher_confidence() {
        let resolution = resolve_conflict(0.5, 0.8);
        assert_eq!(resolution, ConflictResolution::NewSupersedes);
    }

    #[test]
    fn resolve_conflict_existing_higher_confidence() {
        let resolution = resolve_conflict(0.9, 0.5);
        assert_eq!(resolution, ConflictResolution::NewFlaggedForReview);
    }

    #[test]
    fn resolve_conflict_similar_confidence() {
        let resolution = resolve_conflict(0.75, 0.8);
        assert_eq!(resolution, ConflictResolution::BothFlaggedForReview);
    }

    #[test]
    fn resolve_conflict_exact_same_confidence() {
        let resolution = resolve_conflict(0.5, 0.5);
        assert_eq!(resolution, ConflictResolution::BothFlaggedForReview);
    }

    #[test]
    fn resolve_conflict_boundary_above_threshold() {
        // diff = 0.11 > 0.1 threshold
        let resolution = resolve_conflict(0.5, 0.61);
        assert_eq!(resolution, ConflictResolution::NewSupersedes);
    }

    #[test]
    fn resolve_conflict_boundary_at_threshold() {
        // diff = 0.1 exactly, not > threshold
        let resolution = resolve_conflict(0.5, 0.6);
        assert_eq!(resolution, ConflictResolution::BothFlaggedForReview);
    }

    // -- Range overlap tests -----------------------------------------------

    #[test]
    fn ranges_overlap_both_open_ended() {
        assert!(ranges_overlap("2024-01-01", None, "2024-06-01", None));
    }

    #[test]
    fn ranges_overlap_overlapping_closed_ranges() {
        assert!(ranges_overlap(
            "2024-01-01",
            Some("2024-06-01"),
            "2024-03-01",
            Some("2024-09-01"),
        ));
    }

    #[test]
    fn ranges_no_overlap_sequential() {
        assert!(!ranges_overlap(
            "2024-01-01",
            Some("2024-03-01"),
            "2024-06-01",
            Some("2024-09-01"),
        ));
    }

    #[test]
    fn ranges_overlap_one_open_ended() {
        assert!(ranges_overlap(
            "2024-01-01",
            None,
            "2024-06-01",
            Some("2024-09-01"),
        ));
    }

    #[test]
    fn ranges_no_overlap_touching_boundary() {
        // end_a == start_b means no overlap (start_b < end_a is false when equal)
        assert!(!ranges_overlap(
            "2024-01-01",
            Some("2024-06-01"),
            "2024-06-01",
            Some("2024-09-01"),
        ));
    }

    // -- Staleness tests ---------------------------------------------------

    #[test]
    fn staleness_detected_no_valid_to_old() {
        let config = default_config();
        assert!(is_stale(400, false, &config));
    }

    #[test]
    fn staleness_not_detected_with_valid_to() {
        let config = default_config();
        assert!(!is_stale(400, true, &config));
    }

    #[test]
    fn staleness_not_detected_recent() {
        let config = default_config();
        assert!(!is_stale(100, false, &config));
    }

    #[test]
    fn staleness_boundary_at_threshold() {
        let config = default_config();
        // Exactly at threshold: 365 days, not > 365
        assert!(!is_stale(365, false, &config));
    }

    #[test]
    fn staleness_boundary_one_past_threshold() {
        let config = default_config();
        assert!(is_stale(366, false, &config));
    }

    #[test]
    fn staleness_custom_threshold() {
        let config = TemporalConfig {
            staleness_threshold_days: 30,
            ..default_config()
        };
        assert!(is_stale(31, false, &config));
        assert!(!is_stale(30, false, &config));
    }

    // -- Temporal filter tests ---------------------------------------------

    #[test]
    fn filter_default_allows_active() {
        let filter = TemporalFilter::default();
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-01-01".to_string(),
            valid_to: None,
            observed_at: "2024-01-01".to_string(),
            status: MemoryStatus::Active,
        };
        assert!(filter.matches(&meta));
    }

    #[test]
    fn filter_default_rejects_expired() {
        let filter = TemporalFilter::default();
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-01-01".to_string(),
            valid_to: Some("2024-06-01".to_string()),
            observed_at: "2024-01-01".to_string(),
            status: MemoryStatus::Expired,
        };
        assert!(!filter.matches(&meta));
    }

    #[test]
    fn filter_include_inactive_allows_expired() {
        let filter = TemporalFilter {
            include_inactive: true,
            ..Default::default()
        };
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-01-01".to_string(),
            valid_to: Some("2024-06-01".to_string()),
            observed_at: "2024-01-01".to_string(),
            status: MemoryStatus::Expired,
        };
        assert!(filter.matches(&meta));
    }

    #[test]
    fn filter_valid_after_excludes_old_facts() {
        let filter = TemporalFilter {
            valid_after: Some("2024-06-01".to_string()),
            ..Default::default()
        };
        // Fact that expired before the cutoff
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-01-01".to_string(),
            valid_to: Some("2024-03-01".to_string()),
            observed_at: "2024-01-01".to_string(),
            status: MemoryStatus::Active,
        };
        assert!(!filter.matches(&meta));
    }

    #[test]
    fn filter_valid_after_allows_open_ended() {
        let filter = TemporalFilter {
            valid_after: Some("2024-06-01".to_string()),
            ..Default::default()
        };
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-01-01".to_string(),
            valid_to: None,
            observed_at: "2024-01-01".to_string(),
            status: MemoryStatus::Active,
        };
        assert!(filter.matches(&meta));
    }

    #[test]
    fn filter_valid_before_excludes_future_facts() {
        let filter = TemporalFilter {
            valid_before: Some("2024-01-01".to_string()),
            ..Default::default()
        };
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-06-01".to_string(),
            valid_to: None,
            observed_at: "2024-06-01".to_string(),
            status: MemoryStatus::Active,
        };
        assert!(!filter.matches(&meta));
    }

    #[test]
    fn filter_valid_before_allows_past_facts() {
        let filter = TemporalFilter {
            valid_before: Some("2024-06-01".to_string()),
            ..Default::default()
        };
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-01-01".to_string(),
            valid_to: None,
            observed_at: "2024-01-01".to_string(),
            status: MemoryStatus::Active,
        };
        assert!(filter.matches(&meta));
    }

    #[test]
    fn filter_allows_stale_status() {
        let filter = TemporalFilter::default();
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2023-01-01".to_string(),
            valid_to: None,
            observed_at: "2023-01-01".to_string(),
            status: MemoryStatus::Stale,
        };
        assert!(filter.matches(&meta));
    }

    #[test]
    fn filter_rejects_superseded_by_default() {
        let filter = TemporalFilter::default();
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-01-01".to_string(),
            valid_to: None,
            observed_at: "2024-01-01".to_string(),
            status: MemoryStatus::Superseded,
        };
        assert!(!filter.matches(&meta));
    }

    // -- Serialization tests -----------------------------------------------

    #[test]
    fn temporal_config_serde_roundtrip() {
        let config = TemporalConfig::default();
        let json = serde_json::to_string(&config).expect("serialize");
        let parsed: TemporalConfig = serde_json::from_str(&json).expect("deserialize");
        assert!((parsed.min_days - config.min_days).abs() < 1e-6);
        assert_eq!(
            parsed.staleness_threshold_days,
            config.staleness_threshold_days
        );
    }

    #[test]
    fn memory_status_serde_roundtrip() {
        let variants = [
            MemoryStatus::Active,
            MemoryStatus::Superseded,
            MemoryStatus::Expired,
            MemoryStatus::Stale,
            MemoryStatus::Archived,
        ];
        for variant in &variants {
            let json = serde_json::to_string(variant).expect("serialize");
            let parsed: MemoryStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&parsed, variant);
        }
    }

    #[test]
    fn conflict_resolution_serde_roundtrip() {
        let variants = [
            ConflictResolution::NewSupersedes,
            ConflictResolution::NewFlaggedForReview,
            ConflictResolution::BothFlaggedForReview,
            ConflictResolution::UserResolved,
        ];
        for variant in &variants {
            let json = serde_json::to_string(variant).expect("serialize");
            let parsed: ConflictResolution = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&parsed, variant);
        }
    }

    #[test]
    fn temporal_conflict_serde_roundtrip() {
        let conflict = TemporalConflict {
            entity_id: "person-42".to_string(),
            existing_memory_id: "mem-1".to_string(),
            new_memory_id: "mem-2".to_string(),
            existing_fact: "Lives in NYC".to_string(),
            new_fact: "Lives in LA".to_string(),
            overlap_start: "2024-01-01".to_string(),
            overlap_end: None,
            resolution: ConflictResolution::NewSupersedes,
        };
        let json = serde_json::to_string(&conflict).expect("serialize");
        let parsed: TemporalConflict = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.entity_id, "person-42");
        assert_eq!(parsed.resolution, ConflictResolution::NewSupersedes);
    }

    #[test]
    fn temporal_embedding_metadata_serde_roundtrip() {
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2024-01-01T00:00:00Z".to_string(),
            valid_to: Some("2024-12-31T23:59:59Z".to_string()),
            observed_at: "2024-01-15T10:30:00Z".to_string(),
            status: MemoryStatus::Active,
        };
        let json = serde_json::to_string(&meta).expect("serialize");
        let parsed: TemporalEmbeddingMetadata = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.valid_from, meta.valid_from);
        assert_eq!(parsed.valid_to, meta.valid_to);
        assert_eq!(parsed.observed_at, meta.observed_at);
        assert_eq!(parsed.status, MemoryStatus::Active);
    }

    #[test]
    fn temporal_filter_serde_roundtrip() {
        let filter = TemporalFilter {
            valid_after: Some("2024-01-01".to_string()),
            valid_before: Some("2024-12-31".to_string()),
            observed_within_days: Some(30),
            include_inactive: true,
        };
        let json = serde_json::to_string(&filter).expect("serialize");
        let parsed: TemporalFilter = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.valid_after, filter.valid_after);
        assert!(parsed.include_inactive);
    }

    #[test]
    fn temporal_error_display() {
        let err = TemporalError::InvalidRange {
            from: "2024-12-01".to_string(),
            to: "2024-01-01".to_string(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("invalid time range"));
        assert!(msg.contains("2024-12-01"));

        let err = TemporalError::MissingTimestamp {
            field: "valid_from".to_string(),
        };
        assert!(format!("{err}").contains("valid_from"));
    }

    // -- unix_to_iso8601 tests ------------------------------------------------

    #[test]
    fn unix_to_iso8601_epoch() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn unix_to_iso8601_known_date() {
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(unix_to_iso8601(1704067200), "2024-01-01T00:00:00Z");
    }

    // -- observed_within_days filter tests ------------------------------------

    #[test]
    fn filter_observed_within_days_rejects_old() {
        let filter = TemporalFilter {
            observed_within_days: Some(30),
            ..Default::default()
        };
        // Observed far in the past
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2020-01-01T00:00:00Z".to_string(),
            valid_to: None,
            observed_at: "2020-01-01T00:00:00Z".to_string(),
            status: MemoryStatus::Active,
        };
        assert!(!filter.matches(&meta));
    }

    #[test]
    fn filter_observed_within_days_allows_recent() {
        let filter = TemporalFilter {
            observed_within_days: Some(30),
            ..Default::default()
        };
        // Use current time as observed_at
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let meta = TemporalEmbeddingMetadata {
            valid_from: unix_to_iso8601(now_secs),
            valid_to: None,
            observed_at: unix_to_iso8601(now_secs),
            status: MemoryStatus::Active,
        };
        assert!(filter.matches(&meta));
    }

    #[test]
    fn filter_observed_within_days_none_skips_check() {
        let filter = TemporalFilter {
            observed_within_days: None,
            ..Default::default()
        };
        // Old observation should still pass if check is not enabled
        let meta = TemporalEmbeddingMetadata {
            valid_from: "2020-01-01T00:00:00Z".to_string(),
            valid_to: None,
            observed_at: "2020-01-01T00:00:00Z".to_string(),
            status: MemoryStatus::Active,
        };
        assert!(filter.matches(&meta));
    }
}
