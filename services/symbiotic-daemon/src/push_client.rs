//! Push gateway client for Symbiotic managed mode.
//!
//! Sends push notification requests to the Symbiotic push gateway
//! (`https://push.symbiotic.sh/v1/notify`). The gateway holds APNs/FCM
//! credentials and dispatches to Apple/Google on behalf of self-hosted daemons.
//!
//! # Security
//!
//! - Only notification metadata is sent (category, priority, badge).
//! - Message content is never included (the gateway has no field for it).
//! - Device tokens may be encrypted (daemon encrypts, gateway decrypts).
//! - Authenticated via Bearer daemon_token.

use serde::{Deserialize, Serialize};
use tracing;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the push gateway client, read from environment variables.
#[derive(Debug, Clone)]
pub struct PushGatewayConfig {
    /// URL of the push gateway endpoint.
    /// e.g. `https://push.symbiotic.sh/v1/notify`
    pub gateway_url: String,
    /// Bearer token for authenticating with the push gateway.
    /// Same token as the relay daemon token.
    pub daemon_token: String,
}

impl PushGatewayConfig {
    /// Read push gateway configuration from environment variables.
    ///
    /// Returns `None` if `SYMBIOTIC_PUSH_GATEWAY_URL` is not set (push disabled).
    /// Note: Uses `SYMBIOTIC_RELAY_DAEMON_TOKEN` as the auth token (same token
    /// authenticates both relay tunnel and push gateway).
    pub fn from_env() -> Option<Self> {
        let gateway_url = std::env::var("SYMBIOTIC_PUSH_GATEWAY_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())?;

        let daemon_token = std::env::var("SYMBIOTIC_RELAY_DAEMON_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_default();

        if daemon_token.is_empty() {
            tracing::warn!(
                "push_client: SYMBIOTIC_PUSH_GATEWAY_URL is set but SYMBIOTIC_RELAY_DAEMON_TOKEN is missing"
            );
            return None;
        }

        Some(Self {
            gateway_url,
            daemon_token,
        })
    }
}

// ---------------------------------------------------------------------------
// Push notification request/response types
// ---------------------------------------------------------------------------

/// The notification payload within a push request.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NotificationPayload {
    /// Notification category (fixed enum on the gateway side).
    /// e.g. "brief_ready", "goal_progress", "action_required"
    pub category: String,
    /// Priority level: "normal" or "high".
    pub priority: String,
    /// Badge count to display on the app icon (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<u32>,
}

/// Request body sent to the push gateway.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PushGatewayRequest {
    /// Device identifier.
    pub device_id: String,
    /// Platform: "apns" or "fcm".
    pub platform: String,
    /// Encrypted device token (nonce:ciphertext).
    pub device_token_encrypted: String,
    /// Notification metadata.
    pub notification: NotificationPayload,
}

/// Response from the push gateway.
#[derive(Debug, Deserialize)]
pub struct PushGatewayResponse {
    /// Whether the push was accepted for delivery.
    #[serde(default)]
    pub accepted: bool,
    /// Error message if the push was rejected.
    #[serde(default)]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Push gateway client
// ---------------------------------------------------------------------------

/// HTTP client for the Symbiotic push gateway.
///
/// Sends push notification requests to the gateway, which dispatches
/// to APNs/FCM using Symbiotic's credentials.
pub struct PushGatewayClient {
    config: PushGatewayConfig,
    http_client: reqwest::Client,
}

impl PushGatewayClient {
    /// Create a new push gateway client with the given configuration.
    pub fn new(config: PushGatewayConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            config,
            http_client,
        }
    }

    /// Send a push notification through the gateway.
    ///
    /// # Arguments
    ///
    /// * `device_id` - Unique device identifier
    /// * `platform` - "apns" or "fcm"
    /// * `device_token_encrypted` - Encrypted device push token
    /// * `category` - Notification category (e.g. "brief_ready")
    /// * `priority` - "normal" or "high"
    /// * `badge` - Optional badge count
    pub async fn notify(
        &self,
        device_id: &str,
        platform: &str,
        device_token_encrypted: &str,
        category: &str,
        priority: &str,
        badge: Option<u32>,
    ) -> Result<(), PushClientError> {
        let request = PushGatewayRequest {
            device_id: device_id.to_string(),
            platform: platform.to_string(),
            device_token_encrypted: device_token_encrypted.to_string(),
            notification: NotificationPayload {
                category: category.to_string(),
                priority: priority.to_string(),
                badge,
            },
        };

        tracing::debug!(
            device_id = device_id,
            platform = platform,
            category = category,
            "push_client: sending notification"
        );

        let response = self
            .http_client
            .post(&self.config.gateway_url)
            .header(
                "Authorization",
                format!("Bearer {}", self.config.daemon_token),
            )
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| PushClientError::Request(format!("HTTP request failed: {e}")))?;

        let status = response.status();

        if status.is_success() {
            tracing::debug!(device_id = device_id, "push_client: notification accepted");
            return Ok(());
        }

        // Try to parse error response.
        let body = response.text().await.unwrap_or_default();

        match status.as_u16() {
            401 => Err(PushClientError::Unauthorized),
            429 => Err(PushClientError::RateLimited),
            400..=499 => Err(PushClientError::Rejected(format!("HTTP {status}: {body}"))),
            _ => Err(PushClientError::Server(format!("HTTP {status}: {body}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the push gateway client.
#[derive(Debug, thiserror::Error)]
pub enum PushClientError {
    #[error("request failed: {0}")]
    Request(String),
    #[error("unauthorized: invalid daemon token")]
    Unauthorized,
    #[error("rate limited by push gateway")]
    RateLimited,
    #[error("push rejected: {0}")]
    Rejected(String),
    #[error("gateway server error: {0}")]
    Server(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- NotificationPayload serialization ---

    #[test]
    fn notification_payload_serialization() {
        let payload = NotificationPayload {
            category: "brief_ready".to_string(),
            priority: "normal".to_string(),
            badge: Some(3),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"category\":\"brief_ready\""));
        assert!(json.contains("\"priority\":\"normal\""));
        assert!(json.contains("\"badge\":3"));
    }

    #[test]
    fn notification_payload_no_badge() {
        let payload = NotificationPayload {
            category: "goal_progress".to_string(),
            priority: "high".to_string(),
            badge: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("badge"));
    }

    // --- PushGatewayRequest serialization ---

    #[test]
    fn push_request_serialization() {
        let req = PushGatewayRequest {
            device_id: "iphone-abc".to_string(),
            platform: "apns".to_string(),
            device_token_encrypted: "nonce:ciphertext".to_string(),
            notification: NotificationPayload {
                category: "brief_ready".to_string(),
                priority: "normal".to_string(),
                badge: Some(3),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"device_id\":\"iphone-abc\""));
        assert!(json.contains("\"platform\":\"apns\""));
        assert!(json.contains("\"device_token_encrypted\":\"nonce:ciphertext\""));
        assert!(json.contains("\"category\":\"brief_ready\""));
    }

    #[test]
    fn push_request_deserialization_round_trip() {
        let req = PushGatewayRequest {
            device_id: "pixel-xyz".to_string(),
            platform: "fcm".to_string(),
            device_token_encrypted: "encrypted_token_data".to_string(),
            notification: NotificationPayload {
                category: "action_required".to_string(),
                priority: "high".to_string(),
                badge: None,
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: PushGatewayRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.device_id, "pixel-xyz");
        assert_eq!(parsed.platform, "fcm");
        assert_eq!(parsed.notification.category, "action_required");
        assert_eq!(parsed.notification.priority, "high");
        assert!(parsed.notification.badge.is_none());
    }

    // --- Config tests ---
    // Note: env-var tests that set shared vars (SYMBIOTIC_RELAY_DAEMON_TOKEN)
    // are racy in parallel test execution. We test config struct construction
    // directly instead.

    #[test]
    fn push_config_struct_construction() {
        let config = PushGatewayConfig {
            gateway_url: "https://push.example.com/v1/notify".to_string(),
            daemon_token: "dtk_test_push".to_string(),
        };
        assert_eq!(config.gateway_url, "https://push.example.com/v1/notify");
        assert_eq!(config.daemon_token, "dtk_test_push");
    }

    #[test]
    fn push_config_from_env_returns_none_when_gateway_url_unset() {
        // Only tests absence; safe from parallel interference.
        std::env::remove_var("SYMBIOTIC_PUSH_GATEWAY_URL");
        assert!(PushGatewayConfig::from_env().is_none());
    }

    // --- PushGatewayResponse deserialization ---

    #[test]
    fn push_response_success() {
        let json = r#"{"accepted":true}"#;
        let resp: PushGatewayResponse = serde_json::from_str(json).unwrap();
        assert!(resp.accepted);
        assert!(resp.error.is_none());
    }

    #[test]
    fn push_response_error() {
        let json = r#"{"accepted":false,"error":"device token expired"}"#;
        let resp: PushGatewayResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.accepted);
        assert_eq!(resp.error.as_deref(), Some("device token expired"));
    }

    // --- Error display ---

    #[test]
    fn push_client_error_display() {
        let err = PushClientError::Unauthorized;
        assert_eq!(err.to_string(), "unauthorized: invalid daemon token");

        let err = PushClientError::RateLimited;
        assert_eq!(err.to_string(), "rate limited by push gateway");

        let err = PushClientError::Request("timeout".to_string());
        assert_eq!(err.to_string(), "request failed: timeout");
    }
}
