//! Mock APNs server for integration testing.
//!
//! Simulates the Apple Push Notification service HTTP/2 endpoint. Validates
//! JWT auth headers, accepts push payloads, and returns configurable responses.

use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A recorded push request received by the mock APNs server.
#[derive(Debug, Clone)]
pub struct RecordedApnsPush {
    /// The device token from the URL path.
    pub device_token: String,
    /// The raw JSON payload body.
    pub payload: serde_json::Value,
    /// The APNs priority header value.
    pub priority: Option<String>,
    /// The APNs topic header value.
    pub topic: Option<String>,
    /// The APNs push type header value.
    pub push_type: Option<String>,
    /// Whether a bearer token was present in the Authorization header.
    pub has_auth: bool,
}

/// Tracks pushes received by the mock and configures responses.
#[derive(Debug, Clone)]
pub struct ApnsMockState {
    received: Arc<Mutex<Vec<RecordedApnsPush>>>,
}

impl Default for ApnsMockState {
    fn default() -> Self {
        Self::new()
    }
}

impl ApnsMockState {
    /// Create a new empty state tracker.
    pub fn new() -> Self {
        Self {
            received: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Get all recorded push requests.
    pub fn received_pushes(&self) -> Vec<RecordedApnsPush> {
        self.received.lock().expect("lock").clone()
    }

    /// Get count of received pushes.
    pub fn push_count(&self) -> usize {
        self.received.lock().expect("lock").len()
    }

    /// Clear all recorded pushes.
    pub fn clear(&self) {
        self.received.lock().expect("lock").clear();
    }

    fn record(&self, push: RecordedApnsPush) {
        self.received.lock().expect("lock").push(push);
    }
}

/// A responder that records APNs requests and returns a fixed response.
struct ApnsRecordingResponder {
    state: ApnsMockState,
    template: ResponseTemplate,
}

impl Respond for ApnsRecordingResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.state.record(parse_apns_request(req));
        self.template.clone()
    }
}

/// A responder that validates JWT auth and records requests.
struct ApnsAuthValidatingResponder {
    state: ApnsMockState,
}

impl Respond for ApnsAuthValidatingResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.state.record(parse_apns_request(req));

        let has_valid_auth = req
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|auth| {
                auth.starts_with("bearer ")
                    && auth.trim_start_matches("bearer ").split('.').count() == 3
            })
            .unwrap_or(false);

        if has_valid_auth {
            ResponseTemplate::new(200)
        } else {
            ResponseTemplate::new(403)
                .set_body_json(serde_json::json!({"reason": "MissingProviderToken"}))
        }
    }
}

/// Start a mock APNs server that accepts all pushes with 200 OK.
pub async fn start_mock_apns_success() -> (MockServer, ApnsMockState) {
    let server = MockServer::start().await;
    let state = ApnsMockState::new();

    Mock::given(method("POST"))
        .and(path_regex(r"/3/device/[a-fA-F0-9]+"))
        .respond_with(ApnsRecordingResponder {
            state: state.clone(),
            template: ResponseTemplate::new(200),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock APNs server that rejects all pushes with 403 Forbidden.
pub async fn start_mock_apns_forbidden() -> (MockServer, ApnsMockState) {
    let server = MockServer::start().await;
    let state = ApnsMockState::new();

    Mock::given(method("POST"))
        .and(path_regex(r"/3/device/[a-fA-F0-9]+"))
        .respond_with(ApnsRecordingResponder {
            state: state.clone(),
            template: ResponseTemplate::new(403)
                .set_body_json(serde_json::json!({"reason": "ExpiredProviderToken"})),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock APNs server that returns 410 Gone for invalid tokens.
pub async fn start_mock_apns_gone() -> (MockServer, ApnsMockState) {
    let server = MockServer::start().await;
    let state = ApnsMockState::new();

    Mock::given(method("POST"))
        .and(path_regex(r"/3/device/[a-fA-F0-9]+"))
        .respond_with(ApnsRecordingResponder {
            state: state.clone(),
            template: ResponseTemplate::new(410)
                .set_body_json(serde_json::json!({"reason": "Unregistered"})),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock APNs server that returns 429 Too Many Requests.
pub async fn start_mock_apns_rate_limited() -> (MockServer, ApnsMockState) {
    let server = MockServer::start().await;
    let state = ApnsMockState::new();

    Mock::given(method("POST"))
        .and(path_regex(r"/3/device/[a-fA-F0-9]+"))
        .respond_with(ApnsRecordingResponder {
            state: state.clone(),
            template: ResponseTemplate::new(429).append_header("retry-after", "5"),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock APNs server that returns 400 Bad Request.
pub async fn start_mock_apns_bad_request() -> (MockServer, ApnsMockState) {
    let server = MockServer::start().await;
    let state = ApnsMockState::new();

    Mock::given(method("POST"))
        .and(path_regex(r"/3/device/[a-fA-F0-9]+"))
        .respond_with(ApnsRecordingResponder {
            state: state.clone(),
            template: ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"reason": "BadDeviceToken"})),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock APNs server that validates the JWT authorization header
/// is present and well-formed. Returns 200 if valid, 403 if missing/malformed.
pub async fn start_mock_apns_auth_validating() -> (MockServer, ApnsMockState) {
    let server = MockServer::start().await;
    let state = ApnsMockState::new();

    Mock::given(method("POST"))
        .and(path_regex(r"/3/device/[a-fA-F0-9]+"))
        .respond_with(ApnsAuthValidatingResponder {
            state: state.clone(),
        })
        .mount(&server)
        .await;

    (server, state)
}

fn parse_apns_request(req: &Request) -> RecordedApnsPush {
    let device_token = req
        .url
        .path()
        .strip_prefix("/3/device/")
        .unwrap_or("")
        .to_string();

    let payload: serde_json::Value =
        serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);

    let priority = req
        .headers
        .get("apns-priority")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let topic = req
        .headers
        .get("apns-topic")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let push_type = req
        .headers
        .get("apns-push-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let has_auth = req
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|a| a.starts_with("bearer "))
        .unwrap_or(false);

    RecordedApnsPush {
        device_token,
        payload,
        priority,
        topic,
        push_type,
        has_auth,
    }
}
