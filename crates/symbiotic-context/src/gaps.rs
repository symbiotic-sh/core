//! Query gap tracking for the Recall Gateway.
//!
//! Tracks queries that return empty results. After 3+ misses on similar queries,
//! surfaces a gap signal so the system can proactively fill the knowledge gap.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use symbiotic_core::now_unix;

/// Minimum number of misses before a query pattern is considered a gap.
const GAP_THRESHOLD: u32 = 3;

/// A query pattern that has consistently returned empty results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryGap {
    /// Normalized query pattern (lowercased, sorted terms).
    pub query_pattern: String,
    /// Number of times this pattern missed.
    pub miss_count: u32,
    /// ISO 8601 timestamp of the first miss.
    pub first_miss: String,
    /// ISO 8601 timestamp of the most recent miss.
    pub last_miss: String,
}

/// In-memory tracker for query gaps.
///
/// Normalizes queries (lowercase, sorted terms) to group similar queries together,
/// then counts misses per pattern.
#[derive(Debug, Default)]
pub struct QueryGapTracker {
    /// Map from normalized query pattern to miss metadata.
    misses: HashMap<String, GapEntry>,
}

#[derive(Debug, Clone)]
struct GapEntry {
    miss_count: u32,
    first_miss_unix: u64,
    last_miss_unix: u64,
}

impl QueryGapTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Log a query that returned empty results.
    ///
    /// The query is normalized (lowercased, whitespace-split, sorted) so that
    /// "rust daemon" and "daemon rust" are grouped as the same pattern.
    pub fn log_empty_query(&mut self, query: &str) {
        let pattern = normalize_query(query);
        if pattern.is_empty() {
            return;
        }

        let now = now_unix();
        self.misses
            .entry(pattern)
            .and_modify(|entry| {
                entry.miss_count += 1;
                entry.last_miss_unix = now;
            })
            .or_insert(GapEntry {
                miss_count: 1,
                first_miss_unix: now,
                last_miss_unix: now,
            });
    }

    /// Returns queries that have missed 3+ times.
    pub fn get_query_gaps(&self) -> Vec<QueryGap> {
        self.misses
            .iter()
            .filter(|(_, entry)| entry.miss_count >= GAP_THRESHOLD)
            .map(|(pattern, entry)| QueryGap {
                query_pattern: pattern.clone(),
                miss_count: entry.miss_count,
                first_miss: unix_to_iso8601(entry.first_miss_unix),
                last_miss: unix_to_iso8601(entry.last_miss_unix),
            })
            .collect()
    }

    /// Returns the miss count for a given query pattern.
    pub fn miss_count(&self, query: &str) -> u32 {
        let pattern = normalize_query(query);
        self.misses.get(&pattern).map(|e| e.miss_count).unwrap_or(0)
    }

    /// Clears all tracked gaps (e.g., after a bulk knowledge import).
    pub fn clear(&mut self) {
        self.misses.clear();
    }
}

/// Normalizes a query for gap tracking: lowercase, split on whitespace, sort, rejoin.
fn normalize_query(query: &str) -> String {
    let mut terms: Vec<String> = query
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect();
    terms.sort();
    terms.dedup();
    terms.join(" ")
}

/// Converts a unix timestamp to an ISO 8601 string.
fn unix_to_iso8601(ts: u64) -> String {
    // Simple conversion — doesn't require chrono for this basic format.
    // Format: YYYY-MM-DDTHH:MM:SSZ
    let secs = ts;
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Simple days-since-epoch to date conversion
    // Based on the algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_query_lowercases_and_sorts() {
        assert_eq!(normalize_query("Rust Daemon"), "daemon rust");
        assert_eq!(normalize_query("daemon rust"), "daemon rust");
        assert_eq!(normalize_query("  HELLO   world  "), "hello world");
    }

    #[test]
    fn normalize_query_deduplicates() {
        assert_eq!(normalize_query("rust rust daemon"), "daemon rust");
    }

    #[test]
    fn normalize_query_strips_punctuation() {
        assert_eq!(normalize_query("rust, daemon!"), "daemon rust");
    }

    #[test]
    fn normalize_empty_query() {
        assert_eq!(normalize_query(""), "");
        assert_eq!(normalize_query("   "), "");
    }

    #[test]
    fn no_gaps_initially() {
        let tracker = QueryGapTracker::new();
        assert!(tracker.get_query_gaps().is_empty());
    }

    #[test]
    fn single_miss_not_a_gap() {
        let mut tracker = QueryGapTracker::new();
        tracker.log_empty_query("rust memory");
        assert!(tracker.get_query_gaps().is_empty());
        assert_eq!(tracker.miss_count("rust memory"), 1);
    }

    #[test]
    fn two_misses_not_a_gap() {
        let mut tracker = QueryGapTracker::new();
        tracker.log_empty_query("rust memory");
        tracker.log_empty_query("memory rust"); // same normalized pattern
        assert!(tracker.get_query_gaps().is_empty());
        assert_eq!(tracker.miss_count("rust memory"), 2);
    }

    #[test]
    fn three_misses_becomes_a_gap() {
        let mut tracker = QueryGapTracker::new();
        tracker.log_empty_query("rust memory");
        tracker.log_empty_query("memory rust");
        tracker.log_empty_query("RUST Memory");

        let gaps = tracker.get_query_gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].query_pattern, "memory rust");
        assert_eq!(gaps[0].miss_count, 3);
        assert!(!gaps[0].first_miss.is_empty());
        assert!(!gaps[0].last_miss.is_empty());
    }

    #[test]
    fn different_queries_tracked_separately() {
        let mut tracker = QueryGapTracker::new();
        tracker.log_empty_query("rust memory");
        tracker.log_empty_query("rust memory");
        tracker.log_empty_query("flutter widgets");
        tracker.log_empty_query("flutter widgets");
        tracker.log_empty_query("flutter widgets");

        let gaps = tracker.get_query_gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].query_pattern, "flutter widgets");
    }

    #[test]
    fn clear_removes_all_gaps() {
        let mut tracker = QueryGapTracker::new();
        for _ in 0..5 {
            tracker.log_empty_query("missing topic");
        }
        assert_eq!(tracker.get_query_gaps().len(), 1);

        tracker.clear();
        assert!(tracker.get_query_gaps().is_empty());
        assert_eq!(tracker.miss_count("missing topic"), 0);
    }

    #[test]
    fn empty_query_ignored() {
        let mut tracker = QueryGapTracker::new();
        tracker.log_empty_query("");
        tracker.log_empty_query("   ");
        assert!(tracker.get_query_gaps().is_empty());
    }

    #[test]
    fn iso8601_format_valid() {
        let ts = 1710000000u64; // Some unix timestamp
        let iso = unix_to_iso8601(ts);
        // Should match YYYY-MM-DDTHH:MM:SSZ pattern
        assert!(iso.ends_with('Z'));
        assert_eq!(iso.len(), 20);
        assert_eq!(&iso[4..5], "-");
        assert_eq!(&iso[7..8], "-");
        assert_eq!(&iso[10..11], "T");
        assert_eq!(&iso[13..14], ":");
        assert_eq!(&iso[16..17], ":");
    }

    #[test]
    fn miss_count_for_unknown_query() {
        let tracker = QueryGapTracker::new();
        assert_eq!(tracker.miss_count("never asked"), 0);
    }
}
