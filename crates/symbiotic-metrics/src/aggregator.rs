//! Aggregator: computes derived metrics over rolling time windows.

use std::collections::HashMap;

use chrono::Utc;

use crate::store::MetricStore;
use crate::types::{percentile, AggregatedMetrics, MetricFilter, TimeWindow};
use crate::MetricsError;

/// Computes aggregated metrics from the store.
pub struct Aggregator<'a> {
    store: &'a MetricStore,
}

impl<'a> Aggregator<'a> {
    pub fn new(store: &'a MetricStore) -> Self {
        Self { store }
    }

    /// Compute aggregated metrics for a given window and filter.
    pub fn compute(
        &self,
        window: TimeWindow,
        filter: &MetricFilter,
    ) -> Result<AggregatedMetrics, MetricsError> {
        let since = Utc::now() - window.duration();
        let events = self.store.query_since(since, filter)?;

        let total = events.len() as u64;
        let success_count = events.iter().filter(|e| e.outcome.is_success()).count() as u64;
        let failure_count = total - success_count;

        let mut durations: Vec<u64> = events.iter().map(|e| e.duration_ms).collect();
        durations.sort();

        let avg_duration_ms = if total > 0 {
            durations.iter().sum::<u64>() as f64 / total as f64
        } else {
            0.0
        };

        let total_tokens_input: u64 = events
            .iter()
            .filter_map(|e| e.details.tokens_input)
            .map(|v| v as u64)
            .sum();
        let total_tokens_output: u64 = events
            .iter()
            .filter_map(|e| e.details.tokens_output)
            .map(|v| v as u64)
            .sum();
        let total_cost_usd: f64 = events.iter().filter_map(|e| e.details.cost_usd).sum();

        let mut error_counts: HashMap<String, u64> = HashMap::new();
        for event in &events {
            if !event.outcome.is_success() {
                let reason = event
                    .details
                    .extra
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                *error_counts.entry(reason).or_insert(0) += 1;
            }
        }

        Ok(AggregatedMetrics {
            window,
            agent_id: filter.agent_id.clone(),
            domain: filter.domain.clone(),
            total_actions: total,
            success_count,
            failure_count,
            success_rate: if total > 0 {
                success_count as f64 / total as f64
            } else {
                0.0
            },
            avg_duration_ms,
            p50_duration_ms: percentile(&durations, 50),
            p95_duration_ms: percentile(&durations, 95),
            p99_duration_ms: percentile(&durations, 99),
            total_tokens_input,
            total_tokens_output,
            total_cost_usd,
            error_counts,
        })
    }

    /// Compute per-agent breakdown for a given window.
    pub fn per_agent_breakdown(
        &self,
        window: TimeWindow,
    ) -> Result<Vec<AggregatedMetrics>, MetricsError> {
        let since = Utc::now() - window.duration();
        let agents = self.store.distinct_agents_since(since)?;

        let mut results = Vec::new();
        for agent in agents {
            let filter = MetricFilter {
                agent_id: Some(agent),
                ..Default::default()
            };
            results.push(self.compute(window, &filter)?);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use uuid::Uuid;

    fn make_event(agent: &str, outcome: Outcome, ms: u64, cost: f64) -> MetricEvent {
        MetricEvent {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent_id: agent.to_string(),
            action_type: ActionType::Search,
            outcome,
            duration_ms: ms,
            details: EventDetails {
                model: Some("test-model".into()),
                tokens_input: Some(100),
                tokens_output: Some(50),
                cost_usd: Some(cost),
                domain: Some("test".into()),
                workflow_id: None,
                extra: Default::default(),
            },
        }
    }

    #[test]
    fn aggregate_empty_store() {
        let store = MetricStore::open_in_memory().unwrap();
        let agg = Aggregator::new(&store);
        let result = agg
            .compute(TimeWindow::OneHour, &MetricFilter::default())
            .unwrap();
        assert_eq!(result.total_actions, 0);
        assert_eq!(result.success_rate, 0.0);
    }

    #[test]
    fn aggregate_basic_stats() {
        let store = MetricStore::open_in_memory().unwrap();
        // 8 successes, 2 failures
        for i in 0..10 {
            let outcome = if i < 8 {
                Outcome::Success
            } else {
                Outcome::Failure {
                    reason: "timeout".into(),
                }
            };
            store
                .insert_event(&make_event("agent-1", outcome, (i + 1) * 100, 0.01))
                .unwrap();
        }

        let agg = Aggregator::new(&store);
        let result = agg
            .compute(TimeWindow::OneHour, &MetricFilter::default())
            .unwrap();
        assert_eq!(result.total_actions, 10);
        assert_eq!(result.success_count, 8);
        assert_eq!(result.failure_count, 2);
        assert!((result.success_rate - 0.8).abs() < 0.001);
        assert!(result.avg_duration_ms > 0.0);
        assert_eq!(result.total_tokens_input, 1000);
        assert_eq!(result.total_tokens_output, 500);
        assert!((result.total_cost_usd - 0.10).abs() < 0.001);
    }

    #[test]
    fn per_agent_breakdown_works() {
        let store = MetricStore::open_in_memory().unwrap();
        for _ in 0..5 {
            store
                .insert_event(&make_event("agent-a", Outcome::Success, 100, 0.01))
                .unwrap();
        }
        for _ in 0..3 {
            store
                .insert_event(&make_event("agent-b", Outcome::Success, 200, 0.02))
                .unwrap();
        }

        let agg = Aggregator::new(&store);
        let breakdown = agg.per_agent_breakdown(TimeWindow::OneHour).unwrap();
        assert_eq!(breakdown.len(), 2);

        let agent_a = breakdown
            .iter()
            .find(|m| m.agent_id.as_deref() == Some("agent-a"))
            .unwrap();
        assert_eq!(agent_a.total_actions, 5);

        let agent_b = breakdown
            .iter()
            .find(|m| m.agent_id.as_deref() == Some("agent-b"))
            .unwrap();
        assert_eq!(agent_b.total_actions, 3);
    }
}
