//! Public API surface for flutter_rust_bridge codegen.
//!
//! FRB generates Dart bindings from the public functions and types in this
//! module. Types must be FRB-compatible (no `serde_json::Value`, etc.).
//!
//! This module wraps the underlying `symbiotic_client` and `escrow` APIs
//! with FRB-friendly types.

use crate::escrow;

// ── Protocol types (FRB-compatible wrappers) ──────────────────────────

/// What type of interaction an event represents.
///
/// Maps to integer on wire: 0=message, 1=question, 2=notification, 3=state.
#[flutter_rust_bridge::frb]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Message = 0,
    Question = 1,
    Notification = 2,
    State = 3,
}

/// What phase an event is in.
///
/// Maps to integer on wire: 0=working, 1=success, 2=fail, 3=awaiting, 4=accepted.
#[flutter_rust_bridge::frb]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStatus {
    Working = 0,
    Success = 1,
    Fail = 2,
    Awaiting = 3,
    Accepted = 4,
}

/// A parsed Symbiotic event, ready for UI consumption.
///
/// FRB-compatible version of `symbiotic_client::parser::ParsedEvent`.
/// The `details` field is a JSON string instead of `serde_json::Value`.
#[flutter_rust_bridge::frb]
#[derive(Debug, Clone)]
pub struct ParsedEvent {
    pub kind: EventKind,
    pub status: EventStatus,
    pub body: String,
    pub timestamp_secs: u64,
    pub thread_id: Option<String>,
    pub reply_to: Option<String>,
    pub choices: Vec<String>,
    pub action: Option<String>,
    /// JSON-encoded details map. Parse with `jsonDecode()` in Dart.
    pub details_json: String,
    pub progress: Option<f64>,
    /// Goal identifier — extracted from details or thread_id fallback.
    pub goal_id: Option<String>,
}

/// Result of an escrow operation.
#[flutter_rust_bridge::frb]
#[derive(Debug, Clone)]
pub struct EscrowResult {
    pub ok: bool,
    pub blob_path: Option<String>,
    pub error: Option<String>,
}

// ── Conversion helpers ────────────────────────────────────────────────

impl From<symbiotic_client::Kind> for EventKind {
    fn from(k: symbiotic_client::Kind) -> Self {
        match k {
            symbiotic_client::Kind::Message => Self::Message,
            symbiotic_client::Kind::Question => Self::Question,
            symbiotic_client::Kind::Notification => Self::Notification,
            symbiotic_client::Kind::State => Self::State,
        }
    }
}

impl From<symbiotic_client::Status> for EventStatus {
    fn from(s: symbiotic_client::Status) -> Self {
        match s {
            symbiotic_client::Status::Working => Self::Working,
            symbiotic_client::Status::Success => Self::Success,
            symbiotic_client::Status::Fail => Self::Fail,
            symbiotic_client::Status::Awaiting => Self::Awaiting,
            symbiotic_client::Status::Accepted => Self::Accepted,
        }
    }
}

impl From<symbiotic_client::parser::ParsedEvent> for ParsedEvent {
    fn from(e: symbiotic_client::parser::ParsedEvent) -> Self {
        Self {
            kind: e.kind.into(),
            status: e.status.into(),
            body: e.body,
            timestamp_secs: e.timestamp_secs,
            thread_id: e.thread_id,
            reply_to: e.reply_to,
            choices: e.choices,
            action: e.action,
            details_json: e.details.to_string(),
            progress: e.progress,
            goal_id: e.goal_id,
        }
    }
}

impl From<crate::InternalEscrowResult> for EscrowResult {
    fn from(r: crate::InternalEscrowResult) -> Self {
        Self {
            ok: r.ok,
            blob_path: r.blob_path,
            error: r.error,
        }
    }
}

// ── Event parsing ─────────────────────────────────────────────────────

/// Parse a `sym.e` envelope JSON string into a [ParsedEvent].
///
/// Returns `None` if the JSON is not a valid Symbiotic envelope.
///
/// `fallback_ts_secs` is the Matrix `origin_server_ts` (in seconds) to use
/// if the envelope doesn't include an explicit timestamp.
#[flutter_rust_bridge::frb(sync)]
pub fn parse_event(json: String, fallback_ts_secs: u64) -> Option<ParsedEvent> {
    match symbiotic_client::parser::parse_event_str(&json, fallback_ts_secs) {
        Ok(parsed) => Some(ParsedEvent::from(parsed)),
        Err(symbiotic_client::parser::ParseError::NotSymbiotic) => None,
        Err(e) => {
            eprintln!("[parse_event] failed: {e}");
            None
        }
    }
}

/// Check whether an event kind is visible to the user (not internal state).
#[flutter_rust_bridge::frb(sync)]
pub fn is_event_visible(kind: EventKind) -> bool {
    kind != EventKind::State
}

// ── Command builders ──────────────────────────────────────────────────

/// Start deliberation on a new goal.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_goal_deliberate(description: String) -> String {
    symbiotic_client::commands::goal_deliberate(&description)
}

/// Retry a failed goal.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_goal_retry(template: String) -> String {
    symbiotic_client::commands::goal_retry(&template)
}

/// Stop a running goal.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_goal_stop(template: String) -> String {
    symbiotic_client::commands::goal_stop(&template)
}

/// List all goals.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_goal_list() -> String {
    symbiotic_client::commands::goal_list()
}

/// Send a message in a goal thread.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_goal_message(goal_id: String, message: String) -> String {
    symbiotic_client::commands::goal_message(&goal_id, &message)
}

/// Answer a daemon question (quick reply or typed response).
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_goal_answer(goal_id: String, message: String) -> String {
    symbiotic_client::commands::goal_answer(&goal_id, &message)
}

/// Approve a proposed plan.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_goal_plan_approve(goal_id: String) -> String {
    symbiotic_client::commands::goal_plan_approve(&goal_id)
}

/// Reject a proposed plan.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_goal_plan_reject(goal_id: String) -> String {
    symbiotic_client::commands::goal_plan_reject(&goal_id)
}

/// Submit a service credential.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_credential_submit(service: String, username: String, secret: String) -> String {
    symbiotic_client::commands::credential_submit(&service, &username, &secret)
}

/// Submit an API key credential.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_api_credential_submit(key: String, value: String, validate: bool) -> String {
    symbiotic_client::commands::api_credential_submit(&key, &value, validate)
}

/// Query a service credential status.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_credential_query(service: String) -> String {
    symbiotic_client::commands::credential_query(&service)
}

/// Query multiple API credential statuses.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_api_credential_query(keys: Vec<String>, rid: String) -> String {
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    symbiotic_client::commands::api_credential_query(&key_refs, &rid)
}

/// Remove a service credential.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_credential_remove(service: String) -> String {
    symbiotic_client::commands::credential_remove(&service)
}

/// Remove an API key credential.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_api_credential_remove(key: String) -> String {
    symbiotic_client::commands::api_credential_remove(&key)
}

/// Request key rotation.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_vault_key_rotation_start() -> String {
    symbiotic_client::commands::vault_key_rotation_start()
}

/// Submit a secret during install setup.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_install_secret_put(mode: String, key: String, value: String) -> String {
    symbiotic_client::commands::install_secret_put(&mode, &key, &value)
}

/// Trigger full install run.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_install_run(mode: String, install_id: Option<String>) -> String {
    symbiotic_client::commands::install_run(&mode, install_id.as_deref())
}

/// Provision infrastructure for install.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_install_provision(mode: String, install_id: Option<String>) -> String {
    symbiotic_client::commands::install_provision(&mode, install_id.as_deref())
}

/// Bootstrap install (no parameters).
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_install_bootstrap() -> String {
    symbiotic_client::commands::install_bootstrap()
}

/// Verify install.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_install_verify(mode: String, install_id: Option<String>) -> String {
    symbiotic_client::commands::install_verify(&mode, install_id.as_deref())
}

/// Register a push notification token.
#[flutter_rust_bridge::frb(sync)]
pub fn cmd_push_register(device_id: String, token: String, platform: String) -> String {
    symbiotic_client::commands::push_register(&device_id, &token, &platform)
}

// ── Escrow operations ─────────────────────────────────────────────────

/// Create an escrow blob from the device's identity key.
#[flutter_rust_bridge::frb]
pub fn escrow_create(passphrase: String, data_dir: String) -> EscrowResult {
    crate::escrow_create(&passphrase, &data_dir).into()
}

/// Recover identity key from an escrow blob.
#[flutter_rust_bridge::frb]
pub fn escrow_recover(passphrase: String, data_dir: String) -> EscrowResult {
    crate::escrow_recover(&passphrase, &data_dir).into()
}

/// Check whether an escrow blob exists.
#[flutter_rust_bridge::frb(sync)]
pub fn escrow_blob_exists(data_dir: String) -> bool {
    escrow::escrow_exists(&data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_valid() {
        let json = r#"{"msgtype":"sym.e","body":"Hello","sym":{"v":2,"k":0,"s":1,"ts":100}}"#;
        let ev = parse_event(json.to_string(), 0).unwrap();
        assert_eq!(ev.kind, EventKind::Message);
        assert_eq!(ev.status, EventStatus::Success);
        assert_eq!(ev.body, "Hello");
    }

    #[test]
    fn parse_event_invalid_returns_none() {
        let json = r#"{"msgtype":"m.text","body":"not symbiotic"}"#;
        assert!(parse_event(json.to_string(), 0).is_none());
    }

    #[test]
    fn command_builders_return_valid_json() {
        let cmd = cmd_goal_deliberate("test".to_string());
        assert!(cmd.contains("goal.deliberate"));
        assert!(cmd.contains("sym.c"));

        let cmd = cmd_goal_plan_approve("g-1".to_string());
        assert!(cmd.contains("goal.plan.approved"));
    }

    #[test]
    fn is_event_visible_filters_state() {
        assert!(is_event_visible(EventKind::Message));
        assert!(is_event_visible(EventKind::Question));
        assert!(is_event_visible(EventKind::Notification));
        assert!(!is_event_visible(EventKind::State));
    }

    #[test]
    fn escrow_roundtrip_via_api() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap().to_string();

        let result = escrow_create("correct horse battery staple".to_string(), data_dir.clone());
        assert!(result.ok);
        assert!(escrow_blob_exists(data_dir.clone()));

        let result = escrow_recover("correct horse battery staple".to_string(), data_dir);
        assert!(result.ok);
    }
}
