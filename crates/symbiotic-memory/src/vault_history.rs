//! History metadata cache for the Vault-as-Truth architecture.
//!
//! Indexes git commits per file in SQLite (`vault_history` table) for fast
//! history queries without shelling out to `git log` every time.
//!
//! See `docs/design/vault-as-truth.md` § History Metadata Cache.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use rusqlite::params;
use tokio::sync::Mutex;

use crate::sqlite::SqliteMemoryStore;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single entry in the vault history cache.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub file_path: String,
    pub commit_hash: String,
    pub timestamp: i64,
    pub author: String,
    pub summary: String,
}

/// Statistics from a cache rebuild.
#[derive(Debug, Clone, Default)]
pub struct HistoryCacheStats {
    pub files_scanned: usize,
    pub entries_added: usize,
}

/// Errors during history cache operations.
#[derive(Debug, thiserror::Error)]
pub enum HistoryCacheError {
    #[error("database error: {0}")]
    Database(String),
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<rusqlite::Error> for HistoryCacheError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const HISTORY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS vault_history (
    file_path TEXT NOT NULL,
    commit_hash TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    author TEXT NOT NULL,
    summary TEXT NOT NULL,
    PRIMARY KEY (file_path, commit_hash)
);
CREATE INDEX IF NOT EXISTS idx_vh_file ON vault_history(file_path);
CREATE INDEX IF NOT EXISTS idx_vh_time ON vault_history(timestamp);
"#;

// ---------------------------------------------------------------------------
// VaultHistoryCache
// ---------------------------------------------------------------------------

/// Caches git commit metadata per file for fast history queries.
pub struct VaultHistoryCache {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl VaultHistoryCache {
    /// Create a new history cache sharing the store's database connection.
    pub fn new(store: &SqliteMemoryStore) -> Self {
        Self {
            conn: Arc::clone(&store.conn),
        }
    }

    /// Create the vault_history table if it doesn't exist.
    pub async fn initialize(&self) -> Result<(), HistoryCacheError> {
        let conn = self.conn.lock().await;
        conn.execute_batch(HISTORY_SCHEMA)?;
        Ok(())
    }

    /// Rebuild the history cache from git log for all vault files.
    ///
    /// `repo_root` is the git repository root. `vault_prefix` is the path
    /// prefix for vault files relative to the repo root (e.g., "knowledge-base/").
    pub async fn rebuild(
        &self,
        repo_root: &Path,
        vault_prefix: &str,
    ) -> Result<HistoryCacheStats, HistoryCacheError> {
        let mut stats = HistoryCacheStats::default();

        // Clear existing cache
        {
            let conn = self.conn.lock().await;
            conn.execute("DELETE FROM vault_history", [])?;
        }

        // Get git log for all files under vault_prefix
        let entries = git_log_for_prefix(repo_root, vault_prefix)?;
        stats.files_scanned = entries
            .iter()
            .map(|e| &e.file_path)
            .collect::<std::collections::HashSet<_>>()
            .len();

        // Insert entries
        {
            let conn = self.conn.lock().await;
            for entry in &entries {
                conn.execute(
                    "INSERT OR IGNORE INTO vault_history (file_path, commit_hash, timestamp, author, summary)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        entry.file_path,
                        entry.commit_hash,
                        entry.timestamp,
                        entry.author,
                        entry.summary,
                    ],
                )?;
                stats.entries_added += 1;
            }
        }

        Ok(stats)
    }

    /// Get the history for a specific file, ordered by timestamp descending.
    pub async fn get_file_history(
        &self,
        file_path: &str,
    ) -> Result<Vec<HistoryEntry>, HistoryCacheError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT file_path, commit_hash, timestamp, author, summary
             FROM vault_history
             WHERE file_path = ?1
             ORDER BY timestamp DESC",
        )?;

        let entries = stmt
            .query_map([file_path], |row| {
                Ok(HistoryEntry {
                    file_path: row.get(0)?,
                    commit_hash: row.get(1)?,
                    timestamp: row.get(2)?,
                    author: row.get(3)?,
                    summary: row.get(4)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(entries)
    }

    /// Get the last update timestamp for a file. Returns None if no history.
    pub async fn last_updated(&self, file_path: &str) -> Result<Option<i64>, HistoryCacheError> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT MAX(timestamp) FROM vault_history WHERE file_path = ?1",
            [file_path],
            |row| row.get(0),
        );

        match result {
            Ok(ts) => Ok(ts),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all changes within a time range.
    pub async fn changes_since(
        &self,
        since_timestamp: i64,
    ) -> Result<Vec<HistoryEntry>, HistoryCacheError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT file_path, commit_hash, timestamp, author, summary
             FROM vault_history
             WHERE timestamp > ?1
             ORDER BY timestamp DESC",
        )?;

        let entries = stmt
            .query_map([since_timestamp], |row| {
                Ok(HistoryEntry {
                    file_path: row.get(0)?,
                    commit_hash: row.get(1)?,
                    timestamp: row.get(2)?,
                    author: row.get(3)?,
                    summary: row.get(4)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(entries)
    }

    /// Add a single history entry (used after vault commits).
    pub async fn add_entry(&self, entry: &HistoryEntry) -> Result<(), HistoryCacheError> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO vault_history (file_path, commit_hash, timestamp, author, summary)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                entry.file_path,
                entry.commit_hash,
                entry.timestamp,
                entry.author,
                entry.summary,
            ],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

/// Parse git log for files under a prefix.
fn git_log_for_prefix(
    repo_root: &Path,
    vault_prefix: &str,
) -> Result<Vec<HistoryEntry>, HistoryCacheError> {
    let output = Command::new("git")
        .args([
            "log",
            "--format=%H|%at|%an|%s",
            "--name-only",
            "--diff-filter=AMRC",
            "--",
            vault_prefix,
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| HistoryCacheError::GitFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Empty repo or no commits is not an error
        if stderr.contains("does not have any commits") || stderr.contains("unknown revision") {
            return Ok(Vec::new());
        }
        return Err(HistoryCacheError::GitFailed(stderr.to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_git_log(&stdout)
}

/// Parse the custom git log format into HistoryEntry records.
fn parse_git_log(output: &str) -> Result<Vec<HistoryEntry>, HistoryCacheError> {
    let mut entries = Vec::new();
    let mut current_commit: Option<(String, i64, String, String)> = None;

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Check if this is a commit line (hash|timestamp|author|summary)
        let parts: Vec<&str> = line.splitn(4, '|').collect();
        if parts.len() == 4
            && parts[0].len() == 40
            && parts[0].chars().all(|c| c.is_ascii_hexdigit())
        {
            let timestamp = parts[1].parse::<i64>().unwrap_or(0);
            current_commit = Some((
                parts[0].to_string(),
                timestamp,
                parts[2].to_string(),
                parts[3].to_string(),
            ));
        } else if let Some((ref hash, ts, ref author, ref summary)) = current_commit {
            // This is a filename line
            if line.ends_with(".md") {
                entries.push(HistoryEntry {
                    file_path: line.to_string(),
                    commit_hash: hash.clone(),
                    timestamp: ts,
                    author: author.clone(),
                    summary: summary.clone(),
                });
            }
        }
    }

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteMemoryStore;

    fn canonical_path(entity_id: &str) -> String {
        format!("ledger/tools/{entity_id}/{entity_id}.md")
    }

    async fn setup() -> (SqliteMemoryStore, VaultHistoryCache) {
        let store = SqliteMemoryStore::open_in_memory().unwrap();
        store.initialize().await.unwrap();
        let cache = VaultHistoryCache::new(&store);
        cache.initialize().await.unwrap();
        (store, cache)
    }

    #[tokio::test]
    async fn add_and_query_entry() {
        let (_store, cache) = setup().await;

        let entry = HistoryEntry {
            file_path: canonical_path("rust"),
            commit_hash: "abc123def456".to_string(),
            timestamp: 1711234567,
            author: "agent".to_string(),
            summary: "memory(rust): added 2 facts".to_string(),
        };

        cache.add_entry(&entry).await.unwrap();

        let history = cache
            .get_file_history(&canonical_path("rust"))
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].commit_hash, "abc123def456");
        assert_eq!(history[0].summary, "memory(rust): added 2 facts");
    }

    #[tokio::test]
    async fn last_updated_returns_max_timestamp() {
        let (_store, cache) = setup().await;

        for (i, ts) in [100, 300, 200].iter().enumerate() {
            cache
                .add_entry(&HistoryEntry {
                    file_path: canonical_path("test"),
                    commit_hash: format!("hash{}", i),
                    timestamp: *ts,
                    author: "agent".to_string(),
                    summary: "test".to_string(),
                })
                .await
                .unwrap();
        }

        let last = cache.last_updated(&canonical_path("test")).await.unwrap();
        assert_eq!(last, Some(300));
    }

    #[tokio::test]
    async fn last_updated_returns_none_for_unknown() {
        let (_store, cache) = setup().await;

        let last = cache.last_updated("nonexistent.md").await.unwrap();
        assert_eq!(last, None);
    }

    #[tokio::test]
    async fn changes_since_filters_by_timestamp() {
        let (_store, cache) = setup().await;

        for ts in [100, 200, 300, 400] {
            cache
                .add_entry(&HistoryEntry {
                    file_path: canonical_path(&format!("e{ts}")),
                    commit_hash: format!("hash{}", ts),
                    timestamp: ts,
                    author: "agent".to_string(),
                    summary: format!("commit at {}", ts),
                })
                .await
                .unwrap();
        }

        let recent = cache.changes_since(250).await.unwrap();
        assert_eq!(recent.len(), 2); // 300 and 400
    }

    #[tokio::test]
    async fn duplicate_entries_ignored() {
        let (_store, cache) = setup().await;

        let entry = HistoryEntry {
            file_path: canonical_path("rust"),
            commit_hash: "same_hash".to_string(),
            timestamp: 100,
            author: "agent".to_string(),
            summary: "test".to_string(),
        };

        cache.add_entry(&entry).await.unwrap();
        cache.add_entry(&entry).await.unwrap(); // duplicate — should not error

        let history = cache
            .get_file_history(&canonical_path("rust"))
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn parse_git_log_format() {
        let log = "\
abcdef1234567890abcdef1234567890abcdef12|1711234567|Claude|memory(rust): added facts

ledger/tools/rust/rust.md
ledger/tools/go/go.md

1234567890abcdef1234567890abcdef12345678|1711234600|user|manual edit

ledger/tools/rust/rust.md
";

        let entries = parse_git_log(log).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].file_path, "ledger/tools/rust/rust.md");
        assert_eq!(entries[0].author, "Claude");
        assert_eq!(entries[1].file_path, "ledger/tools/go/go.md");
        assert_eq!(entries[2].file_path, "ledger/tools/rust/rust.md");
        assert_eq!(entries[2].author, "user");
    }

    #[test]
    fn parse_git_log_empty() {
        let entries = parse_git_log("").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_git_log_skips_non_md() {
        let log = "\
abcdef1234567890abcdef1234567890abcdef12|1711234567|agent|test

ledger/tools/rust/rust.md
some/other/file.txt
readme.md
";
        let entries = parse_git_log(log).unwrap();
        // Should include both .md files
        assert_eq!(entries.len(), 2);
    }
}
