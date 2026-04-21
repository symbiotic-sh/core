//! OAuth2 PKCE flow implementation for the credential gateway.
//!
//! Provides a generic `OAuthProvider` trait and a concrete GitHub implementation.
//! Tokens are stored encrypted in the vault via the existing ChaCha20-Poly1305 AEAD.

use anyhow::{Context, Result};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use std::collections::HashMap;
use std::sync::Mutex;
use symbiotic_core::now_unix;

/// Concrete client type with auth URL and token URL set (both required for PKCE flow).
type ConfiguredClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::CredentialVault;

/// Errors specific to OAuth operations.
#[derive(Debug, Error)]
pub enum OAuthError {
    #[error("no OAuth token stored for provider: {0}")]
    NoToken(String),
    #[error("token expired for provider: {0}")]
    TokenExpired(String),
    #[error("token revoked for provider: {0}")]
    TokenRevoked(String),
    #[error("no refresh token available for provider: {0}")]
    NoRefreshToken(String),
    #[error("PKCE state mismatch")]
    StateMismatch,
    #[error("OAuth exchange failed: {0}")]
    ExchangeFailed(String),
}

/// An OAuth2 token stored in the vault.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    pub provider: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub scopes: Vec<String>,
    pub issued_at: u64,
    pub expires_at: Option<u64>,
    pub revoked: bool,
}

impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field("provider", &self.provider)
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("token_type", &self.token_type)
            .field("scopes", &self.scopes)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .field("revoked", &self.revoked)
            .finish()
    }
}

impl OAuthToken {
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|exp| exp <= now)
    }

    pub fn needs_refresh(&self, now: u64, buffer_secs: u64) -> bool {
        self.expires_at.is_some_and(|exp| exp <= now + buffer_secs)
    }
}

/// Authorization URL result from starting an OAuth2 PKCE flow.
pub struct AuthorizationRequest {
    pub authorize_url: String,
    pub csrf_token: String,
    pub pkce_verifier: String,
}

impl std::fmt::Debug for AuthorizationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationRequest")
            .field("authorize_url", &self.authorize_url)
            .field("csrf_token", &self.csrf_token)
            .field("pkce_verifier", &"[REDACTED]")
            .finish()
    }
}

/// Configuration for an OAuth2 provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProviderConfig {
    pub provider_name: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub auth_url: String,
    pub token_url: String,
    pub redirect_url: String,
    pub scopes: Vec<String>,
}

/// Trait for OAuth2 providers.
///
/// Each provider implementation handles the specifics of its OAuth2 flow
/// while the credential gateway manages token storage and lifecycle.
#[async_trait::async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Returns the provider name (e.g., "github").
    fn provider_name(&self) -> &str;

    /// Generate an authorization URL with PKCE challenge.
    fn authorize_url(&self) -> Result<AuthorizationRequest>;

    /// Exchange an authorization code for tokens.
    async fn exchange_code(&self, code: &str, pkce_verifier: &str) -> Result<OAuthToken>;

    /// Refresh an expired access token.
    async fn refresh_token(&self, refresh_token: &str) -> Result<OAuthToken>;

    /// Revoke a token (best-effort; not all providers support this).
    async fn revoke_token(&self, token: &str) -> Result<()>;
}

/// Manages OAuth tokens: storage in the vault, refresh lifecycle, revocation.
pub struct OAuthTokenManager {
    vault: std::sync::Arc<dyn CredentialVault>,
    /// In-memory cache of pending PKCE verifiers keyed by CSRF token.
    pending_verifiers: Mutex<HashMap<String, String>>,
    /// Buffer in seconds before expiry to trigger refresh.
    refresh_buffer_secs: u64,
}

impl OAuthTokenManager {
    pub fn new(vault: std::sync::Arc<dyn CredentialVault>, refresh_buffer_secs: u64) -> Self {
        Self {
            vault,
            pending_verifiers: Mutex::new(HashMap::new()),
            refresh_buffer_secs,
        }
    }

    /// Start an OAuth2 flow: generate authorization URL and store the PKCE verifier.
    pub fn start_flow(&self, provider: &dyn OAuthProvider) -> Result<AuthorizationRequest> {
        let auth_req = provider.authorize_url()?;
        self.pending_verifiers
            .lock()
            .map_err(|_| anyhow::anyhow!("pending verifiers lock poisoned"))?
            .insert(auth_req.csrf_token.clone(), auth_req.pkce_verifier.clone());
        Ok(auth_req)
    }

    /// Complete an OAuth2 flow: exchange the code and store the token.
    pub async fn complete_flow(
        &self,
        provider: &dyn OAuthProvider,
        code: &str,
        state: &str,
    ) -> Result<OAuthToken> {
        let verifier = self
            .pending_verifiers
            .lock()
            .map_err(|_| anyhow::anyhow!("pending verifiers lock poisoned"))?
            .remove(state)
            .ok_or(OAuthError::StateMismatch)?;

        let token = provider.exchange_code(code, &verifier).await?;
        self.store_token(&token)?;
        Ok(token)
    }

    /// Get a valid token, refreshing if needed.
    pub async fn get_valid_token(
        &self,
        provider: &dyn OAuthProvider,
        now: u64,
    ) -> Result<OAuthToken> {
        let token = self.load_token(provider.provider_name())?;

        if token.revoked {
            return Err(OAuthError::TokenRevoked(provider.provider_name().to_string()).into());
        }

        if token.needs_refresh(now, self.refresh_buffer_secs) {
            let refresh = token
                .refresh_token
                .as_deref()
                .ok_or_else(|| OAuthError::NoRefreshToken(provider.provider_name().to_string()))?;
            let new_token = provider.refresh_token(refresh).await?;
            self.store_token(&new_token)?;
            return Ok(new_token);
        }

        if token.is_expired(now) {
            return Err(OAuthError::TokenExpired(provider.provider_name().to_string()).into());
        }

        Ok(token)
    }

    /// Revoke a token and mark it as revoked in the vault.
    pub async fn revoke(&self, provider: &dyn OAuthProvider) -> Result<()> {
        let mut token = self.load_token(provider.provider_name())?;
        provider.revoke_token(&token.access_token).await?;
        token.revoked = true;
        self.store_token(&token)?;
        Ok(())
    }

    fn store_token(&self, token: &OAuthToken) -> Result<()> {
        let key = oauth_vault_key(&token.provider);
        let value = serde_json::to_string(token).context("failed to serialize OAuth token")?;
        self.vault.put(crate::CredentialRecord {
            service: key,
            username: token.provider.clone(),
            secret: value,
            totp_secret: None,
        })
    }

    fn load_token(&self, provider_name: &str) -> Result<OAuthToken> {
        let key = oauth_vault_key(provider_name);
        let record = self
            .vault
            .get(&key)?
            .ok_or_else(|| OAuthError::NoToken(provider_name.to_string()))?;
        serde_json::from_str(&record.secret).context("failed to deserialize OAuth token")
    }
}

fn oauth_vault_key(provider: &str) -> String {
    format!("oauth:{provider}")
}

/// GitHub OAuth2 provider using PKCE.
pub struct GitHubOAuthProvider {
    client: ConfiguredClient,
    scopes: Vec<String>,
    http_client: reqwest::Client,
}

impl GitHubOAuthProvider {
    pub fn new(config: &OAuthProviderConfig) -> Result<Self> {
        let client = BasicClient::new(ClientId::new(config.client_id.clone()))
            .set_auth_uri(AuthUrl::new(config.auth_url.clone()).context("invalid auth URL")?)
            .set_token_uri(TokenUrl::new(config.token_url.clone()).context("invalid token URL")?)
            .set_redirect_uri(
                RedirectUrl::new(config.redirect_url.clone()).context("invalid redirect URL")?,
            );

        let client = if let Some(ref secret) = config.client_secret {
            client.set_client_secret(ClientSecret::new(secret.clone()))
        } else {
            client
        };

        Ok(Self {
            client,
            scopes: config.scopes.clone(),
            http_client: reqwest::Client::new(),
        })
    }

    /// Create a provider pointing at custom URLs (for testing with mock servers).
    pub fn with_urls(
        client_id: &str,
        client_secret: Option<&str>,
        auth_url: &str,
        token_url: &str,
        redirect_url: &str,
    ) -> Result<Self> {
        let config = OAuthProviderConfig {
            provider_name: "github".to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.map(String::from),
            auth_url: auth_url.to_string(),
            token_url: token_url.to_string(),
            redirect_url: redirect_url.to_string(),
            scopes: vec!["read:user".to_string()],
        };
        Self::new(&config)
    }
}

#[async_trait::async_trait]
impl OAuthProvider for GitHubOAuthProvider {
    fn provider_name(&self) -> &str {
        "github"
    }

    fn authorize_url(&self) -> Result<AuthorizationRequest> {
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let mut auth_request = self.client.authorize_url(CsrfToken::new_random);

        for scope in &self.scopes {
            auth_request = auth_request.add_scope(Scope::new(scope.clone()));
        }

        let (url, csrf_token) = auth_request.set_pkce_challenge(pkce_challenge).url();

        Ok(AuthorizationRequest {
            authorize_url: url.to_string(),
            csrf_token: csrf_token.secret().clone(),
            pkce_verifier: pkce_verifier.secret().clone(),
        })
    }

    async fn exchange_code(&self, code: &str, pkce_verifier: &str) -> Result<OAuthToken> {
        let verifier = PkceCodeVerifier::new(pkce_verifier.to_string());
        let token_result = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .set_pkce_verifier(verifier)
            .request_async(&self.http_client)
            .await
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;

        let now = now_unix();
        let expires_at = token_result.expires_in().map(|d| now + d.as_secs());

        Ok(OAuthToken {
            provider: "github".to_string(),
            access_token: token_result.access_token().secret().clone(),
            refresh_token: token_result.refresh_token().map(|rt| rt.secret().clone()),
            token_type: "bearer".to_string(),
            scopes: token_result
                .scopes()
                .map(|s| s.iter().map(|sc| sc.to_string()).collect())
                .unwrap_or_default(),
            issued_at: now,
            expires_at,
            revoked: false,
        })
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<OAuthToken> {
        let token_result = self
            .client
            .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
            .request_async(&self.http_client)
            .await
            .map_err(|e| OAuthError::ExchangeFailed(e.to_string()))?;

        let now = now_unix();
        let expires_at = token_result.expires_in().map(|d| now + d.as_secs());

        Ok(OAuthToken {
            provider: "github".to_string(),
            access_token: token_result.access_token().secret().clone(),
            refresh_token: token_result.refresh_token().map(|rt| rt.secret().clone()),
            token_type: "bearer".to_string(),
            scopes: token_result
                .scopes()
                .map(|s| s.iter().map(|sc| sc.to_string()).collect())
                .unwrap_or_default(),
            issued_at: now,
            expires_at,
            revoked: false,
        })
    }

    async fn revoke_token(&self, _token: &str) -> Result<()> {
        // GitHub does not support standard OAuth2 token revocation.
        // Token deletion must be done via the GitHub API (DELETE /applications/{client_id}/token).
        // This is a no-op for now; callers should use the GitHub API directly for revocation.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileCredentialVault;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn temp_vault(name: &str) -> Arc<FileCredentialVault> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "oauth_test_{name}_{}_{}_{}",
            now_unix(),
            std::process::id(),
            id
        ));
        Arc::new(FileCredentialVault::open(root.join("vault.tsv")).expect("vault"))
    }

    #[test]
    fn oauth_token_expiry_detection() {
        let token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_abc".to_string(),
            refresh_token: Some("ghr_xyz".to_string()),
            token_type: "bearer".to_string(),
            scopes: vec!["read:user".to_string()],
            issued_at: 1000,
            expires_at: Some(2000),
            revoked: false,
        };

        assert!(!token.is_expired(1500));
        assert!(token.is_expired(2000));
        assert!(token.is_expired(2001));

        assert!(!token.needs_refresh(1500, 300));
        assert!(token.needs_refresh(1701, 300));
        assert!(token.needs_refresh(2000, 300));
    }

    #[test]
    fn oauth_token_no_expiry_never_expired() {
        let token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_abc".to_string(),
            refresh_token: None,
            token_type: "bearer".to_string(),
            scopes: vec![],
            issued_at: 1000,
            expires_at: None,
            revoked: false,
        };

        assert!(!token.is_expired(999_999_999));
        assert!(!token.needs_refresh(999_999_999, 300));
    }

    #[test]
    fn token_manager_stores_and_loads_tokens() {
        let vault = temp_vault("store_load");
        let mgr = OAuthTokenManager::new(vault, 300);

        let token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_test123".to_string(),
            refresh_token: Some("ghr_refresh456".to_string()),
            token_type: "bearer".to_string(),
            scopes: vec!["read:user".to_string(), "repo".to_string()],
            issued_at: 1000,
            expires_at: Some(4600),
            revoked: false,
        };

        mgr.store_token(&token).expect("store");
        let loaded = mgr.load_token("github").expect("load");

        assert_eq!(loaded.access_token, "gho_test123");
        assert_eq!(loaded.refresh_token.as_deref(), Some("ghr_refresh456"));
        assert_eq!(loaded.scopes, vec!["read:user", "repo"]);
        assert_eq!(loaded.expires_at, Some(4600));
        assert!(!loaded.revoked);
    }

    #[test]
    fn token_manager_load_missing_returns_error() {
        let vault = temp_vault("missing");
        let mgr = OAuthTokenManager::new(vault, 300);
        let err = mgr.load_token("nonexistent").unwrap_err();
        assert!(err.to_string().contains("no OAuth token stored"));
    }

    #[test]
    fn token_manager_stores_revoked_state() {
        let vault = temp_vault("revoked");
        let mgr = OAuthTokenManager::new(vault, 300);

        let mut token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_to_revoke".to_string(),
            refresh_token: None,
            token_type: "bearer".to_string(),
            scopes: vec![],
            issued_at: 1000,
            expires_at: None,
            revoked: false,
        };

        mgr.store_token(&token).expect("store");
        token.revoked = true;
        mgr.store_token(&token).expect("store revoked");

        let loaded = mgr.load_token("github").expect("load");
        assert!(loaded.revoked);
    }

    #[test]
    fn github_provider_generates_authorization_url() {
        let provider = GitHubOAuthProvider::with_urls(
            "test_client_id",
            Some("test_secret"),
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let auth_req = provider.authorize_url().expect("auth url");

        assert!(auth_req
            .authorize_url
            .contains("github.com/login/oauth/authorize"));
        assert!(auth_req.authorize_url.contains("client_id=test_client_id"));
        assert!(auth_req.authorize_url.contains("code_challenge="));
        assert!(auth_req
            .authorize_url
            .contains("code_challenge_method=S256"));
        assert!(!auth_req.csrf_token.is_empty());
        assert!(!auth_req.pkce_verifier.is_empty());
    }

    #[test]
    fn token_manager_start_flow_stores_verifier() {
        let vault = temp_vault("flow_start");
        let mgr = OAuthTokenManager::new(vault, 300);

        let provider = GitHubOAuthProvider::with_urls(
            "test_id",
            None,
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let auth_req = mgr.start_flow(&provider).expect("start_flow");

        let verifiers = mgr.pending_verifiers.lock().unwrap();
        assert!(verifiers.contains_key(&auth_req.csrf_token));
        assert_eq!(verifiers[&auth_req.csrf_token], auth_req.pkce_verifier);
    }

    #[tokio::test]
    async fn token_manager_complete_flow_rejects_bad_state() {
        let vault = temp_vault("bad_state");
        let mgr = OAuthTokenManager::new(vault, 300);

        let provider = GitHubOAuthProvider::with_urls(
            "test_id",
            None,
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let err = mgr
            .complete_flow(&provider, "some_code", "invalid_state")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("PKCE state mismatch"));
    }

    #[tokio::test]
    async fn token_manager_get_valid_token_returns_error_when_revoked() {
        let vault = temp_vault("revoked_check");
        let mgr = OAuthTokenManager::new(vault, 300);

        let token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_revoked".to_string(),
            refresh_token: None,
            token_type: "bearer".to_string(),
            scopes: vec![],
            issued_at: 1000,
            expires_at: None,
            revoked: true,
        };
        mgr.store_token(&token).expect("store");

        let provider = GitHubOAuthProvider::with_urls(
            "test_id",
            None,
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let err = mgr.get_valid_token(&provider, 1500).await.unwrap_err();
        assert!(err.to_string().contains("revoked"));
    }

    #[tokio::test]
    async fn token_manager_get_valid_token_errors_when_expired_no_refresh() {
        let vault = temp_vault("expired_no_refresh");
        let mgr = OAuthTokenManager::new(vault, 300);

        let token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_expired".to_string(),
            refresh_token: None,
            token_type: "bearer".to_string(),
            scopes: vec![],
            issued_at: 1000,
            expires_at: Some(1500),
            revoked: false,
        };
        mgr.store_token(&token).expect("store");

        let provider = GitHubOAuthProvider::with_urls(
            "test_id",
            None,
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        // Token expires at 1500, buffer is 300, so at 1201+ it would try refresh
        // but no refresh token available
        let err = mgr.get_valid_token(&provider, 1201).await.unwrap_err();
        assert!(err.to_string().contains("no refresh token"));
    }

    #[tokio::test]
    async fn token_manager_get_valid_token_returns_non_expired() {
        let vault = temp_vault("valid_token");
        let mgr = OAuthTokenManager::new(vault, 300);

        let token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_valid".to_string(),
            refresh_token: None,
            token_type: "bearer".to_string(),
            scopes: vec!["read:user".to_string()],
            issued_at: 1000,
            expires_at: Some(5000),
            revoked: false,
        };
        mgr.store_token(&token).expect("store");

        let provider = GitHubOAuthProvider::with_urls(
            "test_id",
            None,
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let result = mgr.get_valid_token(&provider, 2000).await.expect("valid");
        assert_eq!(result.access_token, "gho_valid");
    }

    #[test]
    fn oauth_vault_key_format() {
        assert_eq!(oauth_vault_key("github"), "oauth:github");
        assert_eq!(oauth_vault_key("google"), "oauth:google");
    }
}

/// Integration tests that require network access (wiremock binds to local ports).
/// Run with: `cargo test -p credential-gateway --features integration`
#[cfg(all(test, feature = "integration"))]
mod integration_tests {
    use super::*;
    use crate::FileCredentialVault;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn temp_vault(name: &str) -> Arc<FileCredentialVault> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "oauth_test_{name}_{}_{}_{}",
            now_unix(),
            std::process::id(),
            id
        ));
        Arc::new(FileCredentialVault::open(root.join("vault.tsv")).expect("vault"))
    }

    #[tokio::test]
    async fn wiremock_exchange_code_returns_token() {
        let mock_server = MockServer::start().await;
        let token_body = serde_json::json!({
            "access_token": "gho_mock_access",
            "token_type": "bearer",
            "scope": "read:user",
            "expires_in": 3600,
            "refresh_token": "ghr_mock_refresh"
        });

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&token_body)
                    .insert_header("content-type", "application/json"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = GitHubOAuthProvider::with_urls(
            "mock_client_id",
            Some("mock_secret"),
            &format!("{}/login/oauth/authorize", mock_server.uri()),
            &format!("{}/login/oauth/access_token", mock_server.uri()),
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let auth_req = provider.authorize_url().expect("auth url");
        let token = provider
            .exchange_code("mock_auth_code", &auth_req.pkce_verifier)
            .await
            .expect("exchange should succeed with mock server");

        assert_eq!(token.access_token, "gho_mock_access");
        assert_eq!(token.refresh_token.as_deref(), Some("ghr_mock_refresh"));
        assert_eq!(token.token_type, "bearer");
        assert!(token.expires_at.is_some());
        assert!(!token.revoked);
    }

    #[tokio::test]
    async fn wiremock_refresh_token_returns_new_token() {
        let mock_server = MockServer::start().await;
        let refresh_body = serde_json::json!({
            "access_token": "gho_refreshed",
            "token_type": "bearer",
            "scope": "read:user repo",
            "expires_in": 7200
        });

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&refresh_body)
                    .insert_header("content-type", "application/json"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = GitHubOAuthProvider::with_urls(
            "mock_client_id",
            Some("mock_secret"),
            &format!("{}/login/oauth/authorize", mock_server.uri()),
            &format!("{}/login/oauth/access_token", mock_server.uri()),
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let token = provider
            .refresh_token("ghr_old_refresh")
            .await
            .expect("refresh should succeed");

        assert_eq!(token.access_token, "gho_refreshed");
        assert!(token.expires_at.is_some());
    }

    #[tokio::test]
    async fn wiremock_exchange_code_handles_error_response() {
        let mock_server = MockServer::start().await;
        let error_body = serde_json::json!({
            "error": "bad_verification_code",
            "error_description": "The code has expired."
        });

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&error_body)
                    .insert_header("content-type", "application/json"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let provider = GitHubOAuthProvider::with_urls(
            "mock_client_id",
            Some("mock_secret"),
            &format!("{}/login/oauth/authorize", mock_server.uri()),
            &format!("{}/login/oauth/access_token", mock_server.uri()),
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let auth_req = provider.authorize_url().expect("auth url");
        let err = provider
            .exchange_code("expired_code", &auth_req.pkce_verifier)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("OAuth exchange failed"));
    }

    #[tokio::test]
    async fn wiremock_full_flow_via_token_manager() {
        let mock_server = MockServer::start().await;
        let token_body = serde_json::json!({
            "access_token": "gho_full_flow",
            "token_type": "bearer",
            "scope": "read:user",
            "expires_in": 3600,
            "refresh_token": "ghr_full_flow_refresh"
        });

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&token_body)
                    .insert_header("content-type", "application/json"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let vault = temp_vault("wiremock_full_flow");
        let mgr = OAuthTokenManager::new(vault, 300);

        let provider = GitHubOAuthProvider::with_urls(
            "mock_client_id",
            Some("mock_secret"),
            &format!("{}/login/oauth/authorize", mock_server.uri()),
            &format!("{}/login/oauth/access_token", mock_server.uri()),
            "http://localhost:8080/callback",
        )
        .expect("provider");

        // Start flow: get auth URL and store verifier
        let auth_req = mgr.start_flow(&provider).expect("start_flow");
        assert!(auth_req.authorize_url.contains("mock_client_id"));

        // Complete flow: exchange code (using the stored CSRF state)
        let token = mgr
            .complete_flow(&provider, "mock_code", &auth_req.csrf_token)
            .await
            .expect("complete_flow");

        assert_eq!(token.access_token, "gho_full_flow");
        assert_eq!(
            token.refresh_token.as_deref(),
            Some("ghr_full_flow_refresh")
        );

        // Token is now stored in vault; get_valid_token should return it
        let valid = mgr
            .get_valid_token(&provider, now_unix())
            .await
            .expect("get_valid_token");
        assert_eq!(valid.access_token, "gho_full_flow");
    }

    #[tokio::test]
    async fn wiremock_token_manager_refresh_on_near_expiry() {
        let mock_server = MockServer::start().await;
        let refresh_body = serde_json::json!({
            "access_token": "gho_auto_refreshed",
            "token_type": "bearer",
            "expires_in": 7200,
            "refresh_token": "ghr_new_refresh"
        });

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&refresh_body)
                    .insert_header("content-type", "application/json"),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let vault = temp_vault("wiremock_refresh");
        let mgr = OAuthTokenManager::new(vault, 300);

        // Pre-store a token that is about to expire
        let expiring_token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_about_to_expire".to_string(),
            refresh_token: Some("ghr_old".to_string()),
            token_type: "bearer".to_string(),
            scopes: vec!["read:user".to_string()],
            issued_at: 1000,
            expires_at: Some(1400), // expires at 1400
            revoked: false,
        };
        mgr.store_token(&expiring_token).expect("store");

        let provider = GitHubOAuthProvider::with_urls(
            "mock_client_id",
            Some("mock_secret"),
            &format!("{}/login/oauth/authorize", mock_server.uri()),
            &format!("{}/login/oauth/access_token", mock_server.uri()),
            "http://localhost:8080/callback",
        )
        .expect("provider");

        // At time 1101, token needs refresh (1400 - 300 = 1100 threshold)
        let refreshed = mgr
            .get_valid_token(&provider, 1101)
            .await
            .expect("auto-refresh");
        assert_eq!(refreshed.access_token, "gho_auto_refreshed");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("ghr_new_refresh"));

        // Verify the new token was persisted
        let loaded = mgr.load_token("github").expect("load");
        assert_eq!(loaded.access_token, "gho_auto_refreshed");
    }

    #[tokio::test]
    async fn wiremock_token_manager_revoke_marks_stored_token() {
        let vault = temp_vault("wiremock_revoke");
        let mgr = OAuthTokenManager::new(vault, 300);

        let token = OAuthToken {
            provider: "github".to_string(),
            access_token: "gho_to_be_revoked".to_string(),
            refresh_token: None,
            token_type: "bearer".to_string(),
            scopes: vec![],
            issued_at: 1000,
            expires_at: None,
            revoked: false,
        };
        mgr.store_token(&token).expect("store");

        let provider = GitHubOAuthProvider::with_urls(
            "mock_client_id",
            None,
            "https://github.com/login/oauth/authorize",
            "https://github.com/login/oauth/access_token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        // Revoke (GitHub revoke is no-op, but manager still marks token as revoked)
        mgr.revoke(&provider).await.expect("revoke");

        let loaded = mgr.load_token("github").expect("load");
        assert!(loaded.revoked);

        // Now get_valid_token should fail
        let err = mgr.get_valid_token(&provider, 2000).await.unwrap_err();
        assert!(err.to_string().contains("revoked"));
    }

    #[tokio::test]
    async fn wiremock_exchange_with_network_failure() {
        // Use a non-existent server to simulate network failure
        let provider = GitHubOAuthProvider::with_urls(
            "mock_client_id",
            Some("mock_secret"),
            "http://127.0.0.1:1/authorize",
            "http://127.0.0.1:1/token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let auth_req = provider.authorize_url().expect("auth url");
        let err = provider
            .exchange_code("code", &auth_req.pkce_verifier)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("OAuth exchange failed"));
    }
}
