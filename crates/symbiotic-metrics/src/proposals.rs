//! Proposal engine: detects performance gaps and suggests improvements.

use chrono::{Duration, Utc};
use uuid::Uuid;

use crate::aggregator::Aggregator;
use crate::store::MetricStore;
use crate::types::{
    MetricFilter, Proposal, ProposalEvidence, ProposalPriority, ProposalStatus, ProposalTrigger,
    TimeWindow,
};
use crate::MetricsError;

/// Configuration for proposal thresholds.
#[derive(Debug, Clone)]
pub struct ProposalConfig {
    /// Minimum success rate (24h) before triggering LowSuccessRate.
    pub min_success_rate: f64,

    /// Maximum p95 duration (ms, 24h) before triggering HighLatency.
    pub max_p95_duration_ms: u64,

    /// Cost multiplier over 7-day average to trigger CostSpike.
    pub cost_spike_multiplier: f64,

    /// Max repeated errors in 1h before triggering RepeatedErrors.
    pub max_repeated_errors: u64,

    /// Agent underperformance delta from global success rate.
    pub agent_underperformance_delta: f64,

    /// Max active proposals globally.
    pub max_active_proposals: u64,

    /// Cooldown for same trigger type (hours).
    pub trigger_cooldown_hours: i64,

    /// Cooldown for same agent+trigger (hours).
    pub agent_trigger_cooldown_hours: i64,
}

impl Default for ProposalConfig {
    fn default() -> Self {
        Self {
            min_success_rate: 0.80,
            max_p95_duration_ms: 10_000,
            cost_spike_multiplier: 2.0,
            max_repeated_errors: 5,
            agent_underperformance_delta: 0.20,
            max_active_proposals: 5,
            trigger_cooldown_hours: 24,
            agent_trigger_cooldown_hours: 24 * 7,
        }
    }
}

/// Engine that evaluates metrics and generates improvement proposals.
pub struct ProposalEngine<'a> {
    store: &'a MetricStore,
    config: ProposalConfig,
}

impl<'a> ProposalEngine<'a> {
    pub fn new(store: &'a MetricStore, config: ProposalConfig) -> Self {
        Self { store, config }
    }

    /// Evaluate all thresholds and generate any proposals.
    /// Returns the list of newly created proposals.
    pub fn evaluate(&self) -> Result<Vec<Proposal>, MetricsError> {
        // Check global cap
        let active_count = self.store.count_active_proposals()?;
        if active_count >= self.config.max_active_proposals {
            return Ok(Vec::new());
        }

        let remaining = self.config.max_active_proposals - active_count;
        let mut new_proposals = Vec::new();

        let agg = Aggregator::new(self.store);

        // Check global 24h metrics
        let global_24h = agg.compute(TimeWindow::TwentyFourHours, &MetricFilter::default())?;

        // 1. Low success rate
        if global_24h.total_actions > 0 && global_24h.success_rate < self.config.min_success_rate {
            if let Some(p) = self.maybe_create_proposal(
                ProposalTrigger::LowSuccessRate,
                None,
                ProposalEvidence {
                    metric: "success_rate".into(),
                    current_value: global_24h.success_rate,
                    threshold: self.config.min_success_rate,
                    window: TimeWindow::TwentyFourHours,
                    event_count: global_24h.total_actions,
                },
                format!(
                    "Global success rate dropped to {:.1}% (threshold: {:.0}%). Review failing agents and domains.",
                    global_24h.success_rate * 100.0,
                    self.config.min_success_rate * 100.0,
                ),
                ProposalPriority::High,
            )? {
                new_proposals.push(p);
                if new_proposals.len() as u64 >= remaining {
                    return self.persist_proposals(new_proposals);
                }
            }
        }

        // 2. High latency
        if global_24h.total_actions > 0
            && global_24h.p95_duration_ms > self.config.max_p95_duration_ms
        {
            if let Some(p) = self.maybe_create_proposal(
                ProposalTrigger::HighLatency,
                None,
                ProposalEvidence {
                    metric: "p95_duration_ms".into(),
                    current_value: global_24h.p95_duration_ms as f64,
                    threshold: self.config.max_p95_duration_ms as f64,
                    window: TimeWindow::TwentyFourHours,
                    event_count: global_24h.total_actions,
                },
                format!(
                    "P95 latency is {}ms (threshold: {}ms). Investigate slow actions, consider caching.",
                    global_24h.p95_duration_ms, self.config.max_p95_duration_ms,
                ),
                ProposalPriority::Medium,
            )? {
                new_proposals.push(p);
                if new_proposals.len() as u64 >= remaining {
                    return self.persist_proposals(new_proposals);
                }
            }
        }

        // 3. Cost spike: compare 24h cost to 7d daily average
        let global_7d = agg.compute(TimeWindow::SevenDays, &MetricFilter::default())?;
        let daily_avg_cost = global_7d.total_cost_usd / 7.0;
        if daily_avg_cost > 0.0
            && global_24h.total_cost_usd > daily_avg_cost * self.config.cost_spike_multiplier
        {
            if let Some(p) = self.maybe_create_proposal(
                ProposalTrigger::CostSpike,
                None,
                ProposalEvidence {
                    metric: "total_cost_usd".into(),
                    current_value: global_24h.total_cost_usd,
                    threshold: daily_avg_cost * self.config.cost_spike_multiplier,
                    window: TimeWindow::TwentyFourHours,
                    event_count: global_24h.total_actions,
                },
                format!(
                    "Daily cost ${:.2} exceeds {:.0}x the 7-day average (${:.2}/day). Review token usage.",
                    global_24h.total_cost_usd,
                    self.config.cost_spike_multiplier,
                    daily_avg_cost,
                ),
                ProposalPriority::Medium,
            )? {
                new_proposals.push(p);
                if new_proposals.len() as u64 >= remaining {
                    return self.persist_proposals(new_proposals);
                }
            }
        }

        // 4. Repeated errors (1h window)
        let global_1h = agg.compute(TimeWindow::OneHour, &MetricFilter::default())?;
        for (error, count) in &global_1h.error_counts {
            if *count >= self.config.max_repeated_errors {
                if let Some(p) = self.maybe_create_proposal(
                    ProposalTrigger::RepeatedErrors,
                    None,
                    ProposalEvidence {
                        metric: format!("error_count:{error}"),
                        current_value: *count as f64,
                        threshold: self.config.max_repeated_errors as f64,
                        window: TimeWindow::OneHour,
                        event_count: global_1h.total_actions,
                    },
                    format!(
                        "Error \"{error}\" occurred {count} times in 1h (threshold: {}). Fix root cause.",
                        self.config.max_repeated_errors,
                    ),
                    ProposalPriority::High,
                )? {
                    new_proposals.push(p);
                    if new_proposals.len() as u64 >= remaining {
                        return self.persist_proposals(new_proposals);
                    }
                }
                break; // Only one repeated-error proposal per evaluation
            }
        }

        // 5. Agent underperformance
        if global_24h.total_actions > 0 {
            let breakdown = agg.per_agent_breakdown(TimeWindow::TwentyFourHours)?;
            for agent_metrics in &breakdown {
                if let Some(ref agent_id) = agent_metrics.agent_id {
                    if agent_metrics.total_actions > 0
                        && agent_metrics.success_rate
                            < global_24h.success_rate - self.config.agent_underperformance_delta
                    {
                        if let Some(p) = self.maybe_create_proposal(
                            ProposalTrigger::AgentUnderperformance,
                            Some(agent_id.clone()),
                            ProposalEvidence {
                                metric: "agent_success_rate".into(),
                                current_value: agent_metrics.success_rate,
                                threshold: global_24h.success_rate
                                    - self.config.agent_underperformance_delta,
                                window: TimeWindow::TwentyFourHours,
                                event_count: agent_metrics.total_actions,
                            },
                            format!(
                                "Agent {agent_id} success rate {:.1}% is {:.0}pp below global {:.1}%. Review configuration.",
                                agent_metrics.success_rate * 100.0,
                                self.config.agent_underperformance_delta * 100.0,
                                global_24h.success_rate * 100.0,
                            ),
                            ProposalPriority::Medium,
                        )? {
                            new_proposals.push(p);
                            if new_proposals.len() as u64 >= remaining {
                                return self.persist_proposals(new_proposals);
                            }
                        }
                    }
                }
            }
        }

        self.persist_proposals(new_proposals)
    }

    /// Check cooldown and create proposal if allowed.
    fn maybe_create_proposal(
        &self,
        trigger: ProposalTrigger,
        agent_id: Option<String>,
        evidence: ProposalEvidence,
        suggestion: String,
        priority: ProposalPriority,
    ) -> Result<Option<Proposal>, MetricsError> {
        // Check cooldown
        let last = self
            .store
            .last_proposal_for_trigger(&trigger, agent_id.as_deref())?;

        if let Some(last_proposal) = last {
            let cooldown = if agent_id.is_some() {
                Duration::hours(self.config.agent_trigger_cooldown_hours)
            } else {
                Duration::hours(self.config.trigger_cooldown_hours)
            };

            if Utc::now() - last_proposal.created_at < cooldown {
                return Ok(None);
            }
        }

        Ok(Some(Proposal {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            trigger,
            agent_id,
            evidence,
            suggestion,
            priority,
            status: ProposalStatus::Pending,
        }))
    }

    fn persist_proposals(&self, proposals: Vec<Proposal>) -> Result<Vec<Proposal>, MetricsError> {
        for p in &proposals {
            self.store.insert_proposal(p)?;
        }
        Ok(proposals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn make_event(agent: &str, outcome: Outcome, ms: u64, cost: f64) -> MetricEvent {
        MetricEvent {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent_id: agent.to_string(),
            action_type: ActionType::Search,
            outcome,
            duration_ms: ms,
            details: EventDetails {
                model: None,
                tokens_input: Some(100),
                tokens_output: Some(50),
                cost_usd: Some(cost),
                domain: None,
                workflow_id: None,
                extra: Default::default(),
            },
        }
    }

    #[test]
    fn no_proposals_on_empty_store() {
        let store = MetricStore::open_in_memory().unwrap();
        let engine = ProposalEngine::new(&store, ProposalConfig::default());
        let proposals = engine.evaluate().unwrap();
        assert!(proposals.is_empty());
    }

    #[test]
    fn triggers_low_success_rate() {
        let store = MetricStore::open_in_memory().unwrap();
        // 7 successes, 3 failures = 70% success rate
        for i in 0..10 {
            let outcome = if i < 7 {
                Outcome::Success
            } else {
                Outcome::Failure {
                    reason: "err".into(),
                }
            };
            store
                .insert_event(&make_event("a", outcome, 100, 0.01))
                .unwrap();
        }

        let engine = ProposalEngine::new(&store, ProposalConfig::default());
        let proposals = engine.evaluate().unwrap();

        assert!(!proposals.is_empty());
        let low_sr = proposals
            .iter()
            .find(|p| p.trigger == ProposalTrigger::LowSuccessRate);
        assert!(low_sr.is_some());
    }

    #[test]
    fn triggers_high_latency() {
        let store = MetricStore::open_in_memory().unwrap();
        // All events have high latency
        for _ in 0..20 {
            store
                .insert_event(&make_event("a", Outcome::Success, 15_000, 0.01))
                .unwrap();
        }

        let engine = ProposalEngine::new(&store, ProposalConfig::default());
        let proposals = engine.evaluate().unwrap();

        let high_lat = proposals
            .iter()
            .find(|p| p.trigger == ProposalTrigger::HighLatency);
        assert!(high_lat.is_some());
    }

    #[test]
    fn respects_global_cap() {
        let store = MetricStore::open_in_memory().unwrap();
        // Fill up to max proposals
        for i in 0..5 {
            store
                .insert_proposal(&Proposal {
                    id: Uuid::new_v4(),
                    created_at: Utc::now() - Duration::hours(100), // old enough to not cooldown
                    trigger: ProposalTrigger::HighLatency,
                    agent_id: None,
                    evidence: ProposalEvidence {
                        metric: format!("test-{i}"),
                        current_value: 0.0,
                        threshold: 0.0,
                        window: TimeWindow::TwentyFourHours,
                        event_count: 0,
                    },
                    suggestion: "test".into(),
                    priority: ProposalPriority::Low,
                    status: ProposalStatus::Pending,
                })
                .unwrap();
        }

        // Add events that would normally trigger proposals
        for i in 0..10 {
            let outcome = if i < 5 {
                Outcome::Success
            } else {
                Outcome::Failure {
                    reason: "err".into(),
                }
            };
            store
                .insert_event(&make_event("a", outcome, 100, 0.01))
                .unwrap();
        }

        let engine = ProposalEngine::new(&store, ProposalConfig::default());
        let proposals = engine.evaluate().unwrap();
        assert!(
            proposals.is_empty(),
            "should not create proposals when at global cap"
        );
    }

    #[test]
    fn respects_cooldown() {
        let store = MetricStore::open_in_memory().unwrap();
        // Insert a recent proposal for LowSuccessRate
        store
            .insert_proposal(&Proposal {
                id: Uuid::new_v4(),
                created_at: Utc::now(), // just now
                trigger: ProposalTrigger::LowSuccessRate,
                agent_id: None,
                evidence: ProposalEvidence {
                    metric: "success_rate".into(),
                    current_value: 0.5,
                    threshold: 0.8,
                    window: TimeWindow::TwentyFourHours,
                    event_count: 10,
                },
                suggestion: "test".into(),
                priority: ProposalPriority::High,
                status: ProposalStatus::Pending,
            })
            .unwrap();

        // Add events that would trigger LowSuccessRate
        for i in 0..10 {
            let outcome = if i < 5 {
                Outcome::Success
            } else {
                Outcome::Failure {
                    reason: "err".into(),
                }
            };
            store
                .insert_event(&make_event("a", outcome, 100, 0.01))
                .unwrap();
        }

        let engine = ProposalEngine::new(&store, ProposalConfig::default());
        let proposals = engine.evaluate().unwrap();
        let low_sr = proposals
            .iter()
            .find(|p| p.trigger == ProposalTrigger::LowSuccessRate);
        assert!(
            low_sr.is_none(),
            "should not create duplicate LowSuccessRate within cooldown"
        );
    }
}
