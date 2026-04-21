use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use symbiotic_core::protocol::{EventPayload, Kind, Status};

/// Message type for daemon → app events.
pub const MSGTYPE_EVENT: &str = "sym.e";
/// Message type for app → daemon commands.
pub const MSGTYPE_COMMAND: &str = "sym.c";

/// A v2 Matrix message envelope wrapping either an event or command payload.
///
/// Wire format:
/// ```json
/// { "msgtype": "sym.e", "body": "human-readable", "sym": { "v": 2, "k": 0, ... } }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MatrixEventEnvelope {
    pub msgtype: String,
    pub body: String,
    pub sym: EventPayload,
}

impl MatrixEventEnvelope {
    /// Create a new v2 event envelope.
    pub fn new(kind: Kind, status: Status, ts: u64, body: &str) -> Self {
        Self {
            msgtype: MSGTYPE_EVENT.to_string(),
            body: body.to_string(),
            sym: EventPayload::new(kind, status, ts),
        }
    }

    /// Create a state event envelope (kind=3, no status).
    pub fn state(action: &str, ts: u64, body: &str) -> Self {
        Self {
            msgtype: MSGTYPE_EVENT.to_string(),
            body: body.to_string(),
            sym: EventPayload::state(action, ts),
        }
    }

    /// Set the thread identifier.
    pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.sym.t = Some(thread_id.into());
        self
    }

    /// Set the reply-to Matrix event ID.
    pub fn with_reply_to(mut self, event_id: impl Into<String>) -> Self {
        self.sym.r = Some(event_id.into());
        self
    }

    /// Set tappable choice options.
    pub fn with_choices(mut self, choices: Vec<String>) -> Self {
        self.sym.ch = Some(choices);
        self
    }

    /// Set the detail object.
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.sym.d = Some(detail);
        self
    }

    /// Merge a key-value pair into the detail object (creates it if absent).
    pub fn with_detail_field(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        let d = self.sym.d.get_or_insert_with(|| serde_json::json!({}));
        if let Some(map) = d.as_object_mut() {
            map.insert(key.to_string(), value.into());
        }
        self
    }

    /// Tag this envelope with a sensitivity level.
    pub fn with_sensitivity(self, sensitivity: &str) -> Self {
        self.with_detail_field("sensitivity", sensitivity)
    }

    /// Attach a Matrix-spec `m.relates_to` thread relation pointing at
    /// `parent_event_id` (per MSC3440/3771). The relation includes
    /// `is_falling_back: true` and a nested `m.in_reply_to` so older
    /// clients without thread support still render this envelope as a
    /// regular reply rather than a top-level message.
    ///
    /// Stored under the envelope's `sym.d` detail map at key
    /// `"m.relates_to"`. The transport layer is responsible for
    /// projecting this onto the outbound Matrix `room.message` event.
    pub fn with_thread_parent(self, parent_event_id: &str) -> Self {
        self.with_detail_field(
            "m.relates_to",
            serde_json::json!({
                "rel_type": "m.thread",
                "event_id": parent_event_id,
                "is_falling_back": true,
                "m.in_reply_to": { "event_id": parent_event_id },
            }),
        )
    }

    /// Return the sensitivity level if set.
    pub fn sensitivity(&self) -> Option<&str> {
        self.sym
            .d
            .as_ref()
            .and_then(|d| d.get("sensitivity"))
            .and_then(|v| v.as_str())
    }

    /// Return `true` when this envelope is tagged as Private / Tier 3.
    pub fn is_private(&self) -> bool {
        self.sensitivity()
            .map(|s| s.eq_ignore_ascii_case("private"))
            .unwrap_or(false)
    }

    /// Replace body with a metadata-only placeholder and strip content details.
    pub fn redact_to_placeholder(mut self) -> Self {
        self.body = "[Private content \u{2014} phone only]".to_string();
        if let Some(d) = self.sym.d.as_mut() {
            if let Some(map) = d.as_object_mut() {
                let preserve_keys = ["sensitivity", "url", "title", "room", "sender", "record_id"];
                map.retain(|key, _| preserve_keys.contains(&key.as_str()));
                map.insert("phone_only".to_string(), serde_json::json!("true"));
            }
        }
        self
    }

    /// Validate the envelope for correctness before sending.
    pub fn validate(&self) -> Result<()> {
        if self.msgtype != MSGTYPE_EVENT {
            return Err(anyhow!("invalid msgtype: expected {}", MSGTYPE_EVENT));
        }
        if self.body.trim().is_empty() {
            return Err(anyhow!("body cannot be empty"));
        }
        if self.sym.v != 2 {
            return Err(anyhow!("unsupported sym.v: expected 2, got {}", self.sym.v));
        }
        if self.sym.ts == 0 {
            return Err(anyhow!("sym.ts must be > 0"));
        }
        // State events must have an action name
        if self.sym.k == Kind::State && self.sym.a.is_none() {
            return Err(anyhow!("state events (k=3) must have an action (a)"));
        }
        // Non-state events must have a status
        if self.sym.k != Kind::State && self.sym.s.is_none() {
            return Err(anyhow!("non-state events must have a status (s)"));
        }
        Ok(())
    }

    /// Parse and validate a JSON string into an envelope.
    pub fn parse_strict(raw: &str) -> Result<Self> {
        let parsed: Self = serde_json::from_str(raw)?;
        parsed.validate()?;
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_has_v2_defaults() {
        let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, 123, "done");
        assert_eq!(envelope.msgtype, "sym.e");
        assert_eq!(envelope.sym.v, 2);
        assert_eq!(envelope.sym.k, Kind::Message);
        assert_eq!(envelope.sym.s, Some(Status::Success));
    }

    #[test]
    fn state_envelope() {
        let envelope = MatrixEventEnvelope::state("routing.created", 123, "Thread created");
        assert_eq!(envelope.sym.k, Kind::State);
        assert_eq!(envelope.sym.a.as_deref(), Some("routing.created"));
        assert!(envelope.sym.s.is_none());
    }

    #[test]
    fn builder_chain() {
        let envelope = MatrixEventEnvelope::new(Kind::Question, Status::Awaiting, 100, "Budget?")
            .with_thread("thr-1")
            .with_choices(vec!["Low".into(), "High".into()])
            .with_detail_field("plan", "some plan");
        assert_eq!(envelope.sym.t.as_deref(), Some("thr-1"));
        assert_eq!(
            envelope.sym.ch.as_deref(),
            Some(&["Low".to_string(), "High".to_string()][..])
        );
        assert!(envelope.sym.d.is_some());
    }

    #[test]
    fn validate_rejects_empty_body() {
        let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Working, 123, "  ");
        assert!(envelope.validate().is_err());
    }

    #[test]
    fn validate_rejects_state_without_action() {
        let mut envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, 123, "ok");
        envelope.sym.k = Kind::State;
        envelope.sym.s = None;
        envelope.sym.a = None;
        assert!(envelope.validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_envelope() {
        let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, 123, "done");
        assert!(envelope.validate().is_ok());
    }

    #[test]
    fn validate_accepts_valid_state_envelope() {
        let envelope = MatrixEventEnvelope::state("snapshot", 123, "Status snapshot");
        assert!(envelope.validate().is_ok());
    }

    #[test]
    fn sensitivity_and_redaction() {
        let envelope =
            MatrixEventEnvelope::new(Kind::Message, Status::Success, 123, "secret content")
                .with_sensitivity("private")
                .with_detail_field("url", "https://example.com")
                .with_detail_field("some_content", "should be removed");

        assert!(envelope.is_private());

        let redacted = envelope.redact_to_placeholder();
        assert!(redacted.body.contains("Private content"));
        assert_eq!(redacted.sensitivity(), Some("private"));
        assert!(redacted.sym.d.as_ref().unwrap().get("url").is_some());
        assert!(redacted
            .sym
            .d
            .as_ref()
            .unwrap()
            .get("some_content")
            .is_none());
    }

    #[test]
    fn roundtrip_serialization() {
        let envelope =
            MatrixEventEnvelope::new(Kind::Question, Status::Awaiting, 1710841200, "Budget?")
                .with_thread("thr-1")
                .with_choices(vec!["Low".into(), "Mid".into(), "High".into()]);
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: MatrixEventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, envelope);
        // Verify integers on wire
        assert!(json.contains("\"k\":1"), "json={json}");
        assert!(json.contains("\"s\":3"), "json={json}");
        assert!(json.contains("\"msgtype\":\"sym.e\""), "json={json}");
    }

    #[test]
    fn parse_strict_validates() {
        let json = r#"{
            "msgtype": "sym.e",
            "body": "ok",
            "sym": {
                "v": 2,
                "k": 0,
                "s": 1,
                "ts": 123
            }
        }"#;
        assert!(MatrixEventEnvelope::parse_strict(json).is_ok());
    }

    #[test]
    fn parse_strict_rejects_v1() {
        let json = r#"{
            "msgtype": "org.symbiotic.event",
            "body": "ok",
            "sym": {
                "v": 1,
                "k": 0,
                "s": 1,
                "ts": 123
            }
        }"#;
        assert!(MatrixEventEnvelope::parse_strict(json).is_err());
    }
}
