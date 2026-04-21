//! Legacy event types — DEPRECATED.
//!
//! The v1 string-based `EventType` and `EventStatus` enums are replaced by
//! the integer-based `Kind` and `Status` in `crate::protocol`.
//!
//! This module is kept only for `EventSource` which may be used elsewhere.
//! The old enums are deleted.

use serde::{Deserialize, Serialize};

/// Identifies the source of an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum EventSource {
    #[serde(rename = "agent")]
    Agent { id: String },
    #[serde(rename = "system")]
    System,
    #[serde(rename = "user")]
    User { device_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_source_agent_roundtrip() {
        let source = EventSource::Agent {
            id: "worker-1".to_string(),
        };
        let json = serde_json::to_string(&source).expect("serialize");
        assert!(json.contains("\"kind\":\"agent\""));
        let parsed: EventSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, source);
    }

    #[test]
    fn event_source_system_roundtrip() {
        let source = EventSource::System;
        let json = serde_json::to_string(&source).expect("serialize");
        let parsed: EventSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, source);
    }

    #[test]
    fn event_source_user_roundtrip() {
        let source = EventSource::User {
            device_id: "ABCDEF".to_string(),
        };
        let json = serde_json::to_string(&source).expect("serialize");
        let parsed: EventSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, source);
    }
}
