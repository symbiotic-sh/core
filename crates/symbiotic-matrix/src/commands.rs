use std::fmt;
use symbiotic_core::now_unix;

use anyhow::Result;
use symbiotic_core::events::{EventStatus, EventType};

use crate::events::MatrixEventEnvelope;
use crate::transport::MatrixTransport;

/// Parsed command from a Matrix message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    GoalCreate { description: String },
    GoalList,
    GoalStatus { id: String },
    GoalCancel { id: String },
    WorkflowRun { name: String, params: Vec<String> },
    WorkflowList,
    Help,
}

/// Result of a parsed message: either a command or not a command at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseResult {
    /// Successfully parsed a command.
    Command(Command),
    /// The message is not a command (no prefix).
    NotACommand,
    /// The message has a command prefix but is invalid.
    InvalidCommand(String),
}

/// Parse a Matrix message body into a command (or not).
pub fn parse_command(message: &str) -> ParseResult {
    let trimmed = message.trim();
    if !trimmed.starts_with('!') {
        return ParseResult::NotACommand;
    }

    let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
    let prefix = parts[0].to_ascii_lowercase();

    match prefix.as_str() {
        "!goal" => parse_goal_command(&parts[1..]),
        "!workflow" => parse_workflow_command(&parts[1..]),
        "!help" => ParseResult::Command(Command::Help),
        _ => ParseResult::InvalidCommand(format!(
            "Unknown command `{}`. Available: !goal, !workflow, !help",
            parts[0]
        )),
    }
}

fn parse_goal_command(args: &[&str]) -> ParseResult {
    if args.is_empty() {
        return ParseResult::InvalidCommand(
            "Missing subcommand. Usage: !goal create|list|status|cancel".to_string(),
        );
    }

    match args[0].to_ascii_lowercase().as_str() {
        "create" => {
            if args.len() < 2 || args[1].trim().is_empty() {
                return ParseResult::InvalidCommand(
                    "Missing description. Usage: !goal create <description>".to_string(),
                );
            }
            ParseResult::Command(Command::GoalCreate {
                description: args[1].to_string(),
            })
        }
        "list" => ParseResult::Command(Command::GoalList),
        "status" => {
            if args.len() < 2 || args[1].trim().is_empty() {
                return ParseResult::InvalidCommand(
                    "Missing goal ID. Usage: !goal status <id>".to_string(),
                );
            }
            // Extract just the first token from remaining args as the ID.
            let id = args[1].split_whitespace().next().unwrap_or("").to_string();
            ParseResult::Command(Command::GoalStatus { id })
        }
        "cancel" => {
            if args.len() < 2 || args[1].trim().is_empty() {
                return ParseResult::InvalidCommand(
                    "Missing goal ID. Usage: !goal cancel <id>".to_string(),
                );
            }
            let id = args[1].split_whitespace().next().unwrap_or("").to_string();
            ParseResult::Command(Command::GoalCancel { id })
        }
        other => ParseResult::InvalidCommand(format!(
            "Unknown goal subcommand `{other}`. Available: create, list, status, cancel"
        )),
    }
}

fn parse_workflow_command(args: &[&str]) -> ParseResult {
    if args.is_empty() {
        return ParseResult::InvalidCommand(
            "Missing subcommand. Usage: !workflow run|list".to_string(),
        );
    }

    match args[0].to_ascii_lowercase().as_str() {
        "run" => {
            if args.len() < 2 || args[1].trim().is_empty() {
                return ParseResult::InvalidCommand(
                    "Missing workflow name. Usage: !workflow run <name> [params...]".to_string(),
                );
            }
            let remaining = args[1];
            let mut tokens = remaining.split_whitespace();
            let name = tokens.next().unwrap_or("").to_string();
            let params: Vec<String> = tokens.map(String::from).collect();
            ParseResult::Command(Command::WorkflowRun { name, params })
        }
        "list" => ParseResult::Command(Command::WorkflowList),
        other => ParseResult::InvalidCommand(format!(
            "Unknown workflow subcommand `{other}`. Available: run, list"
        )),
    }
}

/// Goal status in the goal manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalStatus {
    Active,
    Completed,
    Cancelled,
    Failed,
}

impl fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Completed => write!(f, "completed"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// A tracked goal.
#[derive(Debug, Clone)]
pub struct Goal {
    pub id: String,
    pub description: String,
    pub status: GoalStatus,
    pub created_at: u64,
}

/// Trait for managing goal lifecycle. Implementations can back onto
/// the domain queue store or any other persistence layer.
pub trait GoalManager: Send + Sync {
    fn create_goal(&self, description: &str) -> Result<Goal>;
    fn list_goals(&self) -> Result<Vec<Goal>>;
    fn get_goal(&self, id: &str) -> Result<Option<Goal>>;
    fn cancel_goal(&self, id: &str) -> Result<Goal>;
}

/// Trait for listing available workflows and triggering runs.
pub trait WorkflowDispatcher: Send + Sync {
    fn list_workflows(&self) -> Result<Vec<String>>;
    fn run_workflow(&self, name: &str, params: &[String]) -> Result<WorkflowRunSummary>;
}

/// Summary of a workflow run, returned to the command handler for reporting.
#[derive(Debug, Clone)]
pub struct WorkflowRunSummary {
    pub run_id: String,
    pub workflow_name: String,
    pub success: bool,
    pub steps_completed: usize,
    pub steps_total: usize,
    pub error: Option<String>,
}

/// Response from handling a command.
#[derive(Debug, Clone)]
pub struct CommandResponse {
    pub body: String,
    pub event_type: EventType,
    pub status: EventStatus,
}

/// Handle a parsed command against the goal manager and workflow dispatcher.
pub fn handle_command(
    command: &Command,
    goals: &dyn GoalManager,
    workflows: &dyn WorkflowDispatcher,
) -> CommandResponse {
    match command {
        Command::GoalCreate { description } => handle_goal_create(description, goals),
        Command::GoalList => handle_goal_list(goals),
        Command::GoalStatus { id } => handle_goal_status(id, goals),
        Command::GoalCancel { id } => handle_goal_cancel(id, goals),
        Command::WorkflowRun { name, params } => handle_workflow_run(name, params, workflows),
        Command::WorkflowList => handle_workflow_list(workflows),
        Command::Help => CommandResponse {
            body: help_text(),
            event_type: EventType::CommandAccepted,
            status: EventStatus::Completed,
        },
    }
}

fn handle_goal_create(description: &str, goals: &dyn GoalManager) -> CommandResponse {
    match goals.create_goal(description) {
        Ok(goal) => CommandResponse {
            body: format!("Goal created: {} ({})", goal.id, goal.description),
            event_type: EventType::GoalStarted,
            status: EventStatus::Queued,
        },
        Err(err) => CommandResponse {
            body: format!("Failed to create goal: {err}"),
            event_type: EventType::CommandRejected,
            status: EventStatus::Failed,
        },
    }
}

fn handle_goal_list(goals: &dyn GoalManager) -> CommandResponse {
    match goals.list_goals() {
        Ok(goals_list) => {
            if goals_list.is_empty() {
                return CommandResponse {
                    body: "No active goals.".to_string(),
                    event_type: EventType::CommandAccepted,
                    status: EventStatus::Completed,
                };
            }
            let mut lines = Vec::new();
            for goal in &goals_list {
                lines.push(format!(
                    "- {} [{}]: {}",
                    goal.id, goal.status, goal.description
                ));
            }
            CommandResponse {
                body: lines.join("\n"),
                event_type: EventType::CommandAccepted,
                status: EventStatus::Completed,
            }
        }
        Err(err) => CommandResponse {
            body: format!("Failed to list goals: {err}"),
            event_type: EventType::CommandRejected,
            status: EventStatus::Failed,
        },
    }
}

fn handle_goal_status(id: &str, goals: &dyn GoalManager) -> CommandResponse {
    match goals.get_goal(id) {
        Ok(Some(goal)) => CommandResponse {
            body: format!(
                "Goal {}: {} (status: {}, created: {})",
                goal.id, goal.description, goal.status, goal.created_at
            ),
            event_type: EventType::CommandAccepted,
            status: EventStatus::Completed,
        },
        Ok(None) => CommandResponse {
            body: format!("Goal not found: {id}"),
            event_type: EventType::CommandRejected,
            status: EventStatus::Failed,
        },
        Err(err) => CommandResponse {
            body: format!("Failed to get goal status: {err}"),
            event_type: EventType::CommandRejected,
            status: EventStatus::Failed,
        },
    }
}

fn handle_goal_cancel(id: &str, goals: &dyn GoalManager) -> CommandResponse {
    match goals.cancel_goal(id) {
        Ok(goal) => CommandResponse {
            body: format!("Goal cancelled: {}", goal.id),
            event_type: EventType::GoalStateChanged,
            status: EventStatus::Completed,
        },
        Err(err) => CommandResponse {
            body: format!("Failed to cancel goal: {err}"),
            event_type: EventType::CommandRejected,
            status: EventStatus::Failed,
        },
    }
}

fn handle_workflow_run(
    name: &str,
    params: &[String],
    workflows: &dyn WorkflowDispatcher,
) -> CommandResponse {
    match workflows.run_workflow(name, params) {
        Ok(summary) => {
            let (status_word, event_type, event_status) = if summary.success {
                (
                    "completed",
                    EventType::GoalCompleted,
                    EventStatus::Completed,
                )
            } else {
                ("failed", EventType::GoalFailed, EventStatus::Failed)
            };
            let mut body = format!(
                "Workflow `{}` {}: {}/{} steps completed (run: {})",
                summary.workflow_name,
                status_word,
                summary.steps_completed,
                summary.steps_total,
                summary.run_id
            );
            if let Some(err) = &summary.error {
                body.push_str(&format!("\nError: {err}"));
            }
            CommandResponse {
                body,
                event_type,
                status: event_status,
            }
        }
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("not found") {
                // Try to list available workflows for a helpful error.
                let available = workflows
                    .list_workflows()
                    .map(|names| {
                        if names.is_empty() {
                            "none".to_string()
                        } else {
                            names.join(", ")
                        }
                    })
                    .unwrap_or_else(|_| "unknown".to_string());
                CommandResponse {
                    body: format!("Workflow `{name}` not found. Available workflows: {available}"),
                    event_type: EventType::CommandRejected,
                    status: EventStatus::Failed,
                }
            } else {
                CommandResponse {
                    body: format!("Failed to run workflow `{name}`: {err}"),
                    event_type: EventType::CommandRejected,
                    status: EventStatus::Failed,
                }
            }
        }
    }
}

fn handle_workflow_list(workflows: &dyn WorkflowDispatcher) -> CommandResponse {
    match workflows.list_workflows() {
        Ok(names) => {
            if names.is_empty() {
                return CommandResponse {
                    body: "No workflows available.".to_string(),
                    event_type: EventType::CommandAccepted,
                    status: EventStatus::Completed,
                };
            }
            let list = names
                .iter()
                .map(|name| format!("- {name}"))
                .collect::<Vec<_>>()
                .join("\n");
            CommandResponse {
                body: format!("Available workflows:\n{list}"),
                event_type: EventType::CommandAccepted,
                status: EventStatus::Completed,
            }
        }
        Err(err) => CommandResponse {
            body: format!("Failed to list workflows: {err}"),
            event_type: EventType::CommandRejected,
            status: EventStatus::Failed,
        },
    }
}

fn help_text() -> String {
    "\
Commands:
  !goal create <description>  - Create a new goal
  !goal list                  - List active goals with status
  !goal status <id>           - Detailed status of a goal
  !goal cancel <id>           - Cancel a running goal
  !workflow run <name> [params] - Trigger a workflow by name
  !workflow list              - List available workflows
  !help                       - Show this help message"
        .to_string()
}

/// Route an incoming Matrix message through the command system.
/// Returns `Ok(Some(response))` if the message was a command, `Ok(None)` if not,
/// or `Err` on internal errors. Sends the response envelope via the transport.
pub async fn route_command_message(
    message: &str,
    room_id: &str,
    transport: &dyn MatrixTransport,
    goals: &dyn GoalManager,
    workflows: &dyn WorkflowDispatcher,
) -> Result<Option<CommandResponse>> {
    let parsed = parse_command(message);

    match parsed {
        ParseResult::NotACommand => Ok(None),
        ParseResult::InvalidCommand(err_msg) => {
            let envelope = MatrixEventEnvelope::typed(
                EventType::CommandRejected,
                EventStatus::Failed,
                &format!("cmd_{}", now_unix()),
                now_unix(),
                &err_msg,
            );
            transport.send_outgoing(room_id, envelope).await?;
            Ok(Some(CommandResponse {
                body: err_msg,
                event_type: EventType::CommandRejected,
                status: EventStatus::Failed,
            }))
        }
        ParseResult::Command(cmd) => {
            let response = handle_command(&cmd, goals, workflows);
            let envelope = MatrixEventEnvelope::typed(
                response.event_type.clone(),
                response.status.clone(),
                &format!("cmd_{}", now_unix()),
                now_unix(),
                &response.body,
            );
            transport.send_outgoing(room_id, envelope).await?;
            Ok(Some(response))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::InMemoryMatrixTransport;
    use anyhow::anyhow;
    use std::sync::Mutex;

    // --- Mock GoalManager ---

    struct MockGoalManager {
        goals: Mutex<Vec<Goal>>,
        next_id: Mutex<u32>,
    }

    impl MockGoalManager {
        fn new() -> Self {
            Self {
                goals: Mutex::new(Vec::new()),
                next_id: Mutex::new(1),
            }
        }

        fn with_goals(goals: Vec<Goal>) -> Self {
            let max_id = goals.len() as u32 + 1;
            Self {
                goals: Mutex::new(goals),
                next_id: Mutex::new(max_id),
            }
        }
    }

    impl GoalManager for MockGoalManager {
        fn create_goal(&self, description: &str) -> Result<Goal> {
            let mut next_id = self.next_id.lock().expect("lock");
            let id = format!("goal-{}", *next_id);
            *next_id += 1;
            let goal = Goal {
                id: id.clone(),
                description: description.to_string(),
                status: GoalStatus::Active,
                created_at: 1000,
            };
            self.goals.lock().expect("lock").push(goal.clone());
            Ok(goal)
        }

        fn list_goals(&self) -> Result<Vec<Goal>> {
            Ok(self.goals.lock().expect("lock").clone())
        }

        fn get_goal(&self, id: &str) -> Result<Option<Goal>> {
            Ok(self
                .goals
                .lock()
                .expect("lock")
                .iter()
                .find(|g| g.id == id)
                .cloned())
        }

        fn cancel_goal(&self, id: &str) -> Result<Goal> {
            let mut goals = self.goals.lock().expect("lock");
            let goal = goals
                .iter_mut()
                .find(|g| g.id == id)
                .ok_or_else(|| anyhow!("goal not found: {id}"))?;
            goal.status = GoalStatus::Cancelled;
            Ok(goal.clone())
        }
    }

    // --- Mock WorkflowDispatcher ---

    struct MockWorkflowDispatcher {
        workflows: Vec<String>,
        should_fail: bool,
    }

    impl MockWorkflowDispatcher {
        fn new(workflows: Vec<String>) -> Self {
            Self {
                workflows,
                should_fail: false,
            }
        }
    }

    impl WorkflowDispatcher for MockWorkflowDispatcher {
        fn list_workflows(&self) -> Result<Vec<String>> {
            Ok(self.workflows.clone())
        }

        fn run_workflow(&self, name: &str, _params: &[String]) -> Result<WorkflowRunSummary> {
            if self.should_fail {
                return Err(anyhow!("executor error"));
            }
            if !self.workflows.contains(&name.to_string()) {
                return Err(anyhow!("workflow `{name}` not found"));
            }
            Ok(WorkflowRunSummary {
                run_id: "wf_test_123".to_string(),
                workflow_name: name.to_string(),
                success: true,
                steps_completed: 3,
                steps_total: 3,
                error: None,
            })
        }
    }

    // --- Command Parsing Tests ---

    #[test]
    fn parse_non_command_returns_not_a_command() {
        assert_eq!(parse_command("hello world"), ParseResult::NotACommand);
        assert_eq!(parse_command(""), ParseResult::NotACommand);
        assert_eq!(
            parse_command("https://example.com"),
            ParseResult::NotACommand
        );
    }

    #[test]
    fn parse_help_command() {
        assert_eq!(parse_command("!help"), ParseResult::Command(Command::Help));
    }

    #[test]
    fn parse_goal_create() {
        assert_eq!(
            parse_command("!goal create Build a trading bot"),
            ParseResult::Command(Command::GoalCreate {
                description: "Build a trading bot".to_string(),
            })
        );
    }

    #[test]
    fn parse_goal_create_missing_description() {
        match parse_command("!goal create") {
            ParseResult::InvalidCommand(msg) => {
                assert!(msg.contains("Missing description"), "got: {msg}");
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_goal_list() {
        assert_eq!(
            parse_command("!goal list"),
            ParseResult::Command(Command::GoalList)
        );
    }

    #[test]
    fn parse_goal_status() {
        assert_eq!(
            parse_command("!goal status goal-1"),
            ParseResult::Command(Command::GoalStatus {
                id: "goal-1".to_string(),
            })
        );
    }

    #[test]
    fn parse_goal_status_missing_id() {
        match parse_command("!goal status") {
            ParseResult::InvalidCommand(msg) => {
                assert!(msg.contains("Missing goal ID"), "got: {msg}");
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_goal_cancel() {
        assert_eq!(
            parse_command("!goal cancel goal-2"),
            ParseResult::Command(Command::GoalCancel {
                id: "goal-2".to_string(),
            })
        );
    }

    #[test]
    fn parse_goal_cancel_missing_id() {
        match parse_command("!goal cancel") {
            ParseResult::InvalidCommand(msg) => {
                assert!(msg.contains("Missing goal ID"), "got: {msg}");
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_goal_unknown_subcommand() {
        match parse_command("!goal foo") {
            ParseResult::InvalidCommand(msg) => {
                assert!(msg.contains("Unknown goal subcommand"), "got: {msg}");
                assert!(msg.contains("create"), "should list available commands");
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_goal_missing_subcommand() {
        match parse_command("!goal") {
            ParseResult::InvalidCommand(msg) => {
                assert!(msg.contains("Missing subcommand"), "got: {msg}");
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_workflow_run() {
        assert_eq!(
            parse_command("!workflow run intake-url"),
            ParseResult::Command(Command::WorkflowRun {
                name: "intake-url".to_string(),
                params: Vec::new(),
            })
        );
    }

    #[test]
    fn parse_workflow_run_with_params() {
        assert_eq!(
            parse_command("!workflow run intake-url param1 param2"),
            ParseResult::Command(Command::WorkflowRun {
                name: "intake-url".to_string(),
                params: vec!["param1".to_string(), "param2".to_string()],
            })
        );
    }

    #[test]
    fn parse_workflow_run_missing_name() {
        match parse_command("!workflow run") {
            ParseResult::InvalidCommand(msg) => {
                assert!(msg.contains("Missing workflow name"), "got: {msg}");
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_workflow_list() {
        assert_eq!(
            parse_command("!workflow list"),
            ParseResult::Command(Command::WorkflowList)
        );
    }

    #[test]
    fn parse_workflow_missing_subcommand() {
        match parse_command("!workflow") {
            ParseResult::InvalidCommand(msg) => {
                assert!(msg.contains("Missing subcommand"), "got: {msg}");
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_command_prefix() {
        match parse_command("!foobar") {
            ParseResult::InvalidCommand(msg) => {
                assert!(msg.contains("Unknown command"), "got: {msg}");
                assert!(msg.contains("!goal"), "should suggest available commands");
            }
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_command_is_case_insensitive() {
        assert_eq!(
            parse_command("!GOAL LIST"),
            ParseResult::Command(Command::GoalList)
        );
        assert_eq!(
            parse_command("!Workflow List"),
            ParseResult::Command(Command::WorkflowList)
        );
    }

    // --- Command Handler Tests ---

    #[test]
    fn handle_goal_create_success() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let cmd = Command::GoalCreate {
            description: "Build AI assistant".to_string(),
        };
        let resp = handle_command(&cmd, &goals, &workflows);
        assert!(resp.body.contains("Goal created"));
        assert!(resp.body.contains("goal-1"));
        assert_eq!(resp.event_type, EventType::GoalStarted);
    }

    #[test]
    fn handle_goal_list_empty() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let resp = handle_command(&Command::GoalList, &goals, &workflows);
        assert!(resp.body.contains("No active goals"));
    }

    #[test]
    fn handle_goal_list_with_goals() {
        let goals = MockGoalManager::with_goals(vec![
            Goal {
                id: "goal-1".to_string(),
                description: "First goal".to_string(),
                status: GoalStatus::Active,
                created_at: 1000,
            },
            Goal {
                id: "goal-2".to_string(),
                description: "Second goal".to_string(),
                status: GoalStatus::Completed,
                created_at: 2000,
            },
        ]);
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let resp = handle_command(&Command::GoalList, &goals, &workflows);
        assert!(resp.body.contains("goal-1"));
        assert!(resp.body.contains("goal-2"));
        assert!(resp.body.contains("active"));
        assert!(resp.body.contains("completed"));
    }

    #[test]
    fn handle_goal_status_found() {
        let goals = MockGoalManager::with_goals(vec![Goal {
            id: "goal-1".to_string(),
            description: "Test goal".to_string(),
            status: GoalStatus::Active,
            created_at: 1000,
        }]);
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let cmd = Command::GoalStatus {
            id: "goal-1".to_string(),
        };
        let resp = handle_command(&cmd, &goals, &workflows);
        assert!(resp.body.contains("goal-1"));
        assert!(resp.body.contains("Test goal"));
        assert!(resp.body.contains("active"));
    }

    #[test]
    fn handle_goal_status_not_found() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let cmd = Command::GoalStatus {
            id: "nonexistent".to_string(),
        };
        let resp = handle_command(&cmd, &goals, &workflows);
        assert!(resp.body.contains("not found"));
        assert_eq!(resp.event_type, EventType::CommandRejected);
    }

    #[test]
    fn handle_goal_cancel_success() {
        let goals = MockGoalManager::with_goals(vec![Goal {
            id: "goal-1".to_string(),
            description: "Cancel me".to_string(),
            status: GoalStatus::Active,
            created_at: 1000,
        }]);
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let cmd = Command::GoalCancel {
            id: "goal-1".to_string(),
        };
        let resp = handle_command(&cmd, &goals, &workflows);
        assert!(resp.body.contains("cancelled"));
        assert_eq!(resp.event_type, EventType::GoalStateChanged);
    }

    #[test]
    fn handle_goal_cancel_not_found() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let cmd = Command::GoalCancel {
            id: "nonexistent".to_string(),
        };
        let resp = handle_command(&cmd, &goals, &workflows);
        assert!(resp.body.contains("Failed to cancel"));
        assert_eq!(resp.event_type, EventType::CommandRejected);
    }

    #[test]
    fn handle_workflow_run_success() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(vec!["intake-url".to_string()]);
        let cmd = Command::WorkflowRun {
            name: "intake-url".to_string(),
            params: Vec::new(),
        };
        let resp = handle_command(&cmd, &goals, &workflows);
        assert!(resp.body.contains("intake-url"));
        assert!(resp.body.contains("completed"));
        assert!(resp.body.contains("3/3"));
    }

    #[test]
    fn handle_workflow_run_not_found_lists_available() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(vec![
            "intake-url".to_string(),
            "domain-queue-pull".to_string(),
        ]);
        let cmd = Command::WorkflowRun {
            name: "nonexistent".to_string(),
            params: Vec::new(),
        };
        let resp = handle_command(&cmd, &goals, &workflows);
        assert!(resp.body.contains("not found"));
        assert!(resp.body.contains("intake-url"));
        assert!(resp.body.contains("domain-queue-pull"));
    }

    #[test]
    fn handle_workflow_list_with_entries() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(vec![
            "intake-url".to_string(),
            "domain-queue-pull".to_string(),
        ]);
        let resp = handle_command(&Command::WorkflowList, &goals, &workflows);
        assert!(resp.body.contains("intake-url"));
        assert!(resp.body.contains("domain-queue-pull"));
    }

    #[test]
    fn handle_workflow_list_empty() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let resp = handle_command(&Command::WorkflowList, &goals, &workflows);
        assert!(resp.body.contains("No workflows available"));
    }

    #[test]
    fn handle_help_shows_all_commands() {
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let resp = handle_command(&Command::Help, &goals, &workflows);
        assert!(resp.body.contains("!goal create"));
        assert!(resp.body.contains("!goal list"));
        assert!(resp.body.contains("!goal status"));
        assert!(resp.body.contains("!goal cancel"));
        assert!(resp.body.contains("!workflow run"));
        assert!(resp.body.contains("!workflow list"));
        assert!(resp.body.contains("!help"));
    }

    // --- Route Command Message Tests ---

    #[tokio::test]
    async fn route_non_command_returns_none() {
        let transport = InMemoryMatrixTransport::default();
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let result =
            route_command_message("hello world", "#control", &transport, &goals, &workflows)
                .await
                .expect("should not error");
        assert!(result.is_none());
        assert!(transport.drain_outgoing().expect("drain_outgoing").is_empty());
    }

    #[tokio::test]
    async fn route_valid_command_sends_envelope() {
        let transport = InMemoryMatrixTransport::default();
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let result =
            route_command_message("!goal list", "#control", &transport, &goals, &workflows)
                .await
                .expect("should not error");
        assert!(result.is_some());
        let out = transport.drain_outgoing().expect("drain_outgoing");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].room_id, "#control");
        assert_eq!(out[0].envelope.sym.t, EventType::CommandAccepted.as_str());
    }

    #[tokio::test]
    async fn route_invalid_command_sends_rejection() {
        let transport = InMemoryMatrixTransport::default();
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let result = route_command_message("!foobar", "#control", &transport, &goals, &workflows)
            .await
            .expect("should not error");
        assert!(result.is_some());
        let resp = result.unwrap();
        assert_eq!(resp.event_type, EventType::CommandRejected);
        let out = transport.drain_outgoing().expect("drain_outgoing");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].envelope.sym.t, EventType::CommandRejected.as_str());
        assert_eq!(out[0].envelope.sym.s, EventStatus::Failed.as_str());
    }

    #[tokio::test]
    async fn route_goal_create_sends_started_envelope() {
        let transport = InMemoryMatrixTransport::default();
        let goals = MockGoalManager::new();
        let workflows = MockWorkflowDispatcher::new(Vec::new());
        let result = route_command_message(
            "!goal create Build trading bot",
            "#control",
            &transport,
            &goals,
            &workflows,
        )
        .await
        .expect("should not error");
        let resp = result.expect("should be a command");
        assert!(resp.body.contains("Goal created"));
        let out = transport.drain_outgoing().expect("drain_outgoing");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].envelope.sym.t, EventType::GoalStarted.as_str());
    }
}
