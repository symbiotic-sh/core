//! Notification dispatcher — fan-out to user devices with retry and auto-prune.

use std::collections::HashMap;

use crate::error::PushError;
use crate::gateway::PushGateway;
use crate::payload::PayloadBuilder;
use crate::store::PushTokenStore;
use crate::types::{
    DispatchResult, NotificationCategory, PushNotification, PushProvider, PushResponse,
};

/// Configuration for the notification dispatcher.
#[derive(Debug, Clone)]
pub struct DispatcherConfig {
    /// Maximum number of retry attempts for transient failures.
    pub max_retries: u32,
    /// Whether to automatically remove tokens that providers report as invalid (410/GONE).
    pub auto_prune_invalid: bool,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            max_retries: 1,
            auto_prune_invalid: true,
        }
    }
}

/// Dispatches push notifications to devices via registered gateways.
///
/// Holds one gateway per [`PushProvider`] and fans out notifications to
/// all of a user's registered devices.
pub struct NotificationDispatcher {
    gateways: HashMap<PushProvider, Box<dyn PushGateway>>,
    config: DispatcherConfig,
}

impl NotificationDispatcher {
    /// Create a new dispatcher with the given configuration.
    pub fn new(config: DispatcherConfig) -> Self {
        Self {
            gateways: HashMap::new(),
            config,
        }
    }

    /// Create a dispatcher with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(DispatcherConfig::default())
    }

    /// Register a push gateway for a specific provider.
    ///
    /// Replaces any previously registered gateway for the same provider.
    pub fn register_gateway(&mut self, gateway: Box<dyn PushGateway>) {
        let provider = gateway.provider();
        tracing::info!(?provider, "registered push gateway");
        self.gateways.insert(provider, gateway);
    }

    /// Dispatch a notification to all of a user's registered devices.
    ///
    /// The `build_fn` closure receives each device's token and returns the
    /// notification to send. This allows customizing per-device if needed.
    pub async fn dispatch<F>(
        &self,
        store: &PushTokenStore,
        user_id: &str,
        build_fn: F,
    ) -> Result<DispatchResult, PushError>
    where
        F: Fn(&str) -> PushNotification,
    {
        let tokens = store.get_tokens_for_user(user_id)?;
        let mut result = DispatchResult::new();

        for push_token in &tokens {
            let gateway = match self.gateways.get(&push_token.platform) {
                Some(gw) => gw,
                None => {
                    tracing::warn!(
                        platform = %push_token.platform,
                        device_id = %push_token.device_id,
                        "no gateway registered for platform, skipping"
                    );
                    result.record_failure();
                    continue;
                }
            };

            let notification = build_fn(&push_token.token).with_token(&push_token.token);

            match self.send_with_retry(gateway.as_ref(), &notification).await {
                Ok(response) => {
                    if response.success {
                        result.record_success();
                    } else {
                        result.record_failure();
                        if response.token_invalid {
                            result.record_invalidation(push_token.token.clone());
                            if self.config.auto_prune_invalid {
                                let _ = store.remove_by_token_string(&push_token.token);
                                tracing::info!(
                                    device_id = %push_token.device_id,
                                    platform = %push_token.platform,
                                    "auto-pruned invalid token"
                                );
                            }
                        }
                    }
                }
                Err(_) => {
                    result.record_failure();
                }
            }
        }

        Ok(result)
    }

    /// Send a single notification to one device.
    pub async fn dispatch_single(
        &self,
        notification: &PushNotification,
        provider: PushProvider,
    ) -> Result<PushResponse, PushError> {
        let gateway = self
            .gateways
            .get(&provider)
            .ok_or_else(|| PushError::ProviderUnavailable(format!("no gateway for {provider}")))?;

        self.send_with_retry(gateway.as_ref(), notification).await
    }

    /// Convenience method: dispatch a notification from an event to all user devices.
    pub async fn dispatch_event(
        &self,
        store: &PushTokenStore,
        user_id: &str,
        category: NotificationCategory,
        title: String,
        body: String,
        data: HashMap<String, String>,
    ) -> Result<DispatchResult, PushError> {
        let notification = PayloadBuilder::from_event(category, &title, &body, data);
        self.dispatch(store, user_id, |token| {
            notification.clone().with_token(token)
        })
        .await
    }

    /// Send with retry logic for transient failures.
    async fn send_with_retry(
        &self,
        gateway: &dyn PushGateway,
        notification: &PushNotification,
    ) -> Result<PushResponse, PushError> {
        let mut last_err = None;
        let attempts = 1 + self.config.max_retries;

        for attempt in 0..attempts {
            match gateway.send(notification).await {
                Ok(response) => return Ok(response),
                Err(PushError::RateLimited { retry_after_secs }) => {
                    if attempt + 1 < attempts {
                        tracing::debug!(attempt, retry_after_secs, "rate limited, will retry");
                        tokio::time::sleep(std::time::Duration::from_secs(
                            retry_after_secs.min(30),
                        ))
                        .await;
                    }
                    last_err = Some(PushError::RateLimited { retry_after_secs });
                }
                Err(PushError::SendFailed(msg)) => {
                    if attempt + 1 < attempts {
                        tracing::debug!(attempt, %msg, "send failed, will retry");
                    }
                    last_err = Some(PushError::SendFailed(msg));
                }
                Err(e) => {
                    // Non-retryable errors: return immediately
                    return Err(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| PushError::SendFailed("unknown error".into())))
    }
}
