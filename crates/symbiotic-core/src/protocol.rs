//! Shared v2 event protocol types for Symbiotic Matrix transport.
//!
//! Defines the integer-based `Kind` and `Status` enums, plus the
//! `EventPayload` and `CommandPayload` wire formats used by both
//! the daemon (`sym.e`) and the app (`sym.c`).
//!
//! See `docs/design/event-protocol-v2.md` for the full specification.

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

/// What type of interaction this event represents.
///
/// Wire format: integer (`0`–`3`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
pub enum Kind {
    /// The daemon says something (bubble or pill).
    Message = 0,
    /// The daemon needs user input before continuing.
    Question = 1,
    /// Background thread needs attention (out-of-thread delivery).
    Notification = 2,
    /// Internal app bookkeeping (never shown in chat).
    State = 3,
}

/// What phase the event is in.
///
/// Wire format: integer (`0`–`4`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
pub enum Status {
    /// Actively processing.
    Working = 0,
    /// Completed successfully.
    Success = 1,
    /// Something went wrong.
    Fail = 2,
    /// Waiting for user response.
    Awaiting = 3,
    /// User already responded (chips disabled).
    Accepted = 4,
}

/// The `sym` object inside a `sym.e` Matrix message (daemon → app).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventPayload {
    /// Protocol version — always `2`.
    pub v: u8,
    /// What type of interaction.
    pub k: Kind,
    /// What phase it's in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub s: Option<Status>,
    /// Thread identifier for routing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    /// Matrix event ID this responds to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r: Option<String>,
    /// Tappable choice options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ch: Option<Vec<String>>,
    /// Dotted action name (only for `Kind::State`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a: Option<String>,
    /// Extra structured data (plan steps, error details, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
    /// Unix timestamp in seconds.
    pub ts: u64,
}

/// The `sym` object inside a `sym.c` Matrix message (app → daemon).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandPayload {
    /// Protocol version — always `2`.
    pub v: u8,
    /// Dotted command name (e.g. `goal.approve_plan`).
    pub c: String,
    /// Thread identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    /// Matrix event ID this responds to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r: Option<String>,
    /// Command parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
}

impl EventPayload {
    /// Create a new v2 event payload.
    pub fn new(kind: Kind, status: Status, ts: u64) -> Self {
        Self {
            v: 2,
            k: kind,
            s: Some(status),
            t: None,
            r: None,
            ch: None,
            a: None,
            d: None,
            ts,
        }
    }

    /// Create a state event (kind=3) with an action name.
    pub fn state(action: &str, ts: u64) -> Self {
        Self {
            v: 2,
            k: Kind::State,
            s: None,
            t: None,
            r: None,
            ch: None,
            a: Some(action.to_string()),
            d: None,
            ts,
        }
    }
}

impl CommandPayload {
    /// Create a new v2 command payload.
    pub fn new(command: &str) -> Self {
        Self {
            v: 2,
            c: command.to_string(),
            t: None,
            r: None,
            d: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Agent Execution Protocol
// ---------------------------------------------------------------------------

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
}

/// A tool that an agent can invoke during execution.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Unique name for this tool (used in LLM prompts and tool call parsing).
    fn name(&self) -> &str;

    /// Human-readable description of what this tool does.
    fn description(&self) -> &str;

    /// JSON Schema describing the expected parameters.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given parameters.
    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<ToolResult>;
}

/// A message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Trait for LLM interactions, enabling test mocking.
#[async_trait::async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat completion request and return the assistant's response.
    async fn chat(&self, messages: &[ChatMessage], json_mode: bool) -> anyhow::Result<String>;
}

/// Format a list of tools into a system prompt section.
pub fn format_tools_for_prompt(tools: &[&dyn Tool]) -> String {
    let mut out = String::from("Available tools:\n\n");
    for tool in tools {
        out.push_str(&format!(
            "- **{}**: {}\n  Parameters: {}\n\n",
            tool.name(),
            tool.description(),
            tool.parameters_schema()
        ));
    }
    out.push_str(
        "To use a tool, respond with a JSON object:\n\
         {\"tool\": \"<tool_name>\", \"params\": {<parameters>}}\n\n\
         When you have completed the task, respond with:\n\
         {\"done\": true, \"result\": \"<final answer>\"}\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_serializes_as_integer() {
        assert_eq!(serde_json::to_string(&Kind::Message).unwrap(), "0");
        assert_eq!(serde_json::to_string(&Kind::Question).unwrap(), "1");
        assert_eq!(serde_json::to_string(&Kind::Notification).unwrap(), "2");
        assert_eq!(serde_json::to_string(&Kind::State).unwrap(), "3");
    }

    #[test]
    fn kind_deserializes_from_integer() {
        assert_eq!(serde_json::from_str::<Kind>("0").unwrap(), Kind::Message);
        assert_eq!(serde_json::from_str::<Kind>("1").unwrap(), Kind::Question);
        assert_eq!(
            serde_json::from_str::<Kind>("2").unwrap(),
            Kind::Notification
        );
        assert_eq!(serde_json::from_str::<Kind>("3").unwrap(), Kind::State);
    }

    #[test]
    fn status_serializes_as_integer() {
        assert_eq!(serde_json::to_string(&Status::Working).unwrap(), "0");
        assert_eq!(serde_json::to_string(&Status::Success).unwrap(), "1");
        assert_eq!(serde_json::to_string(&Status::Fail).unwrap(), "2");
        assert_eq!(serde_json::to_string(&Status::Awaiting).unwrap(), "3");
        assert_eq!(serde_json::to_string(&Status::Accepted).unwrap(), "4");
    }

    #[test]
    fn status_deserializes_from_integer() {
        assert_eq!(
            serde_json::from_str::<Status>("0").unwrap(),
            Status::Working
        );
        assert_eq!(
            serde_json::from_str::<Status>("1").unwrap(),
            Status::Success
        );
        assert_eq!(
            serde_json::from_str::<Status>("4").unwrap(),
            Status::Accepted
        );
    }

    #[test]
    fn event_payload_roundtrip() {
        let payload = EventPayload {
            v: 2,
            k: Kind::Question,
            s: Some(Status::Awaiting),
            t: Some("thr-1".to_string()),
            r: Some("$ev123".to_string()),
            ch: Some(vec!["Budget".to_string(), "Mid-range".to_string()]),
            a: None,
            d: None,
            ts: 1710841200,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: EventPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, payload);
        // Verify integer encoding on wire
        assert!(json.contains("\"k\":1"));
        assert!(json.contains("\"s\":3"));
    }

    #[test]
    fn event_payload_omits_none_fields() {
        let payload = EventPayload::new(Kind::Message, Status::Success, 123);
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("\"t\""));
        assert!(!json.contains("\"r\""));
        assert!(!json.contains("\"ch\""));
        assert!(!json.contains("\"a\""));
        assert!(!json.contains("\"d\""));
    }

    #[test]
    fn state_event_has_action() {
        let payload = EventPayload::state("routing.created", 123);
        assert_eq!(payload.k, Kind::State);
        assert_eq!(payload.a.as_deref(), Some("routing.created"));
        assert!(payload.s.is_none());
    }

    #[test]
    fn command_payload_roundtrip() {
        let payload = CommandPayload {
            v: 2,
            c: "goal.approve_plan".to_string(),
            t: Some("thr-1".to_string()),
            r: Some("$plan_ev".to_string()),
            d: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: CommandPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn kind_unknown_integer_fails() {
        assert!(serde_json::from_str::<Kind>("99").is_err());
    }

    #[test]
    fn status_unknown_integer_fails() {
        assert!(serde_json::from_str::<Status>("99").is_err());
    }
}
