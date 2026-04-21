//! Push notification gateway trait and provider implementations.

use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Mutex;

use crate::error::PushError;
use crate::types::{PushNotification, PushProvider, PushResponse};

/// Trait for sending push notifications through a provider.
#[async_trait]
pub trait PushGateway: Send + Sync {
    /// Returns which push provider this gateway handles.
    fn provider(&self) -> PushProvider;

    /// Send a single push notification.
    async fn send(&self, notification: &PushNotification) -> Result<PushResponse, PushError>;
}

// ---------------------------------------------------------------------------
// Cached JWT token
// ---------------------------------------------------------------------------

/// A cached JWT token with its expiry timestamp (Unix epoch seconds).
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    /// Unix epoch seconds at which this token expires.
    expires_at: i64,
}

impl CachedToken {
    /// Returns true if the token is still valid with at least `margin_secs` remaining.
    fn is_valid(&self, margin_secs: i64) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.expires_at - now > margin_secs
    }
}

// ---------------------------------------------------------------------------
// APNs Configuration
// ---------------------------------------------------------------------------

/// Configuration for the APNs gateway.
#[derive(Debug, Clone)]
pub struct ApnsConfig {
    /// Apple Developer Team ID (10-character string).
    pub team_id: String,
    /// Key ID for the APNs auth key (from App Store Connect).
    pub key_id: String,
    /// The raw `.p8` private key in PEM format.
    pub private_key_pem: String,
    /// Whether to use the sandbox APNs environment.
    pub sandbox: bool,
    /// Override the APNs base URL (for testing with mock servers).
    pub base_url_override: Option<String>,
}

// ---------------------------------------------------------------------------
// APNs Gateway
// ---------------------------------------------------------------------------

/// Gateway for Apple Push Notification service (APNs).
///
/// Uses HTTP/2 with JWT-based authentication (ES256). Connects to either the
/// production or sandbox environment based on the `sandbox` flag.
///
/// JWT tokens are cached and reused for up to 50 minutes (APNs tokens
/// are valid for 60 minutes; we refresh with a 10-minute margin).
pub struct ApnsGateway {
    config: ApnsConfig,
    /// HTTP client for making requests.
    client: reqwest::Client,
    /// Cached JWT token for APNs authentication.
    cached_jwt: Mutex<Option<CachedToken>>,
}

impl std::fmt::Debug for ApnsGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApnsGateway")
            .field("team_id", &self.config.team_id)
            .field("key_id", &self.config.key_id)
            .field("sandbox", &self.config.sandbox)
            .finish()
    }
}

impl ApnsGateway {
    /// Create a new APNs gateway from configuration.
    pub fn new(config: ApnsConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            cached_jwt: Mutex::new(None),
        }
    }

    /// Returns the APNs base URL for the configured environment.
    fn base_url(&self) -> &str {
        if let Some(ref url) = self.config.base_url_override {
            url.as_str()
        } else if self.config.sandbox {
            "https://api.sandbox.push.apple.com"
        } else {
            "https://api.push.apple.com"
        }
    }

    /// Get or refresh the cached APNs JWT token.
    ///
    /// APNs JWT tokens are valid for 60 minutes. We cache and reuse them,
    /// refreshing when less than 10 minutes remain.
    fn get_jwt(&self) -> Result<String, PushError> {
        // Check cached token first.
        {
            let cache = self
                .cached_jwt
                .lock()
                .map_err(|_| PushError::SendFailed("APNs JWT cache lock poisoned".to_string()))?;
            if let Some(ref cached) = *cache {
                if cached.is_valid(600) {
                    // 10-minute margin
                    return Ok(cached.token.clone());
                }
            }
        }

        // Build a fresh JWT.
        let token = self.build_jwt()?;
        let expires_at = chrono::Utc::now().timestamp() + 3600; // 60 minutes

        let mut cache = self
            .cached_jwt
            .lock()
            .map_err(|_| PushError::SendFailed("APNs JWT cache lock poisoned".to_string()))?;
        *cache = Some(CachedToken {
            token: token.clone(),
            expires_at,
        });

        Ok(token)
    }

    /// Build the APNs JWT token for authentication.
    ///
    /// JWT header: `{"alg": "ES256", "kid": "<key_id>", "typ": "JWT"}`
    /// JWT claims: `{"iss": "<team_id>", "iat": <timestamp>}`
    ///
    /// Signed with ES256 using the provided `.p8` key.
    fn build_jwt(&self) -> Result<String, PushError> {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.config.key_id.clone());
        header.typ = Some("JWT".to_string());

        let now = chrono::Utc::now().timestamp();
        let claims = ApnsJwtClaims {
            iss: self.config.team_id.clone(),
            iat: now,
        };

        let encoding_key =
            jsonwebtoken::EncodingKey::from_ec_pem(self.config.private_key_pem.as_bytes())
                .map_err(|e| {
                    PushError::SendFailed(format!("APNs private key parse failed: {e}"))
                })?;

        jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| PushError::SendFailed(format!("APNs JWT signing failed: {e}")))
    }

    /// Invalidate the cached JWT token (e.g., after a 403 response).
    fn invalidate_jwt_cache(&self) {
        if let Ok(mut cache) = self.cached_jwt.lock() {
            *cache = None;
        }
    }
}

/// JWT claims for APNs authentication.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ApnsJwtClaims {
    iss: String,
    iat: i64,
}

#[async_trait]
impl PushGateway for ApnsGateway {
    fn provider(&self) -> PushProvider {
        PushProvider::Apns
    }

    async fn send(&self, notification: &PushNotification) -> Result<PushResponse, PushError> {
        let token = notification
            .token
            .as_deref()
            .ok_or_else(|| PushError::InvalidToken("no push token set".into()))?;

        let jwt = self.get_jwt()?;
        let url = format!("{}/3/device/{}", self.base_url(), token);

        let mut aps = serde_json::json!({
            "alert": {
                "title": notification.title,
                "body": notification.body,
            },
        });

        if let Some(badge) = notification.badge {
            aps["badge"] = serde_json::json!(badge);
        }
        if let Some(ref sound) = notification.sound {
            aps["sound"] = serde_json::json!(sound);
        }
        if let Some(ref thread_id) = notification.thread_id {
            aps["thread-id"] = serde_json::json!(thread_id);
        }

        let mut payload = serde_json::json!({ "aps": aps });
        // Add custom data at the top level of the payload
        for (key, value) in &notification.data {
            payload[key] = serde_json::json!(value);
        }
        payload["category"] = serde_json::json!(notification.category.to_string());

        let priority = match notification.priority {
            crate::types::PushPriority::High => "10",
            crate::types::PushPriority::Normal => "5",
        };

        let response = self
            .client
            .post(&url)
            .header("authorization", format!("bearer {jwt}"))
            .header("apns-priority", priority)
            .header("apns-topic", &self.config.team_id)
            .header("apns-push-type", "alert")
            .json(&payload)
            .send()
            .await
            .map_err(|e| PushError::SendFailed(format!("APNs HTTP request failed: {e}")))?;

        let status = response.status();
        match status.as_u16() {
            200 => {
                let apns_id = response
                    .headers()
                    .get("apns-id")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                Ok(PushResponse {
                    success: true,
                    provider_message_id: apns_id,
                    error_reason: None,
                    token_invalid: false,
                })
            }
            403 => {
                // JWT might be expired or invalid -- invalidate cache for next attempt.
                self.invalidate_jwt_cache();
                let body = response.text().await.unwrap_or_default();
                let reason = parse_apns_error(&body);
                tracing::warn!(
                    provider = "apns",
                    status = 403,
                    reason = %reason,
                    "APNs delivery failed: forbidden"
                );
                Ok(PushResponse {
                    success: false,
                    provider_message_id: None,
                    error_reason: Some(reason),
                    token_invalid: false,
                })
            }
            410 => {
                let body = response.text().await.unwrap_or_default();
                let reason = parse_apns_error(&body);
                tracing::warn!(
                    provider = "apns",
                    status = 410,
                    reason = %reason,
                    "APNs token invalid (unregistered)"
                );
                Ok(PushResponse {
                    success: false,
                    provider_message_id: None,
                    error_reason: Some(reason),
                    token_invalid: true,
                })
            }
            429 => {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60);
                Err(PushError::RateLimited {
                    retry_after_secs: retry_after,
                })
            }
            400 => {
                let body = response.text().await.unwrap_or_default();
                let reason = parse_apns_error(&body);
                tracing::warn!(
                    provider = "apns",
                    status = 400,
                    reason = %reason,
                    "APNs bad request"
                );
                Ok(PushResponse {
                    success: false,
                    provider_message_id: None,
                    error_reason: Some(reason),
                    token_invalid: false,
                })
            }
            _ => {
                let body = response.text().await.unwrap_or_default();
                let reason = parse_apns_error(&body);
                tracing::warn!(
                    provider = "apns",
                    status = %status,
                    reason = %reason,
                    "APNs delivery failed"
                );
                Ok(PushResponse {
                    success: false,
                    provider_message_id: None,
                    error_reason: Some(reason),
                    token_invalid: false,
                })
            }
        }
    }
}

/// Parse the APNs error response body to extract the `reason` field.
fn parse_apns_error(body: &str) -> String {
    #[derive(Deserialize)]
    struct ApnsError {
        reason: Option<String>,
    }
    serde_json::from_str::<ApnsError>(body)
        .ok()
        .and_then(|e| e.reason)
        .unwrap_or_else(|| format!("unparseable response: {}", &body[..body.len().min(200)]))
}

// ---------------------------------------------------------------------------
// FCM Configuration
// ---------------------------------------------------------------------------

/// Configuration for the FCM gateway.
#[derive(Debug, Clone)]
pub struct FcmConfig {
    /// Firebase project ID.
    pub project_id: String,
    /// Service account email address (from the service account JSON).
    pub service_account_email: String,
    /// The RSA private key in PEM format (from the service account JSON).
    pub private_key_pem: String,
    /// Override the OAuth2 token endpoint URL (for testing with mock servers).
    pub token_url_override: Option<String>,
    /// Override the FCM API base URL (for testing with mock servers).
    pub api_url_override: Option<String>,
}

// ---------------------------------------------------------------------------
// FCM Gateway
// ---------------------------------------------------------------------------

/// Gateway for Firebase Cloud Messaging (FCM) HTTP v1 API.
///
/// Uses OAuth2 service account authentication. Access tokens are cached
/// and reused for up to 50 minutes (tokens are typically valid for 60 minutes).
pub struct FcmGateway {
    config: FcmConfig,
    /// HTTP client for making requests.
    client: reqwest::Client,
    /// Cached OAuth2 access token.
    cached_access_token: Mutex<Option<CachedToken>>,
}

impl std::fmt::Debug for FcmGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FcmGateway")
            .field("project_id", &self.config.project_id)
            .field("service_account_email", &self.config.service_account_email)
            .finish()
    }
}

impl FcmGateway {
    /// Create a new FCM gateway from configuration.
    pub fn new(config: FcmConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            cached_access_token: Mutex::new(None),
        }
    }

    /// Get or refresh the cached OAuth2 access token.
    ///
    /// FCM access tokens are typically valid for 3600 seconds (1 hour).
    /// We cache and reuse them, refreshing when less than 10 minutes remain.
    async fn get_access_token(&self) -> Result<String, PushError> {
        // Check cached token first.
        {
            let cache = self
                .cached_access_token
                .lock()
                .map_err(|_| PushError::SendFailed("FCM token cache lock poisoned".to_string()))?;
            if let Some(ref cached) = *cache {
                if cached.is_valid(600) {
                    return Ok(cached.token.clone());
                }
            }
        }

        // Exchange a signed JWT assertion for an access token.
        let access_token = self.exchange_jwt_for_token().await?;

        let mut cache = self
            .cached_access_token
            .lock()
            .map_err(|_| PushError::SendFailed("FCM token cache lock poisoned".to_string()))?;
        *cache = Some(CachedToken {
            token: access_token.clone(),
            expires_at: chrono::Utc::now().timestamp() + 3600,
        });

        Ok(access_token)
    }

    /// Build a signed JWT assertion and exchange it for an OAuth2 access token.
    ///
    /// Flow:
    /// 1. Build JWT with claims: `{"iss": service_email, "scope": "...", "aud": "https://oauth2.googleapis.com/token"}`
    /// 2. Sign with RS256 using the service account private key
    /// 3. POST to `https://oauth2.googleapis.com/token` with `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`
    /// 4. Extract the `access_token` from the response
    async fn exchange_jwt_for_token(&self) -> Result<String, PushError> {
        let now = chrono::Utc::now().timestamp();
        let claims = FcmJwtClaims {
            iss: self.config.service_account_email.clone(),
            scope: "https://www.googleapis.com/auth/firebase.cloud-messaging".to_string(),
            aud: "https://oauth2.googleapis.com/token".to_string(),
            iat: now,
            exp: now + 3600,
        };

        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let encoding_key =
            jsonwebtoken::EncodingKey::from_rsa_pem(self.config.private_key_pem.as_bytes())
                .map_err(|e| PushError::SendFailed(format!("FCM private key parse failed: {e}")))?;

        let assertion = jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| PushError::SendFailed(format!("FCM JWT signing failed: {e}")))?;

        let token_url = self
            .config
            .token_url_override
            .as_deref()
            .unwrap_or("https://oauth2.googleapis.com/token");

        let response = self
            .client
            .post(token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ])
            .send()
            .await
            .map_err(|e| {
                PushError::SendFailed(format!("FCM OAuth2 token exchange request failed: {e}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(PushError::SendFailed(format!(
                "FCM OAuth2 token exchange failed (HTTP {status}): {body}"
            )));
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .map_err(|e| PushError::SendFailed(format!("FCM OAuth2 response parse failed: {e}")))?;

        Ok(token_response.access_token)
    }

    /// Build the signed JWT assertion for OAuth2 (exposed for testing).
    #[cfg(test)]
    pub(crate) fn build_jwt_assertion(&self) -> Result<String, PushError> {
        let now = chrono::Utc::now().timestamp();
        let claims = FcmJwtClaims {
            iss: self.config.service_account_email.clone(),
            scope: "https://www.googleapis.com/auth/firebase.cloud-messaging".to_string(),
            aud: "https://oauth2.googleapis.com/token".to_string(),
            iat: now,
            exp: now + 3600,
        };

        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let encoding_key =
            jsonwebtoken::EncodingKey::from_rsa_pem(self.config.private_key_pem.as_bytes())
                .map_err(|e| PushError::SendFailed(format!("FCM private key parse failed: {e}")))?;

        jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| PushError::SendFailed(format!("FCM JWT signing failed: {e}")))
    }

    /// Invalidate the cached access token (e.g., after a 401 response).
    fn invalidate_token_cache(&self) {
        if let Ok(mut cache) = self.cached_access_token.lock() {
            *cache = None;
        }
    }
}

/// JWT claims for FCM OAuth2 service account authentication.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct FcmJwtClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: i64,
    exp: i64,
}

#[async_trait]
impl PushGateway for FcmGateway {
    fn provider(&self) -> PushProvider {
        PushProvider::Fcm
    }

    async fn send(&self, notification: &PushNotification) -> Result<PushResponse, PushError> {
        let token = notification
            .token
            .as_deref()
            .ok_or_else(|| PushError::InvalidToken("no push token set".into()))?;

        let access_token = self.get_access_token().await?;
        let api_base = self
            .config
            .api_url_override
            .as_deref()
            .unwrap_or("https://fcm.googleapis.com");
        let url = format!(
            "{}/v1/projects/{}/messages:send",
            api_base, self.config.project_id
        );

        let mut data = notification.data.clone();
        data.insert("category".to_string(), notification.category.to_string());

        let mut message = serde_json::json!({
            "message": {
                "token": token,
                "notification": {
                    "title": notification.title,
                    "body": notification.body,
                },
                "data": data,
            }
        });

        // Add Android-specific configuration
        let priority = match notification.priority {
            crate::types::PushPriority::High => "HIGH",
            crate::types::PushPriority::Normal => "NORMAL",
        };
        message["message"]["android"] = serde_json::json!({
            "priority": priority,
        });

        if let Some(ref thread_id) = notification.thread_id {
            message["message"]["android"]["collapseKey"] = serde_json::json!(thread_id);
        }

        let response = self
            .client
            .post(&url)
            .header("authorization", format!("Bearer {access_token}"))
            .json(&message)
            .send()
            .await
            .map_err(|e| PushError::SendFailed(format!("FCM HTTP request failed: {e}")))?;

        let status = response.status();
        match status.as_u16() {
            200 => {
                #[derive(Deserialize)]
                struct FcmSuccess {
                    name: Option<String>,
                }
                let body: FcmSuccess = response.json().await.map_err(|e| {
                    PushError::SendFailed(format!("FCM response parse failed: {e}"))
                })?;
                Ok(PushResponse {
                    success: true,
                    provider_message_id: body.name,
                    error_reason: None,
                    token_invalid: false,
                })
            }
            401 => {
                // Access token might be expired -- invalidate cache for next attempt.
                self.invalidate_token_cache();
                let body = response.text().await.unwrap_or_default();
                tracing::warn!(
                    provider = "fcm",
                    status = 401,
                    "FCM delivery failed: authentication error"
                );
                Ok(PushResponse {
                    success: false,
                    provider_message_id: None,
                    error_reason: Some(format!("authentication error: {body}")),
                    token_invalid: false,
                })
            }
            404 => {
                let body = response.text().await.unwrap_or_default();
                tracing::warn!(
                    provider = "fcm",
                    status = 404,
                    "FCM token invalid (not found)"
                );
                Ok(PushResponse {
                    success: false,
                    provider_message_id: None,
                    error_reason: Some(body),
                    token_invalid: true,
                })
            }
            429 => {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60);
                Err(PushError::RateLimited {
                    retry_after_secs: retry_after,
                })
            }
            400 => {
                let body = response.text().await.unwrap_or_default();
                tracing::warn!(provider = "fcm", status = 400, "FCM bad request");
                Ok(PushResponse {
                    success: false,
                    provider_message_id: None,
                    error_reason: Some(body),
                    token_invalid: false,
                })
            }
            _ => {
                let body = response.text().await.unwrap_or_default();
                tracing::warn!(
                    provider = "fcm",
                    status = %status,
                    "FCM delivery failed"
                );
                Ok(PushResponse {
                    success: false,
                    provider_message_id: None,
                    error_reason: Some(body),
                    token_invalid: false,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mock Gateway (for testing)
// ---------------------------------------------------------------------------

/// A mock push gateway for testing. Records sent notifications and returns
/// queued responses.
#[derive(Debug)]
pub struct MockGateway {
    provider: PushProvider,
    /// Queued responses to return in order. If empty, returns a default success.
    responses: std::sync::Mutex<Vec<Result<PushResponse, PushError>>>,
    /// All notifications that were sent through this gateway.
    sent: std::sync::Mutex<Vec<PushNotification>>,
}

impl MockGateway {
    /// Create a new mock gateway for the given provider.
    pub fn new(provider: PushProvider) -> Self {
        Self {
            provider,
            responses: std::sync::Mutex::new(Vec::new()),
            sent: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Queue a response to be returned on the next `send()` call.
    pub fn queue_response(&self, response: Result<PushResponse, PushError>) {
        self.responses.lock().expect("lock").push(response);
    }

    /// Get all notifications that were sent through this gateway.
    pub fn sent_notifications(&self) -> Vec<PushNotification> {
        self.sent.lock().expect("lock").clone()
    }

    /// Get the number of sent notifications.
    pub fn sent_count(&self) -> usize {
        self.sent.lock().expect("lock").len()
    }

    /// Clear sent notifications.
    pub fn clear_sent(&self) {
        self.sent.lock().expect("lock").clear();
    }
}

#[async_trait]
impl PushGateway for MockGateway {
    fn provider(&self) -> PushProvider {
        self.provider
    }

    async fn send(&self, notification: &PushNotification) -> Result<PushResponse, PushError> {
        self.sent.lock().expect("lock").push(notification.clone());

        let mut responses = self.responses.lock().expect("lock");
        if responses.is_empty() {
            // Default: return success
            Ok(PushResponse {
                success: true,
                provider_message_id: Some("mock-id".to_string()),
                error_reason: None,
                token_invalid: false,
            })
        } else {
            responses.remove(0)
        }
    }
}

// ---------------------------------------------------------------------------
// Test key generation helpers
// ---------------------------------------------------------------------------

/// Generate a test ES256 (P-256 / prime256v1) private key in PKCS#8 PEM format.
///
/// This uses the `jsonwebtoken` crate's underlying ring dependency to
/// generate a key pair suitable for ES256 signing. Used only in tests.
#[cfg(test)]
pub(crate) fn generate_es256_test_key() -> String {
    use base64::Engine;
    // Generate a P-256 key pair using ring (which jsonwebtoken depends on).
    let rng = ring::rand::SystemRandom::new();
    let pkcs8_doc = ring::signature::EcdsaKeyPair::generate_pkcs8(
        &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
        &rng,
    )
    .expect("key generation should succeed");

    let b64 = base64::engine::general_purpose::STANDARD.encode(pkcs8_doc.as_ref());
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("valid utf8"));
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    pem
}

/// Generate a test RSA 2048-bit private key in PKCS#1 PEM format.
///
/// Uses the `rsa` crate for key generation. Used only in tests.
#[cfg(test)]
pub(crate) fn generate_rsa_test_key() -> String {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::RsaPrivateKey;

    let mut rng = rand_core_06::OsRng;
    let private_key =
        RsaPrivateKey::new(&mut rng, 2048).expect("RSA key generation should succeed");
    private_key
        .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
        .expect("PEM encoding should succeed")
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_apns_config() -> ApnsConfig {
        ApnsConfig {
            team_id: "TEAMID1234".to_string(),
            key_id: "KEYID12345".to_string(),
            private_key_pem: generate_es256_test_key(),
            sandbox: true,
            base_url_override: None,
        }
    }

    fn test_fcm_config() -> FcmConfig {
        FcmConfig {
            project_id: "test-project-123".to_string(),
            service_account_email: "test@test-project-123.iam.gserviceaccount.com".to_string(),
            private_key_pem: generate_rsa_test_key(),
            token_url_override: None,
            api_url_override: None,
        }
    }

    // -----------------------------------------------------------------------
    // APNs JWT signing
    // -----------------------------------------------------------------------

    #[test]
    fn test_apns_jwt_generation() {
        use base64::Engine;
        let config = test_apns_config();
        let gateway = ApnsGateway::new(config.clone());
        let jwt = gateway.build_jwt().expect("JWT generation should succeed");

        // Verify the JWT structure (three dot-separated parts).
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT should have 3 parts");

        // Decode and verify the header.
        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[0])
            .expect("header should be valid base64url");
        let header: serde_json::Value =
            serde_json::from_slice(&header_bytes).expect("header should be valid JSON");
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], config.key_id);
        assert_eq!(header["typ"], "JWT");

        // Decode and verify the claims.
        let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .expect("claims should be valid base64url");
        let claims: serde_json::Value =
            serde_json::from_slice(&claims_bytes).expect("claims should be valid JSON");
        assert_eq!(claims["iss"], config.team_id);
        assert!(claims["iat"].is_number(), "iat should be a number");
    }

    #[test]
    fn test_apns_jwt_with_invalid_key() {
        let config = ApnsConfig {
            team_id: "TEAMID1234".to_string(),
            key_id: "KEYID12345".to_string(),
            private_key_pem: "not-a-valid-pem-key".to_string(),
            sandbox: true,
            base_url_override: None,
        };
        let gateway = ApnsGateway::new(config);
        let result = gateway.build_jwt();
        assert!(result.is_err(), "should fail with invalid PEM key");
        let err = result.unwrap_err();
        assert!(
            matches!(err, PushError::SendFailed(_)),
            "should be SendFailed error"
        );
    }

    // -----------------------------------------------------------------------
    // APNs JWT caching
    // -----------------------------------------------------------------------

    #[test]
    fn test_apns_jwt_caching() {
        let config = test_apns_config();
        let gateway = ApnsGateway::new(config);

        // First call should generate a fresh JWT.
        let jwt1 = gateway.get_jwt().expect("first JWT");
        // Second call should return the cached JWT.
        let jwt2 = gateway.get_jwt().expect("second JWT");

        // Both should be the same token (cached).
        assert_eq!(jwt1, jwt2, "cached JWT should be returned");
    }

    #[test]
    fn test_apns_jwt_cache_invalidation() {
        let config = test_apns_config();
        let gateway = ApnsGateway::new(config);

        let _jwt1 = gateway.get_jwt().expect("first JWT");

        // Verify cache is populated.
        {
            let cache = gateway.cached_jwt.lock().expect("lock");
            assert!(cache.is_some(), "cache should be populated");
        }

        gateway.invalidate_jwt_cache();

        // Verify cache is cleared.
        {
            let cache = gateway.cached_jwt.lock().expect("lock");
            assert!(
                cache.is_none(),
                "cache should be cleared after invalidation"
            );
        }

        // Get JWT again -- should work and repopulate cache.
        let _jwt2 = gateway.get_jwt().expect("JWT after invalidation");
        {
            let cache = gateway.cached_jwt.lock().expect("lock");
            assert!(cache.is_some(), "cache should be repopulated");
        }
    }

    #[test]
    fn test_apns_jwt_cache_expiry_triggers_refresh() {
        let config = test_apns_config();
        let gateway = ApnsGateway::new(config);

        // Manually set an almost-expired cached token.
        {
            let mut cache = gateway.cached_jwt.lock().expect("lock");
            *cache = Some(CachedToken {
                token: "old-token".to_string(),
                // Expires in 5 minutes -- less than 10-minute margin.
                expires_at: chrono::Utc::now().timestamp() + 300,
            });
        }

        // get_jwt should generate a fresh token, not return the expired one.
        let jwt = gateway.get_jwt().expect("should generate fresh JWT");
        assert_ne!(jwt, "old-token", "should not return expired cached token");
    }

    // -----------------------------------------------------------------------
    // FCM JWT generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_fcm_jwt_generation() {
        use base64::Engine;
        let config = test_fcm_config();
        let gateway = FcmGateway::new(config.clone());
        let jwt = gateway
            .build_jwt_assertion()
            .expect("FCM JWT should succeed");

        // Verify structure.
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT should have 3 parts");

        // Decode and verify header.
        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[0])
            .expect("header should be valid base64url");
        let header: serde_json::Value =
            serde_json::from_slice(&header_bytes).expect("header should be valid JSON");
        assert_eq!(header["alg"], "RS256");

        // Decode and verify claims.
        let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .expect("claims should be valid base64url");
        let decoded_claims: serde_json::Value =
            serde_json::from_slice(&claims_bytes).expect("claims should be valid JSON");
        assert_eq!(decoded_claims["iss"], config.service_account_email);
        assert_eq!(
            decoded_claims["scope"],
            "https://www.googleapis.com/auth/firebase.cloud-messaging"
        );
        assert_eq!(decoded_claims["aud"], "https://oauth2.googleapis.com/token");
        assert!(decoded_claims["iat"].is_number());
        assert!(decoded_claims["exp"].is_number());
    }

    #[test]
    fn test_fcm_jwt_with_invalid_key() {
        let config = FcmConfig {
            project_id: "test-project".to_string(),
            service_account_email: "test@test.iam.gserviceaccount.com".to_string(),
            private_key_pem: "not-a-valid-rsa-key".to_string(),
            token_url_override: None,
            api_url_override: None,
        };
        let gateway = FcmGateway::new(config);
        let result = gateway.build_jwt_assertion();
        assert!(result.is_err(), "should fail with invalid RSA key");
    }

    // -----------------------------------------------------------------------
    // FCM token caching
    // -----------------------------------------------------------------------

    #[test]
    fn test_fcm_token_cache_invalidation() {
        let config = test_fcm_config();
        let gateway = FcmGateway::new(config);

        // Pre-populate cache.
        {
            let mut cache = gateway.cached_access_token.lock().expect("lock");
            *cache = Some(CachedToken {
                token: "test-access-token".to_string(),
                expires_at: chrono::Utc::now().timestamp() + 3600,
            });
        }

        // Verify cache is populated.
        {
            let cache = gateway.cached_access_token.lock().expect("lock");
            assert!(cache.is_some());
        }

        // Invalidate.
        gateway.invalidate_token_cache();

        // Verify cache is cleared.
        {
            let cache = gateway.cached_access_token.lock().expect("lock");
            assert!(cache.is_none());
        }
    }

    // -----------------------------------------------------------------------
    // CachedToken validity
    // -----------------------------------------------------------------------

    #[test]
    fn test_cached_token_validity() {
        let valid_token = CachedToken {
            token: "valid".to_string(),
            expires_at: chrono::Utc::now().timestamp() + 3600,
        };
        assert!(
            valid_token.is_valid(600),
            "token with 1h remaining should be valid with 10min margin"
        );

        let expiring_token = CachedToken {
            token: "expiring".to_string(),
            expires_at: chrono::Utc::now().timestamp() + 300,
        };
        assert!(
            !expiring_token.is_valid(600),
            "token with 5min remaining should NOT be valid with 10min margin"
        );

        let expired_token = CachedToken {
            token: "expired".to_string(),
            expires_at: chrono::Utc::now().timestamp() - 100,
        };
        assert!(
            !expired_token.is_valid(600),
            "expired token should not be valid"
        );

        let exact_margin_token = CachedToken {
            token: "exact".to_string(),
            expires_at: chrono::Utc::now().timestamp() + 600,
        };
        assert!(
            !exact_margin_token.is_valid(600),
            "token expiring exactly at margin should not be valid (strict >)"
        );
    }

    // -----------------------------------------------------------------------
    // APNs response parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_apns_error_valid_json() {
        let body = r#"{"reason":"BadDeviceToken"}"#;
        assert_eq!(parse_apns_error(body), "BadDeviceToken");
    }

    #[test]
    fn test_parse_apns_error_missing_reason() {
        let body = r#"{"error":"something"}"#;
        let result = parse_apns_error(body);
        assert!(result.starts_with("unparseable response:"));
    }

    #[test]
    fn test_parse_apns_error_invalid_json() {
        let body = "not json at all";
        let result = parse_apns_error(body);
        assert!(result.starts_with("unparseable response:"));
    }

    #[test]
    fn test_parse_apns_error_empty() {
        let body = "";
        let result = parse_apns_error(body);
        assert!(result.starts_with("unparseable response:"));
    }

    // -----------------------------------------------------------------------
    // Gateway base URLs
    // -----------------------------------------------------------------------

    #[test]
    fn test_apns_sandbox_url() {
        let config = ApnsConfig {
            team_id: "T".to_string(),
            key_id: "K".to_string(),
            private_key_pem: "P".to_string(),
            sandbox: true,
            base_url_override: None,
        };
        let gateway = ApnsGateway::new(config);
        assert_eq!(gateway.base_url(), "https://api.sandbox.push.apple.com");
    }

    #[test]
    fn test_apns_production_url() {
        let config = ApnsConfig {
            team_id: "T".to_string(),
            key_id: "K".to_string(),
            private_key_pem: "P".to_string(),
            sandbox: false,
            base_url_override: None,
        };
        let gateway = ApnsGateway::new(config);
        assert_eq!(gateway.base_url(), "https://api.push.apple.com");
    }

    // -----------------------------------------------------------------------
    // MockGateway
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_mock_gateway_provider() {
        let mock = MockGateway::new(PushProvider::Apns);
        assert_eq!(mock.provider(), PushProvider::Apns);

        let mock = MockGateway::new(PushProvider::Fcm);
        assert_eq!(mock.provider(), PushProvider::Fcm);
    }

    // -----------------------------------------------------------------------
    // Integration test: MockGateway full dispatch path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_mock_gateway_full_dispatch_path() {
        use crate::dispatch::{DispatcherConfig, NotificationDispatcher};
        use crate::store::PushTokenStore;
        use crate::types::{NotificationCategory, PushToken};

        let store = PushTokenStore::in_memory().expect("store");

        // Register tokens for two devices.
        store
            .register_token(&PushToken {
                device_id: "iphone-1".to_string(),
                platform: PushProvider::Apns,
                token: "apns-tok-1".to_string(),
                user_id: "user-1".to_string(),
                registered_at: chrono::Utc::now(),
                expires_at: None,
            })
            .expect("register");
        store
            .register_token(&PushToken {
                device_id: "pixel-1".to_string(),
                platform: PushProvider::Fcm,
                token: "fcm-tok-1".to_string(),
                user_id: "user-1".to_string(),
                registered_at: chrono::Utc::now(),
                expires_at: None,
            })
            .expect("register");

        let apns_mock = MockGateway::new(PushProvider::Apns);
        let fcm_mock = MockGateway::new(PushProvider::Fcm);

        let mut dispatcher = NotificationDispatcher::new(DispatcherConfig::default());
        dispatcher.register_gateway(Box::new(apns_mock));
        dispatcher.register_gateway(Box::new(fcm_mock));

        let result = dispatcher
            .dispatch(&store, "user-1", |tok| {
                PushNotification::new(
                    "Test Alert",
                    "Something happened",
                    NotificationCategory::System,
                )
                .with_token(tok)
            })
            .await
            .expect("dispatch");

        assert_eq!(result.total, 2);
        assert_eq!(result.succeeded, 2);
        assert_eq!(result.failed, 0);
        assert!(result.invalidated_tokens.is_empty());
    }
}
