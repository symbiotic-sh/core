//! Graduation store for the Process Engineer meta-agent.
//!
//! Tracks per-goal-type efficiency evaluations and determines when a goal type
//! has stabilized enough to stop spawning PE agents. Graduation is reversible —
//! if efficiency drops below the threshold on a new evaluation, graduation is
//! revoked.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::monitoring::MonitorError;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A single Process Engineer evaluation record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeEvaluation {
    /// Unique evaluation identifier.
    pub eval_id: String,
    /// Goal type that was evaluated (e.g., "research", "intake.fetch").
    pub goal_type: String,
    /// Role of the observed agent.
    pub observed_role: String,
    /// Efficiency score from 0.0 to 1.0.
    pub efficiency_score: f64,
    /// Number of redundant tool calls detected.
    pub redundant_tool_calls: u32,
    /// Number of avoidable errors detected.
    pub avoidable_errors: u32,
    /// Number of rules created during this evaluation.
    pub rules_created: u32,
    /// Number of skills created during this evaluation.
    pub skills_created: u32,
    /// Number of prompt improvements proposed during this evaluation.
    pub prompts_proposed: u32,
    /// When the evaluation was performed.
    pub evaluated_at: DateTime<Utc>,
}

/// Configuration for graduation thresholds.
#[derive(Debug, Clone)]
pub struct GraduationConfig {
    /// Minimum efficiency score to count as a passing evaluation.
    pub min_efficiency_score: f64,
    /// Number of consecutive passing evaluations required for graduation.
    pub required_consecutive: u32,
    /// Minimum total evaluations before graduation is possible.
    pub min_evaluations: u32,
}

impl Default for GraduationConfig {
    fn default() -> Self {
        Self {
            min_efficiency_score: 0.90,
            required_consecutive: 3,
            min_evaluations: 5,
        }
    }
}

/// Current graduation status for a goal type.
#[derive(Debug, Clone, PartialEq)]
pub enum GraduationStatus {
    /// Not enough evaluations yet.
    InsufficientData {
        total_evaluations: u32,
        required: u32,
    },
    /// Enough evaluations but not enough consecutive passes.
    NotConverged {
        consecutive_passing: u32,
        required: u32,
    },
    /// Goal type has graduated — PE should stop spawning.
    Graduated { since: DateTime<Utc> },
}

// ---------------------------------------------------------------------------
// GraduationStore trait
// ---------------------------------------------------------------------------

/// Trait for PE evaluation persistence and graduation tracking.
pub trait GraduationStore: Send + Sync {
    /// Record a PE evaluation.
    fn record_evaluation(&self, eval: &PeEvaluation) -> Result<(), MonitorError>;

    /// Check the graduation status for a goal type.
    fn check_graduation(
        &self,
        goal_type: &str,
        config: &GraduationConfig,
    ) -> Result<GraduationStatus, MonitorError>;

    /// Convenience: returns `true` if the goal type is graduated.
    fn is_graduated(
        &self,
        goal_type: &str,
        config: &GraduationConfig,
    ) -> Result<bool, MonitorError> {
        Ok(matches!(
            self.check_graduation(goal_type, config)?,
            GraduationStatus::Graduated { .. }
        ))
    }

    /// Get all evaluations for a goal type, ordered by evaluation time (newest first).
    fn evaluations_for_goal_type(&self, goal_type: &str)
        -> Result<Vec<PeEvaluation>, MonitorError>;
}

// ---------------------------------------------------------------------------
// SQLite implementation
// ---------------------------------------------------------------------------

/// SQLite-backed graduation store.
pub struct SqliteGraduationStore {
    conn: Mutex<Connection>,
}

impl SqliteGraduationStore {
    /// Open (or create) a graduation store at the given path.
    pub fn open(path: &Path) -> Result<Self, MonitorError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MonitorError::Storage(format!("failed to create directory: {e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| MonitorError::Storage(format!("failed to open database: {e}")))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    /// Create an in-memory store (for testing).
    pub fn open_in_memory() -> Result<Self, MonitorError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| MonitorError::Storage(format!("failed to open in-memory db: {e}")))?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, MonitorError> {
        self.conn
            .lock()
            .map_err(|_| MonitorError::Storage("connection lock poisoned".to_string()))
    }

    fn init_schema(&self) -> Result<(), MonitorError> {
        self.lock_conn()?
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS pe_evaluations (
                    eval_id TEXT PRIMARY KEY,
                    goal_type TEXT NOT NULL,
                    observed_role TEXT NOT NULL,
                    efficiency_score REAL NOT NULL,
                    redundant_tool_calls INTEGER NOT NULL DEFAULT 0,
                    avoidable_errors INTEGER NOT NULL DEFAULT 0,
                    rules_created INTEGER NOT NULL DEFAULT 0,
                    skills_created INTEGER NOT NULL DEFAULT 0,
                    prompts_proposed INTEGER NOT NULL DEFAULT 0,
                    evaluated_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_pe_eval_goal_type
                    ON pe_evaluations(goal_type);
                CREATE INDEX IF NOT EXISTS idx_pe_eval_evaluated_at
                    ON pe_evaluations(evaluated_at);

                CREATE TABLE IF NOT EXISTS pe_graduations (
                    goal_type TEXT PRIMARY KEY,
                    graduated_at TEXT NOT NULL,
                    last_eval_id TEXT NOT NULL
                );
                ",
            )
            .map_err(|e| MonitorError::Storage(format!("schema init failed: {e}")))?;
        Ok(())
    }

    fn row_to_evaluation(row: &rusqlite::Row<'_>) -> rusqlite::Result<PeEvaluation> {
        let eval_id: String = row.get(0)?;
        let goal_type: String = row.get(1)?;
        let observed_role: String = row.get(2)?;
        let efficiency_score: f64 = row.get(3)?;
        let redundant_tool_calls: i32 = row.get(4)?;
        let avoidable_errors: i32 = row.get(5)?;
        let rules_created: i32 = row.get(6)?;
        let skills_created: i32 = row.get(7)?;
        let prompts_proposed: i32 = row.get(8)?;
        let evaluated_at_str: String = row.get(9)?;

        let evaluated_at = DateTime::parse_from_rfc3339(&evaluated_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        Ok(PeEvaluation {
            eval_id,
            goal_type,
            observed_role,
            efficiency_score,
            redundant_tool_calls: redundant_tool_calls as u32,
            avoidable_errors: avoidable_errors as u32,
            rules_created: rules_created as u32,
            skills_created: skills_created as u32,
            prompts_proposed: prompts_proposed as u32,
            evaluated_at,
        })
    }
}

impl GraduationStore for SqliteGraduationStore {
    fn record_evaluation(&self, eval: &PeEvaluation) -> Result<(), MonitorError> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO pe_evaluations
             (eval_id, goal_type, observed_role, efficiency_score,
              redundant_tool_calls, avoidable_errors, rules_created,
              skills_created, prompts_proposed, evaluated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                eval.eval_id,
                eval.goal_type,
                eval.observed_role,
                eval.efficiency_score,
                eval.redundant_tool_calls as i32,
                eval.avoidable_errors as i32,
                eval.rules_created as i32,
                eval.skills_created as i32,
                eval.prompts_proposed as i32,
                eval.evaluated_at.to_rfc3339(),
            ],
        )
        .map_err(|e| MonitorError::Storage(format!("insert failed: {e}")))?;

        // Update graduation table based on current state.
        let config = GraduationConfig::default();
        let status = self.check_graduation_inner(&conn, &eval.goal_type, &config)?;
        match status {
            GraduationStatus::Graduated { since } => {
                conn.execute(
                    "INSERT OR REPLACE INTO pe_graduations (goal_type, graduated_at, last_eval_id)
                     VALUES (?1, ?2, ?3)",
                    params![eval.goal_type, since.to_rfc3339(), eval.eval_id],
                )
                .map_err(|e| MonitorError::Storage(format!("graduation update failed: {e}")))?;
            }
            _ => {
                // Revoke graduation if it existed.
                conn.execute(
                    "DELETE FROM pe_graduations WHERE goal_type = ?1",
                    params![eval.goal_type],
                )
                .map_err(|e| MonitorError::Storage(format!("graduation revoke failed: {e}")))?;
            }
        }

        Ok(())
    }

    fn check_graduation(
        &self,
        goal_type: &str,
        config: &GraduationConfig,
    ) -> Result<GraduationStatus, MonitorError> {
        let conn = self.lock_conn()?;
        self.check_graduation_inner(&conn, goal_type, config)
    }

    fn evaluations_for_goal_type(
        &self,
        goal_type: &str,
    ) -> Result<Vec<PeEvaluation>, MonitorError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT eval_id, goal_type, observed_role, efficiency_score,
                        redundant_tool_calls, avoidable_errors, rules_created,
                        skills_created, prompts_proposed, evaluated_at
                 FROM pe_evaluations WHERE goal_type = ?1
                 ORDER BY evaluated_at DESC",
            )
            .map_err(|e| MonitorError::Storage(format!("query prepare failed: {e}")))?;

        let rows = stmt
            .query_map(params![goal_type], Self::row_to_evaluation)
            .map_err(|e| MonitorError::Storage(format!("query failed: {e}")))?;

        let mut evaluations = Vec::new();
        for row_result in rows {
            let eval =
                row_result.map_err(|e| MonitorError::Storage(format!("row read failed: {e}")))?;
            evaluations.push(eval);
        }

        Ok(evaluations)
    }
}

impl SqliteGraduationStore {
    fn check_graduation_inner(
        &self,
        conn: &Connection,
        goal_type: &str,
        config: &GraduationConfig,
    ) -> Result<GraduationStatus, MonitorError> {
        // Count total evaluations.
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pe_evaluations WHERE goal_type = ?1",
                params![goal_type],
                |row| row.get(0),
            )
            .map_err(|e| MonitorError::Storage(format!("count query failed: {e}")))?;

        if (total as u32) < config.min_evaluations {
            return Ok(GraduationStatus::InsufficientData {
                total_evaluations: total as u32,
                required: config.min_evaluations,
            });
        }

        // Get the N most recent evaluations (where N = required_consecutive)
        // and check if they all pass the threshold.
        let mut stmt = conn
            .prepare(
                "SELECT efficiency_score, evaluated_at FROM pe_evaluations
                 WHERE goal_type = ?1
                 ORDER BY evaluated_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| MonitorError::Storage(format!("query prepare failed: {e}")))?;

        let rows: Vec<(f64, String)> = stmt
            .query_map(
                params![goal_type, config.required_consecutive as i64],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|e| MonitorError::Storage(format!("query failed: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        let mut consecutive_passing = 0u32;
        for (score, _) in &rows {
            if *score >= config.min_efficiency_score {
                consecutive_passing += 1;
            } else {
                break;
            }
        }

        if consecutive_passing >= config.required_consecutive {
            // Find when the graduation streak started (oldest passing eval in the streak).
            let graduated_since_str = &rows[consecutive_passing as usize - 1].1;
            let since = DateTime::parse_from_rfc3339(graduated_since_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            Ok(GraduationStatus::Graduated { since })
        } else {
            Ok(GraduationStatus::NotConverged {
                consecutive_passing,
                required: config.required_consecutive,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_eval(goal_type: &str, score: f64, eval_id: &str) -> PeEvaluation {
        PeEvaluation {
            eval_id: eval_id.to_string(),
            goal_type: goal_type.to_string(),
            observed_role: "researcher".to_string(),
            efficiency_score: score,
            redundant_tool_calls: if score < 0.9 { 3 } else { 0 },
            avoidable_errors: if score < 0.9 { 1 } else { 0 },
            rules_created: 1,
            skills_created: 0,
            prompts_proposed: 0,
            evaluated_at: Utc::now(),
        }
    }

    #[test]
    fn insufficient_data_when_few_evaluations() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();
        let config = GraduationConfig::default();

        // Record 3 evaluations (need 5 minimum).
        for i in 0..3 {
            store
                .record_evaluation(&make_eval("research", 0.95, &format!("eval-{i}")))
                .unwrap();
        }

        let status = store.check_graduation("research", &config).unwrap();
        assert!(matches!(
            status,
            GraduationStatus::InsufficientData {
                total_evaluations: 3,
                required: 5,
            }
        ));
        assert!(!store.is_graduated("research", &config).unwrap());
    }

    #[test]
    fn not_converged_when_mixed_scores() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();
        let config = GraduationConfig::default();

        // 5 evaluations, but scores alternate.
        let scores = [0.95, 0.80, 0.92, 0.85, 0.91];
        for (i, &score) in scores.iter().enumerate() {
            store
                .record_evaluation(&make_eval("research", score, &format!("eval-{i}")))
                .unwrap();
        }

        let status = store.check_graduation("research", &config).unwrap();
        match status {
            GraduationStatus::NotConverged {
                consecutive_passing,
                required,
            } => {
                // Most recent is 0.91 (pass), but before that is 0.85 (fail).
                assert_eq!(consecutive_passing, 1);
                assert_eq!(required, 3);
            }
            other => panic!("expected NotConverged, got {other:?}"),
        }
    }

    #[test]
    fn graduates_after_consecutive_passing() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();
        let config = GraduationConfig::default();

        // 5 evaluations: first 2 mediocre, then 3 consecutive passes.
        let scores = [0.75, 0.80, 0.92, 0.95, 0.93];
        for (i, &score) in scores.iter().enumerate() {
            store
                .record_evaluation(&make_eval("research", score, &format!("eval-{i}")))
                .unwrap();
        }

        let status = store.check_graduation("research", &config).unwrap();
        assert!(matches!(status, GraduationStatus::Graduated { .. }));
        assert!(store.is_graduated("research", &config).unwrap());
    }

    #[test]
    fn graduation_revoked_on_regression() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();
        let config = GraduationConfig::default();

        // Graduate first.
        for i in 0..5 {
            store
                .record_evaluation(&make_eval("research", 0.95, &format!("eval-{i}")))
                .unwrap();
        }
        assert!(store.is_graduated("research", &config).unwrap());

        // Now a bad evaluation breaks the streak.
        store
            .record_evaluation(&make_eval("research", 0.70, "eval-bad"))
            .unwrap();

        assert!(!store.is_graduated("research", &config).unwrap());
    }

    #[test]
    fn different_goal_types_independent() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();
        let config = GraduationConfig::default();

        // Graduate "research".
        for i in 0..5 {
            store
                .record_evaluation(&make_eval("research", 0.95, &format!("res-{i}")))
                .unwrap();
        }

        // "intake" has no evaluations.
        assert!(store.is_graduated("research", &config).unwrap());
        assert!(!store.is_graduated("intake", &config).unwrap());
    }

    #[test]
    fn evaluations_for_goal_type_returns_ordered() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();

        for i in 0..3 {
            let mut eval = make_eval("research", 0.90 + (i as f64 * 0.02), &format!("eval-{i}"));
            // Stagger timestamps slightly.
            eval.evaluated_at = Utc::now() + chrono::Duration::seconds(i as i64);
            store.record_evaluation(&eval).unwrap();
        }

        let evals = store.evaluations_for_goal_type("research").unwrap();
        assert_eq!(evals.len(), 3);
        // Should be newest first.
        assert!(evals[0].evaluated_at >= evals[1].evaluated_at);
        assert!(evals[1].evaluated_at >= evals[2].evaluated_at);
    }

    #[test]
    fn empty_goal_type_returns_insufficient_data() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();
        let config = GraduationConfig::default();

        let status = store.check_graduation("nonexistent", &config).unwrap();
        assert!(matches!(
            status,
            GraduationStatus::InsufficientData {
                total_evaluations: 0,
                required: 5,
            }
        ));
    }

    #[test]
    fn custom_config_thresholds() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();
        let config = GraduationConfig {
            min_efficiency_score: 0.80,
            required_consecutive: 2,
            min_evaluations: 3,
        };

        // 3 evaluations at 0.85 with relaxed thresholds.
        for i in 0..3 {
            store
                .record_evaluation(&make_eval("research", 0.85, &format!("eval-{i}")))
                .unwrap();
        }

        assert!(store.is_graduated("research", &config).unwrap());
    }

    #[test]
    fn record_evaluation_stores_all_fields() {
        let store = SqliteGraduationStore::open_in_memory().unwrap();

        let eval = PeEvaluation {
            eval_id: "eval-full".to_string(),
            goal_type: "test".to_string(),
            observed_role: "coder".to_string(),
            efficiency_score: 0.88,
            redundant_tool_calls: 2,
            avoidable_errors: 1,
            rules_created: 3,
            skills_created: 1,
            prompts_proposed: 2,
            evaluated_at: Utc::now(),
        };
        store.record_evaluation(&eval).unwrap();

        let evals = store.evaluations_for_goal_type("test").unwrap();
        assert_eq!(evals.len(), 1);
        let stored = &evals[0];
        assert_eq!(stored.eval_id, "eval-full");
        assert_eq!(stored.observed_role, "coder");
        assert!((stored.efficiency_score - 0.88).abs() < 0.001);
        assert_eq!(stored.redundant_tool_calls, 2);
        assert_eq!(stored.avoidable_errors, 1);
        assert_eq!(stored.rules_created, 3);
        assert_eq!(stored.skills_created, 1);
        assert_eq!(stored.prompts_proposed, 2);
    }
}
