//! Push notification delivery for Symbiotic.
//!
//! This crate provides push notification delivery to iOS (APNs) and Android (FCM)
//! devices. It includes token management, payload construction, and multi-device
//! fan-out dispatch with automatic pruning of invalid tokens.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐     ┌──────────────────┐     ┌─────────────┐
//! │ PayloadBuilder│────▶│NotificationDispatcher│────▶│ PushGateway │
//! └─────────────┘     └──────────────────┘     │  (APNs/FCM) │
//!                            │                  └─────────────┘
//!                     ┌──────┴───────┐
//!                     │PushTokenStore│
//!                     │  (SQLite)    │
//!                     └──────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,no_run
//! use symbiotic_push::{
//!     dispatch::{NotificationDispatcher, DispatcherConfig},
//!     gateway::MockGateway,
//!     payload::PayloadBuilder,
//!     store::PushTokenStore,
//!     types::{PushProvider, PushToken},
//! };
//! use chrono::Utc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Set up the token store
//! let store = PushTokenStore::in_memory()?;
//!
//! // Register a device token
//! let token = PushToken {
//!     device_id: "iphone-abc".to_string(),
//!     platform: PushProvider::Apns,
//!     token: "device-token-from-apple".to_string(),
//!     user_id: "user-123".to_string(),
//!     registered_at: Utc::now(),
//!     expires_at: None,
//! };
//! store.register_token(&token)?;
//!
//! // Set up the dispatcher with a mock gateway
//! let mut dispatcher = NotificationDispatcher::new(DispatcherConfig::default());
//! dispatcher.register_gateway(Box::new(MockGateway::new(PushProvider::Apns)));
//!
//! // Build and dispatch a notification
//! let notification = PayloadBuilder::approval_request(
//!     "Deploy to production",
//!     "Agent wants to deploy v1.2.0 to production servers",
//! );
//!
//! let result = dispatcher.dispatch(&store, "user-123", |tok| {
//!     notification.clone().with_token(tok)
//! }).await?;
//!
//! assert_eq!(result.succeeded, 1);
//! # Ok(())
//! # }
//! ```

pub mod dispatch;
pub mod error;
pub mod gateway;
pub mod payload;
pub mod store;
pub mod types;

/// Test infrastructure for push notification integration tests.
///
/// Available when the `test-util` feature is enabled.
#[cfg(feature = "test-util")]
pub mod testutil;

#[cfg(test)]
mod tests;
