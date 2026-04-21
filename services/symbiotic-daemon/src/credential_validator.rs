//! Per-provider API credential validation.
//!
//! Each known credential key (e.g. `ANTHROPIC_API_KEY`) has a lightweight
//! validation check that hits the provider's API with a minimal read-only
//! request.  Unknown keys return `Skipped`.
//!
//! Uses blocking reqwest because `SymbioticDaemon` is not `Send` (contains
//! `Rc<TrustStore>`), so we cannot spawn async tasks that borrow `self`.
//!
//! The blocking HTTP client is created on demand (not held as a field) to
//! avoid panics from dropping a nested tokio runtime when the daemon is
//! destroyed inside an async test context.

use std::time::Duration;

/// Result of validating a credential against its provider API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialValidationResult {
    /// The credential was accepted by the provider API.
    Valid,
    /// The provider API rejected the credential (e.g. 401/403).
    Invalid { reason: String },
    /// The provider API could not be reached (network error, timeout).
    Unreachable { reason: String },
    /// No validator exists for this credential key.
    Skipped,
}

/// Trait for credential validation, enabling test stubs.
///
/// The daemon holds a `Box<dyn ValidateCredential>` so tests can inject
/// a stub that returns deterministic results without making HTTP calls.
///
/// Note: intentionally NOT `Send + Sync` — the daemon is `!Send`
/// (contains `Rc<TrustStore>`).
pub trait ValidateCredential {
    fn validate(&self, key: &str, value: &str) -> CredentialValidationResult;
}

/// Validates API credentials against their provider endpoints.
///
/// The HTTP client is created on demand to avoid holding a nested tokio
/// runtime that would panic on drop inside async contexts.
pub struct CredentialValidator {
    timeout: Duration,
}

impl CredentialValidator {
    /// Create a new validator with the given HTTP timeout.
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Build a blocking HTTP client on demand.
    fn client(&self) -> Result<reqwest::blocking::Client, String> {
        reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| format!("failed to create HTTP client: {e}"))
    }
}

impl CredentialValidator {
    /// Anthropic uses a custom `x-api-key` header instead of Bearer auth.
    fn validate_anthropic(&self, api_key: &str) -> CredentialValidationResult {
        let client = match self.client() {
            Ok(c) => c,
            Err(reason) => return CredentialValidationResult::Unreachable { reason },
        };
        let result = client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send();

        match result {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    CredentialValidationResult::Valid
                } else {
                    CredentialValidationResult::Invalid {
                        reason: format!("API returned {status}"),
                    }
                }
            }
            Err(e) => CredentialValidationResult::Unreachable {
                reason: format!("request failed: {e}"),
            },
        }
    }

    /// Gemini uses an API key as a query parameter, not a header.
    fn validate_gemini(&self, api_key: &str) -> CredentialValidationResult {
        let client = match self.client() {
            Ok(c) => c,
            Err(reason) => return CredentialValidationResult::Unreachable { reason },
        };
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models?key={}",
            api_key
        );
        let result = client.get(&url).send();

        match result {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    CredentialValidationResult::Valid
                } else {
                    CredentialValidationResult::Invalid {
                        reason: format!("API returned {status}"),
                    }
                }
            }
            Err(e) => CredentialValidationResult::Unreachable {
                reason: format!("request failed: {e}"),
            },
        }
    }

    /// Generic Bearer token validation: GET the URL with `Authorization: Bearer <token>`.
    fn validate_bearer(&self, url: &str, token: &str) -> CredentialValidationResult {
        let client = match self.client() {
            Ok(c) => c,
            Err(reason) => return CredentialValidationResult::Unreachable { reason },
        };
        let result = client
            .get(url)
            .header("Authorization", format!("Bearer {token}"))
            .send();

        match result {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    CredentialValidationResult::Valid
                } else {
                    CredentialValidationResult::Invalid {
                        reason: format!("API returned {status}"),
                    }
                }
            }
            Err(e) => CredentialValidationResult::Unreachable {
                reason: format!("request failed: {e}"),
            },
        }
    }
}

impl ValidateCredential for CredentialValidator {
    fn validate(&self, key: &str, value: &str) -> CredentialValidationResult {
        match key {
            "ANTHROPIC_API_KEY" => self.validate_anthropic(value),
            "OPENAI_API_KEY" => self.validate_bearer("https://api.openai.com/v1/models", value),
            "GEMINI_API_KEY" => self.validate_gemini(value),
            "OPENROUTER_API_KEY" => {
                self.validate_bearer("https://openrouter.ai/api/v1/models", value)
            }
            "SYMBIOTIC_HCLOUD_TOKEN" => {
                self.validate_bearer("https://api.hetzner.cloud/v1/servers", value)
            }
            "SYMBIOTIC_CF_TUNNEL_TOKEN" => self.validate_bearer(
                "https://api.cloudflare.com/client/v4/user/tokens/verify",
                value,
            ),
            // APNs and FCM require complex JWT signing -- skip for now.
            "APNS_TEAM_ID" | "APNS_KEY_ID" | "APNS_PRIVATE_KEY" | "FCM_SERVICE_ACCOUNT_JSON" => {
                CredentialValidationResult::Skipped
            }
            // X/Twitter credentials need an OAuth2 PKCE flow -- skip.
            "SYMBIOTIC_X_CLIENT_ID" | "SYMBIOTIC_X_CLIENT_SECRET" => {
                CredentialValidationResult::Skipped
            }
            _ => CredentialValidationResult::Skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_key_returns_skipped() {
        let validator = CredentialValidator::new(Duration::from_secs(5));
        let result = validator.validate("UNKNOWN_KEY", "some-value");
        assert_eq!(result, CredentialValidationResult::Skipped);
    }

    #[test]
    fn apns_keys_return_skipped() {
        let validator = CredentialValidator::new(Duration::from_secs(5));
        for key in &[
            "APNS_TEAM_ID",
            "APNS_KEY_ID",
            "APNS_PRIVATE_KEY",
            "FCM_SERVICE_ACCOUNT_JSON",
        ] {
            let result = validator.validate(key, "some-value");
            assert_eq!(result, CredentialValidationResult::Skipped);
        }
    }

    #[test]
    fn x_credentials_return_skipped() {
        let validator = CredentialValidator::new(Duration::from_secs(5));
        for key in &["SYMBIOTIC_X_CLIENT_ID", "SYMBIOTIC_X_CLIENT_SECRET"] {
            let result = validator.validate(key, "some-value");
            assert_eq!(result, CredentialValidationResult::Skipped);
        }
    }
}
