//! LLM I/O Audit Trail — structured logging for every LLM request/response.
//!
//! Provides forensic review and debugging capabilities for all LLM interactions
//! routed through the JSON-RPC gateway. Follows the `VmAuditEntry` pattern from
//! `symbiotic-vm`.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Audit verbosity level for LLM interactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmAuditLevel {
    /// No audit entries are recorded.
    Off,
    /// Record metadata only (no prompt/completion content).
    MetadataOnly,
    /// Record full prompt and completion text alongside metadata.
    FullContent,
}

/// Distinguishes normal LLM chat entries from startup integrity attestations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LlmAuditEntryKind {
    #[default]
    Chat,
    ModelVerification,
}

/// Verification result for a local model digest check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelVerificationStatus {
    Verified,
    Mismatch,
    ManifestMissing,
    ModelMissing,
    DigestMissing,
    ProviderUnavailable,
}

/// Structured metadata for a model integrity verification attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelVerification {
    pub provider: String,
    pub model_name: String,
    pub expected_digest: Option<String>,
    pub actual_digest: Option<String>,
    pub matched: bool,
    pub status: ModelVerificationStatus,
    pub detail: Option<String>,
}

/// A single audit entry for an LLM chat request/response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmAuditEntry {
    /// Unix epoch seconds when the request completed.
    pub timestamp: u64,
    /// Type of audit entry.
    #[serde(default)]
    pub kind: LlmAuditEntryKind,
    /// Agent that initiated the request.
    pub agent_id: String,
    /// Model identifier used for the completion.
    pub model: String,
    /// SHA-256 hex digest of the full prompt text.
    pub prompt_hash: String,
    /// Size of the completion response in bytes.
    pub completion_size: usize,
    /// Token count returned by the provider (if available).
    pub token_count: Option<u64>,
    /// Wall-clock latency of the LLM call in milliseconds.
    pub latency_ms: u64,
    /// Whether the call succeeded.
    pub success: bool,
    /// Error message if the call failed.
    pub error: Option<String>,
    /// Full prompt text (only populated when level is `FullContent`).
    pub prompt_text: Option<String>,
    /// Full completion text (only populated when level is `FullContent`).
    pub completion_text: Option<String>,
    /// Structured model integrity metadata for verification events.
    pub verification: Option<ModelVerification>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmAuditQuery {
    pub kind: Option<LlmAuditEntryKind>,
    pub agent_id: Option<String>,
    pub model: Option<String>,
    pub success: Option<bool>,
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmAuditModelCount {
    pub model: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmAuditSummary {
    pub total_calls: usize,
    pub failure_count: usize,
    pub success_rate: f64,
    pub avg_latency_ms: u64,
    pub top_models: Vec<LlmAuditModelCount>,
}

/// Append-only in-memory audit log for LLM interactions.
pub struct LlmAuditLog {
    level: LlmAuditLevel,
    retention_days: u64,
    entries: Vec<LlmAuditEntry>,
    conn: Option<Connection>,
    next_retention_check_epoch: u64,
}

impl LlmAuditLog {
    /// Create a new audit log with the given verbosity level and retention policy.
    pub fn new(level: LlmAuditLevel, retention_days: u64) -> Self {
        Self {
            level,
            retention_days,
            entries: Vec::new(),
            conn: None,
            next_retention_check_epoch: 0,
        }
    }

    /// Open or create a durable audit log backed by SQLite.
    pub fn open(
        path: impl AsRef<Path>,
        level: LlmAuditLevel,
        retention_days: u64,
    ) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| rusqlite::Error::InvalidPath(parent.to_path_buf()))?;
        }

        let conn = Connection::open(path)?;
        init_schema(&conn)?;

        let now_epoch = now_epoch_secs();
        prune_expired(&conn, retention_days, now_epoch)?;
        let entries = load_entries(&conn, level)?;

        Ok(Self {
            level,
            retention_days,
            entries,
            conn: Some(conn),
            next_retention_check_epoch: now_epoch + 3600,
        })
    }

    /// Record an audit entry. Respects the configured audit level:
    /// - `Off`: entry is silently dropped.
    /// - `MetadataOnly`: prompt_text and completion_text are stripped before storage.
    /// - `FullContent`: entry is stored as-is.
    pub fn record(&mut self, mut entry: LlmAuditEntry) {
        match self.level {
            LlmAuditLevel::Off => return,
            LlmAuditLevel::MetadataOnly => {
                entry.prompt_text = None;
                entry.completion_text = None;
            }
            LlmAuditLevel::FullContent => { /* keep everything */ }
        }

        if let Some(conn) = &self.conn {
            if entry.timestamp >= self.next_retention_check_epoch {
                if let Err(error) = prune_expired(conn, self.retention_days, entry.timestamp) {
                    tracing::warn!(error = %error, "llm_audit: failed to prune expired entries");
                }
                self.entries.retain(|stored| {
                    is_retained(stored.timestamp, self.retention_days, entry.timestamp)
                });
                self.next_retention_check_epoch = entry.timestamp + 3600;
            }

            if let Err(error) = persist_entry(conn, &entry) {
                tracing::warn!(
                    error = %error,
                    agent_id = %entry.agent_id,
                    model = %entry.model,
                    "llm_audit: failed to persist audit entry"
                );
            }
        }

        self.entries.push(entry);
    }

    /// Record a startup or reload-time model verification as an audit entry.
    pub fn record_model_verification(&mut self, verification: ModelVerification, timestamp: u64) {
        let success = verification.matched;
        let error = if success {
            None
        } else {
            Some(match &verification.detail {
                Some(detail) if !detail.trim().is_empty() => detail.clone(),
                _ => format!("{:?}", verification.status).to_lowercase(),
            })
        };
        self.record(LlmAuditEntry {
            timestamp,
            kind: LlmAuditEntryKind::ModelVerification,
            agent_id: "system:model-integrity".to_string(),
            model: verification.model_name.clone(),
            prompt_hash: sha256_hex(&format!(
                "model_verification:{}:{}:{}",
                verification.provider,
                verification.model_name,
                verification.actual_digest.as_deref().unwrap_or("unknown"),
            )),
            completion_size: 0,
            token_count: None,
            latency_ms: 0,
            success,
            error,
            prompt_text: None,
            completion_text: None,
            verification: Some(verification),
        });
    }

    /// Return all recorded entries.
    pub fn entries(&self) -> &[LlmAuditEntry] {
        &self.entries
    }

    /// Return entries with timestamp >= `since_epoch_secs`.
    pub fn entries_since(&self, since_epoch_secs: u64) -> Vec<&LlmAuditEntry> {
        self.entries
            .iter()
            .filter(|e| e.timestamp >= since_epoch_secs)
            .collect()
    }

    pub fn query(&self, query: &LlmAuditQuery) -> Vec<LlmAuditEntry> {
        let limit = query.limit.unwrap_or(100);
        self.entries
            .iter()
            .rev()
            .filter(|entry| query.kind.unwrap_or(LlmAuditEntryKind::Chat) == entry.kind)
            .filter(|entry| {
                query
                    .agent_id
                    .as_ref()
                    .is_none_or(|agent_id| entry.agent_id == *agent_id)
            })
            .filter(|entry| {
                query
                    .model
                    .as_ref()
                    .is_none_or(|model| entry.model == *model)
            })
            .filter(|entry| query.success.is_none_or(|success| entry.success == success))
            .filter(|entry| query.since.is_none_or(|since| entry.timestamp >= since))
            .filter(|entry| query.until.is_none_or(|until| entry.timestamp <= until))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn summary_since(&self, since: Option<u64>) -> LlmAuditSummary {
        let entries: Vec<&LlmAuditEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.kind == LlmAuditEntryKind::Chat)
            .filter(|entry| since.is_none_or(|cutoff| entry.timestamp >= cutoff))
            .collect();

        if entries.is_empty() {
            return LlmAuditSummary {
                total_calls: 0,
                failure_count: 0,
                success_rate: 0.0,
                avg_latency_ms: 0,
                top_models: Vec::new(),
            };
        }

        let total_calls = entries.len();
        let failure_count = entries.iter().filter(|entry| !entry.success).count();
        let success_rate = (total_calls - failure_count) as f64 / total_calls as f64;
        let avg_latency_ms =
            entries.iter().map(|entry| entry.latency_ms).sum::<u64>() / total_calls as u64;

        let mut counts: HashMap<String, usize> = HashMap::new();
        for entry in &entries {
            *counts.entry(entry.model.clone()).or_insert(0) += 1;
        }
        let mut top_models: Vec<LlmAuditModelCount> = counts
            .into_iter()
            .map(|(model, count)| LlmAuditModelCount { model, count })
            .collect();
        top_models.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.model.cmp(&b.model)));

        LlmAuditSummary {
            total_calls,
            failure_count,
            success_rate,
            avg_latency_ms,
            top_models,
        }
    }

    /// Current audit level.
    pub fn level(&self) -> LlmAuditLevel {
        self.level
    }
}

/// Compute SHA-256 hex digest of the given text.
pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS llm_audit (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp       INTEGER NOT NULL,
            entry_kind      TEXT NOT NULL DEFAULT 'chat',
            agent_id        TEXT NOT NULL,
            model           TEXT NOT NULL,
            prompt_hash     TEXT NOT NULL,
            completion_size INTEGER NOT NULL,
            token_count     INTEGER,
            latency_ms      INTEGER NOT NULL,
            success         INTEGER NOT NULL,
            error           TEXT,
            prompt_text     TEXT,
            completion_text TEXT,
            verification_json TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_llm_audit_timestamp
            ON llm_audit (timestamp);
        CREATE INDEX IF NOT EXISTS idx_llm_audit_agent
            ON llm_audit (agent_id);
        CREATE INDEX IF NOT EXISTS idx_llm_audit_model
            ON llm_audit (model);",
    )?;
    ensure_column(
        conn,
        "llm_audit",
        "entry_kind",
        "TEXT NOT NULL DEFAULT 'chat'",
    )?;
    ensure_column(conn, "llm_audit", "verification_json", "TEXT")?;
    Ok(())
}

fn load_entries(conn: &Connection, level: LlmAuditLevel) -> rusqlite::Result<Vec<LlmAuditEntry>> {
    if matches!(level, LlmAuditLevel::Off) {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT timestamp, entry_kind, agent_id, model, prompt_hash, completion_size, token_count,
                latency_ms, success, error, prompt_text, completion_text, verification_json
         FROM llm_audit
         ORDER BY id ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        let kind_text: String = row.get(1)?;
        let verification_json: Option<String> = row.get(12)?;
        let mut entry = LlmAuditEntry {
            timestamp: row.get(0)?,
            kind: parse_entry_kind(&kind_text),
            agent_id: row.get(2)?,
            model: row.get(3)?,
            prompt_hash: row.get(4)?,
            completion_size: row.get(5)?,
            token_count: row.get(6)?,
            latency_ms: row.get(7)?,
            success: row.get::<_, i64>(8)? != 0,
            error: row.get(9)?,
            prompt_text: row.get(10)?,
            completion_text: row.get(11)?,
            verification: verification_json
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok()),
        };
        if matches!(level, LlmAuditLevel::MetadataOnly) {
            entry.prompt_text = None;
            entry.completion_text = None;
        }
        Ok(entry)
    })?;

    rows.collect()
}

fn persist_entry(conn: &Connection, entry: &LlmAuditEntry) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO llm_audit (
            timestamp, entry_kind, agent_id, model, prompt_hash, completion_size, token_count,
            latency_ms, success, error, prompt_text, completion_text, verification_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            entry.timestamp,
            entry_kind_str(entry.kind),
            &entry.agent_id,
            &entry.model,
            &entry.prompt_hash,
            entry.completion_size as i64,
            entry.token_count.map(|count| count as i64),
            entry.latency_ms as i64,
            entry.success as i64,
            &entry.error,
            &entry.prompt_text,
            &entry.completion_text,
            entry
                .verification
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
        ],
    )?;
    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in rows {
        if name? == column {
            return Ok(());
        }
    }
    let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
    conn.execute(&alter, [])?;
    Ok(())
}

fn entry_kind_str(kind: LlmAuditEntryKind) -> &'static str {
    match kind {
        LlmAuditEntryKind::Chat => "chat",
        LlmAuditEntryKind::ModelVerification => "model_verification",
    }
}

fn parse_entry_kind(value: &str) -> LlmAuditEntryKind {
    match value {
        "model_verification" => LlmAuditEntryKind::ModelVerification,
        _ => LlmAuditEntryKind::Chat,
    }
}

fn prune_expired(conn: &Connection, retention_days: u64, now_epoch: u64) -> rusqlite::Result<()> {
    if retention_days == 0 {
        return Ok(());
    }
    let cutoff = retention_cutoff(retention_days, now_epoch);
    conn.execute(
        "DELETE FROM llm_audit WHERE timestamp < ?1",
        params![cutoff],
    )?;
    Ok(())
}

fn is_retained(timestamp: u64, retention_days: u64, now_epoch: u64) -> bool {
    if retention_days == 0 {
        return true;
    }
    timestamp >= retention_cutoff(retention_days, now_epoch)
}

fn retention_cutoff(retention_days: u64, now_epoch: u64) -> u64 {
    now_epoch.saturating_sub(retention_days.saturating_mul(86_400))
}

pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(timestamp: u64, success: bool) -> LlmAuditEntry {
        LlmAuditEntry {
            timestamp,
            kind: LlmAuditEntryKind::Chat,
            agent_id: "test-agent".to_string(),
            model: "gpt-4".to_string(),
            prompt_hash: sha256_hex("hello world"),
            completion_size: 42,
            token_count: Some(10),
            latency_ms: 150,
            success,
            error: if success {
                None
            } else {
                Some("timeout".to_string())
            },
            prompt_text: Some("hello world".to_string()),
            completion_text: Some("hi there".to_string()),
            verification: None,
        }
    }

    #[test]
    fn test_record_entry() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::FullContent, 30);
        log.record(make_entry(1000, true));
        assert_eq!(log.entries().len(), 1);
        assert_eq!(log.entries()[0].agent_id, "test-agent");
        assert_eq!(log.entries()[0].latency_ms, 150);
    }

    #[test]
    fn test_metadata_only_strips_content() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::MetadataOnly, 30);
        log.record(make_entry(1000, true));
        assert_eq!(log.entries().len(), 1);
        assert!(log.entries()[0].prompt_text.is_none());
        assert!(log.entries()[0].completion_text.is_none());
        // Metadata should still be present
        assert_eq!(log.entries()[0].model, "gpt-4");
        assert_eq!(log.entries()[0].completion_size, 42);
    }

    #[test]
    fn test_full_content_preserves_text() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::FullContent, 30);
        log.record(make_entry(1000, true));
        assert_eq!(log.entries()[0].prompt_text.as_deref(), Some("hello world"));
        assert_eq!(
            log.entries()[0].completion_text.as_deref(),
            Some("hi there")
        );
    }

    #[test]
    fn test_entries_since_filtering() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::FullContent, 30);
        log.record(make_entry(100, true));
        log.record(make_entry(200, true));
        log.record(make_entry(300, false));
        log.record(make_entry(400, true));

        let since_250 = log.entries_since(250);
        assert_eq!(since_250.len(), 2);
        assert_eq!(since_250[0].timestamp, 300);
        assert_eq!(since_250[1].timestamp, 400);

        let since_400 = log.entries_since(400);
        assert_eq!(since_400.len(), 1);

        let since_500 = log.entries_since(500);
        assert!(since_500.is_empty());
    }

    #[test]
    fn test_off_mode_records_nothing() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::Off, 30);
        log.record(make_entry(1000, true));
        log.record(make_entry(2000, false));
        assert!(log.entries().is_empty());
    }

    #[test]
    fn test_sha256_hex_deterministic() {
        let hash1 = sha256_hex("test input");
        let hash2 = sha256_hex("test input");
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64); // SHA-256 hex = 64 chars

        // Different input produces different hash
        let hash3 = sha256_hex("different input");
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_error_entry_recorded() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::MetadataOnly, 30);
        log.record(make_entry(1000, false));
        assert_eq!(log.entries().len(), 1);
        assert!(!log.entries()[0].success);
        assert_eq!(log.entries()[0].error.as_deref(), Some("timeout"));
    }

    #[test]
    fn test_persistence_round_trip_reloads_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("llm-audit.db");
        let now_epoch = now_epoch_secs();

        {
            let mut log = LlmAuditLog::open(&path, LlmAuditLevel::MetadataOnly, 30)
                .expect("open durable log");
            log.record(make_entry(now_epoch.saturating_sub(10), true));
            log.record(make_entry(now_epoch.saturating_sub(5), false));
        }

        let log =
            LlmAuditLog::open(&path, LlmAuditLevel::MetadataOnly, 30).expect("reopen durable log");
        assert_eq!(log.entries().len(), 2);
        assert!(log
            .entries()
            .iter()
            .all(|entry| entry.prompt_text.is_none()));
        assert_eq!(log.entries()[1].error.as_deref(), Some("timeout"));
    }

    #[test]
    fn test_open_prunes_entries_older_than_retention_window() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("llm-audit.db");
        let now_epoch = now_epoch_secs();

        {
            let mut log =
                LlmAuditLog::open(&path, LlmAuditLevel::FullContent, 1).expect("open durable log");
            log.record(make_entry(now_epoch.saturating_sub(3 * 86_400), true));
            log.record(make_entry(now_epoch.saturating_sub(60), true));
        }

        let log =
            LlmAuditLog::open(&path, LlmAuditLevel::FullContent, 1).expect("reopen durable log");
        assert_eq!(log.entries().len(), 1);
        assert!(log.entries()[0].timestamp >= now_epoch.saturating_sub(86_400));
    }

    #[test]
    fn test_query_filters_latest_entries() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::FullContent, 30);
        let mut first = make_entry(100, true);
        first.agent_id = "agent-a".to_string();
        first.model = "model-a".to_string();
        let mut second = make_entry(200, false);
        second.agent_id = "agent-b".to_string();
        second.model = "model-b".to_string();
        let mut third = make_entry(300, true);
        third.agent_id = "agent-a".to_string();
        third.model = "model-b".to_string();
        log.record(first);
        log.record(second);
        log.record(third);

        let entries = log.query(&LlmAuditQuery {
            kind: None,
            agent_id: Some("agent-a".to_string()),
            model: Some("model-b".to_string()),
            success: Some(true),
            since: Some(250),
            until: None,
            limit: Some(10),
        });

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].timestamp, 300);
    }

    #[test]
    fn test_summary_since_aggregates_recent_calls() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::FullContent, 30);
        let mut old = make_entry(100, true);
        old.model = "model-a".to_string();
        old.latency_ms = 10;
        let mut recent_fail = make_entry(200, false);
        recent_fail.model = "model-b".to_string();
        recent_fail.latency_ms = 50;
        let mut recent_ok = make_entry(300, true);
        recent_ok.model = "model-b".to_string();
        recent_ok.latency_ms = 30;
        log.record(old);
        log.record(recent_fail);
        log.record(recent_ok);

        let summary = log.summary_since(Some(150));
        assert_eq!(summary.total_calls, 2);
        assert_eq!(summary.failure_count, 1);
        assert!((summary.success_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(summary.avg_latency_ms, 40);
        assert_eq!(
            summary.top_models,
            vec![LlmAuditModelCount {
                model: "model-b".to_string(),
                count: 2,
            }]
        );
    }

    #[test]
    fn test_query_defaults_to_chat_entries_only() {
        let mut log = LlmAuditLog::new(LlmAuditLevel::FullContent, 30);
        log.record(make_entry(100, true));
        log.record_model_verification(
            ModelVerification {
                provider: "ollama".to_string(),
                model_name: "qwen3.5".to_string(),
                expected_digest: Some("sha256:expected".to_string()),
                actual_digest: Some("sha256:actual".to_string()),
                matched: false,
                status: ModelVerificationStatus::Mismatch,
                detail: Some("digest mismatch".to_string()),
            },
            200,
        );

        let entries = log.query(&LlmAuditQuery::default());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, LlmAuditEntryKind::Chat);
    }

    #[test]
    fn test_model_verification_round_trip_persists_structured_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("llm-audit.db");
        let now_epoch = now_epoch_secs();

        {
            let mut log = LlmAuditLog::open(&path, LlmAuditLevel::MetadataOnly, 30)
                .expect("open durable log");
            log.record_model_verification(
                ModelVerification {
                    provider: "ollama".to_string(),
                    model_name: "qwen3.5".to_string(),
                    expected_digest: Some("sha256:expected".to_string()),
                    actual_digest: Some("sha256:actual".to_string()),
                    matched: false,
                    status: ModelVerificationStatus::Mismatch,
                    detail: Some("digest mismatch".to_string()),
                },
                now_epoch,
            );
        }

        let log =
            LlmAuditLog::open(&path, LlmAuditLevel::MetadataOnly, 30).expect("reopen durable log");
        let entries = log.query(&LlmAuditQuery {
            kind: Some(LlmAuditEntryKind::ModelVerification),
            ..LlmAuditQuery::default()
        });
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, LlmAuditEntryKind::ModelVerification);
        assert_eq!(
            entries[0]
                .verification
                .as_ref()
                .and_then(|verification| verification.actual_digest.as_deref()),
            Some("sha256:actual")
        );
    }
}
