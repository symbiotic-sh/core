//! Conflict detection for the Reweave stage of the distillery pipeline.
//!
//! Detects contradictions when new claims conflict with existing Archive
//! entries. When timestamps are available, newer claims automatically
//! supersede older ones (temporal resolution). Otherwise, conflicts are
//! flagged for human review.
//! See `docs/design/distillery-pipeline.md` §Conflict Resolution Strategy.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::distillery::{AtomicClaim, ProposedLink, VerifiedGraph};

// ---------------------------------------------------------------------------
// Conflict types
// ---------------------------------------------------------------------------

/// Outcome of temporal conflict resolution.
///
/// When two claims conflict, the system attempts to resolve them using
/// timestamps before falling back to flagging for human review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemporalResolution {
    /// The new claim is newer than the existing note and automatically wins.
    NewerClaimWins,
    /// Only the new claim has a timestamp; it wins by default because
    /// timestamped evidence is stronger than undated content.
    TimestampedClaimWins,
    /// Cannot auto-resolve: no timestamps, equal timestamps, or the old
    /// note is newer. Flagged for human review.
    FlaggedForReview,
}

impl fmt::Display for TemporalResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemporalResolution::NewerClaimWins => write!(f, "newer_claim_wins"),
            TemporalResolution::TimestampedClaimWins => write!(f, "timestamped_claim_wins"),
            TemporalResolution::FlaggedForReview => write!(f, "flagged_for_review"),
        }
    }
}

/// A detected conflict between a new claim and existing note content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    /// The new claim that conflicts with existing content.
    pub claim_content: String,
    /// The index of the claim in the pipeline's claims array.
    pub claim_idx: usize,
    /// The target note that is contradicted.
    pub target_node_id: String,
    /// The relationship type (typically "contradicts").
    pub relationship: String,
    /// A snippet of the old note content that is contradicted, if available.
    pub old_content_snippet: Option<String>,
    /// The conflict type.
    pub conflict_type: ConflictType,
    /// Result of temporal conflict resolution.
    pub resolution: TemporalResolution,
}

/// Classification of the conflict type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictType {
    /// New claim directly contradicts existing note content.
    ClaimVsNote,
    /// Cross-space conflict: knowledge contradicts self/preference.
    CrossSpace,
}

impl fmt::Display for ConflictType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConflictType::ClaimVsNote => write!(f, "claim_vs_note"),
            ConflictType::CrossSpace => write!(f, "cross_space"),
        }
    }
}

/// Summary of conflicts detected during a pipeline run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConflictReport {
    /// All detected conflicts.
    pub conflicts: Vec<Conflict>,
    /// Number of notes flagged for human review (excludes auto-resolved).
    pub notes_flagged_for_review: usize,
    /// Number of conflicts that were auto-resolved via temporal resolution.
    pub auto_resolved: usize,
}

impl ConflictReport {
    /// Returns true if any conflicts were detected (including auto-resolved).
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Returns conflicts for a specific target note.
    pub fn conflicts_for_target(&self, target_id: &str) -> Vec<&Conflict> {
        self.conflicts
            .iter()
            .filter(|c| c.target_node_id == target_id)
            .collect()
    }

    /// Returns only conflicts that need human review (not auto-resolved).
    pub fn unresolved_conflicts(&self) -> Vec<&Conflict> {
        self.conflicts
            .iter()
            .filter(|c| c.resolution == TemporalResolution::FlaggedForReview)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Temporal resolution
// ---------------------------------------------------------------------------

/// Resolve a conflict temporally using claim and note timestamps.
///
/// Resolution rules (in order):
/// 1. Both timestamps present, new is strictly newer -> `NewerClaimWins`
/// 2. Both timestamps present, equal or old is newer -> `FlaggedForReview`
/// 3. Only new claim has a timestamp -> `TimestampedClaimWins`
/// 4. Only old note has a timestamp -> `FlaggedForReview`
/// 5. Neither has a timestamp -> `FlaggedForReview` (backward compat)
pub fn resolve_temporally(
    claim_timestamp: Option<&str>,
    note_timestamp: Option<&str>,
) -> TemporalResolution {
    match (claim_timestamp, note_timestamp) {
        (Some(new_ts), Some(old_ts)) => {
            if new_ts > old_ts {
                TemporalResolution::NewerClaimWins
            } else {
                TemporalResolution::FlaggedForReview
            }
        }
        (Some(_), None) => TemporalResolution::TimestampedClaimWins,
        (None, Some(_)) => TemporalResolution::FlaggedForReview,
        (None, None) => TemporalResolution::FlaggedForReview,
    }
}

// ---------------------------------------------------------------------------
// Conflict detector
// ---------------------------------------------------------------------------

/// Detects conflicts between new claims and existing Archive entries.
///
/// Scans the verified graph for contradicting relationships and reads the
/// target note content to produce a detailed conflict report. When timestamps
/// are available, applies temporal resolution to auto-resolve conflicts
/// where the newer claim should win.
pub struct ConflictDetector<'a> {
    kb_root: &'a Path,
}

impl<'a> ConflictDetector<'a> {
    /// Create a new conflict detector rooted at the given knowledge base path.
    pub fn new(kb_root: &'a Path) -> Self {
        Self { kb_root }
    }

    /// Detect all conflicts in the verified graph.
    ///
    /// Examines proposed links with `relationship == "contradicts"` and reads
    /// the target note to extract a snippet of the conflicting content.
    /// Applies temporal resolution when timestamps are available.
    pub fn detect(&self, graph: &VerifiedGraph) -> ConflictReport {
        let mut conflicts = Vec::new();
        let mut flagged_notes = std::collections::HashSet::new();
        let mut auto_resolved = 0usize;

        for link in &graph.proposed_links {
            if link.relationship != "contradicts" {
                continue;
            }

            let claim = match graph.claims.get(link.source_claim_idx) {
                Some(c) => c,
                None => continue,
            };

            let old_content_snippet = self.read_note_snippet(link);

            // Determine conflict type: cross-space if target space differs
            // from Knowledge (the default for factual claims)
            let conflict_type = if link.target_space != symbiotic_core::MemorySpace::Knowledge {
                ConflictType::CrossSpace
            } else {
                ConflictType::ClaimVsNote
            };

            // Temporal resolution: compare claim timestamp against note timestamp
            let note_timestamp = self.read_note_timestamp(link);
            let resolution =
                resolve_temporally(claim.observed_at.as_deref(), note_timestamp.as_deref());

            if resolution != TemporalResolution::FlaggedForReview {
                auto_resolved += 1;
            } else {
                flagged_notes.insert(link.target_node_id.clone());
            }

            conflicts.push(Conflict {
                claim_content: claim.content.clone(),
                claim_idx: link.source_claim_idx,
                target_node_id: link.target_node_id.clone(),
                relationship: link.relationship.clone(),
                old_content_snippet,
                conflict_type,
                resolution,
            });
        }

        ConflictReport {
            conflicts,
            notes_flagged_for_review: flagged_notes.len(),
            auto_resolved,
        }
    }

    /// Generate a conflict HTML comment for embedding in rewritten notes.
    ///
    /// Returns a string like:
    /// `<!-- conflict: new="New claim" contradicts existing content -->`
    pub fn conflict_comment(conflict: &Conflict) -> String {
        let resolution_tag = match conflict.resolution {
            TemporalResolution::NewerClaimWins => " [auto-resolved: newer]",
            TemporalResolution::TimestampedClaimWins => " [auto-resolved: timestamped]",
            TemporalResolution::FlaggedForReview => "",
        };
        match &conflict.old_content_snippet {
            Some(old) => format!(
                "<!-- conflict: new=\"{}\" vs old=\"{}\" ({}){} -->",
                conflict.claim_content, old, conflict.conflict_type, resolution_tag
            ),
            None => format!(
                "<!-- conflict: new=\"{}\" contradicts existing content ({}){} -->",
                conflict.claim_content, conflict.conflict_type, resolution_tag
            ),
        }
    }

    /// Read the first non-empty line (after any heading) from the target note
    /// as a representative snippet of what is being contradicted.
    fn read_note_snippet(&self, link: &ProposedLink) -> Option<String> {
        let note_path = link
            .target_space
            .path(self.kb_root)
            .join(format!("{}.md", link.target_node_id));

        let content = std::fs::read_to_string(&note_path).ok()?;

        // Find first substantive line (skip headings and empty lines)
        content
            .lines()
            .find(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            })
            .map(|line| {
                let trimmed = line.trim();
                if trimmed.len() > 120 {
                    format!("{}...", &trimmed[..120])
                } else {
                    trimmed.to_string()
                }
            })
    }

    /// Extract the `ingested_at` timestamp from a note's YAML frontmatter.
    ///
    /// Looks for a line matching `ingested_at: "..."` within a `---` delimited
    /// frontmatter block. Returns `None` if the note has no frontmatter or
    /// no `ingested_at` field.
    fn read_note_timestamp(&self, link: &ProposedLink) -> Option<String> {
        let note_path = link
            .target_space
            .path(self.kb_root)
            .join(format!("{}.md", link.target_node_id));

        let content = std::fs::read_to_string(&note_path).ok()?;

        // Simple frontmatter parser: look for ingested_at between --- delimiters
        let mut in_frontmatter = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed == "---" {
                if in_frontmatter {
                    // End of frontmatter, stop looking
                    break;
                }
                in_frontmatter = true;
                continue;
            }
            if in_frontmatter {
                if let Some(value) = trimmed.strip_prefix("ingested_at:") {
                    let ts = value.trim().trim_matches('"').trim().to_string();
                    if !ts.is_empty() {
                        return Some(ts);
                    }
                }
            }
        }
        None
    }
}

/// Detect conflicts in a verified graph (convenience function).
pub fn detect_conflicts(graph: &VerifiedGraph, kb_root: &Path) -> ConflictReport {
    ConflictDetector::new(kb_root).detect(graph)
}

/// Check if any proposed links for a target note are contradictions.
///
/// Returns the conflicting claim contents for use in `ReweaveStatus`.
pub fn detect_conflicts_for_target(
    _target_id: &str,
    links: &[&ProposedLink],
    claims: &[AtomicClaim],
) -> Vec<String> {
    links
        .iter()
        .filter(|link| link.relationship == "contradicts")
        .filter_map(|link| claims.get(link.source_claim_idx))
        .map(|claim| claim.content.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distillery::{AtomicClaim, ProposedLink, VerifiedGraph};
    use symbiotic_core::MemorySpace;

    fn make_claim(content: &str, score: u8) -> AtomicClaim {
        AtomicClaim {
            content: content.to_string(),
            impact_score: score,
            source_ref: "test-src".to_string(),
            observed_at: None,
        }
    }

    fn make_claim_with_timestamp(content: &str, score: u8, ts: &str) -> AtomicClaim {
        AtomicClaim {
            content: content.to_string(),
            impact_score: score,
            source_ref: "test-src".to_string(),
            observed_at: Some(ts.to_string()),
        }
    }

    fn make_link(idx: usize, target: &str, rel: &str, space: MemorySpace) -> ProposedLink {
        ProposedLink {
            source_claim_idx: idx,
            target_node_id: target.to_string(),
            relationship: rel.to_string(),
            target_space: space,
        }
    }

    // -- Existing tests (updated for new resolution field) --------------------

    #[test]
    fn detects_contradictions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("rust-safety.md"),
            "# Rust Safety\n\nRust prevents all memory bugs at compile time.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim("Rust does not prevent logic errors", 7)],
            proposed_links: vec![make_link(
                0,
                "rust-safety",
                "contradicts",
                MemorySpace::Knowledge,
            )],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(report.has_conflicts());
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.notes_flagged_for_review, 1);
        assert_eq!(report.conflicts[0].conflict_type, ConflictType::ClaimVsNote);
        // No timestamps on either side -> FlaggedForReview
        assert_eq!(
            report.conflicts[0].resolution,
            TemporalResolution::FlaggedForReview
        );
        assert_eq!(report.auto_resolved, 0);
        assert!(report.conflicts[0].old_content_snippet.is_some());
        assert!(report.conflicts[0]
            .old_content_snippet
            .as_ref()
            .expect("snippet")
            .contains("Rust prevents all memory bugs"));
    }

    #[test]
    fn no_conflicts_for_supports() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("note.md"), "# Note\n\nSome content.").expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim("supporting claim", 5)],
            proposed_links: vec![make_link(0, "note", "supports", MemorySpace::Knowledge)],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(!report.has_conflicts());
        assert_eq!(report.notes_flagged_for_review, 0);
        assert_eq!(report.auto_resolved, 0);
    }

    #[test]
    fn detects_cross_space_conflict() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let self_dir = tmp.path().join("self");
        std::fs::create_dir_all(&self_dir).expect("mkdir");
        std::fs::write(
            self_dir.join("preferences.md"),
            "# Preferences\n\nI prefer standing desks.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim("Standing desks reduce productivity", 6)],
            proposed_links: vec![make_link(
                0,
                "preferences",
                "contradicts",
                MemorySpace::Identity,
            )],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(report.has_conflicts());
        assert_eq!(report.conflicts[0].conflict_type, ConflictType::CrossSpace);
        assert_eq!(
            report.conflicts[0].resolution,
            TemporalResolution::FlaggedForReview
        );
    }

    #[test]
    fn multiple_conflicts_same_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("topic.md"),
            "# Topic\n\nOld fact A. Old fact B.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![
                make_claim("Contradicts fact A", 7),
                make_claim("Contradicts fact B", 6),
            ],
            proposed_links: vec![
                make_link(0, "topic", "contradicts", MemorySpace::Knowledge),
                make_link(1, "topic", "contradicts", MemorySpace::Knowledge),
            ],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert_eq!(report.conflicts.len(), 2);
        // Only one unique note flagged
        assert_eq!(report.notes_flagged_for_review, 1);
    }

    #[test]
    fn conflict_comment_with_snippet() {
        let conflict = Conflict {
            claim_content: "New truth".to_string(),
            claim_idx: 0,
            target_node_id: "note".to_string(),
            relationship: "contradicts".to_string(),
            old_content_snippet: Some("Old truth".to_string()),
            conflict_type: ConflictType::ClaimVsNote,
            resolution: TemporalResolution::FlaggedForReview,
        };

        let comment = ConflictDetector::conflict_comment(&conflict);
        assert!(comment.starts_with("<!-- conflict:"));
        assert!(comment.contains("New truth"));
        assert!(comment.contains("Old truth"));
        assert!(comment.ends_with("-->"));
    }

    #[test]
    fn conflict_comment_without_snippet() {
        let conflict = Conflict {
            claim_content: "New claim".to_string(),
            claim_idx: 0,
            target_node_id: "note".to_string(),
            relationship: "contradicts".to_string(),
            old_content_snippet: None,
            conflict_type: ConflictType::CrossSpace,
            resolution: TemporalResolution::FlaggedForReview,
        };

        let comment = ConflictDetector::conflict_comment(&conflict);
        assert!(comment.contains("contradicts existing content"));
    }

    #[test]
    fn conflict_comment_auto_resolved_newer() {
        let conflict = Conflict {
            claim_content: "Updated fact".to_string(),
            claim_idx: 0,
            target_node_id: "note".to_string(),
            relationship: "contradicts".to_string(),
            old_content_snippet: Some("Stale fact".to_string()),
            conflict_type: ConflictType::ClaimVsNote,
            resolution: TemporalResolution::NewerClaimWins,
        };

        let comment = ConflictDetector::conflict_comment(&conflict);
        assert!(comment.contains("[auto-resolved: newer]"));
    }

    #[test]
    fn conflict_comment_auto_resolved_timestamped() {
        let conflict = Conflict {
            claim_content: "Timestamped fact".to_string(),
            claim_idx: 0,
            target_node_id: "note".to_string(),
            relationship: "contradicts".to_string(),
            old_content_snippet: None,
            conflict_type: ConflictType::ClaimVsNote,
            resolution: TemporalResolution::TimestampedClaimWins,
        };

        let comment = ConflictDetector::conflict_comment(&conflict);
        assert!(comment.contains("[auto-resolved: timestamped]"));
    }

    #[test]
    fn conflicts_for_target_filters_correctly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("a.md"), "# A\n\nContent A.").expect("write");
        std::fs::write(knowledge_dir.join("b.md"), "# B\n\nContent B.").expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim("contra A", 7), make_claim("contra B", 6)],
            proposed_links: vec![
                make_link(0, "a", "contradicts", MemorySpace::Knowledge),
                make_link(1, "b", "contradicts", MemorySpace::Knowledge),
            ],
        };

        let report = detect_conflicts(&graph, tmp.path());
        let a_conflicts = report.conflicts_for_target("a");
        assert_eq!(a_conflicts.len(), 1);
        assert_eq!(a_conflicts[0].claim_content, "contra A");

        let b_conflicts = report.conflicts_for_target("b");
        assert_eq!(b_conflicts.len(), 1);
    }

    #[test]
    fn detect_conflicts_for_target_function() {
        let claims = vec![
            make_claim("supporting fact", 5),
            make_claim("contradicting fact", 8),
            make_claim("extending fact", 4),
        ];
        let links = [
            make_link(0, "note", "supports", MemorySpace::Knowledge),
            make_link(1, "note", "contradicts", MemorySpace::Knowledge),
            make_link(2, "note", "extends", MemorySpace::Knowledge),
        ];
        let link_refs: Vec<&ProposedLink> = links.iter().collect();

        let conflicts = detect_conflicts_for_target("note", &link_refs, &claims);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0], "contradicting fact");
    }

    #[test]
    fn conflict_report_default_is_empty() {
        let report = ConflictReport::default();
        assert!(!report.has_conflicts());
        assert_eq!(report.notes_flagged_for_review, 0);
        assert_eq!(report.auto_resolved, 0);
    }

    #[test]
    fn out_of_bounds_claim_idx_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let graph = VerifiedGraph {
            claims: vec![make_claim("only claim", 5)],
            proposed_links: vec![make_link(
                99, // out of bounds
                "note",
                "contradicts",
                MemorySpace::Knowledge,
            )],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(!report.has_conflicts());
    }

    // -- resolve_temporally unit tests ----------------------------------------

    #[test]
    fn resolve_temporally_newer_claim_wins() {
        let result = resolve_temporally(Some("2026-02-25T12:00:00Z"), Some("2026-01-01T00:00:00Z"));
        assert_eq!(result, TemporalResolution::NewerClaimWins);
    }

    #[test]
    fn resolve_temporally_older_claim_flagged() {
        // Old note is newer than the new claim -> flag for review
        let result = resolve_temporally(Some("2025-01-01T00:00:00Z"), Some("2026-02-25T12:00:00Z"));
        assert_eq!(result, TemporalResolution::FlaggedForReview);
    }

    #[test]
    fn resolve_temporally_equal_timestamps_flagged() {
        let result = resolve_temporally(Some("2026-02-25T12:00:00Z"), Some("2026-02-25T12:00:00Z"));
        assert_eq!(result, TemporalResolution::FlaggedForReview);
    }

    #[test]
    fn resolve_temporally_only_claim_has_timestamp() {
        let result = resolve_temporally(Some("2026-02-25T12:00:00Z"), None);
        assert_eq!(result, TemporalResolution::TimestampedClaimWins);
    }

    #[test]
    fn resolve_temporally_only_note_has_timestamp() {
        let result = resolve_temporally(None, Some("2026-02-25T12:00:00Z"));
        assert_eq!(result, TemporalResolution::FlaggedForReview);
    }

    #[test]
    fn resolve_temporally_neither_has_timestamp() {
        let result = resolve_temporally(None, None);
        assert_eq!(result, TemporalResolution::FlaggedForReview);
    }

    // -- Temporal resolution integration tests --------------------------------

    #[test]
    fn newer_timestamped_claim_auto_resolves_conflict() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("fact.md"),
            "---\ningested_at: \"2025-01-01T00:00:00Z\"\n---\n\n# Fact\n\nThe sky is green.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim_with_timestamp(
                "The sky is blue",
                8,
                "2026-02-25T12:00:00Z",
            )],
            proposed_links: vec![make_link(0, "fact", "contradicts", MemorySpace::Knowledge)],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(report.has_conflicts());
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(
            report.conflicts[0].resolution,
            TemporalResolution::NewerClaimWins
        );
        assert_eq!(report.auto_resolved, 1);
        // Auto-resolved conflicts should NOT flag for review
        assert_eq!(report.notes_flagged_for_review, 0);
    }

    #[test]
    fn timestamped_claim_vs_undated_note_auto_resolves() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        // Note without frontmatter (no timestamp)
        std::fs::write(
            knowledge_dir.join("old-note.md"),
            "# Old Note\n\nUndated content.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim_with_timestamp(
                "Updated fact",
                7,
                "2026-02-25T12:00:00Z",
            )],
            proposed_links: vec![make_link(
                0,
                "old-note",
                "contradicts",
                MemorySpace::Knowledge,
            )],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(report.has_conflicts());
        assert_eq!(
            report.conflicts[0].resolution,
            TemporalResolution::TimestampedClaimWins
        );
        assert_eq!(report.auto_resolved, 1);
        assert_eq!(report.notes_flagged_for_review, 0);
    }

    #[test]
    fn undated_claim_vs_dated_note_flagged_for_review() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("dated.md"),
            "---\ningested_at: \"2026-02-20T10:00:00Z\"\n---\n\n# Dated\n\nDated content.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim("Contradicting claim without timestamp", 6)],
            proposed_links: vec![make_link(0, "dated", "contradicts", MemorySpace::Knowledge)],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(report.has_conflicts());
        assert_eq!(
            report.conflicts[0].resolution,
            TemporalResolution::FlaggedForReview
        );
        assert_eq!(report.auto_resolved, 0);
        assert_eq!(report.notes_flagged_for_review, 1);
    }

    #[test]
    fn no_timestamps_backward_compat_flagged_for_review() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("plain.md"),
            "# Plain\n\nNo frontmatter here.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim("Contradicting old fact", 7)],
            proposed_links: vec![make_link(0, "plain", "contradicts", MemorySpace::Knowledge)],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(report.has_conflicts());
        assert_eq!(
            report.conflicts[0].resolution,
            TemporalResolution::FlaggedForReview
        );
        assert_eq!(report.auto_resolved, 0);
        assert_eq!(report.notes_flagged_for_review, 1);
    }

    #[test]
    fn equal_timestamps_flagged_for_review() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("same-time.md"),
            "---\ningested_at: \"2026-02-25T12:00:00Z\"\n---\n\n# Same Time\n\nContent.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![make_claim_with_timestamp(
                "Conflicting claim at same time",
                7,
                "2026-02-25T12:00:00Z",
            )],
            proposed_links: vec![make_link(
                0,
                "same-time",
                "contradicts",
                MemorySpace::Knowledge,
            )],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert!(report.has_conflicts());
        assert_eq!(
            report.conflicts[0].resolution,
            TemporalResolution::FlaggedForReview
        );
        assert_eq!(report.auto_resolved, 0);
        assert_eq!(report.notes_flagged_for_review, 1);
    }

    #[test]
    fn mixed_resolved_and_unresolved_conflicts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");

        // Old note with timestamp
        std::fs::write(
            knowledge_dir.join("dated-fact.md"),
            "---\ningested_at: \"2025-01-01T00:00:00Z\"\n---\n\n# Dated\n\nOld dated fact.",
        )
        .expect("write");

        // Old note without timestamp
        std::fs::write(
            knowledge_dir.join("undated-fact.md"),
            "# Undated\n\nOld undated fact.",
        )
        .expect("write");

        let graph = VerifiedGraph {
            claims: vec![
                // Timestamped claim vs dated note -> auto-resolve (newer wins)
                make_claim_with_timestamp("New dated fact", 8, "2026-02-25T12:00:00Z"),
                // No-timestamp claim vs undated note -> flag for review
                make_claim("New undated fact", 6),
            ],
            proposed_links: vec![
                make_link(0, "dated-fact", "contradicts", MemorySpace::Knowledge),
                make_link(1, "undated-fact", "contradicts", MemorySpace::Knowledge),
            ],
        };

        let report = detect_conflicts(&graph, tmp.path());
        assert_eq!(report.conflicts.len(), 2);
        assert_eq!(report.auto_resolved, 1);
        assert_eq!(report.notes_flagged_for_review, 1);

        // First conflict (dated) should be auto-resolved
        assert_eq!(
            report.conflicts[0].resolution,
            TemporalResolution::NewerClaimWins
        );
        // Second conflict (undated) should be flagged
        assert_eq!(
            report.conflicts[1].resolution,
            TemporalResolution::FlaggedForReview
        );
    }

    #[test]
    fn unresolved_conflicts_filter() {
        let report = ConflictReport {
            conflicts: vec![
                Conflict {
                    claim_content: "auto-resolved".to_string(),
                    claim_idx: 0,
                    target_node_id: "a".to_string(),
                    relationship: "contradicts".to_string(),
                    old_content_snippet: None,
                    conflict_type: ConflictType::ClaimVsNote,
                    resolution: TemporalResolution::NewerClaimWins,
                },
                Conflict {
                    claim_content: "needs review".to_string(),
                    claim_idx: 1,
                    target_node_id: "b".to_string(),
                    relationship: "contradicts".to_string(),
                    old_content_snippet: None,
                    conflict_type: ConflictType::ClaimVsNote,
                    resolution: TemporalResolution::FlaggedForReview,
                },
            ],
            notes_flagged_for_review: 1,
            auto_resolved: 1,
        };

        let unresolved = report.unresolved_conflicts();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].claim_content, "needs review");
    }

    #[test]
    fn read_note_timestamp_from_frontmatter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("ts-note.md"),
            "---\nsource_url: \"https://example.com\"\ningested_at: \"2026-01-15T10:30:00Z\"\nclaim_count: 3\n---\n\n# Note\n\nContent.",
        )
        .expect("write");

        let detector = ConflictDetector::new(tmp.path());
        let link = make_link(0, "ts-note", "contradicts", MemorySpace::Knowledge);
        let ts = detector.read_note_timestamp(&link);
        assert_eq!(ts, Some("2026-01-15T10:30:00Z".to_string()));
    }

    #[test]
    fn read_note_timestamp_returns_none_without_frontmatter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("no-fm.md"),
            "# No Frontmatter\n\nJust content.",
        )
        .expect("write");

        let detector = ConflictDetector::new(tmp.path());
        let link = make_link(0, "no-fm", "contradicts", MemorySpace::Knowledge);
        let ts = detector.read_note_timestamp(&link);
        assert_eq!(ts, None);
    }

    #[test]
    fn read_note_timestamp_returns_none_for_missing_ingested_at() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("no-ts.md"),
            "---\nsource_url: \"https://example.com\"\n---\n\n# Note\n\nContent.",
        )
        .expect("write");

        let detector = ConflictDetector::new(tmp.path());
        let link = make_link(0, "no-ts", "contradicts", MemorySpace::Knowledge);
        let ts = detector.read_note_timestamp(&link);
        assert_eq!(ts, None);
    }

    #[test]
    fn temporal_resolution_serde_roundtrip() {
        let variants = [
            TemporalResolution::NewerClaimWins,
            TemporalResolution::TimestampedClaimWins,
            TemporalResolution::FlaggedForReview,
        ];
        for variant in &variants {
            let json = serde_json::to_string(variant).expect("serialize");
            let parsed: TemporalResolution = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&parsed, variant);
        }
    }

    #[test]
    fn temporal_resolution_display() {
        assert_eq!(
            TemporalResolution::NewerClaimWins.to_string(),
            "newer_claim_wins"
        );
        assert_eq!(
            TemporalResolution::TimestampedClaimWins.to_string(),
            "timestamped_claim_wins"
        );
        assert_eq!(
            TemporalResolution::FlaggedForReview.to_string(),
            "flagged_for_review"
        );
    }
}
