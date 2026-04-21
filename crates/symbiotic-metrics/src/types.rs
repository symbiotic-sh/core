//! Core types for the metrics layer.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single metric event capturing an agent action and its outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricEvent {
    /// Unique event ID.
    pub event_id: Uuid,

    /// When this event occurred.
    pub timestamp: DateTime<Utc>,

    /// Which agent produced this event.
    pub agent_id: String,

    /// What type of action was performed.
    pub action_type: ActionType,

    /// Whether the action succeeded.
    pub outcome: Outcome,

    /// How long the action took (milliseconds).
    pub duration_ms: u64,

    /// Typed detail fields for the action.
    pub details: EventDetails,
}

/// Types of actions that can be tracked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionType {
    Search,
    Retrieve,
    Decide,
    Execute,
    Ingest,
    Review,
}

impl ActionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Retrieve => "Retrieve",
            Self::Decide => "Decide",
            Self::Execute => "Execute",
            Self::Ingest => "Ingest",
            Self::Review => "Review",
        }
    }
}

impl std::fmt::Display for ActionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ActionType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Search" => Ok(Self::Search),
            "Retrieve" => Ok(Self::Retrieve),
            "Decide" => Ok(Self::Decide),
            "Execute" => Ok(Self::Execute),
            "Ingest" => Ok(Self::Ingest),
            "Review" => Ok(Self::Review),
            other => Err(format!("unknown action type: {other}")),
        }
    }
}

/// The outcome of an action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Outcome {
    Success,
    Failure { reason: String },
    Partial { details: String },
    Skipped { reason: String },
}

impl Outcome {
    /// Returns true if this outcome is `Success`.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Returns the short label used for storage.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success => "Success",
            Self::Failure { .. } => "Failure",
            Self::Partial { .. } => "Partial",
            Self::Skipped { .. } => "Skipped",
        }
    }
}

/// Typed detail fields attached to a metric event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDetails {
    /// LLM model used (if any).
    pub model: Option<String>,

    /// Tokens consumed (input).
    pub tokens_input: Option<u32>,

    /// Tokens consumed (output).
    pub tokens_output: Option<u32>,

    /// Estimated cost in USD.
    pub cost_usd: Option<f64>,

    /// Domain context (e.g., "software-engineering", "research").
    pub domain: Option<String>,

    /// Workflow/goal this action belongs to.
    pub workflow_id: Option<String>,

    /// Free-form key-value pairs for action-specific data.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Rolling time window for aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeWindow {
    OneHour,
    TwentyFourHours,
    SevenDays,
}

impl TimeWindow {
    /// Returns the chrono Duration for this window.
    pub fn duration(&self) -> Duration {
        match self {
            Self::OneHour => Duration::hours(1),
            Self::TwentyFourHours => Duration::hours(24),
            Self::SevenDays => Duration::days(7),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::OneHour => "1h",
            Self::TwentyFourHours => "24h",
            Self::SevenDays => "7d",
        }
    }
}

impl std::str::FromStr for TimeWindow {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "1h" => Ok(Self::OneHour),
            "24h" => Ok(Self::TwentyFourHours),
            "7d" => Ok(Self::SevenDays),
            other => Err(format!(
                "unknown time window: {other} (expected 1h, 24h, 7d)"
            )),
        }
    }
}

/// Filter criteria for metric queries.
#[derive(Debug, Clone, Default)]
pub struct MetricFilter {
    pub agent_id: Option<String>,
    pub domain: Option<String>,
    pub action_type: Option<ActionType>,
    pub workflow_id: Option<String>,
}

/// Result of aggregating events over a time window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedMetrics {
    pub window: TimeWindow,
    pub agent_id: Option<String>,
    pub domain: Option<String>,

    pub total_actions: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub success_rate: f64,

    pub avg_duration_ms: f64,
    pub p50_duration_ms: u64,
    pub p95_duration_ms: u64,
    pub p99_duration_ms: u64,

    pub total_tokens_input: u64,
    pub total_tokens_output: u64,
    pub total_cost_usd: f64,

    pub error_counts: HashMap<String, u64>,
}

/// Priority of a self-improvement proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalPriority {
    Low,
    Medium,
    High,
}

impl std::fmt::Display for ProposalPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "LOW"),
            Self::Medium => write!(f, "MEDIUM"),
            Self::High => write!(f, "HIGH"),
        }
    }
}

/// Status of a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
    Implemented,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Approved => "Approved",
            Self::Rejected => "Rejected",
            Self::Implemented => "Implemented",
        }
    }
}

impl std::str::FromStr for ProposalStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Pending" => Ok(Self::Pending),
            "Approved" => Ok(Self::Approved),
            "Rejected" => Ok(Self::Rejected),
            "Implemented" => Ok(Self::Implemented),
            other => Err(format!("unknown proposal status: {other}")),
        }
    }
}

/// What triggered a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProposalTrigger {
    LowSuccessRate,
    HighLatency,
    CostSpike,
    RepeatedErrors,
    AgentUnderperformance,
}

impl ProposalTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LowSuccessRate => "LowSuccessRate",
            Self::HighLatency => "HighLatency",
            Self::CostSpike => "CostSpike",
            Self::RepeatedErrors => "RepeatedErrors",
            Self::AgentUnderperformance => "AgentUnderperformance",
        }
    }
}

impl std::str::FromStr for ProposalTrigger {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "LowSuccessRate" => Ok(Self::LowSuccessRate),
            "HighLatency" => Ok(Self::HighLatency),
            "CostSpike" => Ok(Self::CostSpike),
            "RepeatedErrors" => Ok(Self::RepeatedErrors),
            "AgentUnderperformance" => Ok(Self::AgentUnderperformance),
            other => Err(format!("unknown proposal trigger: {other}")),
        }
    }
}

impl std::fmt::Display for ProposalTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Evidence supporting a proposal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalEvidence {
    /// The metric that triggered this proposal.
    pub metric: String,

    /// Current value.
    pub current_value: f64,

    /// Threshold that was exceeded.
    pub threshold: f64,

    /// Time window of observation.
    pub window: TimeWindow,

    /// Number of events in the window.
    pub event_count: u64,
}

/// A self-improvement proposal generated from metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub trigger: ProposalTrigger,
    pub agent_id: Option<String>,
    pub evidence: ProposalEvidence,
    pub suggestion: String,
    pub priority: ProposalPriority,
    pub status: ProposalStatus,
}

/// Compute the value at a given percentile from a sorted slice.
pub fn percentile(sorted: &[u64], pct: u8) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((pct as f64 / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_type_roundtrip() {
        for at in [
            ActionType::Search,
            ActionType::Retrieve,
            ActionType::Decide,
            ActionType::Execute,
            ActionType::Ingest,
            ActionType::Review,
        ] {
            let s = at.as_str();
            let parsed: ActionType = s.parse().unwrap();
            assert_eq!(parsed, at);
        }
    }

    #[test]
    fn outcome_labels() {
        assert!(Outcome::Success.is_success());
        assert!(!Outcome::Failure { reason: "x".into() }.is_success());
        assert_eq!(Outcome::Success.label(), "Success");
        assert_eq!(Outcome::Failure { reason: "x".into() }.label(), "Failure");
    }

    #[test]
    fn time_window_parse() {
        assert_eq!("1h".parse::<TimeWindow>().unwrap(), TimeWindow::OneHour);
        assert_eq!(
            "24h".parse::<TimeWindow>().unwrap(),
            TimeWindow::TwentyFourHours
        );
        assert_eq!("7d".parse::<TimeWindow>().unwrap(), TimeWindow::SevenDays);
        assert!("bad".parse::<TimeWindow>().is_err());
    }

    #[test]
    fn percentile_basic() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[100], 50), 100);
        assert_eq!(percentile(&[10, 20, 30, 40, 50], 50), 30);
        assert_eq!(percentile(&[10, 20, 30, 40, 50], 95), 50);
    }

    #[test]
    fn metric_event_json_roundtrip() {
        let event = MetricEvent {
            event_id: Uuid::new_v4(),
            timestamp: Utc::now(),
            agent_id: "test-agent".into(),
            action_type: ActionType::Search,
            outcome: Outcome::Success,
            duration_ms: 150,
            details: EventDetails {
                model: Some("test-model".into()),
                tokens_input: Some(100),
                tokens_output: Some(50),
                cost_usd: Some(0.01),
                domain: Some("test".into()),
                workflow_id: None,
                extra: Default::default(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: MetricEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event_id, event.event_id);
        assert_eq!(parsed.agent_id, event.agent_id);
    }

    #[test]
    fn proposal_trigger_roundtrip() {
        for trigger in [
            ProposalTrigger::LowSuccessRate,
            ProposalTrigger::HighLatency,
            ProposalTrigger::CostSpike,
            ProposalTrigger::RepeatedErrors,
            ProposalTrigger::AgentUnderperformance,
        ] {
            let s = trigger.as_str();
            let parsed: ProposalTrigger = s.parse().unwrap();
            assert_eq!(parsed, trigger);
        }
    }
}
