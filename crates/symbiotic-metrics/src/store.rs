//! SQLite-backed metrics store.

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::types::{ActionType, EventDetails, MetricEvent, MetricFilter, Outcome};
use crate::MetricsError;

/// Persistent metrics store backed by SQLite.
pub struct MetricStore {
    conn: Connection,
}

impl MetricStore {
    /// Open (or create) a metrics database at the given path.
    pub fn open(path: &Path) -> Result<Self, MetricsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MetricsError::Storage(format!("failed to create directory: {e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| MetricsError::Storage(format!("failed to open database: {e}")))?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Create an in-memory store (for testing).
    pub fn open_in_memory() -> Result<Self, MetricsError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| MetricsError::Storage(format!("failed to open in-memory db: {e}")))?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<(), MetricsError> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS metric_events (
                    event_id TEXT PRIMARY KEY,
                    timestamp TEXT NOT NULL,
                    agent_id TEXT NOT NULL,
                    action_type TEXT NOT NULL,
                    outcome TEXT NOT NULL,
                    duration_ms INTEGER NOT NULL,
                    model TEXT,
                    tokens_input INTEGER,
                    tokens_output INTEGER,
                    cost_usd REAL,
                    domain TEXT,
                    workflow_id TEXT,
                    details_json TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_events_timestamp ON metric_events(timestamp);
                CREATE INDEX IF NOT EXISTS idx_events_agent ON metric_events(agent_id);
                CREATE INDEX IF NOT EXISTS idx_events_action ON metric_events(action_type);
                CREATE INDEX IF NOT EXISTS idx_events_workflow ON metric_events(workflow_id);

                CREATE TABLE IF NOT EXISTS proposals (
                    id TEXT PRIMARY KEY,
                    created_at TEXT NOT NULL,
                    trigger TEXT NOT NULL,
                    agent_id TEXT,
                    evidence_json TEXT NOT NULL,
                    suggestion TEXT NOT NULL,
                    priority TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'Pending'
                );

                CREATE INDEX IF NOT EXISTS idx_proposals_status ON proposals(status);
                CREATE INDEX IF NOT EXISTS idx_proposals_trigger ON proposals(trigger);
                ",
            )
            .map_err(|e| MetricsError::Storage(format!("schema init failed: {e}")))?;
        Ok(())
    }

    /// Insert a single metric event.
    pub fn insert_event(&self, event: &MetricEvent) -> Result<(), MetricsError> {
        let details_json = serde_json::to_string(&event.details)
            .map_err(|e| MetricsError::Serialization(e.to_string()))?;

        self.conn
            .execute(
                "INSERT OR REPLACE INTO metric_events
                 (event_id, timestamp, agent_id, action_type, outcome, duration_ms,
                  model, tokens_input, tokens_output, cost_usd, domain, workflow_id, details_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    event.event_id.to_string(),
                    event.timestamp.to_rfc3339(),
                    event.agent_id,
                    event.action_type.as_str(),
                    event.outcome.label(),
                    event.duration_ms as i64,
                    event.details.model,
                    event.details.tokens_input.map(|v| v as i64),
                    event.details.tokens_output.map(|v| v as i64),
                    event.details.cost_usd,
                    event.details.domain,
                    event.details.workflow_id,
                    details_json,
                ],
            )
            .map_err(|e| MetricsError::Storage(format!("insert failed: {e}")))?;
        Ok(())
    }

    /// Insert multiple events in a single transaction.
    pub fn insert_batch(&mut self, events: &[MetricEvent]) -> Result<(), MetricsError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| MetricsError::Storage(format!("transaction start failed: {e}")))?;

        for event in events {
            let details_json = serde_json::to_string(&event.details)
                .map_err(|e| MetricsError::Serialization(e.to_string()))?;

            tx.execute(
                "INSERT OR REPLACE INTO metric_events
                 (event_id, timestamp, agent_id, action_type, outcome, duration_ms,
                  model, tokens_input, tokens_output, cost_usd, domain, workflow_id, details_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    event.event_id.to_string(),
                    event.timestamp.to_rfc3339(),
                    event.agent_id,
                    event.action_type.as_str(),
                    event.outcome.label(),
                    event.duration_ms as i64,
                    event.details.model,
                    event.details.tokens_input.map(|v| v as i64),
                    event.details.tokens_output.map(|v| v as i64),
                    event.details.cost_usd,
                    event.details.domain,
                    event.details.workflow_id,
                    details_json,
                ],
            )
            .map_err(|e| MetricsError::Storage(format!("batch insert failed: {e}")))?;
        }

        tx.commit()
            .map_err(|e| MetricsError::Storage(format!("transaction commit failed: {e}")))?;
        Ok(())
    }

    /// Query events since a given timestamp with optional filters.
    pub fn query_since(
        &self,
        since: DateTime<Utc>,
        filter: &MetricFilter,
    ) -> Result<Vec<MetricEvent>, MetricsError> {
        let mut sql = String::from(
            "SELECT event_id, timestamp, agent_id, action_type, outcome, duration_ms, details_json
             FROM metric_events WHERE timestamp >= ?1",
        );
        let mut param_idx = 2u32;
        let mut bind_values: Vec<String> = vec![since.to_rfc3339()];

        if let Some(ref agent) = filter.agent_id {
            sql.push_str(&format!(" AND agent_id = ?{param_idx}"));
            bind_values.push(agent.clone());
            param_idx += 1;
        }
        if let Some(ref domain) = filter.domain {
            sql.push_str(&format!(" AND domain = ?{param_idx}"));
            bind_values.push(domain.clone());
            param_idx += 1;
        }
        if let Some(ref action) = filter.action_type {
            sql.push_str(&format!(" AND action_type = ?{param_idx}"));
            bind_values.push(action.as_str().to_string());
            param_idx += 1;
        }
        if let Some(ref wf) = filter.workflow_id {
            sql.push_str(&format!(" AND workflow_id = ?{param_idx}"));
            bind_values.push(wf.clone());
        }

        sql.push_str(" ORDER BY timestamp ASC");

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| MetricsError::Storage(format!("query prepare failed: {e}")))?;

        let params: Vec<&dyn rusqlite::types::ToSql> = bind_values
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();

        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let event_id_str: String = row.get(0)?;
                let timestamp_str: String = row.get(1)?;
                let agent_id: String = row.get(2)?;
                let action_type_str: String = row.get(3)?;
                let outcome_str: String = row.get(4)?;
                let duration_ms: i64 = row.get(5)?;
                let details_json_str: String = row.get(6)?;

                Ok((
                    event_id_str,
                    timestamp_str,
                    agent_id,
                    action_type_str,
                    outcome_str,
                    duration_ms,
                    details_json_str,
                ))
            })
            .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

        let mut events = Vec::new();
        for row_result in rows {
            let (
                event_id_str,
                timestamp_str,
                agent_id,
                action_type_str,
                outcome_str,
                duration_ms,
                details_json_str,
            ) = row_result.map_err(|e| MetricsError::Storage(format!("row read failed: {e}")))?;

            let event_id = uuid::Uuid::parse_str(&event_id_str)
                .map_err(|e| MetricsError::Storage(format!("invalid uuid: {e}")))?;
            let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                .map_err(|e| MetricsError::Storage(format!("invalid timestamp: {e}")))?
                .with_timezone(&Utc);
            let action_type: ActionType = action_type_str
                .parse()
                .map_err(|e: String| MetricsError::Storage(e))?;
            let outcome = match outcome_str.as_str() {
                "Success" => Outcome::Success,
                "Failure" => Outcome::Failure {
                    reason: String::new(),
                },
                "Partial" => Outcome::Partial {
                    details: String::new(),
                },
                "Skipped" => Outcome::Skipped {
                    reason: String::new(),
                },
                other => return Err(MetricsError::Storage(format!("unknown outcome: {other}"))),
            };
            let details: EventDetails = serde_json::from_str(&details_json_str)
                .map_err(|e| MetricsError::Serialization(e.to_string()))?;

            events.push(MetricEvent {
                event_id,
                timestamp,
                agent_id,
                action_type,
                outcome,
                duration_ms: duration_ms as u64,
                details,
            });
        }

        Ok(events)
    }

    /// Get distinct agent IDs seen in events since a given timestamp.
    pub fn distinct_agents_since(&self, since: DateTime<Utc>) -> Result<Vec<String>, MetricsError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT agent_id FROM metric_events WHERE timestamp >= ?1 ORDER BY agent_id",
            )
            .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

        let rows = stmt
            .query_map(params![since.to_rfc3339()], |row| row.get(0))
            .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

        let mut agents = Vec::new();
        for row in rows {
            agents.push(row.map_err(|e| MetricsError::Storage(format!("row read failed: {e}")))?);
        }
        Ok(agents)
    }

    /// Count events with a specific outcome label since a timestamp.
    pub fn count_errors_since(
        &self,
        since: DateTime<Utc>,
        agent_id: Option<&str>,
    ) -> Result<Vec<(String, u64)>, MetricsError> {
        let (sql, bind) = if let Some(agent) = agent_id {
            (
                "SELECT details_json, outcome FROM metric_events WHERE timestamp >= ?1 AND agent_id = ?2 AND outcome = 'Failure'".to_string(),
                vec![since.to_rfc3339(), agent.to_string()],
            )
        } else {
            (
                "SELECT details_json, outcome FROM metric_events WHERE timestamp >= ?1 AND outcome = 'Failure'".to_string(),
                vec![since.to_rfc3339()],
            )
        };

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

        let params_refs: Vec<&dyn rusqlite::types::ToSql> = bind
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();

        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                let _details: String = row.get(0)?;
                let _outcome: String = row.get(1)?;
                Ok(_details)
            })
            .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

        let mut error_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        for row in rows {
            let details_json =
                row.map_err(|e| MetricsError::Storage(format!("row read failed: {e}")))?;
            // Try to extract a reason from the details extra field
            if let Ok(details) = serde_json::from_str::<EventDetails>(&details_json) {
                let reason = details
                    .extra
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                *error_counts.entry(reason).or_insert(0) += 1;
            } else {
                *error_counts.entry("unknown".to_string()).or_insert(0) += 1;
            }
        }

        let mut sorted: Vec<(String, u64)> = error_counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(sorted)
    }

    /// Insert a proposal.
    pub fn insert_proposal(&self, proposal: &crate::types::Proposal) -> Result<(), MetricsError> {
        let evidence_json = serde_json::to_string(&proposal.evidence)
            .map_err(|e| MetricsError::Serialization(e.to_string()))?;

        self.conn
            .execute(
                "INSERT OR REPLACE INTO proposals
                 (id, created_at, trigger, agent_id, evidence_json, suggestion, priority, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    proposal.id.to_string(),
                    proposal.created_at.to_rfc3339(),
                    proposal.trigger.as_str(),
                    proposal.agent_id,
                    evidence_json,
                    proposal.suggestion,
                    proposal.priority.to_string(),
                    proposal.status.as_str(),
                ],
            )
            .map_err(|e| MetricsError::Storage(format!("proposal insert failed: {e}")))?;
        Ok(())
    }

    /// List proposals with a given status filter (or all if None).
    pub fn list_proposals(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<crate::types::Proposal>, MetricsError> {
        let (sql, bind) = if let Some(s) = status {
            (
                "SELECT id, created_at, trigger, agent_id, evidence_json, suggestion, priority, status
                 FROM proposals WHERE status = ?1 ORDER BY created_at DESC".to_string(),
                vec![s.to_string()],
            )
        } else {
            (
                "SELECT id, created_at, trigger, agent_id, evidence_json, suggestion, priority, status
                 FROM proposals ORDER BY created_at DESC".to_string(),
                vec![],
            )
        };

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

        let params_refs: Vec<&dyn rusqlite::types::ToSql> = bind
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();

        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                let id: String = row.get(0)?;
                let created_at: String = row.get(1)?;
                let trigger: String = row.get(2)?;
                let agent_id: Option<String> = row.get(3)?;
                let evidence_json: String = row.get(4)?;
                let suggestion: String = row.get(5)?;
                let priority: String = row.get(6)?;
                let status: String = row.get(7)?;
                Ok((
                    id,
                    created_at,
                    trigger,
                    agent_id,
                    evidence_json,
                    suggestion,
                    priority,
                    status,
                ))
            })
            .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

        let mut proposals = Vec::new();
        for row_result in rows {
            let (id, created_at, trigger, agent_id, evidence_json, suggestion, priority, status) =
                row_result.map_err(|e| MetricsError::Storage(format!("row read failed: {e}")))?;

            let parsed_id = uuid::Uuid::parse_str(&id)
                .map_err(|e| MetricsError::Storage(format!("invalid uuid: {e}")))?;
            let parsed_created = DateTime::parse_from_rfc3339(&created_at)
                .map_err(|e| MetricsError::Storage(format!("invalid timestamp: {e}")))?
                .with_timezone(&Utc);
            let parsed_trigger: crate::types::ProposalTrigger = trigger
                .parse()
                .map_err(|e: String| MetricsError::Storage(e))?;
            let parsed_evidence: crate::types::ProposalEvidence =
                serde_json::from_str(&evidence_json)
                    .map_err(|e| MetricsError::Serialization(e.to_string()))?;
            let parsed_priority = match priority.as_str() {
                "LOW" => crate::types::ProposalPriority::Low,
                "MEDIUM" => crate::types::ProposalPriority::Medium,
                "HIGH" => crate::types::ProposalPriority::High,
                other => return Err(MetricsError::Storage(format!("unknown priority: {other}"))),
            };
            let parsed_status: crate::types::ProposalStatus = status
                .parse()
                .map_err(|e: String| MetricsError::Storage(e))?;

            proposals.push(crate::types::Proposal {
                id: parsed_id,
                created_at: parsed_created,
                trigger: parsed_trigger,
                agent_id,
                evidence: parsed_evidence,
                suggestion,
                priority: parsed_priority,
                status: parsed_status,
            });
        }

        Ok(proposals)
    }

    /// Update the status of a proposal.
    pub fn update_proposal_status(
        &self,
        proposal_id: &uuid::Uuid,
        new_status: &crate::types::ProposalStatus,
    ) -> Result<(), MetricsError> {
        let changed = self
            .conn
            .execute(
                "UPDATE proposals SET status = ?1 WHERE id = ?2",
                params![new_status.as_str(), proposal_id.to_string()],
            )
            .map_err(|e| MetricsError::Storage(format!("update failed: {e}")))?;

        if changed == 0 {
            return Err(MetricsError::NotFound(format!(
                "proposal {proposal_id} not found"
            )));
        }
        Ok(())
    }

    /// Count active (Pending) proposals.
    pub fn count_active_proposals(&self) -> Result<u64, MetricsError> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM proposals WHERE status = 'Pending'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| MetricsError::Storage(format!("count failed: {e}")))?;
        Ok(count as u64)
    }

    /// Get the most recent proposal for a given trigger and optional agent.
    pub fn last_proposal_for_trigger(
        &self,
        trigger: &crate::types::ProposalTrigger,
        agent_id: Option<&str>,
    ) -> Result<Option<crate::types::Proposal>, MetricsError> {
        let proposals = if let Some(agent) = agent_id {
            let sql = "SELECT id, created_at, trigger, agent_id, evidence_json, suggestion, priority, status
                       FROM proposals WHERE trigger = ?1 AND agent_id = ?2
                       ORDER BY created_at DESC LIMIT 1";
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;
            let rows = stmt
                .query_map(params![trigger.as_str(), agent], |row| {
                    let id: String = row.get(0)?;
                    let created_at: String = row.get(1)?;
                    let trigger_str: String = row.get(2)?;
                    let agent_id: Option<String> = row.get(3)?;
                    let evidence_json: String = row.get(4)?;
                    let suggestion: String = row.get(5)?;
                    let priority: String = row.get(6)?;
                    let status: String = row.get(7)?;
                    Ok((
                        id,
                        created_at,
                        trigger_str,
                        agent_id,
                        evidence_json,
                        suggestion,
                        priority,
                        status,
                    ))
                })
                .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

            let mut result = Vec::new();
            for r in rows {
                result.push(r.map_err(|e| MetricsError::Storage(format!("row failed: {e}")))?);
            }
            result
        } else {
            let sql = "SELECT id, created_at, trigger, agent_id, evidence_json, suggestion, priority, status
                       FROM proposals WHERE trigger = ?1 AND agent_id IS NULL
                       ORDER BY created_at DESC LIMIT 1";
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;
            let rows = stmt
                .query_map(params![trigger.as_str()], |row| {
                    let id: String = row.get(0)?;
                    let created_at: String = row.get(1)?;
                    let trigger_str: String = row.get(2)?;
                    let agent_id: Option<String> = row.get(3)?;
                    let evidence_json: String = row.get(4)?;
                    let suggestion: String = row.get(5)?;
                    let priority: String = row.get(6)?;
                    let status: String = row.get(7)?;
                    Ok((
                        id,
                        created_at,
                        trigger_str,
                        agent_id,
                        evidence_json,
                        suggestion,
                        priority,
                        status,
                    ))
                })
                .map_err(|e| MetricsError::Storage(format!("query failed: {e}")))?;

            let mut result = Vec::new();
            for r in rows {
                result.push(r.map_err(|e| MetricsError::Storage(format!("row failed: {e}")))?);
            }
            result
        };

        if proposals.is_empty() {
            return Ok(None);
        }

        let (id, created_at, trigger_str, agent_id, evidence_json, suggestion, priority, status) =
            proposals.into_iter().next().unwrap();

        let parsed_id = uuid::Uuid::parse_str(&id)
            .map_err(|e| MetricsError::Storage(format!("invalid uuid: {e}")))?;
        let parsed_created = DateTime::parse_from_rfc3339(&created_at)
            .map_err(|e| MetricsError::Storage(format!("invalid timestamp: {e}")))?
            .with_timezone(&Utc);
        let parsed_trigger: crate::types::ProposalTrigger = trigger_str
            .parse()
            .map_err(|e: String| MetricsError::Storage(e))?;
        let parsed_evidence: crate::types::ProposalEvidence = serde_json::from_str(&evidence_json)
            .map_err(|e| MetricsError::Serialization(e.to_string()))?;
        let parsed_priority = match priority.as_str() {
            "LOW" => crate::types::ProposalPriority::Low,
            "MEDIUM" => crate::types::ProposalPriority::Medium,
            "HIGH" => crate::types::ProposalPriority::High,
            other => return Err(MetricsError::Storage(format!("unknown priority: {other}"))),
        };
        let parsed_status: crate::types::ProposalStatus = status
            .parse()
            .map_err(|e: String| MetricsError::Storage(e))?;

        Ok(Some(crate::types::Proposal {
            id: parsed_id,
            created_at: parsed_created,
            trigger: parsed_trigger,
            agent_id,
            evidence: parsed_evidence,
            suggestion,
            priority: parsed_priority,
            status: parsed_status,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use uuid::Uuid;

    fn make_event(agent: &str, action: ActionType, outcome: Outcome, ms: u64) -> MetricEvent {
        MetricEvent {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent_id: agent.to_string(),
            action_type: action,
            outcome,
            duration_ms: ms,
            details: EventDetails {
                model: None,
                tokens_input: Some(100),
                tokens_output: Some(50),
                cost_usd: Some(0.01),
                domain: Some("test".into()),
                workflow_id: None,
                extra: Default::default(),
            },
        }
    }

    #[test]
    fn insert_and_query_event() {
        let store = MetricStore::open_in_memory().unwrap();
        let event = make_event("agent-1", ActionType::Search, Outcome::Success, 100);
        store.insert_event(&event).unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let events = store.query_since(since, &MetricFilter::default()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, event.event_id);
        assert_eq!(events[0].agent_id, "agent-1");
    }

    #[test]
    fn batch_insert() {
        let mut store = MetricStore::open_in_memory().unwrap();
        let events: Vec<MetricEvent> = (0..10)
            .map(|i| {
                make_event(
                    &format!("agent-{i}"),
                    ActionType::Execute,
                    Outcome::Success,
                    50,
                )
            })
            .collect();
        store.insert_batch(&events).unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let queried = store.query_since(since, &MetricFilter::default()).unwrap();
        assert_eq!(queried.len(), 10);
    }

    #[test]
    fn query_with_agent_filter() {
        let store = MetricStore::open_in_memory().unwrap();
        store
            .insert_event(&make_event(
                "agent-a",
                ActionType::Search,
                Outcome::Success,
                100,
            ))
            .unwrap();
        store
            .insert_event(&make_event(
                "agent-b",
                ActionType::Search,
                Outcome::Success,
                200,
            ))
            .unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let filter = MetricFilter {
            agent_id: Some("agent-a".into()),
            ..Default::default()
        };
        let events = store.query_since(since, &filter).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].agent_id, "agent-a");
    }

    #[test]
    fn distinct_agents() {
        let store = MetricStore::open_in_memory().unwrap();
        store
            .insert_event(&make_event(
                "agent-a",
                ActionType::Search,
                Outcome::Success,
                100,
            ))
            .unwrap();
        store
            .insert_event(&make_event(
                "agent-b",
                ActionType::Execute,
                Outcome::Success,
                200,
            ))
            .unwrap();
        store
            .insert_event(&make_event(
                "agent-a",
                ActionType::Ingest,
                Outcome::Success,
                50,
            ))
            .unwrap();

        let since = Utc::now() - chrono::Duration::hours(1);
        let agents = store.distinct_agents_since(since).unwrap();
        assert_eq!(agents, vec!["agent-a", "agent-b"]);
    }

    #[test]
    fn proposal_crud() {
        let store = MetricStore::open_in_memory().unwrap();
        let proposal = Proposal {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            trigger: ProposalTrigger::LowSuccessRate,
            agent_id: Some("agent-1".into()),
            evidence: ProposalEvidence {
                metric: "success_rate".into(),
                current_value: 0.75,
                threshold: 0.80,
                window: TimeWindow::TwentyFourHours,
                event_count: 100,
            },
            suggestion: "Review agent-1 configuration".into(),
            priority: ProposalPriority::High,
            status: ProposalStatus::Pending,
        };

        store.insert_proposal(&proposal).unwrap();

        let pending = store.list_proposals(Some("Pending")).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, proposal.id);

        store
            .update_proposal_status(&proposal.id, &ProposalStatus::Approved)
            .unwrap();

        let pending = store.list_proposals(Some("Pending")).unwrap();
        assert_eq!(pending.len(), 0);

        let approved = store.list_proposals(Some("Approved")).unwrap();
        assert_eq!(approved.len(), 1);
    }

    #[test]
    fn count_active_proposals_works() {
        let store = MetricStore::open_in_memory().unwrap();
        assert_eq!(store.count_active_proposals().unwrap(), 0);

        let proposal = Proposal {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            trigger: ProposalTrigger::HighLatency,
            agent_id: None,
            evidence: ProposalEvidence {
                metric: "p95_duration_ms".into(),
                current_value: 12000.0,
                threshold: 10000.0,
                window: TimeWindow::TwentyFourHours,
                event_count: 50,
            },
            suggestion: "Investigate slow actions".into(),
            priority: ProposalPriority::Medium,
            status: ProposalStatus::Pending,
        };
        store.insert_proposal(&proposal).unwrap();
        assert_eq!(store.count_active_proposals().unwrap(), 1);
    }
}
