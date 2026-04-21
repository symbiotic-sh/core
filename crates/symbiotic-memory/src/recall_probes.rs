//! Active Recall Probes — derived evaluation of retrieval reachability.
//!
//! Probe state is not canonical memory. It measures whether stored knowledge
//! is reachable through the current retrieval stack and surfaces derived
//! remediation signals when it is not.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Target kinds covered by recall probes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RecallProbeTargetKind {
    ArchiveEntry,
    GraphEntity,
}

impl RecallProbeTargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArchiveEntry => "archive_entry",
            Self::GraphEntity => "graph_entity",
        }
    }
}

impl std::str::FromStr for RecallProbeTargetKind {
    type Err = rusqlite::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "archive_entry" => Ok(Self::ArchiveEntry),
            "graph_entity" => Ok(Self::GraphEntity),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("invalid recall probe target kind: {other}").into(),
            )),
        }
    }
}

/// Derived remediation hints produced by probe failures or weak matches.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RecallRemediationFlag {
    ReembedCandidate,
    KeywordAugmentationCandidate,
    DerivedEdgeCandidate,
    ManualReviewRequired,
}

/// Persistent status summary for a probe target.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RecallProbeStatus {
    Healthy,
    AtRisk,
    Unreachable,
}

impl RecallProbeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::AtRisk => "at_risk",
            Self::Unreachable => "unreachable",
        }
    }
}

impl std::str::FromStr for RecallProbeStatus {
    type Err = rusqlite::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "healthy" => Ok(Self::Healthy),
            "at_risk" => Ok(Self::AtRisk),
            "unreachable" => Ok(Self::Unreachable),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("invalid recall probe status: {other}").into(),
            )),
        }
    }
}

/// A target the probe system should test for retrievability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecallProbeSubject {
    pub target_kind: RecallProbeTargetKind,
    pub target_id: String,
    pub title: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub content: String,
}

/// Top-level metadata for a probe run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecallProbeRun {
    pub id: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub cohort: Option<String>,
    pub top_k: usize,
    pub subject_count: usize,
    pub matched_count: usize,
}

/// Result of running one probe query against one target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecallProbeResult {
    pub run_id: String,
    pub target_kind: RecallProbeTargetKind,
    pub target_id: String,
    pub query: String,
    pub matched: bool,
    pub rank: Option<u32>,
    pub retrieval_mode: String,
    #[serde(default)]
    pub top_item_ids: Vec<String>,
    #[serde(default)]
    pub remediation_flags: Vec<RecallRemediationFlag>,
    pub created_at: u64,
}

/// Roll-up summary for a target across probe runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecallProbeSummary {
    pub target_kind: RecallProbeTargetKind,
    pub target_id: String,
    pub last_checked_at: u64,
    pub last_run_id: String,
    pub success_rate: f64,
    pub consecutive_failures: u32,
    pub status: RecallProbeStatus,
}

/// Per-target outcome aggregated from one probe run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecallProbeRunOutcome {
    pub target_kind: RecallProbeTargetKind,
    pub target_id: String,
    pub matched: bool,
    pub best_rank: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecallProbeBaselineTarget {
    pub cohort: String,
    pub position: usize,
    pub target_kind: RecallProbeTargetKind,
    pub target_id: String,
    pub created_at: u64,
}

/// Durable derived store for recall probe runs/results.
pub struct RecallProbeStore {
    conn: Connection,
}

impl RecallProbeStore {
    /// Open or create a durable probe store backed by SQLite.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| rusqlite::Error::InvalidPath(parent.to_path_buf()))?;
        }
        let conn = Connection::open(path)?;
        crate::sqlite_schema::ensure_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory store for testing.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        crate::sqlite_schema::ensure_schema(&conn)?;
        Ok(Self { conn })
    }

    pub fn start_run(&self, run: &RecallProbeRun) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO recall_probe_runs
             (id, started_at, finished_at, cohort, top_k, subject_count, matched_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.id,
                run.started_at as i64,
                run.finished_at.map(|value| value as i64),
                run.cohort.as_deref(),
                run.top_k as i64,
                run.subject_count as i64,
                run.matched_count as i64,
            ],
        )?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        finished_at: u64,
        subject_count: usize,
    ) -> rusqlite::Result<()> {
        let matched_count: usize = self.conn.query_row(
            "SELECT COUNT(*)
             FROM (
                 SELECT target_kind, target_id
                 FROM recall_probe_results
                 WHERE run_id = ?1 AND matched = 1
                 GROUP BY target_kind, target_id
             )",
            [run_id],
            |row| row.get::<_, i64>(0),
        )? as usize;

        self.conn.execute(
            "UPDATE recall_probe_runs
             SET finished_at = ?2, subject_count = ?3, matched_count = ?4
             WHERE id = ?1",
            params![
                run_id,
                finished_at as i64,
                subject_count as i64,
                matched_count as i64
            ],
        )?;
        Ok(())
    }

    pub fn record_result(&self, result: &RecallProbeResult) -> rusqlite::Result<()> {
        let top_item_ids_json = serde_json::to_string(&result.top_item_ids)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
        let remediation_flags_json = serde_json::to_string(&result.remediation_flags)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;

        self.conn.execute(
            "INSERT OR REPLACE INTO recall_probe_results
             (run_id, target_kind, target_id, query, matched, rank, retrieval_mode, top_item_ids_json, remediation_flags_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                result.run_id,
                result.target_kind.as_str(),
                result.target_id,
                result.query,
                if result.matched { 1 } else { 0 },
                result.rank.map(|value| value as i64),
                result.retrieval_mode,
                top_item_ids_json,
                remediation_flags_json,
                result.created_at as i64,
            ],
        )?;

        self.refresh_summary(
            result.target_kind,
            &result.target_id,
            result.created_at,
            &result.run_id,
        )?;

        Ok(())
    }

    pub fn results_for_run(&self, run_id: &str) -> rusqlite::Result<Vec<RecallProbeResult>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, target_kind, target_id, query, matched, rank, retrieval_mode, top_item_ids_json, remediation_flags_json, created_at
             FROM recall_probe_results
             WHERE run_id = ?1
             ORDER BY created_at ASC, target_kind ASC, target_id ASC, query ASC",
        )?;
        let rows = stmt.query_map([run_id], row_to_probe_result)?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    pub fn run(&self, run_id: &str) -> rusqlite::Result<Option<RecallProbeRun>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, finished_at, cohort, top_k, subject_count, matched_count
             FROM recall_probe_runs
             WHERE id = ?1",
        )?;
        let mut rows = stmt.query([run_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(RecallProbeRun {
                id: row.get(0)?,
                started_at: row.get::<_, i64>(1)? as u64,
                finished_at: row.get::<_, Option<i64>>(2)?.map(|value| value as u64),
                cohort: row.get(3)?,
                top_k: row.get::<_, i64>(4)? as usize,
                subject_count: row.get::<_, i64>(5)? as usize,
                matched_count: row.get::<_, i64>(6)? as usize,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn summary_for(
        &self,
        target_kind: RecallProbeTargetKind,
        target_id: &str,
    ) -> rusqlite::Result<Option<RecallProbeSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT target_kind, target_id, last_checked_at, last_run_id, success_rate, consecutive_failures, status
             FROM recall_probe_summary
             WHERE target_kind = ?1 AND target_id = ?2",
        )?;
        let mut rows = stmt.query(params![target_kind.as_str(), target_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_probe_summary(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn results_for_target_in_run(
        &self,
        run_id: &str,
        target_kind: RecallProbeTargetKind,
        target_id: &str,
    ) -> rusqlite::Result<Vec<RecallProbeResult>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, target_kind, target_id, query, matched, rank, retrieval_mode, top_item_ids_json, remediation_flags_json, created_at
             FROM recall_probe_results
             WHERE run_id = ?1 AND target_kind = ?2 AND target_id = ?3
             ORDER BY created_at ASC, query ASC",
        )?;
        let rows = stmt.query_map(
            params![run_id, target_kind.as_str(), target_id],
            row_to_probe_result,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    pub fn list_summaries(&self, limit: usize) -> rusqlite::Result<Vec<RecallProbeSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT target_kind, target_id, last_checked_at, last_run_id, success_rate, consecutive_failures, status
             FROM recall_probe_summary
             ORDER BY
                CASE status
                    WHEN 'unreachable' THEN 0
                    WHEN 'at_risk' THEN 1
                    ELSE 2
                END ASC,
                consecutive_failures DESC,
                success_rate ASC,
                last_checked_at DESC,
                target_kind ASC,
                target_id ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], row_to_probe_summary)?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    pub fn baseline_targets(
        &self,
        cohort: &str,
    ) -> rusqlite::Result<Vec<RecallProbeBaselineTarget>> {
        let mut stmt = self.conn.prepare(
            "SELECT cohort, position, target_kind, target_id, created_at
             FROM recall_probe_baseline_targets
             WHERE cohort = ?1
             ORDER BY position ASC, target_kind ASC, target_id ASC",
        )?;
        let rows = stmt.query_map([cohort], |row| {
            Ok(RecallProbeBaselineTarget {
                cohort: row.get(0)?,
                position: row.get::<_, i64>(1)? as usize,
                target_kind: row.get::<_, String>(2)?.parse::<RecallProbeTargetKind>()?,
                target_id: row.get(3)?,
                created_at: row.get::<_, i64>(4)? as u64,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    pub fn replace_baseline_targets(
        &self,
        cohort: &str,
        targets: &[RecallProbeBaselineTarget],
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM recall_probe_baseline_targets WHERE cohort = ?1",
            [cohort],
        )?;
        let mut stmt = self.conn.prepare(
            "INSERT INTO recall_probe_baseline_targets
             (cohort, position, target_kind, target_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for target in targets {
            stmt.execute(params![
                cohort,
                target.position as i64,
                target.target_kind.as_str(),
                target.target_id,
                target.created_at as i64,
            ])?;
        }
        Ok(())
    }

    pub fn previous_completed_run_id(&self, run_id: &str) -> rusqlite::Result<Option<String>> {
        let (finished_at, cohort) = self.conn.query_row(
            "SELECT finished_at, cohort FROM recall_probe_runs WHERE id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )?;
        let Some(finished_at) = finished_at else {
            return Ok(None);
        };

        if let Some(cohort) = cohort {
            self.conn
                .query_row(
                    "SELECT id
                     FROM recall_probe_runs
                     WHERE finished_at IS NOT NULL
                       AND id != ?1
                       AND finished_at < ?2
                       AND cohort = ?3
                     ORDER BY finished_at DESC, started_at DESC, id DESC
                     LIMIT 1",
                    params![run_id, finished_at, cohort],
                    |row| row.get(0),
                )
                .optional()
        } else {
            self.conn
                .query_row(
                    "SELECT id
                     FROM recall_probe_runs
                     WHERE finished_at IS NOT NULL
                       AND id != ?1
                       AND finished_at < ?2
                       AND cohort IS NULL
                     ORDER BY finished_at DESC, started_at DESC, id DESC
                     LIMIT 1",
                    params![run_id, finished_at],
                    |row| row.get(0),
                )
                .optional()
        }
    }

    pub fn outcomes_for_run(&self, run_id: &str) -> rusqlite::Result<Vec<RecallProbeRunOutcome>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                target_kind,
                target_id,
                MAX(matched) AS matched,
                MIN(rank) AS best_rank
             FROM recall_probe_results
             WHERE run_id = ?1
             GROUP BY target_kind, target_id
             ORDER BY target_kind ASC, target_id ASC",
        )?;
        let rows = stmt.query_map([run_id], row_to_probe_outcome)?;
        rows.collect::<Result<Vec<_>, _>>()
    }

    fn refresh_summary(
        &self,
        target_kind: RecallProbeTargetKind,
        target_id: &str,
        checked_at: u64,
        run_id: &str,
    ) -> rusqlite::Result<()> {
        let total: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM recall_probe_results WHERE target_kind = ?1 AND target_id = ?2",
            params![target_kind.as_str(), target_id],
            |row| row.get::<_, i64>(0),
        )? as u32;
        let matched: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM recall_probe_results WHERE target_kind = ?1 AND target_id = ?2 AND matched = 1",
            params![target_kind.as_str(), target_id],
            |row| row.get::<_, i64>(0),
        )? as u32;
        let consecutive_failures = self.consecutive_failures(target_kind, target_id)?;
        let success_rate = if total == 0 {
            0.0
        } else {
            matched as f64 / total as f64
        };
        let status = if consecutive_failures >= 3 {
            RecallProbeStatus::Unreachable
        } else if success_rate < 0.5 {
            RecallProbeStatus::AtRisk
        } else {
            RecallProbeStatus::Healthy
        };

        self.conn.execute(
            "INSERT OR REPLACE INTO recall_probe_summary
             (target_kind, target_id, last_checked_at, last_run_id, success_rate, consecutive_failures, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                target_kind.as_str(),
                target_id,
                checked_at as i64,
                run_id,
                success_rate,
                consecutive_failures as i64,
                status.as_str(),
            ],
        )?;
        Ok(())
    }

    fn consecutive_failures(
        &self,
        target_kind: RecallProbeTargetKind,
        target_id: &str,
    ) -> rusqlite::Result<u32> {
        let mut stmt = self.conn.prepare(
            "SELECT matched
             FROM recall_probe_results
             WHERE target_kind = ?1 AND target_id = ?2
             ORDER BY created_at DESC, rowid DESC",
        )?;
        let matches = stmt
            .query_map(params![target_kind.as_str(), target_id], |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut failures = 0u32;
        for matched in matches {
            if matched == 1 {
                break;
            }
            failures += 1;
        }
        Ok(failures)
    }
}

/// Generate deterministic probe queries for a subject.
pub fn generate_probe_queries(subject: &RecallProbeSubject, max_queries: usize) -> Vec<String> {
    if max_queries == 0 {
        return Vec::new();
    }

    let mut queries = Vec::new();
    let title = subject.title.trim();
    if !title.is_empty() {
        queries.push(title.to_string());
    }

    for alias in &subject.aliases {
        let alias = alias.trim();
        if !alias.is_empty() && !queries.iter().any(|existing| existing == alias) {
            queries.push(alias.to_string());
        }
        if queries.len() >= max_queries {
            return queries;
        }
    }

    if subject.target_kind == RecallProbeTargetKind::GraphEntity {
        queries.truncate(max_queries);
        return queries;
    }

    let keywords = extract_probe_keywords(&format!("{} {}", subject.title, subject.content));
    if !keywords.is_empty() {
        let keyword_query = keywords
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        if !keyword_query.is_empty() && !queries.iter().any(|existing| existing == &keyword_query) {
            queries.push(keyword_query);
        }
    }

    queries.truncate(max_queries);
    queries
}

/// Derive non-canonical remediation hints from a probe result.
pub fn derive_remediation_flags(result: &RecallProbeResult) -> Vec<RecallRemediationFlag> {
    if result.matched {
        if result.rank.is_some_and(|rank| rank > 5) {
            return vec![RecallRemediationFlag::DerivedEdgeCandidate];
        }
        return Vec::new();
    }

    let mut flags = vec![
        RecallRemediationFlag::ReembedCandidate,
        RecallRemediationFlag::KeywordAugmentationCandidate,
    ];

    if result.rank.is_none() || result.top_item_ids.is_empty() {
        flags.push(RecallRemediationFlag::ManualReviewRequired);
    } else {
        flags.push(RecallRemediationFlag::DerivedEdgeCandidate);
    }

    flags
}

/// Load graph-entity probe subjects directly from a memory SQLite database path.
pub fn load_graph_probe_subjects(
    path: impl AsRef<Path>,
    limit: usize,
) -> rusqlite::Result<Vec<RecallProbeSubject>> {
    let conn = Connection::open(path)?;
    crate::sqlite_schema::ensure_schema(&conn)?;
    query_graph_probe_subjects(&conn, limit)
}

fn extract_probe_keywords(text: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "been", "being", "but", "by", "for", "from",
        "had", "has", "have", "if", "in", "into", "is", "it", "its", "not", "of", "on", "or",
        "that", "the", "their", "them", "there", "these", "this", "to", "was", "were", "which",
        "with", "you", "your",
    ];

    let mut keywords = text
        .split(|ch: char| !ch.is_alphanumeric())
        .map(|word| word.trim().to_ascii_lowercase())
        .filter(|word| word.len() > 2 && !STOPWORDS.contains(&word.as_str()))
        .collect::<Vec<_>>();
    keywords.sort();
    keywords.dedup();
    keywords
}

pub(crate) fn query_graph_probe_subjects(
    conn: &Connection,
    limit: usize,
) -> rusqlite::Result<Vec<RecallProbeSubject>> {
    let mut stmt = conn.prepare(
        "SELECT
             e.id,
             e.name,
             GROUP_CONCAT(m.fact, ' ')
         FROM entities e
         JOIN memories m
           ON m.entity_id = e.id
          AND m.status = 'active'
         WHERE e.status = 'active'
         GROUP BY e.id, e.name
         ORDER BY MAX(e.updated_at) DESC, e.name ASC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok(RecallProbeSubject {
            target_kind: RecallProbeTargetKind::GraphEntity,
            target_id: row.get::<_, String>(0)?,
            title: row.get::<_, String>(1)?,
            aliases: Vec::new(),
            content: row.get::<_, String>(2)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
}

fn row_to_probe_result(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecallProbeResult> {
    let target_kind = row.get::<_, String>(1)?.parse::<RecallProbeTargetKind>()?;
    let top_item_ids =
        serde_json::from_str::<Vec<String>>(&row.get::<_, String>(7)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, error.into())
        })?;
    let remediation_flags = serde_json::from_str::<Vec<RecallRemediationFlag>>(
        &row.get::<_, String>(8)?,
    )
    .map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, error.into())
    })?;

    Ok(RecallProbeResult {
        run_id: row.get(0)?,
        target_kind,
        target_id: row.get(2)?,
        query: row.get(3)?,
        matched: row.get::<_, i64>(4)? == 1,
        rank: row.get::<_, Option<i64>>(5)?.map(|value| value as u32),
        retrieval_mode: row.get(6)?,
        top_item_ids,
        remediation_flags,
        created_at: row.get::<_, i64>(9)? as u64,
    })
}

fn row_to_probe_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecallProbeSummary> {
    Ok(RecallProbeSummary {
        target_kind: row.get::<_, String>(0)?.parse::<RecallProbeTargetKind>()?,
        target_id: row.get(1)?,
        last_checked_at: row.get::<_, i64>(2)? as u64,
        last_run_id: row.get(3)?,
        success_rate: row.get(4)?,
        consecutive_failures: row.get::<_, i64>(5)? as u32,
        status: row.get::<_, String>(6)?.parse::<RecallProbeStatus>()?,
    })
}

fn row_to_probe_outcome(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecallProbeRunOutcome> {
    Ok(RecallProbeRunOutcome {
        target_kind: row.get::<_, String>(0)?.parse::<RecallProbeTargetKind>()?,
        target_id: row.get(1)?,
        matched: row.get::<_, i64>(2)? == 1,
        best_rank: row.get::<_, Option<i64>>(3)?.map(|value| value as u32),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_probe_queries_prefers_title_alias_and_keywords() {
        let subject = RecallProbeSubject {
            target_kind: RecallProbeTargetKind::ArchiveEntry,
            target_id: "entry-1".to_string(),
            title: "Rust Tokio Runtime".to_string(),
            aliases: vec!["tokio runtime".to_string()],
            content: "Async runtime for Rust services and daemon orchestration.".to_string(),
        };

        let queries = generate_probe_queries(&subject, 3);
        assert_eq!(queries.len(), 3);
        assert_eq!(queries[0], "Rust Tokio Runtime");
        assert_eq!(queries[1], "tokio runtime");
        assert!(queries[2].contains("runtime"));
    }

    #[test]
    fn generate_probe_queries_for_graph_entity_uses_name_surface_only() {
        let subject = RecallProbeSubject {
            target_kind: RecallProbeTargetKind::GraphEntity,
            target_id: "tokio".to_string(),
            title: "Tokio".to_string(),
            aliases: vec!["tokio runtime".to_string()],
            content: "Tokio powers the async runtime".to_string(),
        };

        let queries = generate_probe_queries(&subject, 3);
        assert_eq!(
            queries,
            vec!["Tokio".to_string(), "tokio runtime".to_string()]
        );
    }

    #[test]
    fn derive_remediation_flags_for_miss_is_non_canonical_and_explicit() {
        let result = RecallProbeResult {
            run_id: "run-1".to_string(),
            target_kind: RecallProbeTargetKind::GraphEntity,
            target_id: "memory-1".to_string(),
            query: "rust daemon".to_string(),
            matched: false,
            rank: None,
            retrieval_mode: "keyword".to_string(),
            top_item_ids: Vec::new(),
            remediation_flags: Vec::new(),
            created_at: 1,
        };

        let flags = derive_remediation_flags(&result);
        assert!(flags.contains(&RecallRemediationFlag::ReembedCandidate));
        assert!(flags.contains(&RecallRemediationFlag::KeywordAugmentationCandidate));
        assert!(flags.contains(&RecallRemediationFlag::ManualReviewRequired));
    }

    #[test]
    fn recall_probe_store_persists_results_and_rolls_up_summary() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count: 1,
                matched_count: 0,
            })
            .expect("start run");

        for (created_at, matched) in [(11, false), (12, false), (13, false)] {
            let result = RecallProbeResult {
                run_id: "run-1".to_string(),
                target_kind: RecallProbeTargetKind::ArchiveEntry,
                target_id: "entry-1".to_string(),
                query: format!("query-{created_at}"),
                matched,
                rank: None,
                retrieval_mode: "keyword".to_string(),
                top_item_ids: Vec::new(),
                remediation_flags: Vec::new(),
                created_at,
            };
            store.record_result(&result).expect("record");
        }

        store.finish_run("run-1", 14, 1).expect("finish");
        let run = store.run("run-1").expect("run query").expect("run");
        assert_eq!(run.subject_count, 1);
        assert_eq!(run.matched_count, 0);
        let summary = store
            .summary_for(RecallProbeTargetKind::ArchiveEntry, "entry-1")
            .expect("summary query")
            .expect("summary");
        assert_eq!(summary.consecutive_failures, 3);
        assert_eq!(summary.status, RecallProbeStatus::Unreachable);

        let results = store.results_for_run("run-1").expect("results");
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn successful_probe_resets_consecutive_failures() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count: 1,
                matched_count: 0,
            })
            .expect("start run");

        for (created_at, matched) in [(11, false), (12, false), (13, true)] {
            let mut result = RecallProbeResult {
                run_id: "run-1".to_string(),
                target_kind: RecallProbeTargetKind::GraphEntity,
                target_id: "memory-1".to_string(),
                query: format!("query-{created_at}"),
                matched,
                rank: None,
                retrieval_mode: "keyword".to_string(),
                top_item_ids: Vec::new(),
                remediation_flags: Vec::new(),
                created_at,
            };
            result.remediation_flags = derive_remediation_flags(&result);
            store.record_result(&result).expect("record");
        }

        let summary = store
            .summary_for(RecallProbeTargetKind::GraphEntity, "memory-1")
            .expect("summary query")
            .expect("summary");
        assert_eq!(summary.consecutive_failures, 0);
        assert_eq!(summary.status, RecallProbeStatus::AtRisk);
    }

    #[test]
    fn finish_run_counts_distinct_matched_subjects_not_queries() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count: 0,
                matched_count: 0,
            })
            .expect("start run");

        for query in ["tokio", "tokio runtime"] {
            store
                .record_result(&RecallProbeResult {
                    run_id: "run-1".to_string(),
                    target_kind: RecallProbeTargetKind::GraphEntity,
                    target_id: "tokio".to_string(),
                    query: query.to_string(),
                    matched: true,
                    rank: Some(1),
                    retrieval_mode: "hybrid".to_string(),
                    top_item_ids: vec!["tokio".to_string()],
                    remediation_flags: Vec::new(),
                    created_at: 11,
                })
                .expect("record");
        }

        store.finish_run("run-1", 12, 1).expect("finish");
        let run = store.run("run-1").expect("run query").expect("run");
        assert_eq!(run.subject_count, 1);
        assert_eq!(run.matched_count, 1);
    }

    #[test]
    fn list_summaries_orders_unreachable_before_at_risk_before_healthy() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        let now = 10u64;
        store
            .start_run(&RecallProbeRun {
                id: "run-1".to_string(),
                started_at: now,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count: 3,
                matched_count: 0,
            })
            .expect("start");

        for (target_id, matched, created_at) in [
            ("healthy", true, 11),
            ("at-risk", false, 12),
            ("at-risk", false, 13),
            ("at-risk", true, 14),
            ("unreachable", false, 15),
            ("unreachable", false, 16),
            ("unreachable", false, 17),
        ] {
            store
                .record_result(&RecallProbeResult {
                    run_id: "run-1".to_string(),
                    target_kind: RecallProbeTargetKind::ArchiveEntry,
                    target_id: target_id.to_string(),
                    query: format!("q-{target_id}-{created_at}"),
                    matched,
                    rank: matched.then_some(1),
                    retrieval_mode: "keyword".to_string(),
                    top_item_ids: Vec::new(),
                    remediation_flags: Vec::new(),
                    created_at,
                })
                .expect("record");
        }

        let summaries = store.list_summaries(10).expect("summaries");
        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].target_id, "unreachable");
        assert_eq!(summaries[0].status, RecallProbeStatus::Unreachable);
        assert_eq!(summaries[1].target_id, "at-risk");
        assert_eq!(summaries[1].status, RecallProbeStatus::AtRisk);
        assert_eq!(summaries[2].target_id, "healthy");
        assert_eq!(summaries[2].status, RecallProbeStatus::Healthy);
    }

    #[test]
    fn previous_completed_run_id_returns_immediate_prior_run() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        for (id, started_at, finished_at) in
            [("run-1", 10, 20), ("run-2", 30, 40), ("run-3", 50, 60)]
        {
            store
                .start_run(&RecallProbeRun {
                    id: id.to_string(),
                    started_at,
                    finished_at: None,
                    cohort: None,
                    top_k: 10,
                    subject_count: 0,
                    matched_count: 0,
                })
                .expect("start");
            store.finish_run(id, finished_at, 0).expect("finish");
        }

        let previous = store
            .previous_completed_run_id("run-3")
            .expect("previous query");
        assert_eq!(previous.as_deref(), Some("run-2"));
    }

    #[test]
    fn previous_completed_run_id_respects_run_cohort() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        for (id, cohort, started_at, finished_at) in [
            ("adhoc-1", None, 10, 20),
            ("periodic-1", Some("periodic_baseline"), 30, 40),
            ("adhoc-2", None, 50, 60),
            ("periodic-2", Some("periodic_baseline"), 70, 80),
        ] {
            store
                .start_run(&RecallProbeRun {
                    id: id.to_string(),
                    started_at,
                    finished_at: None,
                    cohort: cohort.map(str::to_string),
                    top_k: 10,
                    subject_count: 0,
                    matched_count: 0,
                })
                .expect("start");
            store.finish_run(id, finished_at, 0).expect("finish");
        }

        let previous_periodic = store
            .previous_completed_run_id("periodic-2")
            .expect("previous periodic query");
        assert_eq!(previous_periodic.as_deref(), Some("periodic-1"));

        let previous_adhoc = store
            .previous_completed_run_id("adhoc-2")
            .expect("previous adhoc query");
        assert_eq!(previous_adhoc.as_deref(), Some("adhoc-1"));
    }

    #[test]
    fn outcomes_for_run_aggregates_query_level_matches_to_subject_outcomes() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count: 2,
                matched_count: 0,
            })
            .expect("start");

        for (target_id, matched, rank, created_at) in [
            ("tokio", false, None, 11),
            ("tokio", true, Some(3), 12),
            ("axum", false, None, 13),
            ("axum", false, None, 14),
        ] {
            store
                .record_result(&RecallProbeResult {
                    run_id: "run-1".to_string(),
                    target_kind: RecallProbeTargetKind::GraphEntity,
                    target_id: target_id.to_string(),
                    query: format!("q-{target_id}-{created_at}"),
                    matched,
                    rank,
                    retrieval_mode: "hybrid".to_string(),
                    top_item_ids: Vec::new(),
                    remediation_flags: Vec::new(),
                    created_at,
                })
                .expect("record");
        }

        let outcomes = store.outcomes_for_run("run-1").expect("outcomes");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(
            outcomes[0],
            RecallProbeRunOutcome {
                target_kind: RecallProbeTargetKind::GraphEntity,
                target_id: "axum".to_string(),
                matched: false,
                best_rank: None,
            }
        );
        assert_eq!(
            outcomes[1],
            RecallProbeRunOutcome {
                target_kind: RecallProbeTargetKind::GraphEntity,
                target_id: "tokio".to_string(),
                matched: true,
                best_rank: Some(3),
            }
        );
    }

    #[test]
    fn results_for_target_in_run_filters_to_requested_subject() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count: 2,
                matched_count: 0,
            })
            .expect("start");

        for (target_id, query, created_at) in [
            ("entry-1", "first", 11),
            ("entry-1", "second", 12),
            ("entry-2", "other", 13),
        ] {
            store
                .record_result(&RecallProbeResult {
                    run_id: "run-1".to_string(),
                    target_kind: RecallProbeTargetKind::ArchiveEntry,
                    target_id: target_id.to_string(),
                    query: query.to_string(),
                    matched: false,
                    rank: None,
                    retrieval_mode: "hybrid".to_string(),
                    top_item_ids: Vec::new(),
                    remediation_flags: vec![RecallRemediationFlag::ManualReviewRequired],
                    created_at,
                })
                .expect("record");
        }

        let results = store
            .results_for_target_in_run("run-1", RecallProbeTargetKind::ArchiveEntry, "entry-1")
            .expect("target results");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].query, "first");
        assert_eq!(results[1].query, "second");
        assert!(results.iter().all(|result| result.target_id == "entry-1"));
    }

    #[test]
    fn baseline_targets_round_trip_in_position_order() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .replace_baseline_targets(
                "periodic_baseline",
                &[
                    RecallProbeBaselineTarget {
                        cohort: "periodic_baseline".to_string(),
                        position: 0,
                        target_kind: RecallProbeTargetKind::ArchiveEntry,
                        target_id: "entry-1".to_string(),
                        created_at: 10,
                    },
                    RecallProbeBaselineTarget {
                        cohort: "periodic_baseline".to_string(),
                        position: 1,
                        target_kind: RecallProbeTargetKind::GraphEntity,
                        target_id: "tokio".to_string(),
                        created_at: 10,
                    },
                ],
            )
            .expect("replace baseline");

        let targets = store
            .baseline_targets("periodic_baseline")
            .expect("load baseline");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].target_id, "entry-1");
        assert_eq!(targets[1].target_id, "tokio");
    }
}
