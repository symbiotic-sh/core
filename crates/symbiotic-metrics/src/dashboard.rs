//! CLI dashboard output formatting.

use crate::aggregator::Aggregator;
use crate::store::MetricStore;
use crate::types::{MetricFilter, Proposal, TimeWindow};
use crate::MetricsError;

/// Format a metrics summary for CLI output.
pub fn format_summary(
    store: &MetricStore,
    window: TimeWindow,
    agent_filter: Option<&str>,
) -> Result<String, MetricsError> {
    let agg = Aggregator::new(store);

    let filter = MetricFilter {
        agent_id: agent_filter.map(String::from),
        ..Default::default()
    };

    let metrics = agg.compute(window, &filter)?;

    let mut out = String::new();

    let header = if let Some(agent) = agent_filter {
        format!("Metrics Summary ({}, agent: {agent})", window.label())
    } else {
        format!("Metrics Summary ({})", window.label())
    };
    out.push_str(&header);
    out.push('\n');
    out.push_str(&"=".repeat(header.len()));
    out.push('\n');

    out.push_str(&format!("Total actions:    {}\n", metrics.total_actions));
    out.push_str(&format!(
        "Success rate:     {:.1}%\n",
        metrics.success_rate * 100.0
    ));
    out.push_str(&format!(
        "Avg duration:     {}\n",
        format_duration_ms(metrics.avg_duration_ms as u64)
    ));
    out.push_str(&format!(
        "P95 duration:     {}\n",
        format_duration_ms(metrics.p95_duration_ms)
    ));
    out.push_str(&format!(
        "Total cost:       ${:.2}\n",
        metrics.total_cost_usd
    ));
    out.push_str(&format!(
        "Tokens (in/out):  {} / {}\n",
        format_tokens(metrics.total_tokens_input),
        format_tokens(metrics.total_tokens_output),
    ));

    // Per-agent breakdown (only when not filtering by agent)
    if agent_filter.is_none() && metrics.total_actions > 0 {
        let breakdown = agg.per_agent_breakdown(window)?;
        if !breakdown.is_empty() {
            out.push_str("\nPer-Agent Breakdown:\n");
            for agent_m in &breakdown {
                let name = agent_m.agent_id.as_deref().unwrap_or("unknown");
                out.push_str(&format!(
                    "  {:<24} {:>3} actions   {:.1}% success   ${:.2}\n",
                    name,
                    agent_m.total_actions,
                    agent_m.success_rate * 100.0,
                    agent_m.total_cost_usd,
                ));
            }
        }
    }

    // Top errors
    if !metrics.error_counts.is_empty() {
        let mut errors: Vec<_> = metrics.error_counts.iter().collect();
        errors.sort_by(|a, b| b.1.cmp(a.1));

        out.push_str("\nTop Errors:\n");
        for (reason, count) in errors.iter().take(5) {
            out.push_str(&format!("  \"{reason}\"   {count} occurrences\n"));
        }
    }

    // Active proposals count
    let active_proposals = store.count_active_proposals()?;
    if active_proposals > 0 {
        out.push_str(&format!("\nActive Proposals: {active_proposals}\n"));

        let proposals = store.list_proposals(Some("Pending"))?;
        for p in proposals.iter().take(3) {
            out.push_str(&format!("  [{}] {}\n", p.priority, p.suggestion));
        }
        if active_proposals > 3 {
            out.push_str("  Run: symbiotic metrics proposals\n");
        }
    }

    Ok(out)
}

/// Format proposals list for CLI output.
pub fn format_proposals(proposals: &[Proposal]) -> String {
    if proposals.is_empty() {
        return "No proposals found.\n".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!("Proposals ({})\n", proposals.len()));
    out.push_str(&"=".repeat(40));
    out.push('\n');

    for p in proposals {
        out.push_str(&format!(
            "\n[{}] {} ({})\n",
            p.priority,
            p.trigger,
            p.status.as_str()
        ));
        out.push_str(&format!("  ID: {}\n", p.id));
        out.push_str(&format!(
            "  Created: {}\n",
            p.created_at.format("%Y-%m-%d %H:%M UTC")
        ));
        if let Some(ref agent) = p.agent_id {
            out.push_str(&format!("  Agent: {agent}\n"));
        }
        out.push_str(&format!("  Suggestion: {}\n", p.suggestion));
        out.push_str(&format!(
            "  Evidence: {} = {:.2} (threshold: {:.2}, window: {}, events: {})\n",
            p.evidence.metric,
            p.evidence.current_value,
            p.evidence.threshold,
            p.evidence.window.label(),
            p.evidence.event_count,
        ));
    }

    out
}

fn format_duration_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else {
        format!("{count}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use chrono::Utc;
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
                tokens_input: Some(1200),
                tokens_output: Some(300),
                cost_usd: Some(cost),
                domain: Some("research".into()),
                workflow_id: None,
                extra: Default::default(),
            },
        }
    }

    #[test]
    fn format_summary_empty() {
        let store = MetricStore::open_in_memory().unwrap();
        let output = format_summary(&store, TimeWindow::TwentyFourHours, None).unwrap();
        assert!(output.contains("Metrics Summary (24h)"));
        assert!(output.contains("Total actions:    0"));
    }

    #[test]
    fn format_summary_with_data() {
        let store = MetricStore::open_in_memory().unwrap();
        for _ in 0..5 {
            store
                .insert_event(&make_event("agent-search", Outcome::Success, 1200, 0.02))
                .unwrap();
        }
        store
            .insert_event(&make_event(
                "agent-search",
                Outcome::Failure {
                    reason: "timeout".into(),
                },
                5000,
                0.01,
            ))
            .unwrap();

        let output = format_summary(&store, TimeWindow::TwentyFourHours, None).unwrap();
        assert!(output.contains("Total actions:    6"));
        assert!(output.contains("Success rate:"));
        assert!(output.contains("Per-Agent Breakdown:"));
        assert!(output.contains("agent-search"));
    }

    #[test]
    fn format_summary_with_agent_filter() {
        let store = MetricStore::open_in_memory().unwrap();
        store
            .insert_event(&make_event("agent-a", Outcome::Success, 100, 0.01))
            .unwrap();
        store
            .insert_event(&make_event("agent-b", Outcome::Success, 200, 0.02))
            .unwrap();

        let output = format_summary(&store, TimeWindow::OneHour, Some("agent-a")).unwrap();
        assert!(output.contains("agent: agent-a"));
        assert!(output.contains("Total actions:    1"));
        // Should NOT contain per-agent breakdown when filtered
        assert!(!output.contains("Per-Agent Breakdown:"));
    }

    #[test]
    fn format_proposals_empty() {
        let output = format_proposals(&[]);
        assert!(output.contains("No proposals found"));
    }

    #[test]
    fn format_proposals_with_data() {
        let proposals = vec![Proposal {
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
        }];

        let output = format_proposals(&proposals);
        assert!(output.contains("[HIGH]"));
        assert!(output.contains("LowSuccessRate"));
        assert!(output.contains("Review agent-1 configuration"));
    }

    #[test]
    fn format_duration_formatting() {
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(1500), "1.5s");
        assert_eq!(format_duration_ms(10000), "10.0s");
    }

    #[test]
    fn format_tokens_formatting() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(45200), "45.2k");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }
}
