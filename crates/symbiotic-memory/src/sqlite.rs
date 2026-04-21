//! SQLite-backed implementation of `MemoryStore` with FTS5 full-text search.
//!
//! When compiled with `bundled-sqlcipher`, connections can be encrypted with
//! a hex key derived from the vault master key. Set `PRAGMA key` immediately
//! after opening the connection — before any other SQL statement — to enable
//! at-rest encryption via SQLCipher.

use std::path::Path;
use std::sync::Arc;

use rusqlite::Connection;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::recall_probes::RecallProbeSubject;
use crate::self_improvement::{
    ContradictionEvidenceSummary, ContradictionSummary, FrictionDetector, FrictionSignal,
    GraphMaintenanceEdge, GraphMaintenanceNode, GraphMaintenanceSnapshot, MemoryIntegritySnapshot,
};
use crate::sqlite_schema::ensure_schema;
use crate::sqlite_schema::{row_to_evidence, row_to_memory};
use crate::types::*;

/// Errors specific to SQLCipher key management and migration.
#[derive(Debug, Error)]
pub enum CipherError {
    #[error("failed to set encryption key: {0}")]
    KeyFailed(String),
    #[error("database appears encrypted but no key was provided")]
    EncryptedNoKey,
    #[error("database migration error: {0}")]
    MigrationFailed(String),
}

/// SQLite-backed memory store with FTS5 search, entity deduplication,
/// and optional SQLCipher at-rest encryption.
pub struct SqliteMemoryStore {
    pub(crate) conn: Arc<Mutex<Connection>>,
    dedup_config: DeduplicationConfig,
}

impl SqliteMemoryStore {
    /// Open (or create) a memory store at the given path.
    ///
    /// If `key` is `Some`, the hex-encoded SQLCipher key is applied via
    /// `PRAGMA key` immediately after opening. The database file will be
    /// encrypted at rest.
    pub fn open(path: &Path) -> Result<Self, MemoryStoreError> {
        Self::open_with_key(path, None)
    }

    /// Open (or create) an encrypted memory store at the given path.
    ///
    /// The `key` is a hex-encoded 256-bit key (64 hex characters) applied via
    /// `PRAGMA key = "x'...'";` immediately after opening.
    pub fn open_encrypted(path: &Path, key: &str) -> Result<Self, MemoryStoreError> {
        Self::open_with_key(path, Some(key))
    }

    /// Internal opener that optionally applies an encryption key.
    fn open_with_key(path: &Path, key: Option<&str>) -> Result<Self, MemoryStoreError> {
        let conn = Connection::open(path)?;
        if let Some(k) = key {
            apply_cipher_key(&conn, k).map_err(|e| MemoryStoreError::Database(e.to_string()))?;
        }
        ensure_schema(&conn)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            dedup_config: DeduplicationConfig::default(),
        };
        Ok(store)
    }

    /// Create an in-memory store (useful for testing).
    pub fn open_in_memory() -> Result<Self, MemoryStoreError> {
        let conn = Connection::open_in_memory()?;
        ensure_schema(&conn)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
            dedup_config: DeduplicationConfig::default(),
        };
        Ok(store)
    }

    /// Initialize the database schema (tables, indexes, FTS5 virtual tables).
    pub async fn initialize(&self) -> Result<(), MemoryStoreError> {
        let conn = self.conn.lock().await;
        ensure_schema(&conn)?;
        Ok(())
    }

    /// Set deduplication configuration.
    pub fn set_dedup_config(&mut self, config: DeduplicationConfig) {
        self.dedup_config = config;
    }

    /// Returns a reference to the deduplication configuration.
    pub(crate) fn dedup_config(&self) -> &DeduplicationConfig {
        &self.dedup_config
    }

    /// Check whether the underlying connection has SQLCipher encryption active.
    ///
    /// Returns `true` if `PRAGMA cipher_version` returns a non-empty string,
    /// indicating the database is opened with a valid SQLCipher key.
    pub async fn is_encrypted(&self) -> bool {
        let conn = self.conn.lock().await;
        check_cipher_active(&conn)
    }

    /// Build a typed snapshot of the live graph for derived maintenance
    /// analysis such as orphan repair proposals.
    pub async fn graph_maintenance_snapshot(
        &self,
    ) -> Result<GraphMaintenanceSnapshot, MemoryStoreError> {
        let conn = self.conn.lock().await;

        let mut node_stmt = conn.prepare(
            "SELECT
                 e.id,
                 e.name,
                 e.entity_type,
                 e.created_at,
                 COUNT(m.id) AS memory_count,
                 GROUP_CONCAT(m.fact, ' ')
             FROM entities e
             LEFT JOIN memories m
               ON m.entity_id = e.id
              AND m.status = 'active'
             WHERE e.status = 'active'
             GROUP BY e.id, e.name, e.entity_type, e.created_at",
        )?;
        let nodes = node_stmt
            .query_map([], |row| {
                let created_at = row.get::<_, String>(3)?;
                let created_at = chrono::DateTime::parse_from_rfc3339(&created_at)
                    .ok()
                    .map(|dt| dt.timestamp())
                    .unwrap_or(0);
                let age_days = if created_at > 0 {
                    let now = chrono::Utc::now().timestamp();
                    now.saturating_sub(created_at) as u64 / 86_400
                } else {
                    0
                };
                let name = row.get::<_, String>(1)?;
                let facts = row.get::<_, Option<String>>(5)?.unwrap_or_default();
                Ok(GraphMaintenanceNode {
                    entity_id: row.get::<_, String>(0)?,
                    entity_name: name.clone(),
                    entity_type: row.get::<_, String>(2)?.parse().map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(err),
                        )
                    })?,
                    age_days,
                    memory_count: row.get::<_, usize>(4)?,
                    keywords: maintenance_keywords(&name, &facts),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut edge_stmt = conn.prepare(
            "SELECT from_entity, to_entity FROM relationships WHERE status = 'active'
             UNION ALL
             SELECT source_entity_id, target_entity_id FROM links",
        )?;
        let edges = edge_stmt
            .query_map([], |row| {
                Ok(GraphMaintenanceEdge {
                    source_entity_id: row.get::<_, String>(0)?,
                    target_entity_id: row.get::<_, String>(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(GraphMaintenanceSnapshot { nodes, edges })
    }

    /// Build graph-entity probe subjects for retrieval evaluation.
    pub async fn recall_probe_graph_subjects(
        &self,
        limit: usize,
    ) -> Result<Vec<RecallProbeSubject>, MemoryStoreError> {
        let conn = self.conn.lock().await;
        crate::recall_probes::query_graph_probe_subjects(&conn, limit).map_err(Into::into)
    }

    /// Build a derived contradiction snapshot from the live active memory set.
    pub async fn memory_integrity_snapshot(
        &self,
        limit: usize,
    ) -> Result<MemoryIntegritySnapshot, MemoryStoreError> {
        let conn = self.conn.lock().await;

        let tracked_entity_count = conn.query_row(
            "SELECT COUNT(*) FROM entities WHERE status = 'active'",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;

        let mut entity_stmt =
            conn.prepare("SELECT id, name, entity_type FROM entities WHERE status = 'active'")?;
        let entities = entity_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let entity_lookup = entities
            .into_iter()
            .map(|(id, name, entity_type)| {
                let parsed = entity_type.parse::<EntityType>().map_err(|err| {
                    MemoryStoreError::Database(format!(
                        "failed to parse entity type '{entity_type}' for {id}: {err}"
                    ))
                })?;
                Ok((id, (name, parsed)))
            })
            .collect::<Result<std::collections::HashMap<_, _>, MemoryStoreError>>()?;

        let mut memory_stmt =
            conn.prepare("SELECT * FROM memories WHERE status = 'active' ORDER BY entity_id, id")?;
        let memories = memory_stmt
            .query_map([], row_to_memory)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut evidence_stmt =
            conn.prepare("SELECT * FROM evidence WHERE memory_id IS NOT NULL")?;
        let evidence_rows = evidence_stmt
            .query_map([], row_to_evidence)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(memory_stmt);
        drop(evidence_stmt);
        drop(entity_stmt);
        drop(conn);

        let memory_lookup = memories
            .iter()
            .map(|memory| (memory.id.as_str(), memory))
            .collect::<std::collections::HashMap<_, _>>();
        let mut evidence_lookup = std::collections::HashMap::<String, Vec<Evidence>>::new();
        for evidence in evidence_rows {
            if let Some(memory_id) = evidence.memory_id.clone() {
                evidence_lookup.entry(memory_id).or_default().push(evidence);
            }
        }
        let detector = FrictionDetector::default();
        let signals = detector.detect_contradictions(&memories);

        let mut contradictions = Vec::new();
        for signal in signals {
            let proposal = detector.propose(&signal);
            if let FrictionSignal::Contradiction {
                memory_a,
                memory_b,
                description,
            } = signal
            {
                let Some(left) = memory_lookup.get(memory_a.as_str()) else {
                    continue;
                };
                let Some(right) = memory_lookup.get(memory_b.as_str()) else {
                    continue;
                };
                let Some((entity_name, entity_type)) = entity_lookup.get(&left.entity_id).cloned()
                else {
                    continue;
                };
                let (preferred_memory_id, preferred_fact, preferred_reason) =
                    preferred_contradiction_resolution(left, right);
                let memory_a_evidence =
                    contradiction_evidence_summaries(evidence_lookup.get(&left.id));
                let memory_b_evidence =
                    contradiction_evidence_summaries(evidence_lookup.get(&right.id));
                let investigation_summary = contradiction_investigation_summary(
                    left,
                    right,
                    &memory_a_evidence,
                    &memory_b_evidence,
                    preferred_reason.as_deref(),
                );
                let (resolution_confidence_percent, needs_review) = contradiction_review_assessment(
                    preferred_memory_id.as_deref(),
                    left,
                    right,
                    &memory_a_evidence,
                    &memory_b_evidence,
                );
                contradictions.push(ContradictionSummary {
                    entity_id: left.entity_id.clone(),
                    entity_name,
                    entity_type,
                    memory_a_id: left.id.clone(),
                    memory_a_fact: left.fact.clone(),
                    memory_b_id: right.id.clone(),
                    memory_b_fact: right.fact.clone(),
                    description,
                    suggestion: proposal.suggestion,
                    needs_review,
                    resolution_confidence_percent,
                    preferred_memory_id,
                    preferred_fact,
                    preferred_reason,
                    investigation_summary,
                    memory_a_evidence,
                    memory_b_evidence,
                });
            }
        }

        contradictions.sort_by(|a, b| {
            a.entity_name
                .cmp(&b.entity_name)
                .then_with(|| a.memory_a_id.cmp(&b.memory_a_id))
                .then_with(|| a.memory_b_id.cmp(&b.memory_b_id))
        });

        let contradiction_count = contradictions.len();
        let review_count = contradictions.iter().filter(|row| row.needs_review).count();
        let entities_with_contradictions = contradictions
            .iter()
            .map(|row| row.entity_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();

        Ok(MemoryIntegritySnapshot {
            tracked_entity_count,
            entities_with_contradictions,
            contradiction_count,
            review_count,
            contradictions: contradictions.into_iter().take(limit).collect(),
        })
    }
}

fn contradiction_evidence_summaries(
    evidence: Option<&Vec<Evidence>>,
) -> Vec<ContradictionEvidenceSummary> {
    evidence
        .into_iter()
        .flat_map(|items| items.iter())
        .take(2)
        .map(|evidence| ContradictionEvidenceSummary {
            source_label: contradiction_source_label(evidence),
            source_url: evidence.source_url.clone(),
            evidence_quote: evidence.evidence_quote.clone(),
        })
        .collect()
}

fn contradiction_source_label(evidence: &Evidence) -> String {
    if let Some(url) = evidence.source_url.as_deref() {
        let without_scheme = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(url);
        let host = without_scheme
            .split('/')
            .next()
            .unwrap_or(without_scheme)
            .trim();
        if !host.is_empty() {
            return host.to_string();
        }
    }
    if let Some(article_id) = evidence.article_id.as_deref() {
        return article_id.to_string();
    }
    "archive".to_string()
}

fn contradiction_investigation_summary(
    left: &Memory,
    right: &Memory,
    left_evidence: &[ContradictionEvidenceSummary],
    right_evidence: &[ContradictionEvidenceSummary],
    preferred_reason: Option<&str>,
) -> String {
    let mut parts = vec![format!(
        "Compared two active facts for the same entity with {} vs {} supporting evidence item{}.",
        left_evidence.len(),
        right_evidence.len(),
        if left_evidence.len() == 1 && right_evidence.len() == 1 {
            ""
        } else {
            "s"
        }
    )];
    parts.push(format!(
        "Confidence is {:.2} vs {:.2}; disposition is {} vs {}; authorship is {} vs {}.",
        left.confidence,
        right.confidence,
        left.disposition.as_str(),
        right.disposition.as_str(),
        left.authored_by.as_deref().unwrap_or("unknown"),
        right.authored_by.as_deref().unwrap_or("unknown"),
    ));
    if let Some(reason) = preferred_reason {
        parts.push(reason.to_string());
    } else {
        parts.push(
            "No clear canonical winner was selected automatically, so this still needs review in the brief."
                .to_string(),
        );
    }
    parts.join(" ")
}

fn contradiction_review_assessment(
    preferred_memory_id: Option<&str>,
    left: &Memory,
    right: &Memory,
    left_evidence: &[ContradictionEvidenceSummary],
    right_evidence: &[ContradictionEvidenceSummary],
) -> (u8, bool) {
    let Some(preferred_memory_id) = preferred_memory_id else {
        return (0, true);
    };

    let (winner, loser, winner_evidence, loser_evidence) = if preferred_memory_id == left.id {
        (left, right, left_evidence, right_evidence)
    } else {
        (right, left, right_evidence, left_evidence)
    };

    let mut confidence_score: i32 = 45;
    let disposition_delta = contradiction_disposition_weight(winner.disposition)
        - contradiction_disposition_weight(loser.disposition);
    if disposition_delta > 0 {
        confidence_score += 20;
    }
    let author_delta = contradiction_author_weight(winner.authored_by.as_deref())
        - contradiction_author_weight(loser.authored_by.as_deref());
    if author_delta > 0 {
        confidence_score += 15;
    }

    let confidence_delta = winner.confidence - loser.confidence;
    if confidence_delta >= 0.25 {
        confidence_score += 15;
    } else if confidence_delta >= 0.15 {
        confidence_score += 10;
    } else if confidence_delta >= 0.05 {
        confidence_score += 5;
    }

    if winner.updated_at > loser.updated_at {
        confidence_score += 5;
    }

    if winner_evidence.len() > loser_evidence.len() {
        confidence_score += 10;
    } else if !winner_evidence.is_empty() && loser_evidence.is_empty() {
        confidence_score += 8;
    } else if !winner_evidence.is_empty() && !loser_evidence.is_empty() {
        confidence_score += 3;
    }

    let confidence_percent = confidence_score.clamp(0, 95) as u8;
    let needs_review = confidence_percent < 80;
    (confidence_percent, needs_review)
}

fn preferred_contradiction_resolution(
    left: &Memory,
    right: &Memory,
) -> (Option<String>, Option<String>, Option<String>) {
    let left_disposition = contradiction_disposition_weight(left.disposition);
    let right_disposition = contradiction_disposition_weight(right.disposition);
    let left_author = contradiction_author_weight(left.authored_by.as_deref());
    let right_author = contradiction_author_weight(right.authored_by.as_deref());
    let newer_left = (left.updated_at > right.updated_at) as i32;
    let newer_right = (right.updated_at > left.updated_at) as i32;

    let confidence_delta = left.confidence - right.confidence;
    let (left_confidence, right_confidence) = if confidence_delta >= 0.15 {
        (2, 0)
    } else if confidence_delta <= -0.15 {
        (0, 2)
    } else if confidence_delta >= 0.05 {
        (1, 0)
    } else if confidence_delta <= -0.05 {
        (0, 1)
    } else {
        (0, 0)
    };

    let left_score = left_disposition + left_author + left_confidence + newer_left;
    let right_score = right_disposition + right_author + right_confidence + newer_right;

    let (winner, loser) = if left_score >= right_score + 2 {
        (left, right)
    } else if right_score >= left_score + 2 {
        (right, left)
    } else {
        return (None, None, None);
    };

    let mut reasons = Vec::new();
    if contradiction_disposition_weight(winner.disposition)
        > contradiction_disposition_weight(loser.disposition)
    {
        reasons.push(match winner.disposition {
            FactDisposition::UserConfirmed => "it is user-confirmed".to_string(),
            FactDisposition::ReviewFlagged => "it is already marked for human review".to_string(),
            FactDisposition::AutoStored => "its disposition is stronger".to_string(),
        });
    }
    if contradiction_author_weight(winner.authored_by.as_deref())
        > contradiction_author_weight(loser.authored_by.as_deref())
    {
        reasons.push("it was authored by the user".to_string());
    }
    let winner_confidence = winner.confidence;
    let loser_confidence = loser.confidence;
    if winner_confidence >= loser_confidence + 0.05 {
        reasons.push(format!(
            "it has higher confidence ({winner_confidence:.2} vs {loser_confidence:.2})"
        ));
    }
    if winner.updated_at > loser.updated_at {
        reasons.push("it is newer".to_string());
    }

    let reason = if reasons.is_empty() {
        None
    } else {
        Some(format!("Preferred because {}.", reasons.join(", ")))
    };

    (Some(winner.id.clone()), Some(winner.fact.clone()), reason)
}

fn contradiction_disposition_weight(disposition: FactDisposition) -> i32 {
    match disposition {
        FactDisposition::UserConfirmed => 3,
        FactDisposition::ReviewFlagged => 1,
        FactDisposition::AutoStored => 0,
    }
}

fn contradiction_author_weight(authored_by: Option<&str>) -> i32 {
    match authored_by {
        Some(author) if author.eq_ignore_ascii_case("user") => 2,
        _ => 0,
    }
}

fn maintenance_keywords(name: &str, facts: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "been", "being", "but", "by", "can", "could",
        "did", "do", "does", "for", "from", "had", "has", "have", "how", "into", "is", "it", "its",
        "may", "might", "not", "of", "on", "or", "our", "should", "so", "that", "the", "their",
        "them", "there", "they", "this", "to", "was", "were", "what", "when", "where", "which",
        "who", "will", "with", "would", "you", "your",
    ];

    let mut keywords = name
        .split(|c: char| !c.is_alphanumeric())
        .chain(facts.split(|c: char| !c.is_alphanumeric()))
        .map(|word| word.trim().to_lowercase())
        .filter(|word| word.len() > 2 && !STOPWORDS.contains(&word.as_str()))
        .collect::<Vec<_>>();
    keywords.sort();
    keywords.dedup();
    keywords
}

/// Apply the SQLCipher encryption key to an open connection.
///
/// Must be called before any other SQL statement on the connection.
/// The key should be a 64-character hex string (256-bit key).
fn apply_cipher_key(conn: &Connection, hex_key: &str) -> Result<(), CipherError> {
    let pragma = format!("PRAGMA key = \"x'{hex_key}'\";");
    conn.execute_batch(&pragma)
        .map_err(|e| CipherError::KeyFailed(e.to_string()))?;

    // Verify the key works by running a read operation
    conn.execute_batch("SELECT count(*) FROM sqlite_master;")
        .map_err(|e| CipherError::KeyFailed(format!("key verification failed: {e}")))?;

    Ok(())
}

/// Check whether `PRAGMA cipher_version` returns a value (SQLCipher is active).
fn check_cipher_active(conn: &Connection) -> bool {
    conn.query_row("PRAGMA cipher_version;", [], |row| row.get::<_, String>(0))
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// Detect whether an existing database file is unencrypted.
///
/// Opens the file without a key and tries to read `sqlite_master`.
/// Returns `true` if the file is readable without a key (plaintext SQLite),
/// `false` if it appears encrypted.
///
/// Returns an error for filesystem/IO issues.
pub fn is_database_unencrypted(path: &Path) -> Result<bool, CipherError> {
    if !path.exists() {
        // No file yet — it will be created fresh as encrypted.
        return Ok(false);
    }

    let conn = Connection::open(path).map_err(|e| CipherError::MigrationFailed(e.to_string()))?;

    match conn.execute_batch("SELECT count(*) FROM sqlite_master;") {
        Ok(_) => Ok(true),   // Readable without key — plaintext
        Err(_) => Ok(false), // Not readable — likely encrypted
    }
}

/// Migrate an unencrypted SQLite database to an encrypted SQLCipher database.
///
/// This uses the SQLCipher `sqlcipher_export()` mechanism:
/// 1. Open the plaintext database
/// 2. Attach a new encrypted database
/// 3. Export all data to the encrypted database
/// 4. Replace the original file with the encrypted one
///
/// The caller is responsible for ensuring the database is not in use by other
/// connections during migration.
pub fn migrate_to_encrypted(path: &Path, hex_key: &str) -> Result<(), CipherError> {
    if !path.exists() {
        return Err(CipherError::MigrationFailed(
            "database file does not exist".to_string(),
        ));
    }

    let encrypted_path = path.with_extension("db.encrypted");

    // Open the plaintext database
    let conn = Connection::open(path).map_err(|e| CipherError::MigrationFailed(e.to_string()))?;

    // Attach a new encrypted database
    let attach_sql = format!(
        "ATTACH DATABASE '{}' AS encrypted KEY \"x'{}'\";",
        encrypted_path.display(),
        hex_key
    );
    conn.execute_batch(&attach_sql)
        .map_err(|e| CipherError::MigrationFailed(format!("attach encrypted DB: {e}")))?;

    // Export all data
    conn.execute_batch("SELECT sqlcipher_export('encrypted');")
        .map_err(|e| CipherError::MigrationFailed(format!("sqlcipher_export: {e}")))?;

    // Detach
    conn.execute_batch("DETACH DATABASE encrypted;")
        .map_err(|e| CipherError::MigrationFailed(format!("detach: {e}")))?;

    drop(conn);

    // Replace the original file with the encrypted one
    std::fs::rename(&encrypted_path, path)
        .map_err(|e| CipherError::MigrationFailed(format!("rename: {e}")))?;

    // Verify the new file is encrypted (should not be readable without key)
    let verify_conn =
        Connection::open(path).map_err(|e| CipherError::MigrationFailed(e.to_string()))?;
    match verify_conn.execute_batch("SELECT count(*) FROM sqlite_master;") {
        Ok(_) => Err(CipherError::MigrationFailed(
            "migration verification failed: database still readable without key".to_string(),
        )),
        Err(_) => Ok(()), // Good — not readable without key
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall_probes::RecallProbeTargetKind;
    use crate::store::MemoryStore;
    use chrono::Utc;

    fn test_hex_key() -> String {
        // 256-bit key as 64 hex characters
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string()
    }

    fn wrong_hex_key() -> String {
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_string()
    }

    fn now() -> String {
        Utc::now().to_rfc3339()
    }

    fn make_entity(name: &str) -> Entity {
        let ts = now();
        Entity {
            id: uuid::Uuid::new_v4().to_string(),
            entity_type: EntityType::Person,
            name: name.to_string(),
            attributes: serde_json::json!({}),
            sensitivity: Sensitivity::Private,
            allowed_models: AllowedModels::LocalOnly,
            space: MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: ts.clone(),
            updated_at: ts,
        }
    }

    fn make_evidence(memory_id: &str) -> Evidence {
        let ts = now();
        Evidence {
            id: uuid::Uuid::new_v4().to_string(),
            memory_id: Some(memory_id.to_string()),
            relationship_id: None,
            entity_id: None,
            article_id: Some("art-001".to_string()),
            source_url: Some("https://example.com".to_string()),
            evidence_quote: Some("evidence text".to_string()),
            observed_at: ts.clone(),
            created_at: ts,
        }
    }

    fn make_memory(id: &str, entity_id: &str, fact: &str) -> Memory {
        let ts = now();
        Memory {
            id: id.to_string(),
            entity_id: entity_id.to_string(),
            fact: fact.to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts,
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        }
    }

    // --- Encrypted store tests ---

    #[tokio::test]
    async fn encrypted_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("encrypted.db");
        let key = test_hex_key();

        // Create encrypted store, add data
        {
            let store = SqliteMemoryStore::open_encrypted(&db_path, &key).unwrap();
            store.initialize().await.unwrap();

            let entity = make_entity("Alice");
            store.create_entity(&entity).await.unwrap();

            let mem = make_memory(
                &uuid::Uuid::new_v4().to_string(),
                &entity.id,
                "Alice likes encrypted storage",
            );
            let ev = make_evidence(&mem.id);
            store.create_memory(&mem, &[ev]).await.unwrap();

            assert!(store.is_encrypted().await);
        }

        // Reopen with same key — should work
        {
            let store = SqliteMemoryStore::open_encrypted(&db_path, &key).unwrap();
            store.initialize().await.unwrap();

            let entities = store
                .find_entities_by_type(EntityType::Person, 10)
                .await
                .unwrap();
            assert_eq!(entities.len(), 1);
            assert_eq!(entities[0].name, "Alice");

            let memories = store.get_memories(&entities[0].id, None).await.unwrap();
            assert_eq!(memories.len(), 1);
            assert_eq!(memories[0].fact, "Alice likes encrypted storage");
        }
    }

    #[tokio::test]
    async fn encrypted_store_wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("encrypted.db");
        let key = test_hex_key();

        // Create with correct key
        {
            let store = SqliteMemoryStore::open_encrypted(&db_path, &key).unwrap();
            store.initialize().await.unwrap();
            let entity = make_entity("Alice");
            store.create_entity(&entity).await.unwrap();
        }

        // Open with wrong key — should fail
        let result = SqliteMemoryStore::open_encrypted(&db_path, &wrong_hex_key());
        assert!(result.is_err(), "opening with wrong key should fail");
    }

    #[tokio::test]
    async fn encrypted_store_no_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("encrypted.db");
        let key = test_hex_key();

        // Create encrypted store
        {
            let store = SqliteMemoryStore::open_encrypted(&db_path, &key).unwrap();
            store.initialize().await.unwrap();
            let entity = make_entity("Alice");
            store.create_entity(&entity).await.unwrap();
        }

        // Open without key. Some SQLCipher builds reject this immediately at
        // open time ("file is not a database"), while others allow opening and
        // only fail during initialization or reads. Both behaviors are valid as
        // long as plaintext access is denied.
        match SqliteMemoryStore::open(&db_path) {
            Err(_) => {}
            Ok(store) => {
                let result = store.initialize().await;
                if result.is_ok() {
                    let entities = store.find_entities_by_type(EntityType::Person, 10).await;
                    assert!(
                        entities.is_err(),
                        "reading encrypted DB without key should fail"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn graph_maintenance_snapshot_collects_keywords_and_edges() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();

        let vault = make_entity("Vault Access Broker");
        let gateway = make_entity("Credential Gateway");
        store.create_entity(&vault).await.unwrap();
        store.create_entity(&gateway).await.unwrap();

        let mem = make_memory(
            "mem-vault",
            &vault.id,
            "Vault broker protects credential sessions for secure access",
        );
        store
            .create_memory(&mem, &[make_evidence(&mem.id)])
            .await
            .unwrap();
        store
            .create_relationship(&Relationship {
                id: "rel-vault-gateway".to_string(),
                from_entity: vault.id.clone(),
                to_entity: gateway.id.clone(),
                relation_type: "supports".to_string(),
                strength: 0.9,
                valid_from: now(),
                valid_to: None,
                sensitivity: Sensitivity::Private,
                allowed_models: AllowedModels::LocalOnly,
                status: RelationshipStatus::Active,
                created_at: now(),
                updated_at: now(),
            })
            .await
            .unwrap();

        let snapshot = store.graph_maintenance_snapshot().await.unwrap();
        assert_eq!(snapshot.nodes.len(), 2);
        assert_eq!(snapshot.edges.len(), 1);
        let vault_node = snapshot
            .nodes
            .iter()
            .find(|node| node.entity_id == vault.id)
            .expect("vault node");
        assert_eq!(vault_node.memory_count, 1);
        assert!(vault_node.keywords.contains(&"credential".to_string()));
        assert!(vault_node.keywords.contains(&"vault".to_string()));
    }

    #[tokio::test]
    async fn recall_probe_graph_subjects_return_active_entities_with_active_memories() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();

        let entity = make_entity("Tokio");
        store.create_entity(&entity).await.unwrap();

        let mem = make_memory(
            &uuid::Uuid::new_v4().to_string(),
            &entity.id,
            "Tokio powers the async runtime",
        );
        store
            .create_memory(&mem, &[make_evidence(&mem.id)])
            .await
            .unwrap();

        let subjects = store.recall_probe_graph_subjects(10).await.unwrap();
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].target_kind, RecallProbeTargetKind::GraphEntity);
        assert_eq!(subjects[0].target_id, entity.id);
        assert_eq!(subjects[0].title, "Tokio");
        assert!(subjects[0].content.contains("async runtime"));
    }

    #[tokio::test]
    async fn memory_integrity_snapshot_surfaces_active_contradictions() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();

        let entity = make_entity("Vue");
        store.create_entity(&entity).await.unwrap();

        let active_a = Memory {
            confidence: 0.61,
            updated_at: "2026-04-05T00:00:00Z".to_string(),
            ..make_memory("mem-a", &entity.id, "Vue is no longer SSR-friendly.")
        };
        let active_b = Memory {
            confidence: 0.92,
            updated_at: "2026-04-06T00:00:00Z".to_string(),
            ..make_memory("mem-b", &entity.id, "Vue is SSR-friendly.")
        };
        let archived = Memory {
            id: "mem-c".to_string(),
            entity_id: entity.id.clone(),
            fact: "Vue has poor tooling.".to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: now(),
            valid_to: None,
            status: MemoryStatus::Archived,
            superseded_by: None,
            created_at: now(),
            updated_at: now(),
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        };

        for memory in [&active_a, &active_b, &archived] {
            store
                .create_memory(memory, &[make_evidence(&memory.id)])
                .await
                .unwrap();
        }

        let snapshot = store.memory_integrity_snapshot(10).await.unwrap();
        assert_eq!(snapshot.tracked_entity_count, 1);
        assert_eq!(snapshot.entities_with_contradictions, 1);
        assert_eq!(snapshot.contradiction_count, 1);
        assert_eq!(snapshot.review_count, 1);
        assert_eq!(snapshot.contradictions.len(), 1);
        assert_eq!(snapshot.contradictions[0].entity_name, "Vue");
        assert_eq!(snapshot.contradictions[0].memory_a_id, "mem-a");
        assert_eq!(snapshot.contradictions[0].memory_b_id, "mem-b");
        assert!(snapshot.contradictions[0].needs_review);
        assert_eq!(snapshot.contradictions[0].resolution_confidence_percent, 68);
        assert_eq!(
            snapshot.contradictions[0].preferred_memory_id.as_deref(),
            Some("mem-b")
        );
        assert_eq!(
            snapshot.contradictions[0].preferred_fact.as_deref(),
            Some("Vue is SSR-friendly.")
        );
        assert!(snapshot.contradictions[0]
            .preferred_reason
            .as_deref()
            .unwrap_or_default()
            .contains("higher confidence"));
        assert!(snapshot.contradictions[0].suggestion.contains("Resolve"));
        assert!(snapshot.contradictions[0]
            .investigation_summary
            .contains("Compared two active facts"));
        assert_eq!(snapshot.contradictions[0].memory_a_evidence.len(), 1);
        assert_eq!(snapshot.contradictions[0].memory_b_evidence.len(), 1);
        assert_eq!(
            snapshot.contradictions[0].memory_a_evidence[0].source_label,
            "example.com"
        );
    }

    #[tokio::test]
    async fn memory_integrity_snapshot_downgrades_clear_winner_from_review_queue() {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();

        let entity = make_entity("Binance API");
        store.create_entity(&entity).await.unwrap();

        let decisive = Memory {
            confidence: 0.95,
            disposition: FactDisposition::UserConfirmed,
            authored_by: Some("user".to_string()),
            updated_at: "2026-04-06T00:00:00Z".to_string(),
            ..make_memory(
                "mem-strong",
                &entity.id,
                "Spot REST uses request weight over plain request count.",
            )
        };
        let weak = Memory {
            confidence: 0.52,
            updated_at: "2026-04-04T00:00:00Z".to_string(),
            ..make_memory(
                "mem-weak",
                &entity.id,
                "Spot REST uses plain request count over request weight.",
            )
        };

        store
            .create_memory(
                &decisive,
                &[
                    make_evidence(&decisive.id),
                    Evidence {
                        id: "ev-strong-2".to_string(),
                        memory_id: Some(decisive.id.clone()),
                        relationship_id: None,
                        entity_id: None,
                        article_id: Some("article-2".to_string()),
                        source_url: Some("https://docs.binance.com".to_string()),
                        evidence_quote: Some("Weight quote".to_string()),
                        observed_at: now(),
                        created_at: now(),
                    },
                ],
            )
            .await
            .unwrap();
        store
            .create_memory(&weak, &[make_evidence(&weak.id)])
            .await
            .unwrap();

        let snapshot = store.memory_integrity_snapshot(10).await.unwrap();
        assert_eq!(snapshot.contradiction_count, 1);
        assert_eq!(snapshot.review_count, 0);
        assert_eq!(snapshot.contradictions.len(), 1);
        assert!(!snapshot.contradictions[0].needs_review);
        assert!(snapshot.contradictions[0].resolution_confidence_percent >= 80);
        assert_eq!(
            snapshot.contradictions[0].preferred_memory_id.as_deref(),
            Some("mem-strong")
        );
    }

    #[test]
    fn detect_unencrypted_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("plain.db");

        // Create plaintext database
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE test (id INTEGER);")
                .unwrap();
        }

        assert!(
            is_database_unencrypted(&db_path).unwrap(),
            "plaintext DB should be detected as unencrypted"
        );
    }

    #[test]
    fn detect_encrypted_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cipher.db");
        let key = test_hex_key();

        // Create encrypted database
        {
            let conn = Connection::open(&db_path).unwrap();
            apply_cipher_key(&conn, &key).unwrap();
            conn.execute_batch("CREATE TABLE test (id INTEGER);")
                .unwrap();
        }

        assert!(
            !is_database_unencrypted(&db_path).unwrap(),
            "encrypted DB should not be detected as unencrypted"
        );
    }

    #[test]
    fn detect_nonexistent_database_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("does_not_exist.db");

        assert!(
            !is_database_unencrypted(&db_path).unwrap(),
            "nonexistent DB should return false (will be created fresh)"
        );
    }

    #[test]
    fn migrate_plaintext_to_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("migrate.db");
        let key = test_hex_key();

        // Create plaintext database with data
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE test (id INTEGER, name TEXT);
                 INSERT INTO test VALUES (1, 'alice');
                 INSERT INTO test VALUES (2, 'bob');",
            )
            .unwrap();
        }

        // Verify it's plaintext
        assert!(is_database_unencrypted(&db_path).unwrap());

        // Migrate to encrypted
        migrate_to_encrypted(&db_path, &key).unwrap();

        // Verify it's no longer plaintext
        assert!(!is_database_unencrypted(&db_path).unwrap());

        // Verify data is accessible with key
        let conn = Connection::open(&db_path).unwrap();
        apply_cipher_key(&conn, &key).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM test", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn migrate_nonexistent_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nope.db");

        let result = migrate_to_encrypted(&db_path, &test_hex_key());
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn encrypted_store_fts5_works() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("fts.db");
        let key = test_hex_key();

        let store = SqliteMemoryStore::open_encrypted(&db_path, &key).unwrap();
        store.initialize().await.unwrap();

        let entity = make_entity("Alice");
        store.create_entity(&entity).await.unwrap();

        let ts = now();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            entity_id: entity.id.clone(),
            fact: "Alice is an expert Rust developer".to_string(),
            confidence: 0.95,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Private,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts,
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        };
        let ev = make_evidence(&mem.id);
        store.create_memory(&mem, &[ev]).await.unwrap();

        // FTS5 search should work on encrypted DB
        let results = store.search_memories("Rust", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].fact.contains("Rust"));

        // Entity FTS5 search should also work
        let entities = store.find_entities("Alice", 10).await.unwrap();
        assert_eq!(entities.len(), 1);
    }

    #[tokio::test]
    async fn cipher_version_available() {
        // Verify SQLCipher is actually compiled in by checking PRAGMA cipher_version
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("cipher_version.db");
        let key = test_hex_key();

        let store = SqliteMemoryStore::open_encrypted(&db_path, &key).unwrap();
        assert!(
            store.is_encrypted().await,
            "cipher_version should be available when opened with key"
        );
    }
}
