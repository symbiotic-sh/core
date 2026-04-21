//! Core types for push notification delivery.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Supported push notification providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushProvider {
    /// Apple Push Notification service.
    Apns,
    /// Firebase Cloud Messaging.
    Fcm,
}

impl std::fmt::Display for PushProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushProvider::Apns => write!(f, "apns"),
            PushProvider::Fcm => write!(f, "fcm"),
        }
    }
}

/// Priority level for push notifications.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushPriority {
    /// High priority — delivered immediately, wakes device.
    High,
    /// Normal priority — may be batched by the provider.
    #[default]
    Normal,
}

/// Categories of notifications, used for payload construction and routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationCategory {
    /// A new entry was captured in the Archive.
    NewEntry,
    /// An agent's status changed (started, completed, failed).
    AgentStatus,
    /// An action requires user approval.
    ApprovalRequest,
    /// Progress update on a goal.
    GoalUpdate,
    /// System-level notification.
    System,
}

impl std::fmt::Display for NotificationCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotificationCategory::NewEntry => write!(f, "new_entry"),
            NotificationCategory::AgentStatus => write!(f, "agent_status"),
            NotificationCategory::ApprovalRequest => write!(f, "approval_request"),
            NotificationCategory::GoalUpdate => write!(f, "goal_update"),
            NotificationCategory::System => write!(f, "system"),
        }
    }
}

/// A registered push token for a device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushToken {
    /// Unique device identifier.
    pub device_id: String,
    /// The push provider platform.
    pub platform: PushProvider,
    /// The raw push token string from the provider.
    pub token: String,
    /// The user this token belongs to.
    pub user_id: String,
    /// When this token was registered.
    pub registered_at: DateTime<Utc>,
    /// Optional expiry time for the token.
    pub expires_at: Option<DateTime<Utc>>,
}

/// A push notification ready to be sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushNotification {
    /// Notification title (displayed prominently).
    pub title: String,
    /// Notification body text.
    pub body: String,
    /// Custom data payload.
    #[serde(default)]
    pub data: HashMap<String, String>,
    /// Delivery priority.
    #[serde(default)]
    pub priority: PushPriority,
    /// Target push token (set during dispatch).
    pub token: Option<String>,
    /// Notification category for client-side handling.
    pub category: NotificationCategory,
    /// Badge count to display on app icon.
    pub badge: Option<u32>,
    /// Sound to play on delivery.
    pub sound: Option<String>,
    /// Thread identifier for grouping notifications.
    pub thread_id: Option<String>,
}

impl PushNotification {
    /// Create a new push notification with the given title, body, and category.
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        category: NotificationCategory,
    ) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            data: HashMap::new(),
            priority: PushPriority::Normal,
            token: None,
            category,
            badge: None,
            sound: None,
            thread_id: None,
        }
    }

    /// Add a key-value pair to the data payload.
    pub fn with_data(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.data.insert(key.into(), value.into());
        self
    }

    /// Set the badge count.
    pub fn with_badge(mut self, badge: u32) -> Self {
        self.badge = Some(badge);
        self
    }

    /// Set the notification sound.
    pub fn with_sound(mut self, sound: impl Into<String>) -> Self {
        self.sound = Some(sound.into());
        self
    }

    /// Set the thread identifier for notification grouping.
    pub fn with_thread_id(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    /// Set the delivery priority.
    pub fn with_priority(mut self, priority: PushPriority) -> Self {
        self.priority = priority;
        self
    }

    /// Set the target push token.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
}

/// Response from a push provider after attempting delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResponse {
    /// Whether the notification was accepted by the provider.
    pub success: bool,
    /// Provider-assigned message identifier (if successful).
    pub provider_message_id: Option<String>,
    /// Error reason from the provider (if failed).
    pub error_reason: Option<String>,
    /// Whether the token should be considered invalid and removed.
    pub token_invalid: bool,
}

/// Aggregate result of dispatching to multiple devices.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DispatchResult {
    /// Total number of notifications sent.
    pub total: u32,
    /// Number of successful deliveries.
    pub succeeded: u32,
    /// Number of failed deliveries.
    pub failed: u32,
    /// Tokens that were invalidated by their provider (410/GONE).
    pub invalidated_tokens: Vec<String>,
}

impl DispatchResult {
    /// Create a new empty dispatch result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful delivery.
    pub fn record_success(&mut self) {
        self.total += 1;
        self.succeeded += 1;
    }

    /// Record a failed delivery.
    pub fn record_failure(&mut self) {
        self.total += 1;
        self.failed += 1;
    }

    /// Record a token invalidation.
    pub fn record_invalidation(&mut self, token: String) {
        self.invalidated_tokens.push(token);
    }
}
