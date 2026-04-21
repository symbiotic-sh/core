use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::backend::ExecutionBackend;
use super::classifier::ComplexityAssessment;
use super::types::GoalSource;

/// Every decision point in the pipeline is recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum AuditEvent {
    GoalSubmitted {
        goal_id: String,
        source: GoalSource,
        timestamp: u64,
    },
    ComplexityAssessed {
        goal_id: String,
        assessment: ComplexityAssessment,
    },
    CouncilConvened {
        goal_id: String,
        session_id: String,
        member_count: usize,
    },
    CouncilVerdict {
        goal_id: String,
        session_id: String,
        confidence: f32,
        disagreement_count: usize,
    },
    PlanGenerated {
        goal_id: String,
        plan_name: String,
        phase_count: usize,
        validation_count: usize,
        confidence: f32,
    },
    PlanPresented {
        goal_id: String,
        plan_name: String,
        confidence: f32,
        room_id: String,
    },
    UserResponse {
        goal_id: String,
        response_type: String,
        timestamp: u64,
    },
    PlanRefined {
        goal_id: String,
        refinement_index: usize,
        new_confidence: f32,
        diff_summary: String,
    },
    ExecutionStarted {
        goal_id: String,
        plan_name: String,
        backend: ExecutionBackend,
        timestamp: u64,
    },
    PhaseCompleted {
        goal_id: String,
        phase_name: String,
        passed: bool,
        duration_ms: u64,
    },
    AgentSpawned {
        goal_id: String,
        phase_name: String,
        agent_id: String,
        backend: ExecutionBackend,
    },
    AgentCompleted {
        goal_id: String,
        agent_id: String,
        status: String,
        iterations: usize,
    },
    ExecutionCompleted {
        goal_id: String,
        plan_name: String,
        passed: bool,
        rollback_triggered: bool,
        total_duration_ms: u64,
    },
    GoalRejected {
        goal_id: String,
        reason: String,
        timestamp: u64,
    },
}

/// Audit log that writes to both SQLite and append-only JSONL file.
pub struct AuditLog {
    db_path: PathBuf,
    log_path: PathBuf,
}

impl AuditLog {
    pub fn new(db_path: PathBuf, log_path: PathBuf) -> Result<Self> {
        // Create parent directories.
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Initialize SQLite table.
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pipeline_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                goal_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_audit_goal ON pipeline_audit(goal_id);
            CREATE INDEX IF NOT EXISTS idx_audit_type ON pipeline_audit(event_type);
            CREATE INDEX IF NOT EXISTS idx_audit_time ON pipeline_audit(created_at);",
        )?;

        Ok(Self { db_path, log_path })
    }

    /// Record an audit event.
    pub fn record(&self, event: &AuditEvent) -> Result<()> {
        let payload = serde_json::to_string(event)?;
        let goal_id = event.goal_id();
        let event_type = event.event_type_name();

        // Write to SQLite.
        let conn = rusqlite::Connection::open(&self.db_path)?;
        conn.execute(
            "INSERT INTO pipeline_audit (goal_id, event_type, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![goal_id, event_type, payload],
        )?;

        // Append to JSONL file.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        writeln!(file, "{}", payload)?;

        Ok(())
    }

    /// Query audit events for a goal.
    pub fn events_for_goal(&self, goal_id: &str) -> Result<Vec<AuditEvent>> {
        let conn = rusqlite::Connection::open(&self.db_path)?;
        let mut stmt =
            conn.prepare("SELECT payload FROM pipeline_audit WHERE goal_id = ?1 ORDER BY id")?;
        let events = stmt
            .query_map(rusqlite::params![goal_id], |row| {
                let payload: String = row.get(0)?;
                Ok(payload)
            })?
            .filter_map(|r| r.ok())
            .filter_map(|p| serde_json::from_str(&p).ok())
            .collect();
        Ok(events)
    }

    /// Query all events since a timestamp.
    pub fn events_since(&self, since: u64) -> Result<Vec<AuditEvent>> {
        let conn = rusqlite::Connection::open(&self.db_path)?;
        let mut stmt =
            conn.prepare("SELECT payload FROM pipeline_audit WHERE created_at >= ?1 ORDER BY id")?;
        let events = stmt
            .query_map(rusqlite::params![since], |row| {
                let payload: String = row.get(0)?;
                Ok(payload)
            })?
            .filter_map(|r| r.ok())
            .filter_map(|p| serde_json::from_str(&p).ok())
            .collect();
        Ok(events)
    }
}

impl AuditEvent {
    /// Extract the goal_id from any event variant.
    pub fn goal_id(&self) -> &str {
        match self {
            Self::GoalSubmitted { goal_id, .. }
            | Self::ComplexityAssessed { goal_id, .. }
            | Self::CouncilConvened { goal_id, .. }
            | Self::CouncilVerdict { goal_id, .. }
            | Self::PlanGenerated { goal_id, .. }
            | Self::PlanPresented { goal_id, .. }
            | Self::UserResponse { goal_id, .. }
            | Self::PlanRefined { goal_id, .. }
            | Self::ExecutionStarted { goal_id, .. }
            | Self::PhaseCompleted { goal_id, .. }
            | Self::AgentSpawned { goal_id, .. }
            | Self::AgentCompleted { goal_id, .. }
            | Self::ExecutionCompleted { goal_id, .. }
            | Self::GoalRejected { goal_id, .. } => goal_id,
        }
    }

    /// Returns the serde tag name for this event.
    pub fn event_type_name(&self) -> &'static str {
        match self {
            Self::GoalSubmitted { .. } => "goal_submitted",
            Self::ComplexityAssessed { .. } => "complexity_assessed",
            Self::CouncilConvened { .. } => "council_convened",
            Self::CouncilVerdict { .. } => "council_verdict",
            Self::PlanGenerated { .. } => "plan_generated",
            Self::PlanPresented { .. } => "plan_presented",
            Self::UserResponse { .. } => "user_response",
            Self::PlanRefined { .. } => "plan_refined",
            Self::ExecutionStarted { .. } => "execution_started",
            Self::PhaseCompleted { .. } => "phase_completed",
            Self::AgentSpawned { .. } => "agent_spawned",
            Self::AgentCompleted { .. } => "agent_completed",
            Self::ExecutionCompleted { .. } => "execution_completed",
            Self::GoalRejected { .. } => "goal_rejected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::classifier::{ComplexityAssessment, GoalComplexity, ScoringFactor};
    use crate::pipeline::types::GoalSource;

    fn make_audit_log() -> (AuditLog, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("audit.db");
        let log_path = tmp.path().join("audit.jsonl");
        let audit = AuditLog::new(db_path, log_path).expect("create audit log");
        (audit, tmp)
    }

    fn sample_assessment() -> ComplexityAssessment {
        ComplexityAssessment {
            complexity: GoalComplexity::Moderate,
            aggregate_score: 0.45,
            factors: vec![ScoringFactor {
                name: "domain_count".to_string(),
                weight: 0.15,
                raw_score: 1.0,
                weighted_score: 0.05,
            }],
            llm_override: None,
            reasoning: "test".to_string(),
        }
    }

    #[test]
    fn test_audit_log_record_and_query() {
        let (audit, _tmp) = make_audit_log();

        let event1 = AuditEvent::GoalSubmitted {
            goal_id: "g1".to_string(),
            source: GoalSource::Api {
                client_id: "test".to_string(),
            },
            timestamp: 1000,
        };
        let event2 = AuditEvent::ComplexityAssessed {
            goal_id: "g1".to_string(),
            assessment: sample_assessment(),
        };
        let event3 = AuditEvent::GoalSubmitted {
            goal_id: "g2".to_string(),
            source: GoalSource::Api {
                client_id: "other".to_string(),
            },
            timestamp: 2000,
        };

        audit.record(&event1).unwrap();
        audit.record(&event2).unwrap();
        audit.record(&event3).unwrap();

        let g1_events = audit.events_for_goal("g1").unwrap();
        assert_eq!(g1_events.len(), 2);
        assert_eq!(g1_events[0].goal_id(), "g1");
        assert_eq!(g1_events[0].event_type_name(), "goal_submitted");
        assert_eq!(g1_events[1].event_type_name(), "complexity_assessed");

        let g2_events = audit.events_for_goal("g2").unwrap();
        assert_eq!(g2_events.len(), 1);
    }

    #[test]
    fn test_audit_log_events_since() {
        let (audit, _tmp) = make_audit_log();

        let event = AuditEvent::GoalSubmitted {
            goal_id: "g1".to_string(),
            source: GoalSource::Api {
                client_id: "test".to_string(),
            },
            timestamp: 1000,
        };
        audit.record(&event).unwrap();

        // Query with timestamp 0 should return the event (created_at is set by SQLite).
        let events = audit.events_since(0).unwrap();
        assert!(!events.is_empty());
        assert_eq!(events[0].goal_id(), "g1");
    }

    #[test]
    fn test_audit_event_serde_roundtrip() {
        let events: Vec<AuditEvent> = vec![
            AuditEvent::GoalSubmitted {
                goal_id: "g1".to_string(),
                source: GoalSource::Api {
                    client_id: "c1".to_string(),
                },
                timestamp: 1000,
            },
            AuditEvent::ComplexityAssessed {
                goal_id: "g1".to_string(),
                assessment: sample_assessment(),
            },
            AuditEvent::CouncilConvened {
                goal_id: "g1".to_string(),
                session_id: "s1".to_string(),
                member_count: 3,
            },
            AuditEvent::CouncilVerdict {
                goal_id: "g1".to_string(),
                session_id: "s1".to_string(),
                confidence: 0.85,
                disagreement_count: 1,
            },
            AuditEvent::PlanGenerated {
                goal_id: "g1".to_string(),
                plan_name: "plan1".to_string(),
                phase_count: 3,
                validation_count: 5,
                confidence: 0.9,
            },
            AuditEvent::PlanPresented {
                goal_id: "g1".to_string(),
                plan_name: "plan1".to_string(),
                confidence: 0.9,
                room_id: "!room:test".to_string(),
            },
            AuditEvent::UserResponse {
                goal_id: "g1".to_string(),
                response_type: "approve".to_string(),
                timestamp: 2000,
            },
            AuditEvent::PlanRefined {
                goal_id: "g1".to_string(),
                refinement_index: 1,
                new_confidence: 0.95,
                diff_summary: "Added test phase".to_string(),
            },
            AuditEvent::ExecutionStarted {
                goal_id: "g1".to_string(),
                plan_name: "plan1".to_string(),
                backend: ExecutionBackend::Native,
                timestamp: 3000,
            },
            AuditEvent::PhaseCompleted {
                goal_id: "g1".to_string(),
                phase_name: "build".to_string(),
                passed: true,
                duration_ms: 5000,
            },
            AuditEvent::AgentSpawned {
                goal_id: "g1".to_string(),
                phase_name: "build".to_string(),
                agent_id: "agent-1".to_string(),
                backend: ExecutionBackend::Native,
            },
            AuditEvent::AgentCompleted {
                goal_id: "g1".to_string(),
                agent_id: "agent-1".to_string(),
                status: "success".to_string(),
                iterations: 5,
            },
            AuditEvent::ExecutionCompleted {
                goal_id: "g1".to_string(),
                plan_name: "plan1".to_string(),
                passed: true,
                rollback_triggered: false,
                total_duration_ms: 10000,
            },
            AuditEvent::GoalRejected {
                goal_id: "g2".to_string(),
                reason: "Too risky".to_string(),
                timestamp: 4000,
            },
        ];

        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let deserialized: AuditEvent = serde_json::from_str(&json).unwrap_or_else(|e| {
                panic!("failed to deserialize {}: {e}", event.event_type_name())
            });
            assert_eq!(
                deserialized.goal_id(),
                event.goal_id(),
                "goal_id mismatch for {}",
                event.event_type_name()
            );
            assert_eq!(
                deserialized.event_type_name(),
                event.event_type_name(),
                "event_type_name mismatch"
            );
        }
    }

    #[test]
    fn test_audit_event_goal_id() {
        let cases: Vec<(AuditEvent, &str)> = vec![
            (
                AuditEvent::GoalSubmitted {
                    goal_id: "g1".to_string(),
                    source: GoalSource::Api {
                        client_id: "c".to_string(),
                    },
                    timestamp: 0,
                },
                "g1",
            ),
            (
                AuditEvent::ComplexityAssessed {
                    goal_id: "g2".to_string(),
                    assessment: sample_assessment(),
                },
                "g2",
            ),
            (
                AuditEvent::CouncilConvened {
                    goal_id: "g3".to_string(),
                    session_id: "s".to_string(),
                    member_count: 0,
                },
                "g3",
            ),
            (
                AuditEvent::GoalRejected {
                    goal_id: "g4".to_string(),
                    reason: "no".to_string(),
                    timestamp: 0,
                },
                "g4",
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(event.goal_id(), expected);
        }
    }

    #[test]
    fn test_audit_event_type_name() {
        let cases: Vec<(AuditEvent, &str)> = vec![
            (
                AuditEvent::GoalSubmitted {
                    goal_id: "g".to_string(),
                    source: GoalSource::Api {
                        client_id: "c".to_string(),
                    },
                    timestamp: 0,
                },
                "goal_submitted",
            ),
            (
                AuditEvent::ComplexityAssessed {
                    goal_id: "g".to_string(),
                    assessment: sample_assessment(),
                },
                "complexity_assessed",
            ),
            (
                AuditEvent::CouncilConvened {
                    goal_id: "g".to_string(),
                    session_id: "s".to_string(),
                    member_count: 0,
                },
                "council_convened",
            ),
            (
                AuditEvent::CouncilVerdict {
                    goal_id: "g".to_string(),
                    session_id: "s".to_string(),
                    confidence: 0.5,
                    disagreement_count: 0,
                },
                "council_verdict",
            ),
            (
                AuditEvent::PlanGenerated {
                    goal_id: "g".to_string(),
                    plan_name: "p".to_string(),
                    phase_count: 0,
                    validation_count: 0,
                    confidence: 0.0,
                },
                "plan_generated",
            ),
            (
                AuditEvent::PlanPresented {
                    goal_id: "g".to_string(),
                    plan_name: "p".to_string(),
                    confidence: 0.0,
                    room_id: "r".to_string(),
                },
                "plan_presented",
            ),
            (
                AuditEvent::UserResponse {
                    goal_id: "g".to_string(),
                    response_type: "approve".to_string(),
                    timestamp: 0,
                },
                "user_response",
            ),
            (
                AuditEvent::PlanRefined {
                    goal_id: "g".to_string(),
                    refinement_index: 0,
                    new_confidence: 0.0,
                    diff_summary: "s".to_string(),
                },
                "plan_refined",
            ),
            (
                AuditEvent::ExecutionStarted {
                    goal_id: "g".to_string(),
                    plan_name: "p".to_string(),
                    backend: ExecutionBackend::Native,
                    timestamp: 0,
                },
                "execution_started",
            ),
            (
                AuditEvent::PhaseCompleted {
                    goal_id: "g".to_string(),
                    phase_name: "p".to_string(),
                    passed: true,
                    duration_ms: 0,
                },
                "phase_completed",
            ),
            (
                AuditEvent::AgentSpawned {
                    goal_id: "g".to_string(),
                    phase_name: "p".to_string(),
                    agent_id: "a".to_string(),
                    backend: ExecutionBackend::Native,
                },
                "agent_spawned",
            ),
            (
                AuditEvent::AgentCompleted {
                    goal_id: "g".to_string(),
                    agent_id: "a".to_string(),
                    status: "s".to_string(),
                    iterations: 0,
                },
                "agent_completed",
            ),
            (
                AuditEvent::ExecutionCompleted {
                    goal_id: "g".to_string(),
                    plan_name: "p".to_string(),
                    passed: true,
                    rollback_triggered: false,
                    total_duration_ms: 0,
                },
                "execution_completed",
            ),
            (
                AuditEvent::GoalRejected {
                    goal_id: "g".to_string(),
                    reason: "r".to_string(),
                    timestamp: 0,
                },
                "goal_rejected",
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(event.event_type_name(), expected, "mismatch for {expected}");
        }
    }

    #[test]
    fn test_audit_jsonl_append() {
        let (audit, tmp) = make_audit_log();
        let log_path = tmp.path().join("audit.jsonl");

        audit
            .record(&AuditEvent::GoalSubmitted {
                goal_id: "g1".to_string(),
                source: GoalSource::Api {
                    client_id: "c".to_string(),
                },
                timestamp: 1000,
            })
            .unwrap();

        audit
            .record(&AuditEvent::GoalRejected {
                goal_id: "g1".to_string(),
                reason: "test".to_string(),
                timestamp: 2000,
            })
            .unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "JSONL should have 2 lines");

        // Each line should be valid JSON.
        for line in &lines {
            let parsed: AuditEvent = serde_json::from_str(line).unwrap();
            assert_eq!(parsed.goal_id(), "g1");
        }
    }
}
