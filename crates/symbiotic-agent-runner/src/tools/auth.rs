//! Auth bridge tool for session acquisition.
//!
//! This is a dedicated runner-side tool that talks to the daemon bridge
//! directly. It does not go through the generic `tool.execute` path.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use symbiotic_core::protocol::{Tool, ToolResult};

use crate::{BridgeClient, CredentialAuthenticateRequest};

#[derive(Debug, Clone)]
struct RequestAuthSessionParams {
    target: String,
    scopes: Vec<String>,
    session_type: String,
    purpose: String,
    thread_id: Option<String>,
    auth_profile: Option<String>,
    prefer_existing_session: bool,
    require_human_approval: bool,
}

impl RequestAuthSessionParams {
    fn from_value(value: serde_json::Value) -> Result<Self> {
        let target = value
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing required parameter: target"))?;
        let purpose = value
            .get("purpose")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("missing required parameter: purpose"))?;
        let scopes = value
            .get("scopes")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_else(|| vec!["web.login".to_string()]);
        let session_type = value
            .get("session_type")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "browser".to_string());
        let thread_id = value
            .get("thread_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let auth_profile = value
            .get("auth_profile")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let prefer_existing_session = value
            .get("prefer_existing_session")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let require_human_approval = value
            .get("require_human_approval")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Ok(Self {
            target,
            scopes,
            session_type,
            purpose,
            thread_id,
            auth_profile,
            prefer_existing_session,
            require_human_approval,
        })
    }

    fn scopes_refs(&self) -> Vec<&str> {
        self.scopes.iter().map(|scope| scope.as_str()).collect()
    }
}

pub fn request_auth_session_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "target": {
                "type": "string",
                "description": "Target domain or service (for example github.com)"
            },
            "scopes": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Requested scopes (default: [\"web.login\"])"
            },
            "session_type": {
                "type": "string",
                "description": "Session type, usually browser or api (default: browser)"
            },
            "purpose": {
                "type": "string",
                "description": "Why authentication is needed"
            },
            "thread_id": {
                "type": "string",
                "description": "Optional workflow/thread identifier"
            },
            "auth_profile": {
                "type": "string",
                "description": "Optional auth profile hint"
            },
            "prefer_existing_session": {
                "type": "boolean",
                "description": "Try credential.request first (default: true)"
            },
            "require_human_approval": {
                "type": "boolean",
                "description": "Require human approval for auth fallback (default: true)"
            }
        },
        "required": ["target", "purpose"]
    })
}

pub struct RequestAuthSessionTool {
    bridge: Arc<BridgeClient>,
}

impl RequestAuthSessionTool {
    pub fn new(bridge: Arc<BridgeClient>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl Tool for RequestAuthSessionTool {
    fn name(&self) -> &str {
        "request_auth_session"
    }

    fn description(&self) -> &str {
        "Request an authentication session for a target. Reuses an existing session when available, otherwise starts the dedicated auth bridge flow."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        request_auth_session_schema()
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let params = RequestAuthSessionParams::from_value(params)?;

        let outcome = if params.prefer_existing_session {
            match self
                .bridge
                .request_credential(&params.target, &params.scopes_refs(), &params.session_type)
                .await
            {
                Ok(response) => auth_result_json(
                    "completed",
                    "credential.request",
                    &params,
                    Some(response),
                    "Existing session handle issued",
                    None,
                ),
                Err(error) => {
                    let response = self
                        .bridge
                        .authenticate_credential(CredentialAuthenticateRequest {
                            target: params.target.clone(),
                            scopes: params.scopes.clone(),
                            session_type: params.session_type.clone(),
                            purpose: params.purpose.clone(),
                            thread_id: params.thread_id.clone(),
                            auth_profile: params.auth_profile.clone(),
                            prefer_existing_session: params.prefer_existing_session,
                            require_human_approval: params.require_human_approval,
                        })
                        .await;
                    match response {
                        Ok(value) => auth_result_from_bridge(
                            &params,
                            "credential.authenticate",
                            value,
                            Some(format!(
                                "Existing session was unavailable, using auth bridge: {error}"
                            )),
                        ),
                        Err(auth_error) => auth_result_json(
                            "failed",
                            "credential.authenticate",
                            &params,
                            None,
                            "Authentication request failed",
                            Some(auth_error.to_string()),
                        ),
                    }
                }
            }
        } else {
            match self
                .bridge
                .authenticate_credential(CredentialAuthenticateRequest {
                    target: params.target.clone(),
                    scopes: params.scopes.clone(),
                    session_type: params.session_type.clone(),
                    purpose: params.purpose.clone(),
                    thread_id: params.thread_id.clone(),
                    auth_profile: params.auth_profile.clone(),
                    prefer_existing_session: params.prefer_existing_session,
                    require_human_approval: params.require_human_approval,
                })
                .await
            {
                Ok(value) => auth_result_from_bridge(
                    &params,
                    "credential.authenticate",
                    value,
                    Some("Auth bridge requested directly".to_string()),
                ),
                Err(error) => auth_result_json(
                    "failed",
                    "credential.authenticate",
                    &params,
                    None,
                    "Authentication request failed",
                    Some(error.to_string()),
                ),
            }
        };

        let success = outcome
            .get("state")
            .and_then(|v| v.as_str())
            .map(|state| state != "failed")
            .unwrap_or(true);

        Ok(ToolResult {
            success,
            output: serde_json::to_string(&outcome)
                .map_err(|e| anyhow!("failed to serialize auth result: {e}"))?,
        })
    }
}

fn auth_result_from_bridge(
    params: &RequestAuthSessionParams,
    source: &str,
    raw_response: serde_json::Value,
    message: Option<String>,
) -> serde_json::Value {
    let state = raw_response
        .get("state")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| infer_state(&raw_response));
    let request_id = raw_response
        .get("request_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            raw_response
                .get("rid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    let message = message.or_else(|| {
        raw_response
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    });

    let mut result = auth_result_json(
        &state,
        source,
        params,
        Some(raw_response),
        message
            .as_deref()
            .unwrap_or("Authentication request completed"),
        None,
    );
    if let Some(request_id) = request_id {
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "request_id".to_string(),
                serde_json::Value::String(request_id),
            );
        }
    }
    result
}

fn auth_result_json(
    state: &str,
    source: &str,
    params: &RequestAuthSessionParams,
    bridge_result: Option<serde_json::Value>,
    message: &str,
    error: Option<String>,
) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    result.insert("state".to_string(), serde_json::json!(state));
    result.insert("source".to_string(), serde_json::json!(source));
    result.insert("target".to_string(), serde_json::json!(&params.target));
    result.insert("scopes".to_string(), serde_json::json!(&params.scopes));
    result.insert(
        "session_type".to_string(),
        serde_json::json!(&params.session_type),
    );
    result.insert("purpose".to_string(), serde_json::json!(&params.purpose));
    if let Some(thread_id) = params.thread_id.as_ref() {
        result.insert("thread_id".to_string(), serde_json::json!(thread_id));
    }
    if let Some(auth_profile) = params.auth_profile.as_ref() {
        result.insert("auth_profile".to_string(), serde_json::json!(auth_profile));
    }
    result.insert(
        "prefer_existing_session".to_string(),
        serde_json::json!(params.prefer_existing_session),
    );
    result.insert(
        "require_human_approval".to_string(),
        serde_json::json!(params.require_human_approval),
    );
    result.insert("message".to_string(), serde_json::json!(message));
    if let Some(bridge_result) = bridge_result {
        result.insert("bridge_result".to_string(), bridge_result);
    }
    if let Some(error) = error {
        result.insert("error".to_string(), serde_json::json!(error));
    }
    serde_json::Value::Object(result)
}

fn infer_state(raw_response: &serde_json::Value) -> String {
    if let Some(state) = raw_response.get("state").and_then(|v| v.as_str()) {
        return state.to_string();
    }
    if raw_response.get("handle_id").is_some()
        || raw_response.get("session_handle").is_some()
        || raw_response.get("session").is_some()
    {
        return "completed".to_string();
    }
    if raw_response.get("request_id").is_some() || raw_response.get("rid").is_some() {
        return "awaiting_approval".to_string();
    }
    "failed".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_includes_required_fields_and_defaults() {
        let schema = request_auth_session_schema();
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required fields");
        assert!(required.iter().any(|v| v.as_str() == Some("target")));
        assert!(required.iter().any(|v| v.as_str() == Some("purpose")));
        assert_eq!(
            schema["properties"]["session_type"]["description"],
            "Session type, usually browser or api (default: browser)"
        );
    }

    #[test]
    fn infer_state_prefers_handle_ids() {
        let value = serde_json::json!({ "handle_id": "sh_1" });
        assert_eq!(infer_state(&value), "completed");
    }

    #[test]
    fn infer_state_falls_back_to_pending_request() {
        let value = serde_json::json!({ "request_id": "authreq_1" });
        assert_eq!(infer_state(&value), "awaiting_approval");
    }
}
