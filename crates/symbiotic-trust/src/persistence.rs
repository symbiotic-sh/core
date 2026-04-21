//! SQLite-backed persistence for AccessBroker state and audit logging.
//!
//! Follows the same patterns as `symbiotic-metrics::store::MetricStore`.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::{AccessBroker, AccessDecision, AccessRequest, AgentTrustLevel, CapabilityToken};

/// Errors specific to trust persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
}

/// The type of audit event recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditEventType {
    /// A capability token was granted/issued.
    Grant,
    /// A capability was evaluated (access check).
    Evaluate,
    /// A capability token was revoked.
    Revoke,
    /// A capability token expired and was cleaned up.
    Expire,
}

impl AuditEventType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Grant => "grant",
            Self::Evaluate => "evaluate",
            Self::Revoke => "revoke",
            Self::Expire => "expire",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "grant" => Some(Self::Grant),
            "evaluate" => Some(Self::Evaluate),
            "revoke" => Some(Self::Revoke),
            "expire" => Some(Self::Expire),
            _ => None,
        }
    }
}

/// An entry in the audit log recording a capability operation.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// Auto-incremented row ID (populated on read).
    pub id: Option<i64>,
    /// Unix timestamp of the event.
    pub timestamp: u64,
    /// Type of audit event.
    pub event_type: AuditEventType,
    /// Subject (agent) that requested access.
    pub subject: String,
    /// Token ID used in the request.
    pub token_id: String,
    /// The capability scope requested.
    pub scope: String,
    /// Trust level required by the request.
    pub required_level: AgentTrustLevel,
    /// Goal scope of the request (None = global).
    pub goal_scope: Option<String>,
    /// Whether the operation succeeded.
    pub granted: bool,
    /// Denial reason (empty if granted).
    pub denial_reason: String,
    /// Optional context (why the operation was requested).
    pub context: String,
}

/// SQLite-backed persistence for capability tokens and audit logs.
pub struct TrustStore {
    conn: Connection,
}

impl TrustStore {
    /// Open (or create) a trust store at the given path.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                PersistenceError::Serialization(format!("failed to create directory: {e}"))
            })?;
        }
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Create an in-memory store (for testing).
    pub fn open_in_memory() -> Result<Self, PersistenceError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<(), PersistenceError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS capability_tokens (
                token_id TEXT PRIMARY KEY,
                subject TEXT NOT NULL,
                trust_level INTEGER NOT NULL,
                scopes_json TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                one_time INTEGER NOT NULL DEFAULT 0,
                consumed INTEGER NOT NULL DEFAULT 0,
                goal_scope TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_tokens_subject ON capability_tokens(subject);
            CREATE INDEX IF NOT EXISTS idx_tokens_expires ON capability_tokens(expires_at);

            CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                event_type TEXT NOT NULL DEFAULT 'evaluate',
                subject TEXT NOT NULL,
                token_id TEXT NOT NULL,
                scope TEXT NOT NULL,
                required_level INTEGER NOT NULL,
                goal_scope TEXT,
                granted INTEGER NOT NULL,
                denial_reason TEXT NOT NULL DEFAULT '',
                context TEXT NOT NULL DEFAULT ''
            );

            CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp);
            CREATE INDEX IF NOT EXISTS idx_audit_subject ON audit_log(subject);
            CREATE INDEX IF NOT EXISTS idx_audit_token ON audit_log(token_id);
            CREATE INDEX IF NOT EXISTS idx_audit_event_type ON audit_log(event_type);
            ",
        )?;

        // Migrate existing databases that lack the new columns.
        self.migrate_audit_columns()?;
        Ok(())
    }

    /// Add `event_type` and `context` columns if they don't exist (backward compat).
    fn migrate_audit_columns(&self) -> Result<(), PersistenceError> {
        // Check if event_type column exists by querying pragma.
        let has_event_type: bool = self
            .conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('audit_log') WHERE name = 'event_type'",
            )?
            .query_row([], |row| row.get::<_, i64>(0))
            .map(|c| c > 0)?;

        if !has_event_type {
            self.conn.execute_batch(
                "ALTER TABLE audit_log ADD COLUMN event_type TEXT NOT NULL DEFAULT 'evaluate';
                 ALTER TABLE audit_log ADD COLUMN context TEXT NOT NULL DEFAULT '';
                 CREATE INDEX IF NOT EXISTS idx_audit_event_type ON audit_log(event_type);",
            )?;
        }
        Ok(())
    }

    /// Save a single token (insert or replace).
    pub fn save_token(&self, token: &CapabilityToken) -> Result<(), PersistenceError> {
        let scopes_json = serde_json::to_string(&token.scopes)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

        self.conn.execute(
            "INSERT OR REPLACE INTO capability_tokens
             (token_id, subject, trust_level, scopes_json, expires_at, one_time, consumed, goal_scope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                token.token_id,
                token.subject,
                token.trust_level as i64,
                scopes_json,
                token.expires_at as i64,
                token.one_time as i64,
                token.consumed as i64,
                token.goal_scope,
            ],
        )?;
        Ok(())
    }

    /// Load all tokens from the database.
    pub fn load_tokens(&self) -> Result<Vec<CapabilityToken>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT token_id, subject, trust_level, scopes_json, expires_at, one_time, consumed, goal_scope
             FROM capability_tokens",
        )?;

        let rows = stmt.query_map([], |row| {
            let token_id: String = row.get(0)?;
            let subject: String = row.get(1)?;
            let trust_level_int: i64 = row.get(2)?;
            let scopes_json: String = row.get(3)?;
            let expires_at: i64 = row.get(4)?;
            let one_time: bool = row.get(5)?;
            let consumed: bool = row.get(6)?;
            let goal_scope: Option<String> = row.get(7)?;
            Ok((
                token_id,
                subject,
                trust_level_int,
                scopes_json,
                expires_at,
                one_time,
                consumed,
                goal_scope,
            ))
        })?;

        let mut tokens = Vec::new();
        for row_result in rows {
            let (
                token_id,
                subject,
                trust_level_int,
                scopes_json,
                expires_at,
                one_time,
                consumed,
                goal_scope,
            ) = row_result?;

            let trust_level = match trust_level_int {
                0 => AgentTrustLevel::ReadOnly,
                1 => AgentTrustLevel::ArchiveWrite,
                2 => AgentTrustLevel::CredentialAccess,
                3 => AgentTrustLevel::ExternalAct,
                other => {
                    return Err(PersistenceError::Serialization(format!(
                        "unknown trust level: {other}"
                    )))
                }
            };

            let scopes: HashSet<String> = serde_json::from_str(&scopes_json).map_err(|e| {
                PersistenceError::Serialization(format!("invalid scopes JSON: {e}"))
            })?;

            tokens.push(CapabilityToken {
                token_id,
                subject,
                trust_level,
                scopes,
                expires_at: expires_at as u64,
                one_time,
                consumed,
                goal_scope,
            });
        }

        Ok(tokens)
    }

    /// Remove a token by ID. Returns true if a token was deleted.
    pub fn remove_token(&self, token_id: &str) -> Result<bool, PersistenceError> {
        let changed = self.conn.execute(
            "DELETE FROM capability_tokens WHERE token_id = ?1",
            params![token_id],
        )?;
        Ok(changed > 0)
    }

    /// Record an audit log entry with event type and optional context.
    #[allow(clippy::too_many_arguments)]
    pub fn log_audit_entry(
        &self,
        timestamp: u64,
        event_type: AuditEventType,
        subject: &str,
        token_id: &str,
        scope: &str,
        required_level: AgentTrustLevel,
        goal_scope: Option<&str>,
        granted: bool,
        denial_reason: &str,
        context: &str,
    ) -> Result<(), PersistenceError> {
        self.conn.execute(
            "INSERT INTO audit_log
             (timestamp, event_type, subject, token_id, scope, required_level, goal_scope, granted, denial_reason, context)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                timestamp as i64,
                event_type.as_str(),
                subject,
                token_id,
                scope,
                required_level as i64,
                goal_scope,
                granted as i64,
                denial_reason,
                context,
            ],
        )?;
        Ok(())
    }

    /// Record an audit log entry (backward-compatible convenience method).
    pub fn log_audit(
        &self,
        timestamp: u64,
        request: &AccessRequest,
        token_id: &str,
        granted: bool,
        denial_reason: &str,
    ) -> Result<(), PersistenceError> {
        self.log_audit_entry(
            timestamp,
            AuditEventType::Evaluate,
            &request.subject,
            token_id,
            &request.scope,
            request.required_level,
            request.goal_scope.as_deref(),
            granted,
            denial_reason,
            "",
        )
    }

    /// Query audit log entries since a given timestamp, newest first.
    pub fn query_audit_since(&self, since: u64) -> Result<Vec<AuditEntry>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, event_type, subject, token_id, scope, required_level, goal_scope, granted, denial_reason, context
             FROM audit_log WHERE timestamp >= ?1 ORDER BY timestamp DESC",
        )?;

        let rows = stmt.query_map(params![since as i64], |row| {
            Ok(AuditRow {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                event_type: row.get(2)?,
                subject: row.get(3)?,
                token_id: row.get(4)?,
                scope: row.get(5)?,
                required_level_int: row.get(6)?,
                goal_scope: row.get(7)?,
                granted: row.get(8)?,
                denial_reason: row.get(9)?,
                context: row.get(10)?,
            })
        })?;

        rows_to_entries(rows)
    }

    /// Query audit log entries for a specific subject.
    pub fn query_audit_for_subject(
        &self,
        subject: &str,
    ) -> Result<Vec<AuditEntry>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, event_type, subject, token_id, scope, required_level, goal_scope, granted, denial_reason, context
             FROM audit_log WHERE subject = ?1 ORDER BY timestamp DESC",
        )?;

        let rows = stmt.query_map(params![subject], |row| {
            Ok(AuditRow {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                event_type: row.get(2)?,
                subject: row.get(3)?,
                token_id: row.get(4)?,
                scope: row.get(5)?,
                required_level_int: row.get(6)?,
                goal_scope: row.get(7)?,
                granted: row.get(8)?,
                denial_reason: row.get(9)?,
                context: row.get(10)?,
            })
        })?;

        rows_to_entries(rows)
    }

    /// Query audit log entries for a specific token ID.
    pub fn query_audit_for_token(
        &self,
        token_id: &str,
    ) -> Result<Vec<AuditEntry>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, event_type, subject, token_id, scope, required_level, goal_scope, granted, denial_reason, context
             FROM audit_log WHERE token_id = ?1 ORDER BY timestamp DESC",
        )?;

        let rows = stmt.query_map(params![token_id], |row| {
            Ok(AuditRow {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                event_type: row.get(2)?,
                subject: row.get(3)?,
                token_id: row.get(4)?,
                scope: row.get(5)?,
                required_level_int: row.get(6)?,
                goal_scope: row.get(7)?,
                granted: row.get(8)?,
                denial_reason: row.get(9)?,
                context: row.get(10)?,
            })
        })?;

        rows_to_entries(rows)
    }

    /// Remove expired tokens from the database and log expiry audit events.
    /// Returns the number of tokens cleaned up.
    pub fn cleanup_expired_tokens(&self, now: u64) -> Result<usize, PersistenceError> {
        // First, collect the expired tokens so we can audit them.
        let mut stmt = self.conn.prepare(
            "SELECT token_id, subject, trust_level, scopes_json, expires_at, goal_scope
             FROM capability_tokens WHERE expires_at <= ?1",
        )?;

        let expired: Vec<(String, String, i64, String, i64, Option<String>)> = stmt
            .query_map(params![now as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let count = expired.len();

        // Log an expiry audit event for each and delete.
        for (token_id, subject, trust_level_int, _scopes, _expires, goal_scope) in &expired {
            let trust_level = trust_level_from_int(*trust_level_int);
            let _ = self.log_audit_entry(
                now,
                AuditEventType::Expire,
                subject,
                token_id,
                "",
                trust_level,
                goal_scope.as_deref(),
                false,
                "token expired",
                "",
            );
        }

        self.conn.execute(
            "DELETE FROM capability_tokens WHERE expires_at <= ?1",
            params![now as i64],
        )?;

        Ok(count)
    }

    /// Count total audit entries.
    pub fn audit_count(&self) -> Result<u64, PersistenceError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Snapshot: save all tokens from a broker and return the count saved.
    pub fn save_broker_state(&self, broker: &AccessBroker) -> Result<usize, PersistenceError> {
        let tokens = broker.tokens();
        let tx = self.conn.unchecked_transaction()?;

        // Clear existing tokens and re-insert all.
        tx.execute("DELETE FROM capability_tokens", [])?;

        for token in &tokens {
            let scopes_json = serde_json::to_string(&token.scopes)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

            tx.execute(
                "INSERT INTO capability_tokens
                 (token_id, subject, trust_level, scopes_json, expires_at, one_time, consumed, goal_scope)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    token.token_id,
                    token.subject,
                    token.trust_level as i64,
                    scopes_json,
                    token.expires_at as i64,
                    token.one_time as i64,
                    token.consumed as i64,
                    token.goal_scope,
                ],
            )?;
        }

        tx.commit()?;
        Ok(tokens.len())
    }

    /// Restore: load all tokens and build a fresh AccessBroker.
    pub fn load_broker(&self) -> Result<AccessBroker, PersistenceError> {
        let tokens = self.load_tokens()?;
        Ok(AccessBroker::from_tokens(tokens))
    }
}

/// Internal row struct for audit query result mapping.
struct AuditRow {
    id: i64,
    timestamp: i64,
    event_type: String,
    subject: String,
    token_id: String,
    scope: String,
    required_level_int: i64,
    goal_scope: Option<String>,
    granted: bool,
    denial_reason: String,
    context: String,
}

/// Convert a sequence of AuditRow results into AuditEntry values.
fn rows_to_entries(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<AuditRow>>,
) -> Result<Vec<AuditEntry>, PersistenceError> {
    let mut entries = Vec::new();
    for row_result in rows {
        let row = row_result?;
        let required_level = trust_level_from_int(row.required_level_int);
        let event_type =
            AuditEventType::from_str(&row.event_type).unwrap_or(AuditEventType::Evaluate);

        entries.push(AuditEntry {
            id: Some(row.id),
            timestamp: row.timestamp as u64,
            event_type,
            subject: row.subject,
            token_id: row.token_id,
            scope: row.scope,
            required_level,
            goal_scope: row.goal_scope,
            granted: row.granted,
            denial_reason: row.denial_reason,
            context: row.context,
        });
    }
    Ok(entries)
}

/// Convert an integer trust level to the enum (defaults to ReadOnly for unknown values).
fn trust_level_from_int(val: i64) -> AgentTrustLevel {
    match val {
        0 => AgentTrustLevel::ReadOnly,
        1 => AgentTrustLevel::ArchiveWrite,
        2 => AgentTrustLevel::CredentialAccess,
        3 => AgentTrustLevel::ExternalAct,
        _ => AgentTrustLevel::ReadOnly,
    }
}

/// Extension trait that adds persistence-aware operations to `AccessBroker`.
///
/// This keeps the core `AccessBroker` free of SQLite dependencies while
/// providing convenient methods for callers who have a `TrustStore`.
impl AccessBroker {
    /// Issue a token, persist it, and log a grant audit event.
    pub fn issue_token_persisted(
        &mut self,
        token: CapabilityToken,
        store: &TrustStore,
    ) -> Result<(), PersistenceError> {
        store.save_token(&token)?;
        // Log grant event (best-effort).
        let scopes_str = token.scopes.iter().cloned().collect::<Vec<_>>().join(",");
        let _ = store.log_audit_entry(
            crate::now_unix(),
            AuditEventType::Grant,
            &token.subject,
            &token.token_id,
            &scopes_str,
            token.trust_level,
            token.goal_scope.as_deref(),
            true,
            "",
            "",
        );
        self.issue_token(token);
        Ok(())
    }

    /// Issue a token, persist it, and log a grant audit event with context.
    pub fn issue_token_with_context(
        &mut self,
        token: CapabilityToken,
        store: &TrustStore,
        context: &str,
    ) -> Result<(), PersistenceError> {
        store.save_token(&token)?;
        let scopes_str = token.scopes.iter().cloned().collect::<Vec<_>>().join(",");
        let _ = store.log_audit_entry(
            crate::now_unix(),
            AuditEventType::Grant,
            &token.subject,
            &token.token_id,
            &scopes_str,
            token.trust_level,
            token.goal_scope.as_deref(),
            true,
            "",
            context,
        );
        self.issue_token(token);
        Ok(())
    }

    /// Evaluate a request and log the result to the audit trail.
    pub fn evaluate_audited(
        &mut self,
        token_id: &str,
        request: &AccessRequest,
        now: u64,
        store: &TrustStore,
    ) -> anyhow::Result<AccessDecision> {
        let result = self.evaluate(token_id, request, now);

        // Log regardless of outcome.
        let (granted, denial_reason) = match &result {
            Ok(decision) => (decision.allowed, String::new()),
            Err(e) => (false, e.to_string()),
        };

        // Audit logging is best-effort: don't fail the access decision if logging fails.
        let _ = store.log_audit(now, request, token_id, granted, &denial_reason);

        // If a one-time token was consumed, persist the updated state.
        if result.is_ok() {
            if let Some(token) = self.get_token(token_id) {
                if token.one_time && token.consumed {
                    let _ = store.save_token(token);
                }
            }
        }

        result
    }

    /// Revoke (remove) a token from the broker and persistence, logging a revoke audit event.
    pub fn revoke_token(
        &mut self,
        token_id: &str,
        store: &TrustStore,
    ) -> Result<bool, PersistenceError> {
        // Capture token info before removal for audit logging.
        let token_info = self.get_token(token_id).cloned();
        let existed = self.remove_token(token_id);
        store.remove_token(token_id)?;

        // Log revocation event (best-effort).
        if let Some(token) = token_info {
            let scopes_str = token.scopes.iter().cloned().collect::<Vec<_>>().join(",");
            let _ = store.log_audit_entry(
                crate::now_unix(),
                AuditEventType::Revoke,
                &token.subject,
                &token.token_id,
                &scopes_str,
                token.trust_level,
                token.goal_scope.as_deref(),
                true,
                "",
                "",
            );
        }
        Ok(existed)
    }

    /// Cleanup expired tokens from both the broker's in-memory map and the store.
    /// Returns the number of tokens cleaned up.
    pub fn cleanup_expired(
        &mut self,
        now: u64,
        store: &TrustStore,
    ) -> Result<usize, PersistenceError> {
        // Remove from in-memory broker.
        let expired_ids: Vec<String> = self
            .tokens()
            .iter()
            .filter(|t| t.expires_at <= now)
            .map(|t| t.token_id.clone())
            .collect();

        for id in &expired_ids {
            self.remove_token(id);
        }

        // Remove from store and log expiry events.
        store.cleanup_expired_tokens(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::now_unix;

    fn make_token(id: &str, subject: &str, expires_at: u64) -> CapabilityToken {
        CapabilityToken {
            token_id: id.to_string(),
            subject: subject.to_string(),
            trust_level: AgentTrustLevel::ExternalAct,
            scopes: [
                "action.browser.login".to_string(),
                "archive.write".to_string(),
            ]
            .into_iter()
            .collect(),
            expires_at,
            one_time: false,
            consumed: false,
            goal_scope: None,
        }
    }

    // --- Token persistence round-trip ---

    #[test]
    fn token_roundtrip() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();
        let token = make_token("t-persist", "agent-a", now + 3600);

        store.save_token(&token).unwrap();

        let loaded = store.load_tokens().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].token_id, "t-persist");
        assert_eq!(loaded[0].subject, "agent-a");
        assert_eq!(loaded[0].trust_level, AgentTrustLevel::ExternalAct);
        assert!(loaded[0].scopes.contains("action.browser.login"));
        assert!(loaded[0].scopes.contains("archive.write"));
        assert_eq!(loaded[0].expires_at, now + 3600);
        assert!(!loaded[0].one_time);
        assert!(!loaded[0].consumed);
        assert_eq!(loaded[0].goal_scope, None);
    }

    #[test]
    fn token_with_goal_scope_roundtrip() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();
        let mut token = make_token("t-goal", "agent-b", now + 7200);
        token.goal_scope = Some("trading".to_string());
        token.one_time = true;

        store.save_token(&token).unwrap();
        let loaded = store.load_tokens().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].goal_scope, Some("trading".to_string()));
        assert!(loaded[0].one_time);
    }

    #[test]
    fn token_upsert_overwrites() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();
        let token = make_token("t-up", "agent-a", now + 100);
        store.save_token(&token).unwrap();

        // Overwrite with consumed=true
        let mut updated = token.clone();
        updated.consumed = true;
        store.save_token(&updated).unwrap();

        let loaded = store.load_tokens().unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].consumed);
    }

    #[test]
    fn token_remove() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();
        store
            .save_token(&make_token("t-rm", "agent-a", now + 100))
            .unwrap();

        assert!(store.remove_token("t-rm").unwrap());
        assert!(!store.remove_token("t-rm").unwrap()); // already gone
        assert!(store.load_tokens().unwrap().is_empty());
    }

    // --- Broker persistence ---

    #[test]
    fn broker_save_and_restore() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let mut broker = AccessBroker::new();
        broker.issue_token(make_token("t1", "agent-a", now + 100));
        broker.issue_token(make_token("t2", "agent-b", now + 200));

        let saved = store.save_broker_state(&broker).unwrap();
        assert_eq!(saved, 2);

        let restored = store.load_broker().unwrap();
        let tokens = restored.tokens();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].token_id, "t1");
        assert_eq!(tokens[1].token_id, "t2");
    }

    // --- Audit logging ---

    #[test]
    fn audit_log_records_evaluate_grant() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let request = AccessRequest {
            subject: "agent-a".to_string(),
            required_level: AgentTrustLevel::ArchiveWrite,
            scope: "archive.write".to_string(),
            goal_scope: None,
        };

        store.log_audit(now, &request, "t1", true, "").unwrap();

        let entries = store.query_audit_since(now).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].subject, "agent-a");
        assert_eq!(entries[0].token_id, "t1");
        assert_eq!(entries[0].scope, "archive.write");
        assert!(entries[0].granted);
        assert_eq!(entries[0].denial_reason, "");
        assert_eq!(entries[0].event_type, AuditEventType::Evaluate);
        assert_eq!(entries[0].context, "");
    }

    #[test]
    fn audit_log_records_denial() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let request = AccessRequest {
            subject: "agent-evil".to_string(),
            required_level: AgentTrustLevel::ExternalAct,
            scope: "credential.read".to_string(),
            goal_scope: Some("banking".to_string()),
        };

        store
            .log_audit(now, &request, "t-fake", false, "token not found: t-fake")
            .unwrap();

        let entries = store.query_audit_since(now).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].granted);
        assert_eq!(entries[0].denial_reason, "token not found: t-fake");
        assert_eq!(entries[0].goal_scope, Some("banking".to_string()));
        assert_eq!(entries[0].event_type, AuditEventType::Evaluate);
    }

    #[test]
    fn audit_log_query_by_subject() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let req_a = AccessRequest {
            subject: "agent-a".to_string(),
            required_level: AgentTrustLevel::ReadOnly,
            scope: "archive.read".to_string(),
            goal_scope: None,
        };
        let req_b = AccessRequest {
            subject: "agent-b".to_string(),
            required_level: AgentTrustLevel::ReadOnly,
            scope: "archive.read".to_string(),
            goal_scope: None,
        };

        store.log_audit(now, &req_a, "t1", true, "").unwrap();
        store.log_audit(now, &req_b, "t2", true, "").unwrap();
        store
            .log_audit(now + 1, &req_a, "t1", false, "expired")
            .unwrap();

        let a_entries = store.query_audit_for_subject("agent-a").unwrap();
        assert_eq!(a_entries.len(), 2);

        let b_entries = store.query_audit_for_subject("agent-b").unwrap();
        assert_eq!(b_entries.len(), 1);
    }

    #[test]
    fn audit_count() {
        let store = TrustStore::open_in_memory().unwrap();
        assert_eq!(store.audit_count().unwrap(), 0);

        let request = AccessRequest {
            subject: "agent-a".to_string(),
            required_level: AgentTrustLevel::ReadOnly,
            scope: "archive.read".to_string(),
            goal_scope: None,
        };
        store.log_audit(1000, &request, "t1", true, "").unwrap();
        store.log_audit(1001, &request, "t1", true, "").unwrap();

        assert_eq!(store.audit_count().unwrap(), 2);
    }

    // --- Audit event types ---

    #[test]
    fn audit_entry_with_event_type_and_context() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        store
            .log_audit_entry(
                now,
                AuditEventType::Grant,
                "agent-a",
                "t-ctx",
                "archive.write",
                AgentTrustLevel::ArchiveWrite,
                None,
                true,
                "",
                "user requested archive access for research",
            )
            .unwrap();

        let entries = store.query_audit_since(now).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, AuditEventType::Grant);
        assert_eq!(
            entries[0].context,
            "user requested archive access for research"
        );
    }

    #[test]
    fn audit_event_type_roundtrips() {
        assert_eq!(
            AuditEventType::from_str(AuditEventType::Grant.as_str()),
            Some(AuditEventType::Grant)
        );
        assert_eq!(
            AuditEventType::from_str(AuditEventType::Evaluate.as_str()),
            Some(AuditEventType::Evaluate)
        );
        assert_eq!(
            AuditEventType::from_str(AuditEventType::Revoke.as_str()),
            Some(AuditEventType::Revoke)
        );
        assert_eq!(
            AuditEventType::from_str(AuditEventType::Expire.as_str()),
            Some(AuditEventType::Expire)
        );
        assert_eq!(AuditEventType::from_str("unknown"), None);
    }

    // --- Query by token ID ---

    #[test]
    fn audit_query_by_token_id() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        store
            .log_audit_entry(
                now,
                AuditEventType::Grant,
                "agent-a",
                "t-query",
                "archive.write",
                AgentTrustLevel::ArchiveWrite,
                None,
                true,
                "",
                "",
            )
            .unwrap();
        store
            .log_audit_entry(
                now + 1,
                AuditEventType::Evaluate,
                "agent-a",
                "t-query",
                "archive.write",
                AgentTrustLevel::ArchiveWrite,
                None,
                true,
                "",
                "",
            )
            .unwrap();
        store
            .log_audit_entry(
                now + 2,
                AuditEventType::Evaluate,
                "agent-b",
                "t-other",
                "archive.read",
                AgentTrustLevel::ReadOnly,
                None,
                true,
                "",
                "",
            )
            .unwrap();

        let entries = store.query_audit_for_token("t-query").unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.token_id == "t-query"));

        let other = store.query_audit_for_token("t-other").unwrap();
        assert_eq!(other.len(), 1);

        let none = store.query_audit_for_token("nonexistent").unwrap();
        assert!(none.is_empty());
    }

    // --- Expired token cleanup ---

    #[test]
    fn cleanup_expired_tokens_removes_expired() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = 10_000u64;

        // Two expired, one valid.
        store
            .save_token(&make_token("t-exp1", "agent-a", now - 100))
            .unwrap();
        store
            .save_token(&make_token("t-exp2", "agent-b", now - 1))
            .unwrap();
        store
            .save_token(&make_token("t-valid", "agent-c", now + 3600))
            .unwrap();

        let cleaned = store.cleanup_expired_tokens(now).unwrap();
        assert_eq!(cleaned, 2);

        let remaining = store.load_tokens().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].token_id, "t-valid");
    }

    #[test]
    fn cleanup_expired_tokens_logs_expiry_events() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = 10_000u64;

        store
            .save_token(&make_token("t-exp", "agent-a", now - 50))
            .unwrap();

        store.cleanup_expired_tokens(now).unwrap();

        let entries = store.query_audit_for_token("t-exp").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, AuditEventType::Expire);
        assert!(!entries[0].granted);
        assert_eq!(entries[0].denial_reason, "token expired");
    }

    #[test]
    fn cleanup_expired_tokens_noop_when_none_expired() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        store
            .save_token(&make_token("t-fresh", "agent-a", now + 9999))
            .unwrap();

        let cleaned = store.cleanup_expired_tokens(now).unwrap();
        assert_eq!(cleaned, 0);
        assert_eq!(store.load_tokens().unwrap().len(), 1);
    }

    // --- Broker cleanup_expired ---

    #[test]
    fn broker_cleanup_expired_removes_from_memory_and_store() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = 10_000u64;

        let mut broker = AccessBroker::new();
        broker.issue_token(make_token("t-exp-b", "agent-a", now - 10));
        broker.issue_token(make_token("t-live-b", "agent-b", now + 3600));

        // Also persist them.
        store
            .save_token(&make_token("t-exp-b", "agent-a", now - 10))
            .unwrap();
        store
            .save_token(&make_token("t-live-b", "agent-b", now + 3600))
            .unwrap();

        let cleaned = broker.cleanup_expired(now, &store).unwrap();
        assert_eq!(cleaned, 1);

        // In-memory: only live token remains.
        assert_eq!(broker.tokens().len(), 1);
        assert_eq!(broker.tokens()[0].token_id, "t-live-b");

        // Store: only live token remains.
        let loaded = store.load_tokens().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].token_id, "t-live-b");
    }

    // --- Integrated: issue_token_persisted + evaluate_audited ---

    #[test]
    fn issue_token_persisted_logs_grant_event() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let mut broker = AccessBroker::new();
        let token = make_token("t-grant-log", "agent-a", now + 3600);
        broker.issue_token_persisted(token, &store).unwrap();

        // Token persisted.
        let loaded = store.load_tokens().unwrap();
        assert_eq!(loaded.len(), 1);

        // Grant audit event logged.
        let entries = store.query_audit_for_token("t-grant-log").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, AuditEventType::Grant);
        assert!(entries[0].granted);
        assert_eq!(entries[0].subject, "agent-a");
    }

    #[test]
    fn issue_token_with_context_logs_context() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let mut broker = AccessBroker::new();
        let token = make_token("t-ctx-log", "agent-a", now + 3600);
        broker
            .issue_token_with_context(token, &store, "need archive access for goal: research-ai")
            .unwrap();

        let entries = store.query_audit_for_token("t-ctx-log").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, AuditEventType::Grant);
        assert_eq!(
            entries[0].context,
            "need archive access for goal: research-ai"
        );
    }

    #[test]
    fn issue_and_evaluate_with_persistence() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let mut broker = AccessBroker::new();
        let token = make_token("t-int", "agent-a", now + 3600);
        broker.issue_token_persisted(token, &store).unwrap();

        // Token persisted
        let loaded = store.load_tokens().unwrap();
        assert_eq!(loaded.len(), 1);

        // Evaluate with audit
        let request = AccessRequest {
            subject: "agent-a".to_string(),
            required_level: AgentTrustLevel::ArchiveWrite,
            scope: "archive.write".to_string(),
            goal_scope: None,
        };
        let decision = broker
            .evaluate_audited("t-int", &request, now, &store)
            .unwrap();
        assert!(decision.allowed);

        // Audit logged: grant + evaluate = 2 entries
        let entries = store.query_audit_since(now).unwrap();
        assert_eq!(entries.len(), 2);
        // Newest first: evaluate, then grant.
        assert_eq!(entries[0].event_type, AuditEventType::Evaluate);
        assert!(entries[0].granted);
        assert_eq!(entries[1].event_type, AuditEventType::Grant);
    }

    #[test]
    fn evaluate_audited_logs_denials() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let mut broker = AccessBroker::new();
        let token = make_token("t-deny", "agent-a", now + 3600);
        broker.issue_token_persisted(token, &store).unwrap();

        // Request with wrong subject
        let request = AccessRequest {
            subject: "agent-evil".to_string(),
            required_level: AgentTrustLevel::ReadOnly,
            scope: "archive.write".to_string(),
            goal_scope: None,
        };
        let result = broker.evaluate_audited("t-deny", &request, now, &store);
        assert!(result.is_err());

        // Find evaluate entries only.
        let entries = store.query_audit_for_token("t-deny").unwrap();
        let eval_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.event_type == AuditEventType::Evaluate)
            .collect();
        assert_eq!(eval_entries.len(), 1);
        assert!(!eval_entries[0].granted);
        assert!(eval_entries[0].denial_reason.contains("subject mismatch"));
    }

    #[test]
    fn revoke_token_removes_from_both_and_logs() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let mut broker = AccessBroker::new();
        let token = make_token("t-revoke", "agent-a", now + 3600);
        broker.issue_token_persisted(token, &store).unwrap();

        assert!(broker.revoke_token("t-revoke", &store).unwrap());
        assert!(!broker.revoke_token("t-revoke", &store).unwrap()); // idempotent

        // Gone from both
        assert!(store.load_tokens().unwrap().is_empty());
        assert!(broker.tokens().is_empty());

        // Revocation logged.
        let entries = store.query_audit_for_token("t-revoke").unwrap();
        let revoke_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.event_type == AuditEventType::Revoke)
            .collect();
        assert_eq!(revoke_entries.len(), 1);
        assert_eq!(revoke_entries[0].subject, "agent-a");
    }

    #[test]
    fn one_time_token_consumed_state_persisted() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let mut broker = AccessBroker::new();
        let mut token = make_token("t-once", "agent-a", now + 3600);
        token.one_time = true;
        broker.issue_token_persisted(token, &store).unwrap();

        let request = AccessRequest {
            subject: "agent-a".to_string(),
            required_level: AgentTrustLevel::ArchiveWrite,
            scope: "archive.write".to_string(),
            goal_scope: None,
        };
        let decision = broker
            .evaluate_audited("t-once", &request, now, &store)
            .unwrap();
        assert!(decision.allowed);

        // Consumed state persisted
        let loaded = store.load_tokens().unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].consumed);
    }

    #[test]
    fn file_backed_store_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("trust.db");
        let now = now_unix();

        // Write
        {
            let store = TrustStore::open(&db_path).unwrap();
            let mut broker = AccessBroker::new();
            broker
                .issue_token_persisted(make_token("t-file", "agent-a", now + 3600), &store)
                .unwrap();

            let request = AccessRequest {
                subject: "agent-a".to_string(),
                required_level: AgentTrustLevel::ArchiveWrite,
                scope: "archive.write".to_string(),
                goal_scope: None,
            };
            broker
                .evaluate_audited("t-file", &request, now, &store)
                .unwrap();
        }

        // Re-open and verify
        {
            let store = TrustStore::open(&db_path).unwrap();
            let broker = store.load_broker().unwrap();
            assert_eq!(broker.tokens().len(), 1);
            assert_eq!(broker.tokens()[0].token_id, "t-file");

            // grant + evaluate = 2 entries
            let entries = store.query_audit_since(0).unwrap();
            assert_eq!(entries.len(), 2);

            // Verify event types present.
            let types: Vec<_> = entries.iter().map(|e| e.event_type).collect();
            assert!(types.contains(&AuditEventType::Grant));
            assert!(types.contains(&AuditEventType::Evaluate));
        }
    }

    // --- Full lifecycle test ---

    #[test]
    fn full_token_lifecycle_grant_evaluate_revoke() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = now_unix();

        let mut broker = AccessBroker::new();

        // 1. Grant
        let token = make_token("t-lifecycle", "agent-a", now + 3600);
        broker
            .issue_token_with_context(token, &store, "need browser access for web search")
            .unwrap();

        // 2. Evaluate (success)
        let request = AccessRequest {
            subject: "agent-a".to_string(),
            required_level: AgentTrustLevel::ArchiveWrite,
            scope: "archive.write".to_string(),
            goal_scope: None,
        };
        let decision = broker
            .evaluate_audited("t-lifecycle", &request, now, &store)
            .unwrap();
        assert!(decision.allowed);

        // 3. Evaluate (failure - wrong scope)
        let bad_request = AccessRequest {
            subject: "agent-a".to_string(),
            required_level: AgentTrustLevel::ReadOnly,
            scope: "credential.read".to_string(),
            goal_scope: None,
        };
        let _ = broker.evaluate_audited("t-lifecycle", &bad_request, now, &store);

        // 4. Revoke
        broker.revoke_token("t-lifecycle", &store).unwrap();

        // Verify full audit trail.
        let entries = store.query_audit_for_token("t-lifecycle").unwrap();
        assert_eq!(entries.len(), 4); // grant, eval-ok, eval-fail, revoke
        let types: Vec<_> = entries.iter().map(|e| e.event_type).collect();
        assert!(types.contains(&AuditEventType::Grant));
        assert!(types.contains(&AuditEventType::Revoke));
        let eval_count = types
            .iter()
            .filter(|t| **t == AuditEventType::Evaluate)
            .count();
        assert_eq!(eval_count, 2);

        // Context preserved on grant.
        let grant_entry = entries
            .iter()
            .find(|e| e.event_type == AuditEventType::Grant)
            .unwrap();
        assert_eq!(grant_entry.context, "need browser access for web search");
    }

    #[test]
    fn cleanup_expired_full_flow() {
        let store = TrustStore::open_in_memory().unwrap();
        let now = 50_000u64;

        let mut broker = AccessBroker::new();

        // Issue 3 tokens: 2 will expire, 1 stays.
        broker.issue_token(make_token("t-short1", "agent-a", now - 100));
        broker.issue_token(make_token("t-short2", "agent-b", now - 1));
        broker.issue_token(make_token("t-long", "agent-c", now + 86400));

        // Persist.
        store.save_broker_state(&broker).unwrap();

        // Cleanup.
        let cleaned = broker.cleanup_expired(now, &store).unwrap();
        assert_eq!(cleaned, 2);

        // Verify in-memory and store state.
        assert_eq!(broker.tokens().len(), 1);
        assert_eq!(broker.tokens()[0].token_id, "t-long");

        let stored = store.load_tokens().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].token_id, "t-long");

        // Expiry events logged.
        let all_audit = store.query_audit_since(0).unwrap();
        let expire_events: Vec<_> = all_audit
            .iter()
            .filter(|e| e.event_type == AuditEventType::Expire)
            .collect();
        assert_eq!(expire_events.len(), 2);
    }
}
