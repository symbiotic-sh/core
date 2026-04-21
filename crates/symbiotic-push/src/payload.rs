//! Factory methods for constructing push notification payloads by category.

use std::collections::HashMap;

use crate::types::{NotificationCategory, PushNotification, PushPriority};

/// Payload builder with factory methods for each notification category.
///
/// Produces pre-configured [`PushNotification`] instances with appropriate
/// defaults for priority, category, and data fields.
pub struct PayloadBuilder;

impl PayloadBuilder {
    /// Build a notification for a new Archive entry.
    ///
    /// Normal priority — the user will see it next time they check their device.
    pub fn new_entry(title: impl Into<String>, source_url: impl Into<String>) -> PushNotification {
        PushNotification::new(title, "New entry captured", NotificationCategory::NewEntry)
            .with_data("source_url", source_url)
            .with_sound("default")
    }

    /// Build a notification for an agent status change.
    ///
    /// Failed agents get High priority; other statuses get Normal.
    pub fn agent_status(
        agent_name: impl Into<String>,
        status: impl Into<String>,
        detail: impl Into<String>,
    ) -> PushNotification {
        let agent_name = agent_name.into();
        let status = status.into();
        let detail = detail.into();

        let title = format!("Agent: {agent_name}");
        let body = format!("{status} — {detail}");
        let priority = if status == "failed" {
            PushPriority::High
        } else {
            PushPriority::Normal
        };

        PushNotification::new(title, body, NotificationCategory::AgentStatus)
            .with_data("agent_name", agent_name)
            .with_data("status", status)
            .with_priority(priority)
    }

    /// Build a notification for an approval request.
    ///
    /// Always High priority — the user needs to act.
    pub fn approval_request(
        action: impl Into<String>,
        description: impl Into<String>,
    ) -> PushNotification {
        let action = action.into();
        let description = description.into();

        PushNotification::new(
            format!("Approval needed: {action}"),
            &description,
            NotificationCategory::ApprovalRequest,
        )
        .with_data("action", action)
        .with_priority(PushPriority::High)
        .with_sound("default")
    }

    /// Build a notification for a goal progress update.
    pub fn goal_update(
        goal_name: impl Into<String>,
        progress: impl Into<String>,
    ) -> PushNotification {
        let goal_name = goal_name.into();
        let progress = progress.into();

        PushNotification::new(
            format!("Goal: {goal_name}"),
            &progress,
            NotificationCategory::GoalUpdate,
        )
        .with_data("goal_name", goal_name)
        .with_data("progress", progress)
    }

    /// Build a system notification.
    pub fn system(title: impl Into<String>, body: impl Into<String>) -> PushNotification {
        PushNotification::new(title, body, NotificationCategory::System)
    }

    /// Build a notification from an arbitrary event with category, title, body, and data.
    pub fn from_event(
        category: NotificationCategory,
        title: impl Into<String>,
        body: impl Into<String>,
        data: HashMap<String, String>,
    ) -> PushNotification {
        let mut notification = PushNotification::new(title, body, category);
        for (key, value) in data {
            notification = notification.with_data(key, value);
        }
        notification
    }
}
