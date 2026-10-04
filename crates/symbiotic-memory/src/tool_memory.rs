//! Tool Invocation Memory — per-tool success/failure/latency tracking.
//!
//! Logs every tool invocation for reliability analysis and evolution fitness
//! evaluation. Provides query APIs for success rates, latency percentiles,
//! and error frequency grouping.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A single tool invocation record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    /// Name of the tool that was invoked.
    pub tool_name: String,
    /// Agent that triggered the invocation.
    pub agent_id: String,
    /// Unix epoch seconds when the invocation started.
    pub timestamp: u64,
    /// Wall-clock duration of the invocation in milliseconds.
    pub duration_ms: u64,
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Error message if the invocation failed.
    pub error: Option<String>,
    /// SHA-256 hex digest of the input parameters.
    pub input_hash: String,
    /// Size of the output in bytes.
    pub output_size: usize,
    /// Stable fingerprint of the agent identity for correlation/provenance.
    #[serde(default)]
    pub agent_fingerprint: String,
    /// Per-agent hash-chain head after recording this invocation.
    #[serde(default)]
    pub chain_hash: String,
}

/// Summary of the most recent failure in a stats window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolFailureSummary {
    pub timestamp: u64,
    pub error: String,
}

/// Rolling reliability snapshot for a tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolStats {
    pub tool_name: String,
    pub window_size: usize,
    pub total_invocations: usize,
    pub success_rate: f64,
    pub p50_latency_ms: u64,
    pub p99_latency_ms: u64,
    pub last_failure: Option<ToolFailureSummary>,
}

/// Per-agent effectiveness summary for a tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentToolAffinity {
    pub agent_id: String,
    pub agent_fingerprint: String,
    pub tool_name: String,
    pub window_size: usize,
    pub total_invocations: usize,
    pub success_rate: f64,
    pub p50_latency_ms: u64,
    pub p99_latency_ms: u64,
    pub last_failure: Option<ToolFailureSummary>,
    pub latest_chain_hash: String,
}

/// In-memory store for tool invocation history.
pub struct ToolMemoryStore {
    invocations: Vec<ToolInvocation>,
    conn: Option<Connection>,
    agent_chain_heads: HashMap<String, String>,
}

impl ToolMemoryStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            invocations: Vec::new(),
            conn: None,
            agent_chain_heads: HashMap::new(),
        }
    }

    /// Open or create a durable store backed by SQLite.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| rusqlite::Error::InvalidPath(parent.to_path_buf()))?;
        }

        let conn = Connection::open(path)?;
        init_schema(&conn)?;
        let mut invocations = load_invocations(&conn)?;
        let agent_chain_heads = hydrate_provenance(&conn, &mut invocations)?;

        Ok(Self {
            invocations,
            conn: Some(conn),
            agent_chain_heads,
        })
    }

    /// Record a tool invocation.
    pub fn record(&mut self, invocation: ToolInvocation) {
        let invocation = self.with_provenance(invocation);
        if let Some(conn) = &self.conn {
            if let Err(error) = persist_invocation(conn, &invocation) {
                tracing::warn!(
                    error = %error,
                    tool = %invocation.tool_name,
                    agent_id = %invocation.agent_id,
                    "tool_memory: failed to persist invocation"
                );
            }
        }
        self.invocations.push(invocation);
    }

    /// Return all recorded invocations.
    pub fn invocations(&self) -> &[ToolInvocation] {
        &self.invocations
    }

    /// Calculate the success rate for a specific tool.
    /// Returns `None` if no invocations exist for the tool.
    pub fn success_rate(&self, tool_name: &str) -> Option<f64> {
        let tool_invocations: Vec<&ToolInvocation> = self
            .invocations
            .iter()
            .filter(|i| i.tool_name == tool_name)
            .collect();

        if tool_invocations.is_empty() {
            return None;
        }

        let successes = tool_invocations.iter().filter(|i| i.success).count();
        Some(successes as f64 / tool_invocations.len() as f64)
    }

    /// Calculate the latency at the given percentile for a specific tool.
    /// `percentile` should be in `[0.0, 100.0]` (e.g., 50.0 for p50, 99.0 for p99).
    /// Returns `None` if no invocations exist for the tool.
    pub fn latency_percentile(&self, tool_name: &str, percentile: f64) -> Option<u64> {
        let mut latencies: Vec<u64> = self
            .invocations
            .iter()
            .filter(|i| i.tool_name == tool_name)
            .map(|i| i.duration_ms)
            .collect();

        if latencies.is_empty() {
            return None;
        }

        latencies.sort_unstable();

        let index = ((percentile / 100.0) * (latencies.len() as f64 - 1.0))
            .round()
            .max(0.0) as usize;
        let index = index.min(latencies.len() - 1);
        Some(latencies[index])
    }

    /// Group error messages by frequency for a specific tool.
    /// Returns a Vec of `(error_message, count)` sorted by count descending.
    pub fn error_frequency(&self, tool_name: &str) -> Vec<(String, usize)> {
        let mut freq: HashMap<String, usize> = HashMap::new();
        for inv in self.invocations.iter().filter(|i| i.tool_name == tool_name) {
            if let Some(ref err) = inv.error {
                *freq.entry(err.clone()).or_insert(0) += 1;
            }
        }
        let mut result: Vec<(String, usize)> = freq.into_iter().collect();
        result.sort_by_key(|a| std::cmp::Reverse(a.1));
        result
    }

    /// Calculate rolling stats for the latest `window_size` invocations of a tool.
    /// When `window_size` is 0, all invocations are considered.
    pub fn stats(&self, tool_name: &str, window_size: usize) -> Option<ToolStats> {
        let window = self.window_for_tool(tool_name, window_size);
        if window.is_empty() {
            return None;
        }

        let successes = window.iter().filter(|inv| inv.success).count();
        let success_rate = successes as f64 / window.len() as f64;
        let p50_latency_ms = latency_percentile_from_slice(&window, 50.0)?;
        let p99_latency_ms = latency_percentile_from_slice(&window, 99.0)?;
        let last_failure = window.iter().find_map(|inv| {
            if inv.success {
                None
            } else {
                inv.error.as_ref().map(|error| ToolFailureSummary {
                    timestamp: inv.timestamp,
                    error: error.clone(),
                })
            }
        });

        Some(ToolStats {
            tool_name: tool_name.to_string(),
            window_size,
            total_invocations: window.len(),
            success_rate,
            p50_latency_ms,
            p99_latency_ms,
            last_failure,
        })
    }

    /// Rank tools by how effectively a given agent uses them.
    pub fn affinity_for_agent(&self, agent_id: &str, window_size: usize) -> Vec<AgentToolAffinity> {
        let mut by_tool: HashMap<&str, Vec<&ToolInvocation>> = HashMap::new();
        for invocation in self
            .invocations
            .iter()
            .rev()
            .filter(|invocation| invocation.agent_id == agent_id)
        {
            let bucket = by_tool.entry(invocation.tool_name.as_str()).or_default();
            if window_size == 0 || bucket.len() < window_size {
                bucket.push(invocation);
            }
        }

        let mut affinities = by_tool
            .into_iter()
            .filter_map(|(tool_name, window)| {
                let total_invocations = window.len();
                if total_invocations == 0 {
                    return None;
                }
                let successes = window.iter().filter(|inv| inv.success).count();
                let success_rate = successes as f64 / total_invocations as f64;
                let p50_latency_ms = latency_percentile_from_slice(&window, 50.0)?;
                let p99_latency_ms = latency_percentile_from_slice(&window, 99.0)?;
                let last_failure = window.iter().find_map(|inv| {
                    if inv.success {
                        None
                    } else {
                        inv.error.as_ref().map(|error| ToolFailureSummary {
                            timestamp: inv.timestamp,
                            error: error.clone(),
                        })
                    }
                });
                let latest = window.first()?;
                Some(AgentToolAffinity {
                    agent_id: agent_id.to_string(),
                    agent_fingerprint: latest.agent_fingerprint.clone(),
                    tool_name: tool_name.to_string(),
                    window_size,
                    total_invocations,
                    success_rate,
                    p50_latency_ms,
                    p99_latency_ms,
                    last_failure,
                    latest_chain_hash: latest.chain_hash.clone(),
                })
            })
            .collect::<Vec<_>>();

        affinities.sort_by(|a, b| {
            b.success_rate
                .partial_cmp(&a.success_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.p50_latency_ms.cmp(&b.p50_latency_ms))
                .then_with(|| b.total_invocations.cmp(&a.total_invocations))
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
        affinities
    }

    fn window_for_tool(&self, tool_name: &str, window_size: usize) -> Vec<&ToolInvocation> {
        let iter = self
            .invocations
            .iter()
            .rev()
            .filter(|inv| inv.tool_name == tool_name);
        if window_size == 0 {
            iter.collect()
        } else {
            iter.take(window_size).collect()
        }
    }

    fn with_provenance(&mut self, mut invocation: ToolInvocation) -> ToolInvocation {
        if invocation.agent_fingerprint.is_empty() {
            invocation.agent_fingerprint = sha256_hex(&invocation.agent_id);
        }
        let previous = self
            .agent_chain_heads
            .get(&invocation.agent_id)
            .map(String::as_str);
        if invocation.chain_hash.is_empty() {
            invocation.chain_hash = invocation_chain_hash(previous, &invocation);
        }
        self.agent_chain_heads
            .insert(invocation.agent_id.clone(), invocation.chain_hash.clone());
        invocation
    }
}

impl Default for ToolMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute SHA-256 hex digest of the given input string.
pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn latency_percentile_from_slice(invocations: &[&ToolInvocation], percentile: f64) -> Option<u64> {
    if invocations.is_empty() {
        return None;
    }

    let mut latencies: Vec<u64> = invocations.iter().map(|inv| inv.duration_ms).collect();
    latencies.sort_unstable();

    let index = ((percentile / 100.0) * (latencies.len() as f64 - 1.0))
        .round()
        .max(0.0) as usize;
    let index = index.min(latencies.len() - 1);
    Some(latencies[index])
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tool_invocations (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            tool_name   TEXT NOT NULL,
            agent_id    TEXT NOT NULL,
            timestamp   INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL,
            success     INTEGER NOT NULL,
            error       TEXT,
            input_hash  TEXT NOT NULL,
            output_size INTEGER NOT NULL,
            agent_fingerprint TEXT NOT NULL DEFAULT '',
            chain_hash  TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_tool_invocations_tool_time
            ON tool_invocations (tool_name, timestamp);
        CREATE INDEX IF NOT EXISTS idx_tool_invocations_agent_time
            ON tool_invocations (agent_id, timestamp);",
    )?;
    ensure_column(
        conn,
        "tool_invocations",
        "agent_fingerprint",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(
        conn,
        "tool_invocations",
        "chain_hash",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    Ok(())
}

fn load_invocations(conn: &Connection) -> rusqlite::Result<Vec<ToolInvocation>> {
    let mut stmt = conn.prepare(
        "SELECT tool_name, agent_id, timestamp, duration_ms, success, error, input_hash, output_size, agent_fingerprint, chain_hash
         FROM tool_invocations
         ORDER BY id ASC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(ToolInvocation {
            tool_name: row.get(0)?,
            agent_id: row.get(1)?,
            timestamp: row.get(2)?,
            duration_ms: row.get(3)?,
            success: row.get::<_, i64>(4)? != 0,
            error: row.get(5)?,
            input_hash: row.get(6)?,
            output_size: row.get(7)?,
            agent_fingerprint: row.get(8)?,
            chain_hash: row.get(9)?,
        })
    })?;

    rows.collect()
}

fn persist_invocation(conn: &Connection, invocation: &ToolInvocation) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO tool_invocations (
            tool_name, agent_id, timestamp, duration_ms, success, error, input_hash, output_size,
            agent_fingerprint, chain_hash
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            &invocation.tool_name,
            &invocation.agent_id,
            invocation.timestamp,
            invocation.duration_ms,
            invocation.success as i64,
            &invocation.error,
            &invocation.input_hash,
            invocation.output_size as i64,
            &invocation.agent_fingerprint,
            &invocation.chain_hash,
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
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == column);
    if !exists {
        let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
        conn.execute_batch(&alter)?;
    }
    Ok(())
}

fn hydrate_provenance(
    conn: &Connection,
    invocations: &mut [ToolInvocation],
) -> rusqlite::Result<HashMap<String, String>> {
    let mut heads = HashMap::new();
    let needs_backfill = invocations.iter().any(|invocation| {
        invocation.agent_fingerprint.is_empty() || invocation.chain_hash.is_empty()
    });

    for invocation in invocations.iter_mut() {
        if invocation.agent_fingerprint.is_empty() {
            invocation.agent_fingerprint = sha256_hex(&invocation.agent_id);
        }
        if invocation.chain_hash.is_empty() {
            let previous = heads.get(&invocation.agent_id).map(String::as_str);
            invocation.chain_hash = invocation_chain_hash(previous, invocation);
        }
        heads.insert(invocation.agent_id.clone(), invocation.chain_hash.clone());
    }

    if needs_backfill {
        let mut stmt = conn.prepare("SELECT id FROM tool_invocations ORDER BY id ASC")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let tx = conn.unchecked_transaction()?;
        for (row_id, invocation) in ids.into_iter().zip(invocations.iter()) {
            tx.execute(
                "UPDATE tool_invocations
                 SET agent_fingerprint = ?1, chain_hash = ?2
                 WHERE id = ?3",
                params![
                    &invocation.agent_fingerprint,
                    &invocation.chain_hash,
                    row_id,
                ],
            )?;
        }
        tx.commit()?;
    }

    Ok(heads)
}

fn invocation_chain_hash(previous: Option<&str>, invocation: &ToolInvocation) -> String {
    sha256_hex(&format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}",
        previous.unwrap_or("root"),
        invocation.agent_id,
        invocation.tool_name,
        invocation.timestamp,
        invocation.duration_ms,
        invocation.success,
        invocation.input_hash,
        invocation.output_size,
        invocation.error.as_deref().unwrap_or("")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_invocation(
        tool_name: &str,
        success: bool,
        duration_ms: u64,
        error: Option<&str>,
    ) -> ToolInvocation {
        ToolInvocation {
            tool_name: tool_name.to_string(),
            agent_id: "test-agent".to_string(),
            timestamp: 1000,
            duration_ms,
            success,
            error: error.map(|s| s.to_string()),
            input_hash: sha256_hex("test-params"),
            output_size: 100,
            agent_fingerprint: String::new(),
            chain_hash: String::new(),
        }
    }

    #[test]
    fn test_record_invocation() {
        let mut store = ToolMemoryStore::new();
        store.record(make_invocation("recall", true, 50, None));
        assert_eq!(store.invocations().len(), 1);
        assert_eq!(store.invocations()[0].tool_name, "recall");
        assert!(store.invocations()[0].success);
        assert!(!store.invocations()[0].agent_fingerprint.is_empty());
        assert!(!store.invocations()[0].chain_hash.is_empty());
    }

    #[test]
    fn test_success_rate_calculation() {
        let mut store = ToolMemoryStore::new();
        store.record(make_invocation("recall", true, 50, None));
        store.record(make_invocation("recall", true, 60, None));
        store.record(make_invocation("recall", false, 70, Some("timeout")));
        store.record(make_invocation("recall", true, 40, None));

        let rate = store.success_rate("recall").unwrap();
        assert!((rate - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_latency_percentile_p50() {
        let mut store = ToolMemoryStore::new();
        // Latencies: 10, 20, 30, 40, 50
        for ms in [10, 20, 30, 40, 50] {
            store.record(make_invocation("archive", true, ms, None));
        }

        let p50 = store.latency_percentile("archive", 50.0).unwrap();
        assert_eq!(p50, 30); // median of [10,20,30,40,50]

        let p0 = store.latency_percentile("archive", 0.0).unwrap();
        assert_eq!(p0, 10);

        let p100 = store.latency_percentile("archive", 100.0).unwrap();
        assert_eq!(p100, 50);
    }

    #[test]
    fn test_error_frequency_grouping() {
        let mut store = ToolMemoryStore::new();
        store.record(make_invocation("queue", false, 50, Some("timeout")));
        store.record(make_invocation("queue", false, 60, Some("timeout")));
        store.record(make_invocation("queue", false, 70, Some("rate limit")));
        store.record(make_invocation("queue", true, 40, None));

        let freq = store.error_frequency("queue");
        assert_eq!(freq.len(), 2);
        assert_eq!(freq[0], ("timeout".to_string(), 2));
        assert_eq!(freq[1], ("rate limit".to_string(), 1));
    }

    #[test]
    fn test_empty_store_returns_none() {
        let store = ToolMemoryStore::new();
        assert!(store.success_rate("nonexistent").is_none());
        assert!(store.latency_percentile("nonexistent", 50.0).is_none());
        assert!(store.error_frequency("nonexistent").is_empty());
        assert!(store.invocations().is_empty());
    }

    #[test]
    fn test_multiple_tools_tracked_independently() {
        let mut store = ToolMemoryStore::new();
        store.record(make_invocation("recall", true, 100, None));
        store.record(make_invocation("recall", false, 200, Some("err")));
        store.record(make_invocation("archive", true, 50, None));
        store.record(make_invocation("archive", true, 60, None));

        // recall: 1/2 = 50%
        let recall_rate = store.success_rate("recall").unwrap();
        assert!((recall_rate - 0.5).abs() < f64::EPSILON);

        // archive: 2/2 = 100%
        let archive_rate = store.success_rate("archive").unwrap();
        assert!((archive_rate - 1.0).abs() < f64::EPSILON);

        // Latencies are independent
        let recall_p50 = store.latency_percentile("recall", 50.0).unwrap();
        let archive_p50 = store.latency_percentile("archive", 50.0).unwrap();
        assert_ne!(recall_p50, archive_p50);

        // Error frequency is independent
        let recall_errors = store.error_frequency("recall");
        assert_eq!(recall_errors.len(), 1);
        let archive_errors = store.error_frequency("archive");
        assert!(archive_errors.is_empty());
    }

    #[test]
    fn test_sha256_hex_deterministic() {
        let h1 = sha256_hex("hello");
        let h2 = sha256_hex("hello");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);

        let h3 = sha256_hex("world");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_persistence_round_trip_preserves_query_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("tool-memory.db");

        {
            let mut store = ToolMemoryStore::open(&path).expect("open durable store");
            let mut ok = make_invocation("recall", true, 40, None);
            ok.timestamp = 1_000;
            let mut err = make_invocation("recall", false, 120, Some("timeout"));
            err.timestamp = 1_001;
            store.record(ok);
            store.record(err);
        }

        let store = ToolMemoryStore::open(&path).expect("reopen durable store");
        assert_eq!(store.invocations().len(), 2);
        let rate = store.success_rate("recall").expect("success rate");
        assert!((rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(store.latency_percentile("recall", 50.0), Some(120));
        assert_eq!(
            store.error_frequency("recall"),
            vec![("timeout".to_string(), 1)]
        );
    }

    #[test]
    fn test_stats_uses_latest_window_and_reports_last_failure() {
        let mut store = ToolMemoryStore::new();

        let mut old = make_invocation("queue", true, 10, None);
        old.timestamp = 1_000;
        let mut middle = make_invocation("queue", false, 40, Some("rate limit"));
        middle.timestamp = 1_100;
        let mut latest = make_invocation("queue", true, 20, None);
        latest.timestamp = 1_200;

        store.record(old);
        store.record(middle);
        store.record(latest);

        let stats = store.stats("queue", 2).expect("stats");
        assert_eq!(stats.total_invocations, 2);
        assert!((stats.success_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(stats.p50_latency_ms, 40);
        assert_eq!(stats.p99_latency_ms, 40);
        assert_eq!(
            stats.last_failure,
            Some(ToolFailureSummary {
                timestamp: 1_100,
                error: "rate limit".to_string(),
            })
        );
    }

    #[test]
    fn test_affinity_for_agent_ranks_tools_by_effectiveness() {
        let mut store = ToolMemoryStore::new();

        let mut recall_ok = make_invocation("recall", true, 20, None);
        recall_ok.agent_id = "agent-a".to_string();
        recall_ok.timestamp = 100;
        let mut recall_bad = make_invocation("recall", false, 60, Some("timeout"));
        recall_bad.agent_id = "agent-a".to_string();
        recall_bad.timestamp = 101;
        let mut archive_ok = make_invocation("archive", true, 10, None);
        archive_ok.agent_id = "agent-a".to_string();
        archive_ok.timestamp = 102;
        let mut queue_other = make_invocation("queue", true, 5, None);
        queue_other.agent_id = "agent-b".to_string();
        queue_other.timestamp = 103;

        store.record(recall_ok);
        store.record(recall_bad);
        store.record(archive_ok);
        store.record(queue_other);

        let affinity = store.affinity_for_agent("agent-a", 50);
        assert_eq!(affinity.len(), 2);
        assert_eq!(affinity[0].tool_name, "archive");
        assert!((affinity[0].success_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(affinity[1].tool_name, "recall");
        assert!((affinity[1].success_rate - 0.5).abs() < f64::EPSILON);
        assert!(!affinity[0].latest_chain_hash.is_empty());
    }

    #[test]
    fn test_persistence_round_trip_preserves_affinity_provenance() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("tool-memory.db");

        {
            let mut store = ToolMemoryStore::open(&path).expect("open durable store");
            let mut first = make_invocation("recall", true, 40, None);
            first.agent_id = "agent-a".to_string();
            first.timestamp = 1_000;
            let mut second = make_invocation("archive", true, 20, None);
            second.agent_id = "agent-a".to_string();
            second.timestamp = 1_001;
            store.record(first);
            store.record(second);
        }

        let store = ToolMemoryStore::open(&path).expect("reopen durable store");
        let affinity = store.affinity_for_agent("agent-a", 50);
        assert_eq!(affinity.len(), 2);
        assert!(affinity
            .iter()
            .all(|entry| !entry.agent_fingerprint.is_empty()));
        assert!(affinity
            .iter()
            .all(|entry| !entry.latest_chain_hash.is_empty()));
    }
}
