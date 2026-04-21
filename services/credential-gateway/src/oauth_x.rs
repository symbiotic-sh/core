//! X (Twitter) OAuth2 PKCE provider for the credential gateway.
//!
//! Implements the `OAuthProvider` trait for X's OAuth2 flow using PKCE with S256.
//! Tokens are stored encrypted in the vault via the existing ChaCha20-Poly1305 AEAD.

use anyhow::{Context, Result};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use symbiotic_core::now_unix;

use crate::oauth::{
    AuthorizationRequest, OAuthError, OAuthProvider, OAuthProviderConfig, OAuthToken,
};

/// Concrete client type with auth URL and token URL set (both required for PKCE flow).
type ConfiguredClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// X OAuth2 provider using PKCE with S256 challenge method.
///
/// X's OAuth2 flow requires:
/// - Auth URL: `https://twitter.com/i/oauth2/authorize`
/// - Token URL: `https://api.twitter.com/2/oauth2/token`
/// - Scopes: `tweet.read users.read bookmark.read offline.access`
/// - PKCE with S256 challenge method (no client secret required for public clients)
pub struct XOAuthProvider {
    client: ConfiguredClient,
    scopes: Vec<String>,
    http_client: reqwest::Client,
}

/// Default scopes for X API access (bookmarks, tweets, user info, offline refresh).
pub const X_DEFAULT_SCOPES: &[&str] = &[
    "tweet.read",
    "users.read",
    "bookmark.read",
    "offline.access",
];

/// Token format matching what `load_x_access_token_from_vault()` expects in x_intake.rs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XTokenFile {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl XOAuthProvider {
    pub fn new(config: &OAuthProviderConfig) -> Result<Self> {
        let client = BasicClient::new(ClientId::new(config.client_id.clone()))
            .set_auth_uri(AuthUrl::new(config.auth_url.clone()).context("invalid auth URL")?)
            .set_token_uri(TokenUrl::new(config.token_url.clone()).context("invalid token URL")?)
            .set_redirect_uri(
                RedirectUrl::new(config.redirect_url.clone()).context("invalid redirect URL")?,
            );

        // X public clients use PKCE only, no client secret needed.
        // The oauth2 crate handles this correctly when no secret is set.

        Ok(Self {
            client,
            scopes: config.scopes.clone(),
            http_client: reqwest::Client::new(),
        })
    }

    /// Create a provider with default X OAuth2 URLs.
    pub fn with_defaults(client_id: &str, redirect_url: &str) -> Result<Self> {
        let config = OAuthProviderConfig {
            provider_name: "x".to_string(),
            client_id: client_id.to_string(),
            client_secret: None,
            auth_url: "https://twitter.com/i/oauth2/authorize".to_string(),
            token_url: "https://api.twitter.com/2/oauth2/token".to_string(),
            redirect_url: redirect_url.to_string(),
            scopes: X_DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
        };
        Self::new(&config)
    }

    /// Create a provider pointing at custom URLs (for testing with mock servers).
    pub fn with_urls(
        client_id: &str,
        auth_url: &str,
        token_url: &str,
        redirect_url: &str,
    ) -> Result<Self> {
        let config = OAuthProviderConfig {
            provider_name: "x".to_string(),
            client_id: client_id.to_string(),
            client_secret: None,
            auth_url: auth_url.to_string(),
            token_url: token_url.to_string(),
            redirect_url: redirect_url.to_string(),
            scopes: X_DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
        };
        Self::new(&config)
    }

    /// Serialize token for the daemon's `load_x_access_token_from_vault()`.
    ///
    /// Returns JSON: `{"access_token":"...","refresh_token":"..."}`.
    pub fn serialize_for_vault(token: &OAuthToken) -> Result<String> {
        let file = XTokenFile {
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
        };
        serde_json::to_string(&file).context("failed to serialize X token for vault")
    }
}

#[async_trait::async_trait]
impl OAuthProvider for XOAuthProvider {
    fn provider_name(&self) -> &str {
        "x"
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
            provider: "x".to_string(),
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
            provider: "x".to_string(),
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
        // X supports token revocation via POST https://api.twitter.com/2/oauth2/revoke
        // with body: token={token}&token_type_hint=access_token&client_id={client_id}
        // For now this is a no-op; callers should revoke via the X API directly.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x_provider_generates_authorization_url() {
        let provider = XOAuthProvider::with_urls(
            "test_x_client_id",
            "https://twitter.com/i/oauth2/authorize",
            "https://api.twitter.com/2/oauth2/token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let auth_req = provider.authorize_url().expect("auth url");

        assert!(auth_req
            .authorize_url
            .contains("twitter.com/i/oauth2/authorize"));
        assert!(auth_req
            .authorize_url
            .contains("client_id=test_x_client_id"));
        assert!(auth_req.authorize_url.contains("code_challenge="));
        assert!(auth_req
            .authorize_url
            .contains("code_challenge_method=S256"));
        assert!(!auth_req.csrf_token.is_empty());
        assert!(!auth_req.pkce_verifier.is_empty());
    }

    #[test]
    fn x_provider_includes_required_scopes() {
        let provider = XOAuthProvider::with_urls(
            "test_x_client_id",
            "https://twitter.com/i/oauth2/authorize",
            "https://api.twitter.com/2/oauth2/token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let auth_req = provider.authorize_url().expect("auth url");
        let url = &auth_req.authorize_url;

        // URL-encoded scopes should be present
        assert!(url.contains("tweet.read"), "missing tweet.read scope");
        assert!(url.contains("users.read"), "missing users.read scope");
        assert!(url.contains("bookmark.read"), "missing bookmark.read scope");
        assert!(
            url.contains("offline.access"),
            "missing offline.access scope"
        );
    }

    #[test]
    fn x_provider_name() {
        let provider = XOAuthProvider::with_urls(
            "test",
            "https://twitter.com/i/oauth2/authorize",
            "https://api.twitter.com/2/oauth2/token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        assert_eq!(provider.provider_name(), "x");
    }

    #[test]
    fn x_provider_with_defaults() {
        let provider = XOAuthProvider::with_defaults("my_client_id", "http://localhost:8080/cb")
            .expect("provider");

        let auth_req = provider.authorize_url().expect("auth url");
        assert!(auth_req
            .authorize_url
            .contains("twitter.com/i/oauth2/authorize"));
        assert!(auth_req.authorize_url.contains("client_id=my_client_id"));
    }

    #[test]
    fn pkce_verifier_is_nonempty_and_unique() {
        let provider = XOAuthProvider::with_urls(
            "test",
            "https://twitter.com/i/oauth2/authorize",
            "https://api.twitter.com/2/oauth2/token",
            "http://localhost:8080/callback",
        )
        .expect("provider");

        let req1 = provider.authorize_url().expect("auth url 1");
        let req2 = provider.authorize_url().expect("auth url 2");

        assert!(!req1.pkce_verifier.is_empty());
        assert!(!req2.pkce_verifier.is_empty());
        // Each call should produce a different verifier
        assert_ne!(req1.pkce_verifier, req2.pkce_verifier);
        // And different CSRF tokens
        assert_ne!(req1.csrf_token, req2.csrf_token);
    }

    #[test]
    fn serialize_for_vault_matches_x_intake_format() {
        let token = OAuthToken {
            provider: "x".to_string(),
            access_token: "test_access_token_123".to_string(),
            refresh_token: Some("test_refresh_token_456".to_string()),
            token_type: "bearer".to_string(),
            scopes: vec!["tweet.read".to_string()],
            issued_at: 1000,
            expires_at: Some(8200),
            revoked: false,
        };

        let serialized = XOAuthProvider::serialize_for_vault(&token).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&serialized).expect("valid JSON");

        assert_eq!(
            parsed["access_token"].as_str(),
            Some("test_access_token_123")
        );
        assert_eq!(
            parsed["refresh_token"].as_str(),
            Some("test_refresh_token_456")
        );

        // Verify it deserializes with the same struct used by x_intake
        #[derive(Deserialize)]
        struct XOAuthTokenFile {
            access_token: String,
        }
        let file: XOAuthTokenFile = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(file.access_token, "test_access_token_123");
    }

    #[test]
    fn serialize_for_vault_without_refresh_token() {
        let token = OAuthToken {
            provider: "x".to_string(),
            access_token: "access_only".to_string(),
            refresh_token: None,
            token_type: "bearer".to_string(),
            scopes: vec![],
            issued_at: 1000,
            expires_at: None,
            revoked: false,
        };

        let serialized = XOAuthProvider::serialize_for_vault(&token).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&serialized).expect("valid JSON");

        assert_eq!(parsed["access_token"].as_str(), Some("access_only"));
        // refresh_token should be omitted (skip_serializing_if = None)
        assert!(parsed.get("refresh_token").is_none());
    }

    #[test]
    fn x_default_scopes_are_correct() {
        assert_eq!(
            X_DEFAULT_SCOPES,
            &[
                "tweet.read",
                "users.read",
                "bookmark.read",
                "offline.access"
            ]
        );
    }
}
