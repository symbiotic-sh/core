//! Mock FCM server for integration testing.
//!
//! Simulates the Firebase Cloud Messaging HTTP v1 API and the Google OAuth2
//! token exchange endpoint. Validates OAuth2 bearer tokens, accepts push
//! payloads, and returns configurable responses.

use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A recorded push request received by the mock FCM server.
#[derive(Debug, Clone)]
pub struct RecordedFcmPush {
    /// The project ID from the URL path.
    pub project_id: String,
    /// The raw JSON message body.
    pub payload: serde_json::Value,
    /// Whether a bearer token was present in the Authorization header.
    pub has_auth: bool,
}

/// A recorded OAuth2 token exchange request.
#[derive(Debug, Clone)]
pub struct RecordedTokenExchange {
    /// The grant_type parameter.
    pub grant_type: String,
    /// Whether an assertion JWT was present.
    pub has_assertion: bool,
}

/// Tracks pushes and token exchanges received by the mock.
#[derive(Debug, Clone)]
pub struct FcmMockState {
    pushes: Arc<Mutex<Vec<RecordedFcmPush>>>,
    token_exchanges: Arc<Mutex<Vec<RecordedTokenExchange>>>,
}

impl Default for FcmMockState {
    fn default() -> Self {
        Self::new()
    }
}

impl FcmMockState {
    /// Create a new empty state tracker.
    pub fn new() -> Self {
        Self {
            pushes: Arc::new(Mutex::new(Vec::new())),
            token_exchanges: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Get all recorded push requests.
    pub fn received_pushes(&self) -> Vec<RecordedFcmPush> {
        self.pushes.lock().expect("lock").clone()
    }

    /// Get count of received pushes.
    pub fn push_count(&self) -> usize {
        self.pushes.lock().expect("lock").len()
    }

    /// Get all recorded token exchange requests.
    pub fn token_exchanges(&self) -> Vec<RecordedTokenExchange> {
        self.token_exchanges.lock().expect("lock").clone()
    }

    /// Clear all recorded state.
    pub fn clear(&self) {
        self.pushes.lock().expect("lock").clear();
        self.token_exchanges.lock().expect("lock").clear();
    }

    fn record_push(&self, push: RecordedFcmPush) {
        self.pushes.lock().expect("lock").push(push);
    }

    fn record_exchange(&self, exchange: RecordedTokenExchange) {
        self.token_exchanges.lock().expect("lock").push(exchange);
    }
}

/// A responder that records OAuth2 token exchanges and returns a fixed access token.
struct TokenExchangeResponder {
    state: FcmMockState,
    template: ResponseTemplate,
}

impl Respond for TokenExchangeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.state.record_exchange(parse_token_exchange(req));
        self.template.clone()
    }
}

/// A responder that records FCM push requests and returns a fixed response.
struct FcmPushResponder {
    state: FcmMockState,
    template: ResponseTemplate,
}

impl Respond for FcmPushResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.state.record_push(parse_fcm_request(req));
        self.template.clone()
    }
}

fn success_token_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": "mock-access-token-12345",
        "token_type": "Bearer",
        "expires_in": 3600
    }))
}

/// Start a mock FCM server that:
/// - Returns a fake access token on OAuth2 token exchange
/// - Accepts all push messages with 200 OK
pub async fn start_mock_fcm_success() -> (MockServer, FcmMockState) {
    let server = MockServer::start().await;
    let state = FcmMockState::new();

    // OAuth2 token exchange endpoint
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(TokenExchangeResponder {
            state: state.clone(),
            template: success_token_response(),
        })
        .mount(&server)
        .await;

    // FCM v1 send endpoint
    Mock::given(method("POST"))
        .and(path_regex(r"/v1/projects/[^/]+/messages:send"))
        .respond_with(FcmPushResponder {
            state: state.clone(),
            template: ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "projects/test-project-push/messages/mock-msg-id-001"
            })),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock FCM server that returns 401 Unauthorized for push requests.
pub async fn start_mock_fcm_unauthorized() -> (MockServer, FcmMockState) {
    let server = MockServer::start().await;
    let state = FcmMockState::new();

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(TokenExchangeResponder {
            state: state.clone(),
            template: success_token_response(),
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v1/projects/[^/]+/messages:send"))
        .respond_with(FcmPushResponder {
            state: state.clone(),
            template: ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {
                    "code": 401,
                    "message": "Request had invalid authentication credentials.",
                    "status": "UNAUTHENTICATED"
                }
            })),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock FCM server that returns 404 Not Found for push requests.
pub async fn start_mock_fcm_not_found() -> (MockServer, FcmMockState) {
    let server = MockServer::start().await;
    let state = FcmMockState::new();

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(TokenExchangeResponder {
            state: state.clone(),
            template: success_token_response(),
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v1/projects/[^/]+/messages:send"))
        .respond_with(FcmPushResponder {
            state: state.clone(),
            template: ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {
                    "code": 404,
                    "message": "Requested entity was not found.",
                    "status": "NOT_FOUND"
                }
            })),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock FCM server that returns 429 Too Many Requests.
pub async fn start_mock_fcm_rate_limited() -> (MockServer, FcmMockState) {
    let server = MockServer::start().await;
    let state = FcmMockState::new();

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(TokenExchangeResponder {
            state: state.clone(),
            template: success_token_response(),
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v1/projects/[^/]+/messages:send"))
        .respond_with(FcmPushResponder {
            state: state.clone(),
            template: ResponseTemplate::new(429).append_header("retry-after", "5"),
        })
        .mount(&server)
        .await;

    (server, state)
}

/// Start a mock FCM server where the OAuth2 token exchange itself fails.
pub async fn start_mock_fcm_token_exchange_failure() -> (MockServer, FcmMockState) {
    let server = MockServer::start().await;
    let state = FcmMockState::new();

    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(TokenExchangeResponder {
            state: state.clone(),
            template: ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "Invalid JWT Signature."
            })),
        })
        .mount(&server)
        .await;

    (server, state)
}

fn parse_fcm_request(req: &Request) -> RecordedFcmPush {
    let project_id = req
        .url
        .path()
        .strip_prefix("/v1/projects/")
        .and_then(|s| s.strip_suffix("/messages:send"))
        .unwrap_or("")
        .to_string();

    let payload: serde_json::Value =
        serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);

    let has_auth = req
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|a| a.starts_with("Bearer "))
        .unwrap_or(false);

    RecordedFcmPush {
        project_id,
        payload,
        has_auth,
    }
}

fn parse_token_exchange(req: &Request) -> RecordedTokenExchange {
    let body_str = String::from_utf8_lossy(&req.body);
    let params: Vec<(&str, &str)> = body_str
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect();

    let grant_type = params
        .iter()
        .find(|(k, _)| *k == "grant_type")
        .map(|(_, v)| urlencoding::decode(v).unwrap_or_default().into_owned())
        .unwrap_or_default();

    let has_assertion = params.iter().any(|(k, _)| *k == "assertion");

    RecordedTokenExchange {
        grant_type,
        has_assertion,
    }
}
