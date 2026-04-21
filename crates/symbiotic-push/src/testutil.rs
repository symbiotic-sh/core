//! Test infrastructure for push notification integration tests.
//!
//! Provides mock APNs and FCM servers that simulate real provider behavior,
//! plus helpers for creating test device tokens, payloads, and gateway configs.
//!
//! # Real Credential Helpers
//!
//! Use [`setup_real_apns_config`] and [`setup_real_fcm_config`] to build
//! gateway configs from environment variables. Pair with [`skip_unless_creds`]
//! to gate tests on credential availability.

pub mod mock_apns;
pub mod mock_fcm;

use crate::gateway::{ApnsConfig, FcmConfig};

// ---------------------------------------------------------------------------
// Credential-gating macro
// ---------------------------------------------------------------------------

/// Skip the current test (via early return) unless **all** of the named
/// environment variables are set to non-empty values.
///
/// Use inside `#[ignore]` tests so that `cargo test -- --ignored` still
/// skips gracefully when credentials are absent.
///
/// ```rust,ignore
/// #[tokio::test]
/// #[ignore]
/// async fn real_apns_test() {
///     symbiotic_push::skip_unless_creds!(
///         "APNS_TEST_TEAM_ID",
///         "APNS_TEST_KEY_ID",
///         "APNS_TEST_KEY",
///         "APNS_TEST_DEVICE_TOKEN"
///     );
///     // ... rest of test
/// }
/// ```
#[macro_export]
macro_rules! skip_unless_creds {
    ($($var:expr),+ $(,)?) => {{
        let mut missing = Vec::new();
        $(
            match ::std::env::var($var) {
                Ok(ref v) if !v.trim().is_empty() => {}
                _ => missing.push($var),
            }
        )+
        if !missing.is_empty() {
            eprintln!(
                "SKIP: missing credential env vars: {}",
                missing.join(", ")
            );
            return;
        }
    }};
}

// ---------------------------------------------------------------------------
// Test helper functions
// ---------------------------------------------------------------------------

/// Generate a fake APNs device token (64 hex characters).
pub fn fake_apns_device_token() -> String {
    use std::fmt::Write;
    let bytes: Vec<u8> = (0..32).map(|i| (i * 7 + 13) as u8).collect();
    let mut hex = String::with_capacity(64);
    for b in bytes {
        write!(hex, "{b:02x}").expect("hex write");
    }
    hex
}

/// Generate a fake FCM registration token.
pub fn fake_fcm_registration_token() -> String {
    "fMh7yK3dT9q:APA91bH_test_fake_fcm_token_0123456789abcdef".to_string()
}

/// Create an APNs config pointing at a local mock server.
pub fn mock_apns_config(mock_url: &str) -> ApnsConfig {
    ApnsConfig {
        team_id: "TEAM000001".to_string(),
        key_id: "KEY0000001".to_string(),
        private_key_pem: generate_es256_test_key(),
        sandbox: true,
        base_url_override: Some(mock_url.to_string()),
    }
}

/// Create an FCM config pointing at a local mock server.
///
/// The `mock_url` is used as both the OAuth2 token endpoint base and
/// the FCM API base (the mock server hosts both).
pub fn mock_fcm_config(mock_url: &str) -> FcmConfig {
    FcmConfig {
        project_id: "test-project-push".to_string(),
        service_account_email: "push-test@test-project-push.iam.gserviceaccount.com".to_string(),
        private_key_pem: generate_rsa_test_key(),
        token_url_override: Some(format!("{mock_url}/token")),
        api_url_override: Some(mock_url.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Real credential config builders
// ---------------------------------------------------------------------------

/// Build an [`ApnsConfig`] from environment variables for real APNs testing.
///
/// Reads:
/// - `APNS_TEST_TEAM_ID` — 10-character Apple Developer Team ID
/// - `APNS_TEST_KEY_ID` — Key ID from App Store Connect
/// - `APNS_TEST_KEY` — Raw `.p8` private key content (PEM)
///
/// The `sandbox` flag controls whether to use the APNs sandbox or production
/// environment. Most testing should use `sandbox = true`.
///
/// # Panics
///
/// Panics if any required env var is missing. Call [`skip_unless_creds!`] first
/// to skip the test gracefully when credentials are absent.
pub fn setup_real_apns_config(sandbox: bool) -> ApnsConfig {
    ApnsConfig {
        team_id: std::env::var("APNS_TEST_TEAM_ID").expect("APNS_TEST_TEAM_ID must be set"),
        key_id: std::env::var("APNS_TEST_KEY_ID").expect("APNS_TEST_KEY_ID must be set"),
        private_key_pem: std::env::var("APNS_TEST_KEY").expect("APNS_TEST_KEY must be set"),
        sandbox,
        base_url_override: None,
    }
}

/// Build an [`FcmConfig`] from environment variables for real FCM testing.
///
/// Reads `FCM_TEST_CREDENTIALS` which must be a JSON string containing at
/// minimum `project_id`, `client_email`, and `private_key` fields (the
/// standard Google Cloud service account JSON key format).
///
/// # Panics
///
/// Panics if the env var is missing or the JSON is malformed. Call
/// [`skip_unless_creds!`] first.
pub fn setup_real_fcm_config() -> FcmConfig {
    let creds_json =
        std::env::var("FCM_TEST_CREDENTIALS").expect("FCM_TEST_CREDENTIALS must be set");

    #[derive(serde::Deserialize)]
    struct ServiceAccount {
        project_id: String,
        client_email: String,
        private_key: String,
    }

    let sa: ServiceAccount =
        serde_json::from_str(&creds_json).expect("FCM_TEST_CREDENTIALS must be valid JSON");

    FcmConfig {
        project_id: sa.project_id,
        service_account_email: sa.client_email,
        private_key_pem: sa.private_key,
        token_url_override: None,
        api_url_override: None,
    }
}

/// Read the APNs device token from the `APNS_TEST_DEVICE_TOKEN` env var.
///
/// # Panics
///
/// Panics if the env var is missing.
pub fn real_apns_device_token() -> String {
    std::env::var("APNS_TEST_DEVICE_TOKEN").expect("APNS_TEST_DEVICE_TOKEN must be set")
}

/// Read the FCM registration token from the `FCM_TEST_DEVICE_TOKEN` env var.
///
/// # Panics
///
/// Panics if the env var is missing.
pub fn real_fcm_device_token() -> String {
    std::env::var("FCM_TEST_DEVICE_TOKEN").expect("FCM_TEST_DEVICE_TOKEN must be set")
}

/// Wait briefly for push delivery confirmation.
///
/// This is a best-effort helper. Push notifications are inherently
/// asynchronous and there is no server-side delivery receipt API
/// exposed by APNs or FCM. This helper simply asserts that the
/// gateway accepted the notification (i.e., the `PushResponse` indicates
/// success and provides a `provider_message_id`).
///
/// For true end-to-end delivery verification, a device-side confirmation
/// mechanism (e.g., silent push + ack callback) would be needed.
pub fn assert_push_accepted(response: &crate::types::PushResponse) {
    assert!(
        response.success,
        "push should be accepted by provider: error={:?}",
        response.error_reason
    );
    assert!(
        response.provider_message_id.is_some(),
        "accepted push should have a provider_message_id"
    );
}

// ---------------------------------------------------------------------------
// Key generation helpers
// ---------------------------------------------------------------------------

/// Generate a test ES256 (P-256) private key in PKCS#8 PEM format.
pub fn generate_es256_test_key() -> String {
    use base64::Engine;
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
pub fn generate_rsa_test_key() -> String {
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
