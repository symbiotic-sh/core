//! Parse `sym.e` envelopes from Matrix messages into typed events.
//!
//! Replaces `event_parser.dart` — the same logic, in Rust, shared across all
//! client platforms (mobile, desktop, web/WASM).
//!
//! # Wire format
//!
//! ```json
//! {
//!   "msgtype": "sym.e",
//!   "body": "Human readable text",
//!   "sym": {
//!     "v": 2,
//!     "k": 1,
//!     "s": 3,
//!     "t": "thr-xyz",
//!     "r": "$event_id",
//!     "ch": ["Budget", "Mid-range"],
//!     "a": "routing.created",
//!     "d": {},
//!     "ts": 1710841200
//!   }
//! }
//! ```

use serde_json::Value;
use symbiotic_core::protocol::{EventPayload, Kind, Status};

/// A parsed Symbiotic event ready for UI consumption.
///
/// This is the client-side view of an event — it contains the same data as
/// [`EventPayload`] but with fields expanded for direct use (e.g., `choices`
/// as `Vec<String>` instead of `Option<Vec<String>>`).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    /// What type of interaction (message, question, notification, state).
    pub kind: Kind,
    /// What phase (working, success, fail, awaiting, accepted).
    pub status: Status,
    /// Human-readable body text.
    pub body: String,
    /// Unix timestamp in seconds.
    pub timestamp_secs: u64,
    /// Thread context identifier for routing.
    pub thread_id: Option<String>,
    /// Matrix event ID this responds to.
    pub reply_to: Option<String>,
    /// Tappable choice options (for question events).
    pub choices: Vec<String>,
    /// Dotted action name (for state events).
    pub action: Option<String>,
    /// Extra structured data (plan steps, error details, etc.).
    pub details: Value,
    /// Optional progress float (0.0–1.0).
    pub progress: Option<f64>,
    /// Goal identifier — extracted from `d.goal_id`, falls back to `thread_id`.
    pub goal_id: Option<String>,
}

/// Errors from event parsing.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("not a sym.e envelope (wrong or missing msgtype)")]
    NotSymbiotic,
    #[error("missing or invalid 'sym' object")]
    MissingSym,
    #[error("unsupported protocol version: {0} (expected 2)")]
    UnsupportedVersion(u8),
    #[error("missing or invalid required field: {0}")]
    MissingField(&'static str),
    #[error("JSON deserialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Custom message type for Symbiotic v2 events.
pub const SYM_EVENT_MSGTYPE: &str = "sym.e";

/// Parse a Matrix message content map into a [`ParsedEvent`].
///
/// Returns `Err` if the content is not a valid `sym.e` envelope.
/// This is the Rust equivalent of `EventParser.parse()` in Dart.
///
/// # Arguments
///
/// * `content` — the `content` object from a Matrix room message event
/// * `fallback_ts_secs` — timestamp to use if the envelope has no `ts` field
///   (typically `origin_server_ts / 1000` from the Matrix event)
pub fn parse_event(content: &Value, fallback_ts_secs: u64) -> Result<ParsedEvent, ParseError> {
    let obj = content.as_object().ok_or(ParseError::MissingSym)?;

    // Only handle our custom message type.
    match obj.get("msgtype").and_then(Value::as_str) {
        Some(SYM_EVENT_MSGTYPE) => {}
        _ => return Err(ParseError::NotSymbiotic),
    }

    let sym = obj
        .get("sym")
        .and_then(Value::as_object)
        .ok_or(ParseError::MissingSym)?;

    // Version check — reject unknown protocol versions.
    if let Some(v) = sym.get("v").and_then(Value::as_u64) {
        if v != 2 {
            return Err(ParseError::UnsupportedVersion(v as u8));
        }
    }

    // Required fields: k (kind), s (status), body.
    let kind: Kind = sym
        .get("k")
        .ok_or(ParseError::MissingField("k"))
        .and_then(|v| {
            serde_json::from_value(v.clone()).map_err(|_| ParseError::MissingField("k"))
        })?;

    let status: Status = sym
        .get("s")
        .ok_or(ParseError::MissingField("s"))
        .and_then(|v| {
            serde_json::from_value(v.clone()).map_err(|_| ParseError::MissingField("s"))
        })?;

    let body = obj
        .get("body")
        .and_then(Value::as_str)
        .ok_or(ParseError::MissingField("body"))?
        .to_string();

    // Timestamp: prefer explicit `ts`, fall back to caller-provided value.
    // Daemon sends Unix seconds; detect milliseconds via threshold heuristic.
    let timestamp_secs = match sym.get("ts").and_then(Value::as_u64) {
        Some(raw) if raw > 10_000_000_000 => raw / 1000, // milliseconds → seconds
        Some(raw) => raw,
        None => fallback_ts_secs,
    };

    // Optional thread context.
    let thread_id = sym.get("t").and_then(Value::as_str).map(String::from);

    // Optional reply-to event ID.
    let reply_to = sym.get("r").and_then(Value::as_str).map(String::from);

    // Optional choices for question events.
    let choices = sym
        .get("ch")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    // Optional action name for state events.
    let action = sym.get("a").and_then(Value::as_str).map(String::from);

    // Optional details map — accept object or JSON-encoded string.
    let details = match sym.get("d") {
        Some(v) if v.is_object() => v.clone(),
        Some(Value::String(s)) => {
            serde_json::from_str(s).unwrap_or(Value::Object(Default::default()))
        }
        _ => Value::Object(Default::default()),
    };

    // Optional progress float.
    let progress = sym.get("p").and_then(Value::as_f64);

    // Extract goal_id from details, falling back to thread_id.
    let goal_id = details
        .get("goal_id")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| thread_id.clone());

    Ok(ParsedEvent {
        kind,
        status,
        body,
        timestamp_secs,
        thread_id,
        reply_to,
        choices,
        action,
        details,
        progress,
        goal_id,
    })
}

/// Parse from a raw JSON string (convenience for testing and WASM).
pub fn parse_event_str(json: &str, fallback_ts_secs: u64) -> Result<ParsedEvent, ParseError> {
    let value: Value = serde_json::from_str(json)?;
    parse_event(&value, fallback_ts_secs)
}

/// Check whether a [`ParsedEvent`] is visible to the user (not internal state).
pub fn is_visible(event: &ParsedEvent) -> bool {
    event.kind != Kind::State
}

/// Convert an [`EventPayload`] (from symbiotic-core) into a [`ParsedEvent`].
///
/// Useful when you have a deserialized `sym` object but need the
/// client-side `ParsedEvent` representation.
pub fn from_payload(payload: &EventPayload, body: &str) -> ParsedEvent {
    let details = payload
        .d
        .clone()
        .unwrap_or(Value::Object(Default::default()));
    let goal_id = details
        .get("goal_id")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| payload.t.clone());
    ParsedEvent {
        kind: payload.k,
        status: payload.s.unwrap_or(Status::Working),
        body: body.to_string(),
        timestamp_secs: payload.ts,
        thread_id: payload.t.clone(),
        reply_to: payload.r.clone(),
        choices: payload.ch.clone().unwrap_or_default(),
        action: payload.a.clone(),
        details,
        progress: None,
        goal_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(sym: Value, body: &str) -> Value {
        json!({
            "msgtype": "sym.e",
            "body": body,
            "sym": sym,
        })
    }

    #[test]
    fn parse_minimal_message() {
        let content = envelope(json!({"v": 2, "k": 0, "s": 1, "ts": 1710841200}), "Hello");
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.kind, Kind::Message);
        assert_eq!(ev.status, Status::Success);
        assert_eq!(ev.body, "Hello");
        assert_eq!(ev.timestamp_secs, 1710841200);
        assert!(ev.thread_id.is_none());
        assert!(ev.choices.is_empty());
    }

    #[test]
    fn parse_question_with_choices() {
        let content = envelope(
            json!({
                "v": 2, "k": 1, "s": 3, "ts": 100,
                "t": "thr-1",
                "ch": ["Budget", "Mid-range", "Premium"],
            }),
            "What's your budget?",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.kind, Kind::Question);
        assert_eq!(ev.status, Status::Awaiting);
        assert_eq!(ev.thread_id.as_deref(), Some("thr-1"));
        assert_eq!(ev.choices, vec!["Budget", "Mid-range", "Premium"]);
    }

    #[test]
    fn parse_state_event_with_action() {
        let content = envelope(
            json!({"v": 2, "k": 3, "s": 1, "ts": 100, "a": "routing.created"}),
            "",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.kind, Kind::State);
        assert_eq!(ev.action.as_deref(), Some("routing.created"));
        assert!(!is_visible(&ev));
    }

    #[test]
    fn parse_with_details_object() {
        let content = envelope(
            json!({"v": 2, "k": 0, "s": 1, "ts": 100, "d": {"goal_id": "g-1", "step": "classify"}}),
            "Running step",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.details["goal_id"], "g-1");
        assert_eq!(ev.details["step"], "classify");
    }

    #[test]
    fn parse_with_details_string() {
        let inner = json!({"plan": "do stuff"}).to_string();
        let content = envelope(
            json!({"v": 2, "k": 0, "s": 1, "ts": 100, "d": inner}),
            "Plan",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.details["plan"], "do stuff");
    }

    #[test]
    fn parse_with_progress() {
        let content = envelope(
            json!({"v": 2, "k": 0, "s": 0, "ts": 100, "p": 0.75}),
            "Working...",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.progress, Some(0.75));
    }

    #[test]
    fn parse_with_reply_to() {
        let content = envelope(
            json!({"v": 2, "k": 0, "s": 1, "ts": 100, "r": "$ev123"}),
            "Reply",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.reply_to.as_deref(), Some("$ev123"));
    }

    #[test]
    fn reject_wrong_msgtype() {
        let content = json!({"msgtype": "m.text", "body": "hello"});
        let err = parse_event(&content, 0).unwrap_err();
        assert!(matches!(err, ParseError::NotSymbiotic));
    }

    #[test]
    fn reject_missing_sym_object() {
        let content = json!({"msgtype": "sym.e", "body": "hello"});
        let err = parse_event(&content, 0).unwrap_err();
        assert!(matches!(err, ParseError::MissingSym));
    }

    #[test]
    fn reject_missing_kind() {
        let content = envelope(json!({"v": 2, "s": 1, "ts": 100}), "hello");
        let err = parse_event(&content, 0).unwrap_err();
        assert!(matches!(err, ParseError::MissingField("k")));
    }

    #[test]
    fn reject_invalid_kind() {
        let content = envelope(json!({"v": 2, "k": 99, "s": 1, "ts": 100}), "hello");
        let err = parse_event(&content, 0).unwrap_err();
        assert!(matches!(err, ParseError::MissingField("k")));
    }

    #[test]
    fn timestamp_fallback_to_caller() {
        let content = envelope(json!({"v": 2, "k": 0, "s": 1}), "No ts");
        let ev = parse_event(&content, 999).unwrap();
        assert_eq!(ev.timestamp_secs, 999);
    }

    #[test]
    fn timestamp_millisecond_detection() {
        let content = envelope(
            json!({"v": 2, "k": 0, "s": 1, "ts": 1710841200000u64}),
            "Ms",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.timestamp_secs, 1710841200);
    }

    #[test]
    fn parse_from_json_string() {
        let json_str = r#"{"msgtype":"sym.e","body":"Hi","sym":{"v":2,"k":0,"s":1,"ts":100}}"#;
        let ev = parse_event_str(json_str, 0).unwrap();
        assert_eq!(ev.body, "Hi");
    }

    #[test]
    fn message_is_visible() {
        let content = envelope(json!({"v": 2, "k": 0, "s": 1, "ts": 100}), "Visible");
        let ev = parse_event(&content, 0).unwrap();
        assert!(is_visible(&ev));
    }

    #[test]
    fn from_payload_roundtrip() {
        let payload = EventPayload::new(Kind::Question, Status::Awaiting, 42);
        let parsed = from_payload(&payload, "Question text");
        assert_eq!(parsed.kind, Kind::Question);
        assert_eq!(parsed.status, Status::Awaiting);
        assert_eq!(parsed.body, "Question text");
        assert_eq!(parsed.timestamp_secs, 42);
        // No thread_id or d.goal_id → goal_id is None.
        assert!(parsed.goal_id.is_none());
    }

    #[test]
    fn goal_id_from_details() {
        let content = envelope(
            json!({"v": 2, "k": 0, "s": 1, "ts": 100, "t": "thr-1", "d": {"goal_id": "g-42"}}),
            "With goal",
        );
        let ev = parse_event(&content, 0).unwrap();
        // d.goal_id takes precedence over thread_id.
        assert_eq!(ev.goal_id.as_deref(), Some("g-42"));
    }

    #[test]
    fn goal_id_falls_back_to_thread_id() {
        let content = envelope(
            json!({"v": 2, "k": 0, "s": 1, "ts": 100, "t": "thr-1"}),
            "Fallback",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.goal_id.as_deref(), Some("thr-1"));
    }

    #[test]
    fn goal_id_none_when_no_thread_or_details() {
        let content = envelope(json!({"v": 2, "k": 0, "s": 1, "ts": 100}), "No goal");
        let ev = parse_event(&content, 0).unwrap();
        assert!(ev.goal_id.is_none());
    }

    #[test]
    fn malformed_details_string_becomes_empty_object() {
        let content = envelope(
            json!({"v": 2, "k": 0, "s": 1, "ts": 100, "d": "not{json"}),
            "Bad details",
        );
        let ev = parse_event(&content, 0).unwrap();
        assert!(ev.details.is_object());
        assert_eq!(ev.details.as_object().unwrap().len(), 0);
    }

    #[test]
    fn reject_unsupported_version() {
        let content = envelope(json!({"v": 3, "k": 0, "s": 1, "ts": 100}), "v3");
        let err = parse_event(&content, 0).unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedVersion(3)));
    }

    #[test]
    fn accept_missing_version_field() {
        // Gracefully handle envelopes without a version field (legacy tolerance).
        let content = envelope(json!({"k": 0, "s": 1, "ts": 100}), "No version");
        let ev = parse_event(&content, 0).unwrap();
        assert_eq!(ev.kind, Kind::Message);
    }
}
