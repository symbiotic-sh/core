//! Build `sym.c` command envelopes for the Symbiotic daemon.
//!
//! Replaces 15+ hand-rolled JSON maps in `app_state.dart` with a single
//! typed builder. Each method produces a valid v2 envelope string ready
//! to send as a Matrix message body.
//!
//! # Wire format
//!
//! ```json
//! {
//!   "msgtype": "sym.c",
//!   "body": "",
//!   "sym": {
//!     "v": 2,
//!     "c": "goal.deliberate",
//!     "t": "thread-id",
//!     "d": { "description": "..." }
//!   }
//! }
//! ```

use serde_json::{json, Value};

/// Custom message type for Symbiotic v2 commands.
pub const SYM_COMMAND_MSGTYPE: &str = "sym.c";

/// Protocol version.
const PROTOCOL_VERSION: u8 = 2;

/// Build a complete `sym.c` envelope as a JSON string.
///
/// This is the single point of envelope construction — all command methods
/// below call this, ensuring consistent structure.
fn build_envelope(command: &str, thread: Option<&str>, data: Value) -> String {
    let mut sym = json!({
        "v": PROTOCOL_VERSION,
        "c": command,
    });

    if let Some(t) = thread {
        sym["t"] = Value::String(t.to_string());
    }

    // Only include `d` if it's a non-empty object.
    if data.is_object() && !data.as_object().unwrap().is_empty() {
        sym["d"] = data;
    }

    json!({
        "msgtype": SYM_COMMAND_MSGTYPE,
        "body": "",
        "sym": sym,
    })
    .to_string()
}

// ── Goal commands ─────────────────────────────────────────────────────

/// Start deliberation on a new goal.
///
/// Daemon command: `goal.deliberate`
pub fn goal_deliberate(description: &str) -> String {
    build_envelope("goal.deliberate", None, json!({"description": description}))
}

/// Retry a failed goal.
///
/// Daemon command: `goal.retry`
pub fn goal_retry(template: &str) -> String {
    build_envelope("goal.retry", None, json!({"template": template}))
}

/// Stop a running goal.
///
/// Daemon command: `goal.stop`
pub fn goal_stop(template: &str) -> String {
    build_envelope("goal.stop", None, json!({"template": template}))
}

/// List all goals.
///
/// Daemon command: `goal.list`
pub fn goal_list() -> String {
    build_envelope("goal.list", None, json!({}))
}

/// Send a free-form message in a goal thread.
///
/// Daemon command: `goal.message`
pub fn goal_message(goal_id: &str, message: &str) -> String {
    build_envelope("goal.message", Some(goal_id), json!({"message": message}))
}

/// Answer a daemon question (quick reply chip or typed response).
///
/// Daemon command: `goal.answer`
pub fn goal_answer(goal_id: &str, message: &str) -> String {
    build_envelope("goal.answer", Some(goal_id), json!({"message": message}))
}

/// Approve a proposed plan.
///
/// Daemon command: `goal.plan.approved`
///
/// Note: the daemon expects `goal.plan.approved`, NOT `goal.approve_plan`.
/// This fixes the known naming mismatch (see NEXT.md).
pub fn goal_plan_approve(goal_id: &str) -> String {
    build_envelope("goal.plan.approved", Some(goal_id), json!({}))
}

/// Reject a proposed plan.
///
/// Daemon command: `goal.plan.rejected`
pub fn goal_plan_reject(goal_id: &str) -> String {
    build_envelope("goal.plan.rejected", Some(goal_id), json!({}))
}

/// Send a generic goal command with custom data.
///
/// Use this for commands not covered by the typed methods above.
pub fn goal_command(command: &str, goal_id: &str, data: Value) -> String {
    build_envelope(command, Some(goal_id), data)
}

// ── Credential commands ───────────────────────────────────────────────

/// Submit a service credential (username/password).
///
/// Daemon command: `credential.submit`
pub fn credential_submit(service: &str, username: &str, secret: &str) -> String {
    build_envelope(
        "credential.submit",
        None,
        json!({
            "service": service,
            "username": username,
            "secret": secret,
        }),
    )
}

/// Submit an API key credential.
///
/// Daemon command: `api_credential.submit`
pub fn api_credential_submit(key: &str, value: &str, validate: bool) -> String {
    build_envelope(
        "api_credential.submit",
        None,
        json!({
            "key": key,
            "value": value,
            "validate": validate,
        }),
    )
}

/// Query a service credential status.
///
/// Daemon command: `credential.query`
pub fn credential_query(service: &str) -> String {
    build_envelope("credential.query", None, json!({"service": service}))
}

/// Query multiple API credential statuses.
///
/// Daemon command: `credential.query` (with keys format)
pub fn api_credential_query(keys: &[&str], rid: &str) -> String {
    build_envelope(
        "credential.query",
        None,
        json!({
            "keys": keys.join(","),
            "rid": rid,
        }),
    )
}

/// Remove a service credential.
///
/// Daemon command: `credential.remove`
pub fn credential_remove(service: &str) -> String {
    build_envelope("credential.remove", None, json!({"service": service}))
}

/// Remove an API key credential.
///
/// Daemon command: `api_credential.remove`
pub fn api_credential_remove(key: &str) -> String {
    build_envelope("api_credential.remove", None, json!({"key": key}))
}

// ── Vault commands ────────────────────────────────────────────────────

/// Request key rotation.
///
/// Daemon command: `vault.key_rotation.start`
pub fn vault_key_rotation_start() -> String {
    build_envelope("vault.key_rotation.start", None, json!({}))
}

// ── Install commands ──────────────────────────────────────────────────

/// Submit a secret during setup.
///
/// Daemon command: `install.secret.put`
pub fn install_secret_put(mode: &str, key: &str, value: &str) -> String {
    build_envelope(
        "install.secret.put",
        None,
        json!({
            "mode": mode,
            "key": key,
            "value": value,
        }),
    )
}

/// Trigger full install run.
///
/// Daemon command: `install.run`
pub fn install_run(mode: &str, install_id: Option<&str>) -> String {
    let mut data = serde_json::Map::new();
    data.insert("mode".to_string(), json!(mode));
    if let Some(install_id) = install_id.filter(|value| !value.trim().is_empty()) {
        data.insert("install_id".to_string(), json!(install_id));
    }
    build_envelope("install.run", None, Value::Object(data))
}

/// Provision infrastructure for install.
///
/// Daemon command: `install.provision`
pub fn install_provision(mode: &str, install_id: Option<&str>) -> String {
    let mut data = serde_json::Map::new();
    data.insert("mode".to_string(), json!(mode));
    if let Some(install_id) = install_id.filter(|value| !value.trim().is_empty()) {
        data.insert("install_id".to_string(), json!(install_id));
    }
    build_envelope("install.provision", None, Value::Object(data))
}

/// Bootstrap install (no parameters).
///
/// Daemon command: `install.bootstrap`
pub fn install_bootstrap() -> String {
    build_envelope("install.bootstrap", None, json!({}))
}

/// Verify install.
///
/// Daemon command: `install.verify`
pub fn install_verify(mode: &str, install_id: Option<&str>) -> String {
    let mut data = serde_json::Map::new();
    data.insert("mode".to_string(), json!(mode));
    if let Some(install_id) = install_id.filter(|value| !value.trim().is_empty()) {
        data.insert("install_id".to_string(), json!(install_id));
    }
    build_envelope("install.verify", None, Value::Object(data))
}

// ── Push commands ─────────────────────────────────────────────────────

/// Register a push notification token.
///
/// Daemon command: `push.register`
pub fn push_register(device_id: &str, token: &str, platform: &str) -> String {
    build_envelope(
        "push.register",
        None,
        json!({
            "device_id": device_id,
            "token": token,
            "platform": platform,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Parse the envelope and return the `sym` object.
    fn sym(envelope: &str) -> Value {
        let v: Value = serde_json::from_str(envelope).unwrap();
        assert_eq!(v["msgtype"], "sym.c");
        assert_eq!(v["body"], "");
        v["sym"].clone()
    }

    #[test]
    fn goal_deliberate_envelope() {
        let s = sym(&goal_deliberate("Buy groceries"));
        assert_eq!(s["v"], 2);
        assert_eq!(s["c"], "goal.deliberate");
        assert_eq!(s["d"]["description"], "Buy groceries");
        assert!(s.get("t").is_none() || s["t"].is_null());
    }

    #[test]
    fn goal_retry_envelope() {
        let s = sym(&goal_retry("shopping"));
        assert_eq!(s["c"], "goal.retry");
        assert_eq!(s["d"]["template"], "shopping");
    }

    #[test]
    fn goal_stop_envelope() {
        let s = sym(&goal_stop("shopping"));
        assert_eq!(s["c"], "goal.stop");
        assert_eq!(s["d"]["template"], "shopping");
    }

    #[test]
    fn goal_list_envelope() {
        let s = sym(&goal_list());
        assert_eq!(s["c"], "goal.list");
        // Empty data should be omitted.
        assert!(s.get("d").is_none() || s["d"].is_null());
    }

    #[test]
    fn goal_message_envelope() {
        let s = sym(&goal_message("g-1", "Hello daemon"));
        assert_eq!(s["c"], "goal.message");
        assert_eq!(s["t"], "g-1");
        assert_eq!(s["d"]["message"], "Hello daemon");
    }

    #[test]
    fn goal_answer_envelope() {
        let s = sym(&goal_answer("g-1", "Budget"));
        assert_eq!(s["c"], "goal.answer");
        assert_eq!(s["t"], "g-1");
        assert_eq!(s["d"]["message"], "Budget");
    }

    #[test]
    fn goal_plan_approve_uses_correct_command_name() {
        let s = sym(&goal_plan_approve("g-1"));
        // Must be "goal.plan.approved" — NOT "goal.approve_plan".
        assert_eq!(s["c"], "goal.plan.approved");
        assert_eq!(s["t"], "g-1");
    }

    #[test]
    fn goal_plan_reject_uses_correct_command_name() {
        let s = sym(&goal_plan_reject("g-1"));
        assert_eq!(s["c"], "goal.plan.rejected");
        assert_eq!(s["t"], "g-1");
    }

    #[test]
    fn goal_command_generic() {
        let s = sym(&goal_command("goal.custom", "g-2", json!({"foo": "bar"})));
        assert_eq!(s["c"], "goal.custom");
        assert_eq!(s["t"], "g-2");
        assert_eq!(s["d"]["foo"], "bar");
    }

    #[test]
    fn credential_submit_envelope() {
        let s = sym(&credential_submit("twitter", "user", "pass"));
        assert_eq!(s["c"], "credential.submit");
        assert_eq!(s["d"]["service"], "twitter");
        assert_eq!(s["d"]["username"], "user");
        assert_eq!(s["d"]["secret"], "pass");
    }

    #[test]
    fn api_credential_submit_envelope() {
        let s = sym(&api_credential_submit("OPENAI_API_KEY", "sk-abc", true));
        assert_eq!(s["c"], "api_credential.submit");
        assert_eq!(s["d"]["key"], "OPENAI_API_KEY");
        assert_eq!(s["d"]["value"], "sk-abc");
        assert_eq!(s["d"]["validate"], true);
    }

    #[test]
    fn credential_query_envelope() {
        let s = sym(&credential_query("twitter"));
        assert_eq!(s["c"], "credential.query");
        assert_eq!(s["d"]["service"], "twitter");
    }

    #[test]
    fn api_credential_query_envelope() {
        let s = sym(&api_credential_query(
            &["OPENAI_API_KEY", "ANTHROPIC_API_KEY"],
            "req-1",
        ));
        assert_eq!(s["c"], "credential.query");
        assert_eq!(s["d"]["keys"], "OPENAI_API_KEY,ANTHROPIC_API_KEY");
        assert_eq!(s["d"]["rid"], "req-1");
    }

    #[test]
    fn credential_remove_envelope() {
        let s = sym(&credential_remove("twitter"));
        assert_eq!(s["c"], "credential.remove");
        assert_eq!(s["d"]["service"], "twitter");
    }

    #[test]
    fn api_credential_remove_envelope() {
        let s = sym(&api_credential_remove("OPENAI_API_KEY"));
        assert_eq!(s["c"], "api_credential.remove");
        assert_eq!(s["d"]["key"], "OPENAI_API_KEY");
    }

    #[test]
    fn vault_key_rotation_envelope() {
        let s = sym(&vault_key_rotation_start());
        assert_eq!(s["c"], "vault.key_rotation.start");
    }

    #[test]
    fn install_secret_put_envelope() {
        let s = sym(&install_secret_put("byok", "OPENAI_API_KEY", "sk-test"));
        assert_eq!(s["c"], "install.secret.put");
        assert_eq!(s["d"]["mode"], "byok");
        assert_eq!(s["d"]["key"], "OPENAI_API_KEY");
        assert_eq!(s["d"]["value"], "sk-test");
    }

    #[test]
    fn install_run_envelope() {
        let s = sym(&install_run("byok", Some("install-001")));
        assert_eq!(s["c"], "install.run");
        assert_eq!(s["d"]["mode"], "byok");
        assert_eq!(s["d"]["install_id"], "install-001");
    }

    #[test]
    fn install_provision_envelope() {
        let s = sym(&install_provision("managed", Some("install-002")));
        assert_eq!(s["c"], "install.provision");
        assert_eq!(s["d"]["mode"], "managed");
        assert_eq!(s["d"]["install_id"], "install-002");
    }

    #[test]
    fn install_bootstrap_envelope() {
        let s = sym(&install_bootstrap());
        assert_eq!(s["c"], "install.bootstrap");
        // Empty data should be omitted.
        assert!(s.get("d").is_none() || s["d"].is_null());
    }

    #[test]
    fn install_verify_envelope() {
        let s = sym(&install_verify("byok", Some("install-003")));
        assert_eq!(s["c"], "install.verify");
        assert_eq!(s["d"]["mode"], "byok");
        assert_eq!(s["d"]["install_id"], "install-003");
    }

    #[test]
    fn push_register_envelope() {
        let s = sym(&push_register("dev-1", "fcm-token-xyz", "android"));
        assert_eq!(s["c"], "push.register");
        assert_eq!(s["d"]["device_id"], "dev-1");
        assert_eq!(s["d"]["token"], "fcm-token-xyz");
        assert_eq!(s["d"]["platform"], "android");
    }

    #[test]
    fn all_envelopes_are_valid_json() {
        let envelopes = [
            goal_deliberate("test"),
            goal_retry("test"),
            goal_stop("test"),
            goal_list(),
            goal_message("g-1", "hi"),
            goal_answer("g-1", "yes"),
            goal_plan_approve("g-1"),
            goal_plan_reject("g-1"),
            credential_submit("svc", "user", "pass"),
            api_credential_submit("key", "val", false),
            credential_query("svc"),
            api_credential_query(&["k1"], "r1"),
            credential_remove("svc"),
            api_credential_remove("key"),
            vault_key_rotation_start(),
            install_secret_put("byok", "k", "v"),
            install_run("byok", None),
            install_provision("byok", None),
            install_bootstrap(),
            install_verify("byok", None),
            push_register("d", "t", "ios"),
        ];
        for env in &envelopes {
            let parsed: Value =
                serde_json::from_str(env).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{env}"));
            assert_eq!(parsed["msgtype"], "sym.c");
            assert_eq!(parsed["sym"]["v"], 2);
        }
    }
}
