//! Push notification dispatcher for daemon events.
//!
//! Translates `DaemonEvent` values from the job execution loop into push
//! notifications and dispatches them to registered devices via the daemon's
//! existing `PushProvider` + `PushRegistry` infrastructure.
//!
//! # Design
//!
//! - Fire-and-forget: push errors are logged but never propagate to callers.
//! - Non-blocking: dispatch is spawned onto the tokio runtime so the event
//!   loop is not stalled by slow push delivery (file I/O, HTTP retries).
//! - Only "interesting" events trigger push notifications; routine successes
//!   are suppressed to avoid notification fatigue.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::events::{simple_hash, DaemonEvent, EventType};
use crate::push::{
    append_push_telemetry, PushNotification as DaemonPushNotification, PushPreferences,
    PushProvider, PushRegistry,
};
use std::path::{Path, PathBuf};

/// Determines whether a `DaemonEvent` warrants a push notification.
///
/// Returns a `(title, priority)` pair when the event should be pushed,
/// or `None` when it should be silently dropped.
pub(crate) fn classify_event(event: &DaemonEvent) -> Option<(&'static str, &'static str)> {
    match (event.event_type, event.status.as_str()) {
        // --- Failures always push (user needs to know) ---
        (EventType::IngestFetch, "dlq") => Some(("Ingest failed (DLQ)", "high")),
        (EventType::IngestFetch, "retry") => None, // Transient — wait for DLQ or success
        (EventType::ArchiveReview, "dlq") => Some(("Review failed (DLQ)", "high")),
        (EventType::ArchiveReviewEnqueue, "dlq") => Some(("Review enqueue failed", "high")),
        (EventType::AuthIssue, "failed") => Some(("Auth issue failed", "critical")),
        (EventType::AuthIssue, "dlq") => Some(("Auth issue failed (DLQ)", "critical")),
        (EventType::WorkflowRun, "dlq") => Some(("Workflow failed (DLQ)", "high")),
        (EventType::BookmarksSync, "dlq") => Some(("Bookmarks sync failed", "high")),
        (EventType::JobUnknown, _) => Some(("Unknown job type", "high")),

        // --- Goal completions push (user wants progress) ---
        (EventType::WorkflowRun, "completed") => Some(("Workflow completed", "high")),

        // --- Ingest completions push (user wants feedback on captures) ---
        (EventType::IngestFetch, "completed") => Some(("Entry captured", "high")),

        // --- Install events push (user is waiting on provisioning) ---
        (EventType::InstallProvision, "completed") => Some(("VPS provisioned", "high")),
        (EventType::InstallProvision, "dlq") => Some(("VPS provisioning failed", "critical")),
        (EventType::InstallBootstrap, "completed") => Some(("Bootstrap complete", "high")),
        (EventType::InstallBootstrap, "dlq") => Some(("Bootstrap failed", "critical")),
        (EventType::InstallVerify, "completed") => Some(("Install verified", "high")),
        (EventType::InstallVerify, "dlq") => Some(("Install verification failed", "critical")),
        (EventType::InstallRun, "completed") => Some(("Install complete", "high")),
        (EventType::InstallRun, "dlq") => Some(("Install failed", "critical")),

        // Everything else: suppress
        _ => None,
    }
}

/// Build the notification body text from a `DaemonEvent`.
pub(crate) fn build_body(event: &DaemonEvent) -> String {
    let mut parts = Vec::new();

    // Include URL when available (ingest events)
    if let Some(ref url) = event.url {
        parts.push(url.clone());
    }
    // Include title when available
    if let Some(ref title) = event.title {
        parts.push(title.clone());
    }
    // Always include the detail (status description)
    if !event.detail.is_empty() {
        parts.push(event.detail.clone());
    }

    if parts.is_empty() {
        event.event_type.to_string()
    } else {
        parts.join(" — ")
    }
}

// ---------------------------------------------------------------------------
// Rate limiter
// ---------------------------------------------------------------------------

/// Per-device rate limiter for push notifications.
///
/// Prevents notification spam during cascading failures.
pub(crate) struct PushRateLimiter {
    /// Maximum notifications per device per window.
    max_per_window: u32,
    /// Window duration in seconds.
    window_secs: u64,
    /// device_id -> (count, window_start_epoch)
    counters: HashMap<String, (u32, u64)>,
}

impl PushRateLimiter {
    pub(crate) fn new(max_per_window: u32, window_secs: u64) -> Self {
        Self {
            max_per_window,
            window_secs,
            counters: HashMap::new(),
        }
    }

    /// Returns `true` if the device is under its rate limit and the counter
    /// should be incremented; `false` if rate limited.
    pub(crate) fn check_and_increment(&mut self, device_id: &str, now: u64) -> bool {
        let entry = self
            .counters
            .entry(device_id.to_string())
            .or_insert((0, now));
        if now.saturating_sub(entry.1) >= self.window_secs {
            // Window expired, reset.
            *entry = (1, now);
            true
        } else if entry.0 < self.max_per_window {
            entry.0 += 1;
            true
        } else {
            false // Rate limited
        }
    }
}

/// Dispatches a `DaemonEvent` as a push notification to all registered devices.
///
/// This is synchronous (matches the daemon's existing `PushProvider::send`
/// signature). Callers should wrap this in `tokio::task::spawn_blocking` or
/// a spawned task to avoid blocking the event loop.
///
/// Returns the number of devices that were successfully notified.
pub(crate) fn dispatch_event_push(
    event: &DaemonEvent,
    registry: &PushRegistry,
    provider: &dyn PushProvider,
    telemetry_file: &Path,
    now: u64,
) -> usize {
    dispatch_event_push_full(event, registry, provider, telemetry_file, now, None, None)
}

/// Dispatches a `DaemonEvent` as a push notification with optional
/// preferences checking and rate limiting.
pub(crate) fn dispatch_event_push_full(
    event: &DaemonEvent,
    registry: &PushRegistry,
    provider: &dyn PushProvider,
    telemetry_file: &Path,
    now: u64,
    preferences: Option<&Mutex<PushPreferences>>,
    rate_limiter: Option<&Mutex<PushRateLimiter>>,
) -> usize {
    let (title, priority) = match classify_event(event) {
        Some(pair) => pair,
        None => return 0,
    };

    // Check preferences: is this event category enabled?
    if let Some(prefs_mutex) = preferences {
        if let Ok(prefs) = prefs_mutex.lock() {
            if !prefs.is_event_enabled(event.event_type.as_str(), &event.status) {
                tracing::debug!(
                    event_type = %event.event_type,
                    event_status = %event.status,
                    "push_dispatcher: suppressed by preferences"
                );
                return 0;
            }
        }
    }

    let devices = match registry.list() {
        Ok(devices) => devices,
        Err(err) => {
            tracing::warn!(
                error = %err,
                event_type = %event.event_type,
                "push_dispatcher: failed to list devices"
            );
            return 0;
        }
    };

    if devices.is_empty() {
        return 0;
    }

    let body = build_body(event);
    let rid = event
        .intake_run_id
        .as_deref()
        .or(event.goal_run_id.as_deref())
        .or(event.job_id.as_deref())
        .unwrap_or("unknown");

    // Thread ID for iOS notification grouping.
    let thread_id = event
        .intake_run_id
        .as_deref()
        .or(event.goal_run_id.as_deref())
        .unwrap_or("symbiotic")
        .to_string();

    let mut success_count = 0usize;
    for device in &devices {
        // Rate limiter check.
        if let Some(rl_mutex) = rate_limiter {
            if let Ok(mut rl) = rl_mutex.lock() {
                if !rl.check_and_increment(&device.device_id, now) {
                    tracing::warn!(
                        device_id = %device.device_id,
                        event_type = %event.event_type,
                        "push_dispatcher: rate limited"
                    );
                    continue;
                }
            }
        }

        // Increment badge count for this device.
        let badge = registry.increment_badge(&device.device_id);

        let notification = DaemonPushNotification {
            notification_id: format!(
                "push_{:x}",
                simple_hash(&format!(
                    "{}:{}:{}:{}",
                    device.device_id, event.event_type, rid, now
                ))
            ),
            device_id: device.device_id.clone(),
            token_hash: device.token_hash.clone(),
            encrypted_token: device.encrypted_token.clone(),
            platform: device.platform.clone(),
            priority: priority.to_string(),
            title: title.to_string(),
            body: body.clone(),
            rid: rid.to_string(),
            event_type: event.event_type.to_string(),
            event_status: event.status.clone(),
            ts: now,
            thread_id: Some(thread_id.clone()),
            badge: Some(badge),
        };

        match provider.send(&notification) {
            Ok(()) => {
                let _ = append_push_telemetry(telemetry_file, &notification, "sent", None);
                success_count += 1;
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    device_id = %device.device_id,
                    event_type = %event.event_type,
                    "push_dispatcher: delivery failed"
                );
                let _ = append_push_telemetry(
                    telemetry_file,
                    &notification,
                    "failed",
                    Some(&err.to_string()),
                );
            }
        }
    }

    if success_count > 0 {
        tracing::info!(
            event_type = %event.event_type,
            event_status = %event.status,
            devices = devices.len(),
            delivered = success_count,
            "push_dispatcher: notifications sent"
        );
    }

    success_count
}

/// Fire-and-forget push dispatch.
///
/// Spawns the push dispatch as a blocking task on the tokio runtime so it
/// never stalls the daemon's event loop. Errors are logged internally.
#[allow(dead_code)]
pub(crate) fn fire_push_for_event(
    event: DaemonEvent,
    registry: Arc<PushRegistry>,
    provider: Arc<dyn PushProvider>,
    telemetry_file: PathBuf,
    now: u64,
) {
    // Quick check: if the event doesn't classify as push-worthy, skip the spawn.
    if classify_event(&event).is_none() {
        return;
    }

    tokio::task::spawn_blocking(move || {
        dispatch_event_push(&event, &registry, &*provider, &telemetry_file, now);
    });
}

/// Fire-and-forget push dispatch with preferences and rate limiting.
#[allow(dead_code)]
pub(crate) fn fire_push_for_event_full(
    event: DaemonEvent,
    registry: Arc<PushRegistry>,
    provider: Arc<dyn PushProvider>,
    telemetry_file: PathBuf,
    now: u64,
    preferences: Arc<Mutex<PushPreferences>>,
    rate_limiter: Arc<Mutex<PushRateLimiter>>,
) {
    if classify_event(&event).is_none() {
        return;
    }

    tokio::task::spawn_blocking(move || {
        dispatch_event_push_full(
            &event,
            &registry,
            &*provider,
            &telemetry_file,
            now,
            Some(&preferences),
            Some(&rate_limiter),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::PushNotification as DaemonPushNotification;
    use anyhow::Result;
    use std::sync::Mutex;

    // --- Test helpers ---

    /// A push provider that records all notifications it receives.
    struct RecordingPushProvider {
        sent: Mutex<Vec<DaemonPushNotification>>,
    }

    impl RecordingPushProvider {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent_notifications(&self) -> Vec<DaemonPushNotification> {
            self.sent.lock().expect("lock").clone()
        }
    }

    impl PushProvider for RecordingPushProvider {
        fn send(&self, notification: &DaemonPushNotification) -> Result<()> {
            self.sent.lock().expect("lock").push(notification.clone());
            Ok(())
        }
    }

    /// A push provider that always fails.
    struct FailingPushProvider;

    impl PushProvider for FailingPushProvider {
        fn send(&self, _notification: &DaemonPushNotification) -> Result<()> {
            Err(anyhow::anyhow!("intentional push failure"))
        }
    }

    fn sample_event(event_type: EventType, status: &str) -> DaemonEvent {
        DaemonEvent {
            event_type,
            status: status.to_string(),
            job_id: Some("job-123".to_string()),
            detail: "test detail".to_string(),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: Some("run-456".to_string()),
            url: Some("https://example.com/article".to_string()),
            title: Some("Example Article".to_string()),
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        }
    }

    fn test_registry_and_telemetry() -> (PushRegistry, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "symbiotic_push_dispatch_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            rand_suffix(),
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");

        let registry_file = dir.join("tokens.tsv");
        let key_file = dir.join("token.key");
        let telemetry_file = dir.join("delivery.log");

        let registry = PushRegistry::open(&registry_file, &key_file).expect("open test registry");
        (registry, telemetry_file)
    }

    fn rand_suffix() -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        std::thread::current().id().hash(&mut h);
        std::time::Instant::now().hash(&mut h);
        h.finish()
    }

    // --- Tests ---

    #[test]
    fn classify_event_ingest_dlq_is_pushworthy() {
        let event = sample_event(EventType::IngestFetch, "dlq");
        let result = classify_event(&event);
        assert!(result.is_some());
        let (title, priority) = result.unwrap();
        assert_eq!(title, "Ingest failed (DLQ)");
        assert_eq!(priority, "high");
    }

    #[test]
    fn classify_event_ingest_retry_is_suppressed() {
        let event = sample_event(EventType::IngestFetch, "retry");
        assert!(classify_event(&event).is_none());
    }

    #[test]
    fn classify_event_ingest_completed_is_pushworthy() {
        let event = sample_event(EventType::IngestFetch, "completed");
        let result = classify_event(&event);
        assert!(result.is_some());
        let (title, _) = result.unwrap();
        assert_eq!(title, "Entry captured");
    }

    #[test]
    fn classify_event_workflow_completed_is_pushworthy() {
        let event = sample_event(EventType::WorkflowRun, "completed");
        assert!(classify_event(&event).is_some());
    }

    #[test]
    fn classify_event_unknown_type_is_suppressed() {
        let event = sample_event(EventType::GoalCreated, "completed");
        assert!(classify_event(&event).is_none());
    }

    #[test]
    fn classify_event_auth_failed_is_critical() {
        let event = sample_event(EventType::AuthIssue, "failed");
        let (_, priority) = classify_event(&event).unwrap();
        assert_eq!(priority, "critical");
    }

    #[test]
    fn build_body_includes_url_and_title() {
        let event = sample_event(EventType::IngestFetch, "completed");
        let body = build_body(&event);
        assert!(body.contains("https://example.com/article"));
        assert!(body.contains("Example Article"));
        assert!(body.contains("test detail"));
    }

    #[test]
    fn build_body_without_url() {
        let mut event = sample_event(EventType::WorkflowRun, "completed");
        event.url = None;
        event.title = None;
        let body = build_body(&event);
        assert_eq!(body, "test detail");
    }

    #[test]
    fn dispatch_with_no_devices_returns_zero() {
        let (registry, telemetry_file) = test_registry_and_telemetry();
        let provider = RecordingPushProvider::new();
        let event = sample_event(EventType::IngestFetch, "completed");

        let count = dispatch_event_push(&event, &registry, &provider, &telemetry_file, 1000);
        assert_eq!(count, 0);
        assert!(provider.sent_notifications().is_empty());
    }

    #[test]
    fn dispatch_sends_to_registered_device() {
        let (registry, telemetry_file) = test_registry_and_telemetry();
        registry
            .register("device-1", "token-abc", "apns", 1000)
            .expect("register device");

        let provider = RecordingPushProvider::new();
        let event = sample_event(EventType::IngestFetch, "completed");

        let count = dispatch_event_push(&event, &registry, &provider, &telemetry_file, 1001);
        assert_eq!(count, 1);

        let sent = provider.sent_notifications();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].device_id, "device-1");
        assert_eq!(sent[0].event_type, "ingest.fetch");
        assert_eq!(sent[0].event_status, "completed");
        assert_eq!(sent[0].title, "Entry captured");
        assert!(sent[0].body.contains("https://example.com/article"));
        assert_eq!(sent[0].rid, "run-456");
    }

    #[test]
    fn dispatch_sends_to_multiple_devices() {
        let (registry, telemetry_file) = test_registry_and_telemetry();
        registry
            .register("device-1", "token-abc", "apns", 1000)
            .expect("register");
        registry
            .register("device-2", "token-def", "fcm", 1000)
            .expect("register");

        let provider = RecordingPushProvider::new();
        let event = sample_event(EventType::IngestFetch, "dlq");

        let count = dispatch_event_push(&event, &registry, &provider, &telemetry_file, 1001);
        assert_eq!(count, 2);
        assert_eq!(provider.sent_notifications().len(), 2);
    }

    #[test]
    fn dispatch_skips_non_pushworthy_events() {
        let (registry, telemetry_file) = test_registry_and_telemetry();
        registry
            .register("device-1", "token-abc", "apns", 1000)
            .expect("register");

        let provider = RecordingPushProvider::new();
        let event = sample_event(EventType::IngestFetch, "retry"); // suppressed

        let count = dispatch_event_push(&event, &registry, &provider, &telemetry_file, 1001);
        assert_eq!(count, 0);
        assert!(provider.sent_notifications().is_empty());
    }

    #[test]
    fn dispatch_failure_does_not_panic() {
        let (registry, telemetry_file) = test_registry_and_telemetry();
        registry
            .register("device-1", "token-abc", "apns", 1000)
            .expect("register");

        let provider = FailingPushProvider;
        let event = sample_event(EventType::IngestFetch, "completed");

        // Should not panic, should return 0
        let count = dispatch_event_push(&event, &registry, &provider, &telemetry_file, 1001);
        assert_eq!(count, 0);
    }

    #[test]
    fn dispatch_writes_telemetry_on_success() {
        let (registry, telemetry_file) = test_registry_and_telemetry();
        registry
            .register("device-1", "token-abc", "apns", 1000)
            .expect("register");

        let provider = RecordingPushProvider::new();
        let event = sample_event(EventType::IngestFetch, "completed");

        dispatch_event_push(&event, &registry, &provider, &telemetry_file, 1001);

        let telemetry = std::fs::read_to_string(&telemetry_file).unwrap_or_default();
        assert!(telemetry.contains("\"status\":\"sent\""));
    }

    #[test]
    fn dispatch_writes_telemetry_on_failure() {
        let (registry, telemetry_file) = test_registry_and_telemetry();
        registry
            .register("device-1", "token-abc", "apns", 1000)
            .expect("register");

        let provider = FailingPushProvider;
        let event = sample_event(EventType::IngestFetch, "completed");

        dispatch_event_push(&event, &registry, &provider, &telemetry_file, 1001);

        let telemetry = std::fs::read_to_string(&telemetry_file).unwrap_or_default();
        assert!(telemetry.contains("\"status\":\"failed\""));
        assert!(telemetry.contains("intentional push failure"));
    }

    #[test]
    fn dispatch_uses_goal_run_id_when_no_intake_run_id() {
        let (registry, telemetry_file) = test_registry_and_telemetry();
        registry
            .register("device-1", "token-abc", "apns", 1000)
            .expect("register");

        let provider = RecordingPushProvider::new();
        let mut event = sample_event(EventType::WorkflowRun, "completed");
        event.intake_run_id = None;
        event.goal_run_id = Some("goal-789".to_string());
        event.url = None;
        event.title = None;

        dispatch_event_push(&event, &registry, &provider, &telemetry_file, 1001);

        let sent = provider.sent_notifications();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].rid, "goal-789");
    }

    #[test]
    fn fire_push_skips_non_pushworthy_without_spawn() {
        // Verify that fire_push_for_event returns immediately for suppressed events
        // (no tokio runtime needed because it exits before spawning).
        let event = sample_event(EventType::IngestFetch, "retry");
        let (registry, telemetry_file) = test_registry_and_telemetry();
        let provider = Arc::new(RecordingPushProvider::new());

        // This should NOT panic even without a tokio runtime because
        // classify_event returns None and it exits early.
        fire_push_for_event(
            event,
            Arc::new(registry),
            provider.clone(),
            telemetry_file,
            1000,
        );
        assert!(provider.sent_notifications().is_empty());
    }
}
