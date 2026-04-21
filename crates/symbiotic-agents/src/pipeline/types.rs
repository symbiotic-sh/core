use serde::{Deserialize, Serialize};

/// Autonomy level for goal execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    /// Fully automatic execution.
    Auto,
    /// Auto for routine, escalate for novel.
    #[default]
    Semi,
    /// Always require human approval.
    Manual,
}

/// Optional constraints on goal execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalConstraints {
    /// Maximum budget in USD.
    pub max_budget_usd: Option<f64>,
    /// Maximum execution time in seconds.
    pub max_duration_secs: Option<u64>,
    /// Required completion deadline (unix timestamp).
    pub deadline: Option<u64>,
}

/// Parsed metadata extracted from a goal submission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalMetadata {
    pub title: String,
    pub description: String,
    pub domains: Vec<String>,
    pub phases: Vec<String>,
    pub has_known_template: bool,
    pub template_name: Option<String>,
    pub external_dependencies: Vec<String>,
    pub required_scopes: Vec<String>,
    pub estimated_cost_usd: Option<f64>,
    pub autonomy_level: AutonomyLevel,
    pub constraints: Option<GoalConstraints>,
}

/// Where the goal came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalSource {
    Matrix {
        room_id: String,
        sender: String,
    },
    Api {
        client_id: String,
    },
    Manifest {
        path: String,
    },
    Scheduled {
        goal_slug: String,
    },
    /// System-originated goal from friction detection or metrics proposals.
    System {
        trigger: String,
        proposal_id: String,
    },
}

/// Raw goal input from any source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalSubmission {
    pub id: String,
    pub title: String,
    pub description: String,
    pub source: GoalSource,
    /// Pre-parsed metadata, if available.
    pub metadata: Option<GoalMetadata>,
}
