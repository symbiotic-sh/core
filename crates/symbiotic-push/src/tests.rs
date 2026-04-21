//! Tests for symbiotic-push crate.

use std::collections::HashMap;

use chrono::{Duration, Utc};

use crate::dispatch::{DispatcherConfig, NotificationDispatcher};
use crate::error::PushError;
use crate::gateway::{MockGateway, PushGateway};
use crate::payload::PayloadBuilder;
use crate::store::PushTokenStore;
use crate::types::{
    DispatchResult, NotificationCategory, PushNotification, PushPriority, PushProvider,
    PushResponse, PushToken,
};

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn make_token(device_id: &str, provider: PushProvider, user_id: &str) -> PushToken {
    PushToken {
        device_id: device_id.to_string(),
        platform: provider,
        token: format!("tok-{device_id}-{provider}"),
        user_id: user_id.to_string(),
        registered_at: Utc::now(),
        expires_at: None,
    }
}

fn make_expired_token(device_id: &str, provider: PushProvider, user_id: &str) -> PushToken {
    PushToken {
        device_id: device_id.to_string(),
        platform: provider,
        token: format!("tok-{device_id}-{provider}"),
        user_id: user_id.to_string(),
        registered_at: Utc::now() - Duration::hours(48),
        expires_at: Some(Utc::now() - Duration::hours(1)),
    }
}

// ---------------------------------------------------------------------------
// Token Store: CRUD
// ---------------------------------------------------------------------------

#[test]
fn test_store_register_and_retrieve() {
    let store = PushTokenStore::in_memory().unwrap();
    let token = make_token("dev1", PushProvider::Apns, "user1");
    store.register_token(&token).unwrap();

    let retrieved = store
        .get_token("dev1", PushProvider::Apns)
        .unwrap()
        .expect("token should exist");

    assert_eq!(retrieved.device_id, "dev1");
    assert_eq!(retrieved.token, "tok-dev1-apns");
    assert_eq!(retrieved.user_id, "user1");
}

#[test]
fn test_store_upsert_updates_token() {
    let store = PushTokenStore::in_memory().unwrap();
    let mut token = make_token("dev1", PushProvider::Apns, "user1");
    store.register_token(&token).unwrap();

    token.token = "new-token-value".to_string();
    store.register_token(&token).unwrap();

    let retrieved = store
        .get_token("dev1", PushProvider::Apns)
        .unwrap()
        .unwrap();
    assert_eq!(retrieved.token, "new-token-value");

    // Only one record should exist
    assert_eq!(store.count_for_user("user1").unwrap(), 1);
}

#[test]
fn test_store_get_tokens_for_user() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();
    store
        .register_token(&make_token("dev2", PushProvider::Fcm, "user1"))
        .unwrap();
    store
        .register_token(&make_token("dev3", PushProvider::Apns, "user2"))
        .unwrap();

    let tokens = store.get_tokens_for_user("user1").unwrap();
    assert_eq!(tokens.len(), 2);

    let tokens2 = store.get_tokens_for_user("user2").unwrap();
    assert_eq!(tokens2.len(), 1);
}

#[test]
fn test_store_get_tokens_for_nonexistent_user() {
    let store = PushTokenStore::in_memory().unwrap();
    let tokens = store.get_tokens_for_user("nobody").unwrap();
    assert!(tokens.is_empty());
}

#[test]
fn test_store_remove_token() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();

    assert!(store.remove_token("dev1", PushProvider::Apns).unwrap());
    assert!(store
        .get_token("dev1", PushProvider::Apns)
        .unwrap()
        .is_none());
}

#[test]
fn test_store_remove_nonexistent_token() {
    let store = PushTokenStore::in_memory().unwrap();
    assert!(!store.remove_token("dev1", PushProvider::Apns).unwrap());
}

#[test]
fn test_store_remove_device() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Fcm, "user1"))
        .unwrap();

    let removed = store.remove_device("dev1").unwrap();
    assert_eq!(removed, 2);
    assert_eq!(store.count_for_user("user1").unwrap(), 0);
}

#[test]
fn test_store_prune_expired() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_expired_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();
    store
        .register_token(&make_token("dev2", PushProvider::Fcm, "user1"))
        .unwrap();

    let pruned = store.prune_expired().unwrap();
    assert_eq!(pruned, 1);
    assert_eq!(store.count_for_user("user1").unwrap(), 1);
}

#[test]
fn test_store_prune_no_expired() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();

    let pruned = store.prune_expired().unwrap();
    assert_eq!(pruned, 0);
}

#[test]
fn test_store_count_for_user() {
    let store = PushTokenStore::in_memory().unwrap();
    assert_eq!(store.count_for_user("user1").unwrap(), 0);

    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();
    assert_eq!(store.count_for_user("user1").unwrap(), 1);

    store
        .register_token(&make_token("dev2", PushProvider::Fcm, "user1"))
        .unwrap();
    assert_eq!(store.count_for_user("user1").unwrap(), 2);
}

// ---------------------------------------------------------------------------
// Token Validation
// ---------------------------------------------------------------------------

#[test]
fn test_store_reject_empty_device_id() {
    let store = PushTokenStore::in_memory().unwrap();
    let mut token = make_token("dev1", PushProvider::Apns, "user1");
    token.device_id = "".to_string();

    let err = store.register_token(&token).unwrap_err();
    assert!(matches!(err, PushError::InvalidToken(_)));
}

#[test]
fn test_store_reject_whitespace_device_id() {
    let store = PushTokenStore::in_memory().unwrap();
    let mut token = make_token("dev1", PushProvider::Apns, "user1");
    token.device_id = "   ".to_string();

    let err = store.register_token(&token).unwrap_err();
    assert!(matches!(err, PushError::InvalidToken(_)));
}

#[test]
fn test_store_reject_empty_token_string() {
    let store = PushTokenStore::in_memory().unwrap();
    let mut token = make_token("dev1", PushProvider::Apns, "user1");
    token.token = "".to_string();

    let err = store.register_token(&token).unwrap_err();
    assert!(matches!(err, PushError::InvalidToken(_)));
}

#[test]
fn test_store_reject_empty_user_id() {
    let store = PushTokenStore::in_memory().unwrap();
    let mut token = make_token("dev1", PushProvider::Apns, "user1");
    token.user_id = "  ".to_string();

    let err = store.register_token(&token).unwrap_err();
    assert!(matches!(err, PushError::InvalidToken(_)));
}

// ---------------------------------------------------------------------------
// PushNotification Builder
// ---------------------------------------------------------------------------

#[test]
fn test_notification_builder_basic() {
    let n = PushNotification::new("Title", "Body", NotificationCategory::System);
    assert_eq!(n.title, "Title");
    assert_eq!(n.body, "Body");
    assert_eq!(n.category, NotificationCategory::System);
    assert_eq!(n.priority, PushPriority::Normal);
    assert!(n.data.is_empty());
    assert!(n.badge.is_none());
    assert!(n.sound.is_none());
    assert!(n.thread_id.is_none());
    assert!(n.token.is_none());
}

#[test]
fn test_notification_builder_chaining() {
    let n = PushNotification::new("T", "B", NotificationCategory::NewEntry)
        .with_data("key", "value")
        .with_badge(5)
        .with_sound("ping.wav")
        .with_thread_id("thread-abc")
        .with_priority(PushPriority::High)
        .with_token("device-token");

    assert_eq!(n.data.get("key").unwrap(), "value");
    assert_eq!(n.badge, Some(5));
    assert_eq!(n.sound.as_deref(), Some("ping.wav"));
    assert_eq!(n.thread_id.as_deref(), Some("thread-abc"));
    assert_eq!(n.priority, PushPriority::High);
    assert_eq!(n.token.as_deref(), Some("device-token"));
}

#[test]
fn test_notification_builder_multiple_data() {
    let n = PushNotification::new("T", "B", NotificationCategory::System)
        .with_data("a", "1")
        .with_data("b", "2")
        .with_data("c", "3");

    assert_eq!(n.data.len(), 3);
    assert_eq!(n.data.get("b").unwrap(), "2");
}

// ---------------------------------------------------------------------------
// PayloadBuilder Factory Methods
// ---------------------------------------------------------------------------

#[test]
fn test_payload_new_entry() {
    let n = PayloadBuilder::new_entry("Rust weekly digest", "https://example.com/rust");
    assert_eq!(n.category, NotificationCategory::NewEntry);
    assert_eq!(
        n.data.get("source_url").unwrap(),
        "https://example.com/rust"
    );
    assert_eq!(n.priority, PushPriority::Normal);
    assert!(n.sound.is_some());
}

#[test]
fn test_payload_agent_status_failed() {
    let n = PayloadBuilder::agent_status("research-agent", "failed", "HTTP 500 from API");
    assert_eq!(n.category, NotificationCategory::AgentStatus);
    assert_eq!(n.priority, PushPriority::High);
    assert_eq!(n.data.get("agent_name").unwrap(), "research-agent");
    assert_eq!(n.data.get("status").unwrap(), "failed");
}

#[test]
fn test_payload_agent_status_running() {
    let n = PayloadBuilder::agent_status("ingest-agent", "running", "Processing 5 items");
    assert_eq!(n.priority, PushPriority::Normal);
}

#[test]
fn test_payload_approval_request() {
    let n = PayloadBuilder::approval_request("deploy", "Deploy v1.0 to production");
    assert_eq!(n.category, NotificationCategory::ApprovalRequest);
    assert_eq!(n.priority, PushPriority::High);
    assert_eq!(n.data.get("action").unwrap(), "deploy");
    assert!(n.sound.is_some());
}

#[test]
fn test_payload_goal_update() {
    let n = PayloadBuilder::goal_update("Weekly review", "3/5 tasks completed");
    assert_eq!(n.category, NotificationCategory::GoalUpdate);
    assert_eq!(n.data.get("goal_name").unwrap(), "Weekly review");
    assert_eq!(n.data.get("progress").unwrap(), "3/5 tasks completed");
}

#[test]
fn test_payload_system() {
    let n = PayloadBuilder::system("Daemon restarted", "Runtime recovered after crash");
    assert_eq!(n.category, NotificationCategory::System);
    assert_eq!(n.title, "Daemon restarted");
    assert_eq!(n.body, "Runtime recovered after crash");
}

#[test]
fn test_payload_from_event() {
    let mut data = HashMap::new();
    data.insert("key1".to_string(), "val1".to_string());
    data.insert("key2".to_string(), "val2".to_string());

    let n = PayloadBuilder::from_event(
        NotificationCategory::NewEntry,
        "Custom",
        "Custom body",
        data,
    );
    assert_eq!(n.category, NotificationCategory::NewEntry);
    assert_eq!(n.data.len(), 2);
}

// ---------------------------------------------------------------------------
// Type Serialization Roundtrips
// ---------------------------------------------------------------------------

#[test]
fn test_push_provider_serde_roundtrip() {
    let apns = PushProvider::Apns;
    let json = serde_json::to_string(&apns).unwrap();
    assert_eq!(json, "\"apns\"");
    let deser: PushProvider = serde_json::from_str(&json).unwrap();
    assert_eq!(deser, apns);

    let fcm = PushProvider::Fcm;
    let json = serde_json::to_string(&fcm).unwrap();
    assert_eq!(json, "\"fcm\"");
    let deser: PushProvider = serde_json::from_str(&json).unwrap();
    assert_eq!(deser, fcm);
}

#[test]
fn test_push_priority_serde_roundtrip() {
    let high = PushPriority::High;
    let json = serde_json::to_string(&high).unwrap();
    let deser: PushPriority = serde_json::from_str(&json).unwrap();
    assert_eq!(deser, high);
}

#[test]
fn test_notification_category_serde_roundtrip() {
    for cat in [
        NotificationCategory::NewEntry,
        NotificationCategory::AgentStatus,
        NotificationCategory::ApprovalRequest,
        NotificationCategory::GoalUpdate,
        NotificationCategory::System,
    ] {
        let json = serde_json::to_string(&cat).unwrap();
        let deser: NotificationCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(deser, cat);
    }
}

#[test]
fn test_push_notification_serde_roundtrip() {
    let n = PushNotification::new("Test", "Body", NotificationCategory::System)
        .with_data("foo", "bar")
        .with_badge(3)
        .with_sound("ding")
        .with_thread_id("thread-1")
        .with_priority(PushPriority::High)
        .with_token("tok-123");

    let json = serde_json::to_string(&n).unwrap();
    let deser: PushNotification = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.title, "Test");
    assert_eq!(deser.badge, Some(3));
    assert_eq!(deser.priority, PushPriority::High);
    assert_eq!(deser.data.get("foo").unwrap(), "bar");
}

#[test]
fn test_push_token_serde_roundtrip() {
    let token = make_token("dev1", PushProvider::Apns, "user1");
    let json = serde_json::to_string(&token).unwrap();
    let deser: PushToken = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.device_id, "dev1");
    assert_eq!(deser.platform, PushProvider::Apns);
}

#[test]
fn test_push_response_serde_roundtrip() {
    let resp = PushResponse {
        success: true,
        provider_message_id: Some("msg-123".to_string()),
        error_reason: None,
        token_invalid: false,
    };
    let json = serde_json::to_string(&resp).unwrap();
    let deser: PushResponse = serde_json::from_str(&json).unwrap();
    assert!(deser.success);
    assert_eq!(deser.provider_message_id.as_deref(), Some("msg-123"));
}

// ---------------------------------------------------------------------------
// DispatchResult
// ---------------------------------------------------------------------------

#[test]
fn test_dispatch_result_new() {
    let r = DispatchResult::new();
    assert_eq!(r.total, 0);
    assert_eq!(r.succeeded, 0);
    assert_eq!(r.failed, 0);
    assert!(r.invalidated_tokens.is_empty());
}

#[test]
fn test_dispatch_result_record() {
    let mut r = DispatchResult::new();
    r.record_success();
    r.record_success();
    r.record_failure();
    r.record_invalidation("tok-dead".to_string());

    assert_eq!(r.total, 3);
    assert_eq!(r.succeeded, 2);
    assert_eq!(r.failed, 1);
    assert_eq!(r.invalidated_tokens.len(), 1);
    assert_eq!(r.invalidated_tokens[0], "tok-dead");
}

// ---------------------------------------------------------------------------
// MockGateway
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mock_gateway_default_success() {
    let mock = MockGateway::new(PushProvider::Apns);
    let n = PushNotification::new("T", "B", NotificationCategory::System).with_token("tok");

    let resp = mock.send(&n).await.unwrap();
    assert!(resp.success);
    assert_eq!(mock.sent_count(), 1);
}

#[tokio::test]
async fn test_mock_gateway_queued_responses() {
    let mock = MockGateway::new(PushProvider::Fcm);
    mock.queue_response(Ok(PushResponse {
        success: false,
        provider_message_id: None,
        error_reason: Some("bad token".into()),
        token_invalid: true,
    }));

    let n = PushNotification::new("T", "B", NotificationCategory::System).with_token("tok");

    let resp = mock.send(&n).await.unwrap();
    assert!(!resp.success);
    assert!(resp.token_invalid);

    // Second call returns default success (queue is empty)
    let resp2 = mock.send(&n).await.unwrap();
    assert!(resp2.success);
    assert_eq!(mock.sent_count(), 2);
}

#[tokio::test]
async fn test_mock_gateway_records_notifications() {
    let mock = MockGateway::new(PushProvider::Apns);
    let n1 = PushNotification::new("First", "B1", NotificationCategory::System).with_token("tok1");
    let n2 =
        PushNotification::new("Second", "B2", NotificationCategory::NewEntry).with_token("tok2");

    mock.send(&n1).await.unwrap();
    mock.send(&n2).await.unwrap();

    let sent = mock.sent_notifications();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].title, "First");
    assert_eq!(sent[1].title, "Second");
}

#[tokio::test]
async fn test_mock_gateway_clear_sent() {
    let mock = MockGateway::new(PushProvider::Apns);
    let n = PushNotification::new("T", "B", NotificationCategory::System).with_token("tok");
    mock.send(&n).await.unwrap();
    assert_eq!(mock.sent_count(), 1);

    mock.clear_sent();
    assert_eq!(mock.sent_count(), 0);
}

// ---------------------------------------------------------------------------
// NotificationDispatcher
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dispatcher_fan_out_multiple_devices() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();
    store
        .register_token(&make_token("dev2", PushProvider::Apns, "user1"))
        .unwrap();

    let mock_gw = MockGateway::new(PushProvider::Apns);

    let mut dispatcher = NotificationDispatcher::new(DispatcherConfig::default());
    dispatcher.register_gateway(Box::new(mock_gw));

    let result = dispatcher
        .dispatch(&store, "user1", |tok| {
            PushNotification::new("Hello", "World", NotificationCategory::System).with_token(tok)
        })
        .await
        .unwrap();

    assert_eq!(result.total, 2);
    assert_eq!(result.succeeded, 2);
    assert_eq!(result.failed, 0);
}

#[tokio::test]
async fn test_dispatcher_auto_prune_invalid_tokens() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();

    let mock = MockGateway::new(PushProvider::Apns);
    mock.queue_response(Ok(PushResponse {
        success: false,
        provider_message_id: None,
        error_reason: Some("Unregistered".into()),
        token_invalid: true,
    }));

    let mut dispatcher = NotificationDispatcher::new(DispatcherConfig {
        max_retries: 0,
        auto_prune_invalid: true,
    });
    dispatcher.register_gateway(Box::new(mock));

    let result = dispatcher
        .dispatch(&store, "user1", |tok| {
            PushNotification::new("T", "B", NotificationCategory::System).with_token(tok)
        })
        .await
        .unwrap();

    assert_eq!(result.failed, 1);
    assert_eq!(result.invalidated_tokens.len(), 1);

    // Token should have been auto-pruned from the store
    assert_eq!(store.count_for_user("user1").unwrap(), 0);
}

#[tokio::test]
async fn test_dispatcher_no_auto_prune_when_disabled() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();

    let mock = MockGateway::new(PushProvider::Apns);
    mock.queue_response(Ok(PushResponse {
        success: false,
        provider_message_id: None,
        error_reason: Some("Unregistered".into()),
        token_invalid: true,
    }));

    let mut dispatcher = NotificationDispatcher::new(DispatcherConfig {
        max_retries: 0,
        auto_prune_invalid: false,
    });
    dispatcher.register_gateway(Box::new(mock));

    let result = dispatcher
        .dispatch(&store, "user1", |tok| {
            PushNotification::new("T", "B", NotificationCategory::System).with_token(tok)
        })
        .await
        .unwrap();

    assert_eq!(result.invalidated_tokens.len(), 1);
    // Token should still be in the store
    assert_eq!(store.count_for_user("user1").unwrap(), 1);
}

#[tokio::test]
async fn test_dispatcher_missing_gateway() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Fcm, "user1"))
        .unwrap();

    // Only register APNs gateway, not FCM
    let mut dispatcher = NotificationDispatcher::with_defaults();
    dispatcher.register_gateway(Box::new(MockGateway::new(PushProvider::Apns)));

    let result = dispatcher
        .dispatch(&store, "user1", |tok| {
            PushNotification::new("T", "B", NotificationCategory::System).with_token(tok)
        })
        .await
        .unwrap();

    // Should record as failure since no FCM gateway exists
    assert_eq!(result.total, 1);
    assert_eq!(result.failed, 1);
}

#[tokio::test]
async fn test_dispatcher_dispatch_single_no_gateway() {
    let dispatcher = NotificationDispatcher::with_defaults();
    let n = PushNotification::new("T", "B", NotificationCategory::System).with_token("tok");

    let err = dispatcher
        .dispatch_single(&n, PushProvider::Apns)
        .await
        .unwrap_err();
    assert!(matches!(err, PushError::ProviderUnavailable(_)));
}

#[tokio::test]
async fn test_dispatcher_dispatch_event_convenience() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();

    let mock = MockGateway::new(PushProvider::Apns);
    let mut dispatcher = NotificationDispatcher::with_defaults();
    dispatcher.register_gateway(Box::new(mock));

    let data = HashMap::new();
    let result = dispatcher
        .dispatch_event(
            &store,
            "user1",
            NotificationCategory::System,
            "Test".to_string(),
            "Test body".to_string(),
            data,
        )
        .await
        .unwrap();

    assert_eq!(result.succeeded, 1);
}

#[tokio::test]
async fn test_dispatcher_empty_user_tokens() {
    let store = PushTokenStore::in_memory().unwrap();
    let mock = MockGateway::new(PushProvider::Apns);
    let mut dispatcher = NotificationDispatcher::with_defaults();
    dispatcher.register_gateway(Box::new(mock));

    let result = dispatcher
        .dispatch(&store, "nonexistent-user", |tok| {
            PushNotification::new("T", "B", NotificationCategory::System).with_token(tok)
        })
        .await
        .unwrap();

    assert_eq!(result.total, 0);
}

// ---------------------------------------------------------------------------
// PushError Display Messages
// ---------------------------------------------------------------------------

#[test]
fn test_error_display_invalid_token() {
    let err = PushError::InvalidToken("bad format".into());
    assert_eq!(err.to_string(), "invalid push token: bad format");
}

#[test]
fn test_error_display_expired_token() {
    let err = PushError::ExpiredToken;
    assert_eq!(err.to_string(), "push token has expired");
}

#[test]
fn test_error_display_send_failed() {
    let err = PushError::SendFailed("connection timeout".into());
    assert_eq!(err.to_string(), "send failed: connection timeout");
}

#[test]
fn test_error_display_rate_limited() {
    let err = PushError::RateLimited {
        retry_after_secs: 60,
    };
    assert_eq!(err.to_string(), "rate limited: retry after 60s");
}

#[test]
fn test_error_display_store_error() {
    let err = PushError::StoreError("disk full".into());
    assert_eq!(err.to_string(), "store error: disk full");
}

#[test]
fn test_error_display_payload_too_large() {
    let err = PushError::PayloadTooLarge {
        size: 5000,
        max: 4096,
    };
    assert_eq!(
        err.to_string(),
        "payload too large: 5000 bytes exceeds 4096 byte limit"
    );
}

#[test]
fn test_error_display_provider_unavailable() {
    let err = PushError::ProviderUnavailable("maintenance".into());
    assert_eq!(err.to_string(), "provider unavailable: maintenance");
}

// ---------------------------------------------------------------------------
// Additional Edge Cases
// ---------------------------------------------------------------------------

#[test]
fn test_push_provider_display() {
    assert_eq!(PushProvider::Apns.to_string(), "apns");
    assert_eq!(PushProvider::Fcm.to_string(), "fcm");
}

#[test]
fn test_notification_category_display() {
    assert_eq!(NotificationCategory::NewEntry.to_string(), "new_entry");
    assert_eq!(
        NotificationCategory::AgentStatus.to_string(),
        "agent_status"
    );
    assert_eq!(
        NotificationCategory::ApprovalRequest.to_string(),
        "approval_request"
    );
    assert_eq!(NotificationCategory::GoalUpdate.to_string(), "goal_update");
    assert_eq!(NotificationCategory::System.to_string(), "system");
}

#[test]
fn test_default_push_priority() {
    let p = PushPriority::default();
    assert_eq!(p, PushPriority::Normal);
}

#[test]
fn test_dispatch_result_default() {
    let r = DispatchResult::default();
    assert_eq!(r.total, 0);
    assert_eq!(r.succeeded, 0);
    assert_eq!(r.failed, 0);
    assert!(r.invalidated_tokens.is_empty());
}

#[test]
fn test_store_remove_by_token_string() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();

    let removed = store.remove_by_token_string("tok-dev1-apns").unwrap();
    assert!(removed);

    let removed_again = store.remove_by_token_string("tok-dev1-apns").unwrap();
    assert!(!removed_again);
}

#[test]
fn test_store_multiple_providers_same_device() {
    let store = PushTokenStore::in_memory().unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Apns, "user1"))
        .unwrap();
    store
        .register_token(&make_token("dev1", PushProvider::Fcm, "user1"))
        .unwrap();

    assert_eq!(store.count_for_user("user1").unwrap(), 2);

    // Removing one provider should leave the other
    store.remove_token("dev1", PushProvider::Apns).unwrap();
    assert_eq!(store.count_for_user("user1").unwrap(), 1);

    let remaining = store.get_token("dev1", PushProvider::Fcm).unwrap();
    assert!(remaining.is_some());
}

#[test]
fn test_dispatcher_config_defaults() {
    let config = DispatcherConfig::default();
    assert_eq!(config.max_retries, 1);
    assert!(config.auto_prune_invalid);
}
