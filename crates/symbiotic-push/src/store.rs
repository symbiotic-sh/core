//! SQLite-backed push token storage.

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

use crate::error::PushError;
use crate::types::{PushProvider, PushToken};

/// SQLite-backed store for push notification tokens.
///
/// Manages device token registration, lookup, and lifecycle (expiry, removal).
pub struct PushTokenStore {
    conn: Connection,
}

impl PushTokenStore {
    /// Open or create a push token store at the given database path.
    pub fn open(path: &str) -> Result<Self, PushError> {
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Create an in-memory push token store (useful for testing).
    pub fn in_memory() -> Result<Self, PushError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Initialize the database schema.
    fn init_schema(&self) -> Result<(), PushError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS push_tokens (
                device_id   TEXT NOT NULL,
                platform    TEXT NOT NULL,
                token       TEXT NOT NULL,
                user_id     TEXT NOT NULL,
                registered_at TEXT NOT NULL,
                expires_at  TEXT,
                PRIMARY KEY (device_id, platform)
            );
            CREATE INDEX IF NOT EXISTS idx_push_tokens_user_id
                ON push_tokens (user_id);
            CREATE INDEX IF NOT EXISTS idx_push_tokens_expires_at
                ON push_tokens (expires_at);",
        )?;
        Ok(())
    }

    /// Register or update a push token (upsert by device_id + provider).
    ///
    /// Validates that the token, device_id, and user_id are non-empty.
    pub fn register_token(&self, token: &PushToken) -> Result<(), PushError> {
        // Validate fields
        if token.device_id.trim().is_empty() {
            return Err(PushError::InvalidToken("device_id is empty".into()));
        }
        if token.token.trim().is_empty() {
            return Err(PushError::InvalidToken("token string is empty".into()));
        }
        if token.user_id.trim().is_empty() {
            return Err(PushError::InvalidToken("user_id is empty".into()));
        }

        let platform = token.platform.to_string();
        let registered_at = token.registered_at.to_rfc3339();
        let expires_at = token.expires_at.map(|t| t.to_rfc3339());

        self.conn.execute(
            "INSERT INTO push_tokens (device_id, platform, token, user_id, registered_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(device_id, platform) DO UPDATE SET
                token = excluded.token,
                user_id = excluded.user_id,
                registered_at = excluded.registered_at,
                expires_at = excluded.expires_at",
            params![
                token.device_id,
                platform,
                token.token,
                token.user_id,
                registered_at,
                expires_at,
            ],
        )?;

        tracing::debug!(
            device_id = %token.device_id,
            platform = %platform,
            user_id = %token.user_id,
            "registered push token"
        );

        Ok(())
    }

    /// Remove a specific token by device_id and provider.
    ///
    /// Returns `true` if a token was removed.
    pub fn remove_token(&self, device_id: &str, provider: PushProvider) -> Result<bool, PushError> {
        let platform = provider.to_string();
        let changes = self.conn.execute(
            "DELETE FROM push_tokens WHERE device_id = ?1 AND platform = ?2",
            params![device_id, platform],
        )?;
        Ok(changes > 0)
    }

    /// Remove all tokens for a device (across all providers).
    ///
    /// Returns the number of tokens removed.
    pub fn remove_device(&self, device_id: &str) -> Result<u32, PushError> {
        let changes = self.conn.execute(
            "DELETE FROM push_tokens WHERE device_id = ?1",
            params![device_id],
        )?;
        Ok(changes as u32)
    }

    /// Get all push tokens for a user.
    pub fn get_tokens_for_user(&self, user_id: &str) -> Result<Vec<PushToken>, PushError> {
        let mut stmt = self.conn.prepare(
            "SELECT device_id, platform, token, user_id, registered_at, expires_at
                 FROM push_tokens WHERE user_id = ?1",
        )?;

        let tokens = stmt
            .query_map(params![user_id], |row| {
                Ok(RawTokenRow {
                    device_id: row.get(0)?,
                    platform: row.get(1)?,
                    token: row.get(2)?,
                    user_id: row.get(3)?,
                    registered_at: row.get(4)?,
                    expires_at: row.get(5)?,
                })
            })?
            .filter_map(|r| r.ok())
            .filter_map(|row| row.into_push_token())
            .collect();

        Ok(tokens)
    }

    /// Get a specific token by device_id and provider.
    pub fn get_token(
        &self,
        device_id: &str,
        provider: PushProvider,
    ) -> Result<Option<PushToken>, PushError> {
        let platform = provider.to_string();
        let mut stmt = self.conn.prepare(
            "SELECT device_id, platform, token, user_id, registered_at, expires_at
             FROM push_tokens WHERE device_id = ?1 AND platform = ?2",
        )?;

        let row = stmt
            .query_row(params![device_id, platform], |row| {
                Ok(RawTokenRow {
                    device_id: row.get(0)?,
                    platform: row.get(1)?,
                    token: row.get(2)?,
                    user_id: row.get(3)?,
                    registered_at: row.get(4)?,
                    expires_at: row.get(5)?,
                })
            })
            .optional()?;

        Ok(row.and_then(|r| r.into_push_token()))
    }

    /// Remove all expired tokens.
    ///
    /// Returns the number of tokens pruned.
    pub fn prune_expired(&self) -> Result<u32, PushError> {
        let now = Utc::now().to_rfc3339();
        let changes = self.conn.execute(
            "DELETE FROM push_tokens WHERE expires_at IS NOT NULL AND expires_at < ?1",
            params![now],
        )?;

        if changes > 0 {
            tracing::info!(pruned = changes, "pruned expired push tokens");
        }

        Ok(changes as u32)
    }

    /// Count tokens registered for a user.
    pub fn count_for_user(&self, user_id: &str) -> Result<u32, PushError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM push_tokens WHERE user_id = ?1",
            params![user_id],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// Remove a token by its raw token string (used for auto-pruning invalid tokens).
    pub fn remove_by_token_string(&self, token_str: &str) -> Result<bool, PushError> {
        let changes = self.conn.execute(
            "DELETE FROM push_tokens WHERE token = ?1",
            params![token_str],
        )?;
        Ok(changes > 0)
    }
}

/// Internal helper for reading rows from SQLite.
struct RawTokenRow {
    device_id: String,
    platform: String,
    token: String,
    user_id: String,
    registered_at: String,
    expires_at: Option<String>,
}

impl RawTokenRow {
    fn into_push_token(self) -> Option<PushToken> {
        let platform = match self.platform.as_str() {
            "apns" => PushProvider::Apns,
            "fcm" => PushProvider::Fcm,
            _ => return None,
        };

        let registered_at = chrono::DateTime::parse_from_rfc3339(&self.registered_at)
            .ok()?
            .with_timezone(&Utc);

        let expires_at = self.expires_at.and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        });

        Some(PushToken {
            device_id: self.device_id,
            platform,
            token: self.token,
            user_id: self.user_id,
            registered_at,
            expires_at,
        })
    }
}
