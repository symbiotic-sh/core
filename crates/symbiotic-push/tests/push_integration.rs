//! Integration tests for push notification delivery against mock APNs/FCM servers.
//!
//! These tests spin up real HTTP servers (via wiremock) that simulate APNs and FCM
//! endpoints, then send notifications through the actual `ApnsGateway`/`FcmGateway`
//! code paths — exercising JWT signing, HTTP request construction, header validation,
//! and response parsing end-to-end.

use symbiotic_push::gateway::{ApnsGateway, FcmGateway, PushGateway};
use symbiotic_push::testutil::mock_apns;
use symbiotic_push::testutil::mock_fcm;
use symbiotic_push::testutil::{
    fake_apns_device_token, fake_fcm_registration_token, mock_apns_config, mock_fcm_config,
};
use symbiotic_push::types::{NotificationCategory, PushNotification, PushPriority};

// ===========================================================================
// APNs mock server tests
// ===========================================================================

#[tokio::test]
async fn apns_mock_success_sends_and_records() {
    let (server, state) = mock_apns::start_mock_apns_success().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let token = fake_apns_device_token();
    let notification =
        PushNotification::new("Test Title", "Test body", NotificationCategory::System)
            .with_token(&token)
            .with_priority(PushPriority::High)
            .with_badge(3)
            .with_sound("default");

    let response = gateway
        .send(&notification)
        .await
        .expect("send should succeed");

    assert!(response.success);
    assert!(!response.token_invalid);

    // Verify the mock recorded the push
    let pushes = state.received_pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].device_token, token);
    assert!(pushes[0].has_auth, "should have bearer auth header");
    assert_eq!(pushes[0].priority.as_deref(), Some("10")); // High priority = 10
    assert_eq!(pushes[0].push_type.as_deref(), Some("alert"));

    // Verify payload structure
    let aps = &pushes[0].payload["aps"];
    assert_eq!(aps["alert"]["title"], "Test Title");
    assert_eq!(aps["alert"]["body"], "Test body");
    assert_eq!(aps["badge"], 3);
    assert_eq!(aps["sound"], "default");
}

#[tokio::test]
async fn apns_mock_forbidden_returns_failure() {
    let (server, state) = mock_apns::start_mock_apns_forbidden().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let token = fake_apns_device_token();
    let notification =
        PushNotification::new("Test", "Body", NotificationCategory::System).with_token(&token);

    let response = gateway
        .send(&notification)
        .await
        .expect("should not error on 403");

    assert!(!response.success);
    assert!(!response.token_invalid);
    assert!(response
        .error_reason
        .as_ref()
        .unwrap()
        .contains("ExpiredProviderToken"));
    assert_eq!(state.push_count(), 1);
}

#[tokio::test]
async fn apns_mock_gone_marks_token_invalid() {
    let (server, _state) = mock_apns::start_mock_apns_gone().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let token = fake_apns_device_token();
    let notification =
        PushNotification::new("Test", "Body", NotificationCategory::System).with_token(&token);

    let response = gateway
        .send(&notification)
        .await
        .expect("should not error on 410");

    assert!(!response.success);
    assert!(response.token_invalid, "410 should mark token as invalid");
    assert!(response
        .error_reason
        .as_ref()
        .unwrap()
        .contains("Unregistered"));
}

#[tokio::test]
async fn apns_mock_rate_limited_returns_error() {
    let (server, _state) = mock_apns::start_mock_apns_rate_limited().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let token = fake_apns_device_token();
    let notification =
        PushNotification::new("Test", "Body", NotificationCategory::System).with_token(&token);

    let err = gateway
        .send(&notification)
        .await
        .expect_err("429 should return error");

    match err {
        symbiotic_push::error::PushError::RateLimited { retry_after_secs } => {
            assert_eq!(retry_after_secs, 5);
        }
        other => panic!("expected RateLimited, got: {other:?}"),
    }
}

#[tokio::test]
async fn apns_mock_bad_request_returns_failure() {
    let (server, _state) = mock_apns::start_mock_apns_bad_request().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let token = fake_apns_device_token();
    let notification =
        PushNotification::new("Test", "Body", NotificationCategory::System).with_token(&token);

    let response = gateway
        .send(&notification)
        .await
        .expect("should not error on 400");

    assert!(!response.success);
    assert!(response
        .error_reason
        .as_ref()
        .unwrap()
        .contains("BadDeviceToken"));
}

#[tokio::test]
async fn apns_mock_validates_jwt_auth_header() {
    let (server, state) = mock_apns::start_mock_apns_auth_validating().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let token = fake_apns_device_token();
    let notification =
        PushNotification::new("Auth Test", "Body", NotificationCategory::System).with_token(&token);

    let response = gateway
        .send(&notification)
        .await
        .expect("send should succeed");

    // The gateway should produce a valid JWT, so the auth-validating mock
    // should return 200.
    assert!(response.success, "valid JWT should pass auth validation");

    let pushes = state.received_pushes();
    assert_eq!(pushes.len(), 1);
    assert!(pushes[0].has_auth);
}

#[tokio::test]
async fn apns_sends_notification_data_fields() {
    let (server, state) = mock_apns::start_mock_apns_success().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let token = fake_apns_device_token();
    let notification = PushNotification::new(
        "Entry",
        "New entry captured",
        NotificationCategory::NewEntry,
    )
    .with_token(&token)
    .with_data("source_url", "https://example.com")
    .with_data("entry_id", "entry-42")
    .with_thread_id("thread-abc");

    gateway
        .send(&notification)
        .await
        .expect("send should succeed");

    let pushes = state.received_pushes();
    assert_eq!(pushes[0].payload["source_url"], "https://example.com");
    assert_eq!(pushes[0].payload["entry_id"], "entry-42");
    assert_eq!(pushes[0].payload["category"], "new_entry");
    assert_eq!(pushes[0].payload["aps"]["thread-id"], "thread-abc");
}

#[tokio::test]
async fn apns_missing_token_returns_error() {
    let (server, _state) = mock_apns::start_mock_apns_success().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    // No token set
    let notification = PushNotification::new("Test", "Body", NotificationCategory::System);

    let err = gateway
        .send(&notification)
        .await
        .expect_err("missing token should error");
    assert!(matches!(
        err,
        symbiotic_push::error::PushError::InvalidToken(_)
    ));
}

// ===========================================================================
// FCM mock server tests
// ===========================================================================

#[tokio::test]
async fn fcm_mock_success_sends_and_records() {
    let (server, state) = mock_fcm::start_mock_fcm_success().await;
    let config = mock_fcm_config(&server.uri());
    let gateway = FcmGateway::new(config);

    let token = fake_fcm_registration_token();
    let notification =
        PushNotification::new("FCM Test", "Hello Android", NotificationCategory::System)
            .with_token(&token)
            .with_priority(PushPriority::High)
            .with_data("action", "test_action");

    let response = gateway
        .send(&notification)
        .await
        .expect("send should succeed");

    assert!(response.success);
    assert!(response
        .provider_message_id
        .as_ref()
        .unwrap()
        .contains("mock-msg-id-001"));

    // Verify OAuth2 token exchange happened
    let exchanges = state.token_exchanges();
    assert_eq!(exchanges.len(), 1);
    assert!(exchanges[0].has_assertion);
    assert_eq!(
        exchanges[0].grant_type,
        "urn:ietf:params:oauth:grant-type:jwt-bearer"
    );

    // Verify push was recorded
    let pushes = state.received_pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].project_id, "test-project-push");
    assert!(pushes[0].has_auth, "should have Bearer auth header");

    // Verify FCM payload structure
    let msg = &pushes[0].payload["message"];
    assert_eq!(msg["token"], token);
    assert_eq!(msg["notification"]["title"], "FCM Test");
    assert_eq!(msg["notification"]["body"], "Hello Android");
    assert_eq!(msg["data"]["action"], "test_action");
    assert_eq!(msg["android"]["priority"], "HIGH");
}

#[tokio::test]
async fn fcm_mock_unauthorized_returns_failure() {
    let (server, _state) = mock_fcm::start_mock_fcm_unauthorized().await;
    let config = mock_fcm_config(&server.uri());
    let gateway = FcmGateway::new(config);

    let token = fake_fcm_registration_token();
    let notification =
        PushNotification::new("Test", "Body", NotificationCategory::System).with_token(&token);

    let response = gateway
        .send(&notification)
        .await
        .expect("should not error on 401");

    assert!(!response.success);
    assert!(!response.token_invalid);
    assert!(response
        .error_reason
        .as_ref()
        .unwrap()
        .contains("authentication error"));
}

#[tokio::test]
async fn fcm_mock_not_found_marks_token_invalid() {
    let (server, _state) = mock_fcm::start_mock_fcm_not_found().await;
    let config = mock_fcm_config(&server.uri());
    let gateway = FcmGateway::new(config);

    let token = fake_fcm_registration_token();
    let notification =
        PushNotification::new("Test", "Body", NotificationCategory::System).with_token(&token);

    let response = gateway
        .send(&notification)
        .await
        .expect("should not error on 404");

    assert!(!response.success);
    assert!(response.token_invalid, "404 should mark token as invalid");
}

#[tokio::test]
async fn fcm_mock_rate_limited_returns_error() {
    let (server, _state) = mock_fcm::start_mock_fcm_rate_limited().await;
    let config = mock_fcm_config(&server.uri());
    let gateway = FcmGateway::new(config);

    let token = fake_fcm_registration_token();
    let notification =
        PushNotification::new("Test", "Body", NotificationCategory::System).with_token(&token);

    let err = gateway
        .send(&notification)
        .await
        .expect_err("429 should return error");

    match err {
        symbiotic_push::error::PushError::RateLimited { retry_after_secs } => {
            assert_eq!(retry_after_secs, 5);
        }
        other => panic!("expected RateLimited, got: {other:?}"),
    }
}

#[tokio::test]
async fn fcm_mock_token_exchange_failure() {
    let (server, _state) = mock_fcm::start_mock_fcm_token_exchange_failure().await;
    let config = mock_fcm_config(&server.uri());
    let gateway = FcmGateway::new(config);

    let token = fake_fcm_registration_token();
    let notification =
        PushNotification::new("Test", "Body", NotificationCategory::System).with_token(&token);

    let err = gateway
        .send(&notification)
        .await
        .expect_err("token exchange failure should error");

    match err {
        symbiotic_push::error::PushError::SendFailed(msg) => {
            assert!(
                msg.contains("token exchange failed"),
                "error should mention token exchange: {msg}"
            );
        }
        other => panic!("expected SendFailed, got: {other:?}"),
    }
}

#[tokio::test]
async fn fcm_sends_collapse_key_for_thread_id() {
    let (server, state) = mock_fcm::start_mock_fcm_success().await;
    let config = mock_fcm_config(&server.uri());
    let gateway = FcmGateway::new(config);

    let token = fake_fcm_registration_token();
    let notification = PushNotification::new("Test", "Body", NotificationCategory::System)
        .with_token(&token)
        .with_thread_id("thread-xyz");

    gateway
        .send(&notification)
        .await
        .expect("send should succeed");

    let pushes = state.received_pushes();
    assert_eq!(
        pushes[0].payload["message"]["android"]["collapseKey"],
        "thread-xyz"
    );
}

#[tokio::test]
async fn fcm_missing_token_returns_error() {
    let (server, _state) = mock_fcm::start_mock_fcm_success().await;
    let config = mock_fcm_config(&server.uri());
    let gateway = FcmGateway::new(config);

    // No token set
    let notification = PushNotification::new("Test", "Body", NotificationCategory::System);

    let err = gateway
        .send(&notification)
        .await
        .expect_err("missing token should error");
    assert!(matches!(
        err,
        symbiotic_push::error::PushError::InvalidToken(_)
    ));
}

// ===========================================================================
// Dispatcher integration with mock servers
// ===========================================================================

#[tokio::test]
async fn dispatcher_with_mock_apns_server() {
    use symbiotic_push::dispatch::{DispatcherConfig, NotificationDispatcher};
    use symbiotic_push::store::PushTokenStore;
    use symbiotic_push::types::{PushProvider, PushToken};

    let (server, state) = mock_apns::start_mock_apns_success().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let store = PushTokenStore::in_memory().expect("store");
    let device_token = fake_apns_device_token();
    store
        .register_token(&PushToken {
            device_id: "test-iphone".to_string(),
            platform: PushProvider::Apns,
            token: device_token.clone(),
            user_id: "user-1".to_string(),
            registered_at: chrono::Utc::now(),
            expires_at: None,
        })
        .expect("register");

    let mut dispatcher = NotificationDispatcher::new(DispatcherConfig::default());
    dispatcher.register_gateway(Box::new(gateway));

    let result = dispatcher
        .dispatch(&store, "user-1", |tok| {
            PushNotification::new("Alert", "Something happened", NotificationCategory::System)
                .with_token(tok)
        })
        .await
        .expect("dispatch");

    assert_eq!(result.total, 1);
    assert_eq!(result.succeeded, 1);
    assert_eq!(result.failed, 0);
    assert_eq!(state.push_count(), 1);
}

#[tokio::test]
async fn dispatcher_auto_prunes_with_mock_apns_gone() {
    use symbiotic_push::dispatch::{DispatcherConfig, NotificationDispatcher};
    use symbiotic_push::store::PushTokenStore;
    use symbiotic_push::types::{PushProvider, PushToken};

    let (server, _state) = mock_apns::start_mock_apns_gone().await;
    let config = mock_apns_config(&server.uri());
    let gateway = ApnsGateway::new(config);

    let store = PushTokenStore::in_memory().expect("store");
    let device_token = fake_apns_device_token();
    store
        .register_token(&PushToken {
            device_id: "test-iphone".to_string(),
            platform: PushProvider::Apns,
            token: device_token,
            user_id: "user-1".to_string(),
            registered_at: chrono::Utc::now(),
            expires_at: None,
        })
        .expect("register");

    let mut dispatcher = NotificationDispatcher::new(DispatcherConfig {
        max_retries: 0,
        auto_prune_invalid: true,
    });
    dispatcher.register_gateway(Box::new(gateway));

    let result = dispatcher
        .dispatch(&store, "user-1", |tok| {
            PushNotification::new("Test", "Body", NotificationCategory::System).with_token(tok)
        })
        .await
        .expect("dispatch");

    assert_eq!(result.failed, 1);
    assert_eq!(result.invalidated_tokens.len(), 1);
    // Token should be pruned from store
    assert_eq!(store.count_for_user("user-1").unwrap(), 0);
}

// ===========================================================================
// Real APNs sandbox tests (gated on environment variables)
// ===========================================================================

#[tokio::test]
#[ignore] // Requires APNS_TEST_KEY, APNS_TEST_TEAM_ID, APNS_TEST_KEY_ID, APNS_TEST_DEVICE_TOKEN
async fn real_apns_sandbox_send() {
    symbiotic_push::skip_unless_creds!(
        "APNS_TEST_TEAM_ID",
        "APNS_TEST_KEY_ID",
        "APNS_TEST_KEY",
        "APNS_TEST_DEVICE_TOKEN"
    );

    let config = symbiotic_push::testutil::setup_real_apns_config(true);
    let device_token = symbiotic_push::testutil::real_apns_device_token();
    let gateway = ApnsGateway::new(config);

    let notification = PushNotification::new(
        "Symbiotic Test",
        "APNs sandbox test",
        NotificationCategory::System,
    )
    .with_token(&device_token)
    .with_priority(PushPriority::High)
    .with_sound("default");

    let response = gateway
        .send(&notification)
        .await
        .expect("APNs sandbox send should not error");

    // The real APNs will either succeed or return a specific error.
    // If the device token is valid, we expect success.
    // If not, we at least verify the gateway handles the response correctly.
    println!("APNs sandbox response: {response:?}");
    if response.success {
        symbiotic_push::testutil::assert_push_accepted(&response);
    }
}

#[tokio::test]
#[ignore] // Requires APNS_TEST_KEY, APNS_TEST_TEAM_ID, APNS_TEST_KEY_ID, APNS_TEST_PRODUCTION_DEVICE_TOKEN
async fn real_apns_production_send() {
    symbiotic_push::skip_unless_creds!(
        "APNS_TEST_TEAM_ID",
        "APNS_TEST_KEY_ID",
        "APNS_TEST_KEY",
        "APNS_TEST_PRODUCTION_DEVICE_TOKEN"
    );

    let config = symbiotic_push::testutil::setup_real_apns_config(false); // production
    let device_token = std::env::var("APNS_TEST_PRODUCTION_DEVICE_TOKEN")
        .expect("APNS_TEST_PRODUCTION_DEVICE_TOKEN must be set");
    let gateway = ApnsGateway::new(config);

    let notification = PushNotification::new(
        "Symbiotic Production Test",
        "APNs production test — if you see this, push delivery is working",
        NotificationCategory::System,
    )
    .with_token(&device_token)
    .with_priority(PushPriority::High)
    .with_sound("default");

    let response = gateway
        .send(&notification)
        .await
        .expect("APNs production send should not error");

    println!("APNs production response: {response:?}");
    if response.success {
        symbiotic_push::testutil::assert_push_accepted(&response);
    }
}

#[tokio::test]
#[ignore] // Requires APNS_TEST_KEY, APNS_TEST_TEAM_ID, APNS_TEST_KEY_ID
async fn real_apns_invalid_token() {
    symbiotic_push::skip_unless_creds!("APNS_TEST_TEAM_ID", "APNS_TEST_KEY_ID", "APNS_TEST_KEY");

    let config = symbiotic_push::testutil::setup_real_apns_config(true);
    let gateway = ApnsGateway::new(config);

    // Use a syntactically valid but non-existent device token (64 hex chars).
    let invalid_token = "0000000000000000000000000000000000000000000000000000000000000000";

    let notification = PushNotification::new(
        "Invalid Token Test",
        "This should be rejected",
        NotificationCategory::System,
    )
    .with_token(invalid_token)
    .with_priority(PushPriority::High);

    let result = gateway.send(&notification).await;

    println!("APNs invalid token result: {result:?}");

    // APNs should either:
    // - Return a response with success=false and token_invalid=true (410 Gone), or
    // - Return a response with success=false and an error like BadDeviceToken (400), or
    // - Return a RateLimited error (if we're being throttled)
    match result {
        Ok(response) => {
            assert!(
                !response.success,
                "invalid token should not succeed: {response:?}"
            );
            // If APNs returns 410, the token should be marked invalid.
            if response.token_invalid {
                println!(
                    "APNs correctly identified token as invalid (410 Gone): {:?}",
                    response.error_reason
                );
            } else {
                // 400 BadDeviceToken is also acceptable
                println!("APNs rejected with: {:?}", response.error_reason);
            }
        }
        Err(e) => {
            // RateLimited or transient errors are acceptable — the gateway
            // handled the response correctly either way.
            println!("APNs returned error (acceptable): {e}");
        }
    }
}

#[tokio::test]
#[ignore] // Requires APNS_TEST_KEY, APNS_TEST_TEAM_ID, APNS_TEST_KEY_ID, APNS_TEST_DEVICE_TOKEN
async fn real_apns_batch_send() {
    symbiotic_push::skip_unless_creds!(
        "APNS_TEST_TEAM_ID",
        "APNS_TEST_KEY_ID",
        "APNS_TEST_KEY",
        "APNS_TEST_DEVICE_TOKEN"
    );

    let config = symbiotic_push::testutil::setup_real_apns_config(true);
    let device_token = symbiotic_push::testutil::real_apns_device_token();
    let gateway = ApnsGateway::new(config);

    let count = 3;
    let mut results = Vec::new();

    for i in 1..=count {
        let notification = PushNotification::new(
            format!("Batch Test {i}/{count}"),
            format!("Symbiotic batch push test message {i} of {count}"),
            NotificationCategory::System,
        )
        .with_token(&device_token)
        .with_priority(PushPriority::Normal)
        .with_badge(i as u32);

        let result = gateway.send(&notification).await;
        println!("APNs batch [{i}/{count}] result: {result:?}");
        results.push(result);
    }

    // All sends should complete without panicking. Count successes.
    let ok_count = results
        .iter()
        .filter(|r| r.as_ref().map(|resp| resp.success).unwrap_or(false))
        .count();

    println!("APNs batch: {ok_count}/{count} succeeded");

    // If the token is valid, we expect all to succeed.
    // If the token is stale, 0 successes is still acceptable — we're testing
    // that the gateway handles a burst of sends without panicking or deadlocking.
    assert_eq!(
        results.len(),
        count,
        "all sends should complete (no panics)"
    );
}

#[tokio::test]
#[ignore] // Requires APNS_TEST_KEY, APNS_TEST_TEAM_ID, APNS_TEST_KEY_ID, APNS_TEST_DEVICE_TOKEN
async fn real_apns_rich_payload() {
    symbiotic_push::skip_unless_creds!(
        "APNS_TEST_TEAM_ID",
        "APNS_TEST_KEY_ID",
        "APNS_TEST_KEY",
        "APNS_TEST_DEVICE_TOKEN"
    );

    let config = symbiotic_push::testutil::setup_real_apns_config(true);
    let device_token = symbiotic_push::testutil::real_apns_device_token();
    let gateway = ApnsGateway::new(config);

    let notification = PushNotification::new(
        "Rich Notification Test",
        "Entry captured: Understanding Rust Ownership",
        NotificationCategory::NewEntry,
    )
    .with_token(&device_token)
    .with_priority(PushPriority::High)
    .with_badge(5)
    .with_sound("default")
    .with_thread_id("symbiotic-test-thread")
    .with_data(
        "source_url",
        "https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html",
    )
    .with_data("entry_id", "entry-test-rich-001")
    .with_data("test_run", "true");

    let response = gateway
        .send(&notification)
        .await
        .expect("APNs rich payload send should not error");

    println!("APNs rich payload response: {response:?}");

    // Verify the gateway successfully constructed and sent the rich payload.
    // APNs may accept or reject depending on token validity, but the gateway
    // code path for building the payload with all fields is exercised either way.
    if response.success {
        symbiotic_push::testutil::assert_push_accepted(&response);
    }
}

// ===========================================================================
// Real FCM tests (gated on environment variables)
// ===========================================================================

#[tokio::test]
#[ignore] // Requires FCM_TEST_CREDENTIALS, FCM_TEST_DEVICE_TOKEN
async fn real_fcm_send() {
    symbiotic_push::skip_unless_creds!("FCM_TEST_CREDENTIALS", "FCM_TEST_DEVICE_TOKEN");

    let config = symbiotic_push::testutil::setup_real_fcm_config();
    let device_token = symbiotic_push::testutil::real_fcm_device_token();
    let gateway = FcmGateway::new(config);

    let notification = PushNotification::new(
        "Symbiotic Test",
        "FCM integration test",
        NotificationCategory::System,
    )
    .with_token(&device_token)
    .with_priority(PushPriority::High)
    .with_data("test_run", "true");

    let response = gateway
        .send(&notification)
        .await
        .expect("FCM send should not error");

    println!("FCM response: {response:?}");
    if response.success {
        symbiotic_push::testutil::assert_push_accepted(&response);
    }
}

#[tokio::test]
#[ignore] // Requires FCM_TEST_CREDENTIALS
async fn real_fcm_invalid_token() {
    symbiotic_push::skip_unless_creds!("FCM_TEST_CREDENTIALS");

    let config = symbiotic_push::testutil::setup_real_fcm_config();
    let gateway = FcmGateway::new(config);

    // Use a clearly invalid FCM registration token.
    let invalid_token = "invalid_fcm_registration_token_that_does_not_exist";

    let notification = PushNotification::new(
        "Invalid Token Test",
        "This should be rejected by FCM",
        NotificationCategory::System,
    )
    .with_token(invalid_token)
    .with_priority(PushPriority::Normal);

    let result = gateway.send(&notification).await;

    println!("FCM invalid token result: {result:?}");

    // FCM should either:
    // - Return a response with success=false and token_invalid=true (404), or
    // - Return a response with success=false and a 400 error, or
    // - Return a RateLimited error
    match result {
        Ok(response) => {
            assert!(
                !response.success,
                "invalid token should not succeed: {response:?}"
            );
            if response.token_invalid {
                println!(
                    "FCM correctly identified token as invalid (404): {:?}",
                    response.error_reason
                );
            } else {
                println!("FCM rejected with: {:?}", response.error_reason);
            }
        }
        Err(e) => {
            println!("FCM returned error (acceptable): {e}");
        }
    }
}

#[tokio::test]
#[ignore] // Requires FCM_TEST_CREDENTIALS, FCM_TEST_DEVICE_TOKEN
async fn real_fcm_data_message() {
    symbiotic_push::skip_unless_creds!("FCM_TEST_CREDENTIALS", "FCM_TEST_DEVICE_TOKEN");

    let config = symbiotic_push::testutil::setup_real_fcm_config();
    let device_token = symbiotic_push::testutil::real_fcm_device_token();
    let gateway = FcmGateway::new(config);

    // Data-only messages have no visible notification — they're processed
    // silently by the app. We still set title/body in the PushNotification
    // struct (the FCM gateway puts them in the `notification` field), but
    // the important part is the custom data fields.
    let notification = PushNotification::new(
        "Background Sync",
        "Data-only message for background processing",
        NotificationCategory::System,
    )
    .with_token(&device_token)
    .with_priority(PushPriority::Normal)
    .with_data("action", "background_sync")
    .with_data("sync_type", "entries")
    .with_data("since_ts", "1709000000")
    .with_data("test_run", "true");

    let response = gateway
        .send(&notification)
        .await
        .expect("FCM data message send should not error");

    println!("FCM data message response: {response:?}");
    if response.success {
        symbiotic_push::testutil::assert_push_accepted(&response);
    }
}

#[tokio::test]
#[ignore] // Requires FCM_TEST_CREDENTIALS, FCM_TEST_TOPIC
async fn real_fcm_topic_send() {
    symbiotic_push::skip_unless_creds!("FCM_TEST_CREDENTIALS", "FCM_TEST_TOPIC");

    let config = symbiotic_push::testutil::setup_real_fcm_config();
    let topic = std::env::var("FCM_TEST_TOPIC").expect("FCM_TEST_TOPIC must be set");
    let gateway = FcmGateway::new(config);

    // FCM topics use the token field with the format "/topics/<topic_name>".
    let topic_token = format!("/topics/{topic}");

    let notification = PushNotification::new(
        "Topic Test",
        "FCM topic notification test",
        NotificationCategory::System,
    )
    .with_token(&topic_token)
    .with_priority(PushPriority::Normal)
    .with_data("test_run", "true");

    let result = gateway.send(&notification).await;

    println!("FCM topic send result: {result:?}");

    // Topic sends may fail if no devices are subscribed or the topic doesn't
    // exist, but the gateway should handle the response cleanly.
    match result {
        Ok(response) => {
            println!(
                "FCM topic send: success={}, message_id={:?}",
                response.success, response.provider_message_id
            );
        }
        Err(e) => {
            println!("FCM topic send error (may be expected): {e}");
        }
    }
}

// ===========================================================================
// End-to-end dispatcher tests with real credentials
// ===========================================================================

#[tokio::test]
#[ignore] // Requires APNS credentials + APNS_TEST_DEVICE_TOKEN
async fn real_apns_dispatcher_e2e() {
    symbiotic_push::skip_unless_creds!(
        "APNS_TEST_TEAM_ID",
        "APNS_TEST_KEY_ID",
        "APNS_TEST_KEY",
        "APNS_TEST_DEVICE_TOKEN"
    );

    use symbiotic_push::dispatch::{DispatcherConfig, NotificationDispatcher};
    use symbiotic_push::store::PushTokenStore;
    use symbiotic_push::types::{PushProvider, PushToken};

    let config = symbiotic_push::testutil::setup_real_apns_config(true);
    let device_token = symbiotic_push::testutil::real_apns_device_token();
    let gateway = ApnsGateway::new(config);

    // Set up an in-memory token store with the real device token.
    let store = PushTokenStore::in_memory().expect("store");
    store
        .register_token(&PushToken {
            device_id: "test-iphone-e2e".to_string(),
            platform: PushProvider::Apns,
            token: device_token,
            user_id: "e2e-test-user".to_string(),
            registered_at: chrono::Utc::now(),
            expires_at: None,
        })
        .expect("register");

    let mut dispatcher = NotificationDispatcher::new(DispatcherConfig {
        max_retries: 1,
        auto_prune_invalid: true,
    });
    dispatcher.register_gateway(Box::new(gateway));

    let result = dispatcher
        .dispatch(&store, "e2e-test-user", |tok| {
            PushNotification::new(
                "E2E Dispatch Test",
                "Notification dispatched through full stack",
                NotificationCategory::System,
            )
            .with_token(tok)
            .with_priority(PushPriority::High)
            .with_sound("default")
            .with_data("test_run", "true")
        })
        .await
        .expect("dispatch");

    println!("Dispatcher E2E result: {result:?}");
    assert_eq!(result.total, 1, "should have dispatched to 1 device");

    // If the token is valid, expect success. If stale, the dispatcher
    // may auto-prune it — both paths are valid.
    if result.succeeded == 1 {
        println!("E2E dispatch: notification accepted by APNs");
    } else {
        println!(
            "E2E dispatch: notification rejected (token may be stale), invalidated={:?}",
            result.invalidated_tokens
        );
    }
}
