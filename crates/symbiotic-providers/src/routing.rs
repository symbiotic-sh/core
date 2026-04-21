//! Generic provider router with sensitivity-aware routing, budget enforcement,
//! health checking, and retry with exponential backoff.
//!
//! [`ProviderRouter`] generalises the T84 `EmbeddingRouter` pattern across all
//! modalities (completions, embeddings, image generation, video generation, and
//! agent tasks). The selection algorithm:
//!
//! 1. **ProviderCapability** — only consider providers that support the requested capability.
//! 2. **Sensitivity** — `Restricted`/`Private` content may only use `Local` providers.
//! 3. **Budget** — skip providers that are over budget.
//! 4. **Health** — skip providers that have failed recent health checks (optional).
//! 5. **Preference** — prefer the configured default, then fall back to any eligible.
//!
//! Retries use exponential backoff: on `Unavailable` or `RateLimited`, wait and
//! retry up to `max_retries` times, then move to the next candidate.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use symbiotic_core::Sensitivity;

use crate::budget::BudgetEnforcer;
use crate::error::ProviderError;
use crate::registry::{ProviderRegistry, RegisteredProvider};
use crate::types::{
    CompletionRequest, CompletionResponse, EmbedResult, ImageRequest, ImageResponse, ModelHint,
    ProviderCapability, ProviderClass, RequestType, TaskRequest, TaskSession, VideoRequest,
    VideoResponse,
};

/// Minimal health check interface.
///
/// Implementations can wrap T83's `LlmRuntime` or any other health-monitoring
/// system to report whether a provider is currently reachable.
#[async_trait]
pub trait HealthChecker: Send + Sync {
    /// Returns `true` if the provider is believed to be healthy and reachable.
    async fn is_healthy(&self, provider_name: &str) -> bool;
}

/// Configuration for retry behaviour with exponential backoff.
#[derive(Debug, Clone, Copy)]
pub struct RetryConfig {
    /// Maximum number of retries per candidate provider.
    pub max_retries: u32,
    /// Initial backoff duration in milliseconds before the first retry.
    pub initial_backoff_ms: u64,
    /// Multiplier applied to the backoff duration after each retry.
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 1000,
            backoff_multiplier: 2.0,
        }
    }
}

/// Generic provider router with sensitivity, budget, health, and retry support.
///
/// Use the builder-style methods [`with_budget`](Self::with_budget),
/// [`with_retry_config`](Self::with_retry_config), and
/// [`with_health_checker`](Self::with_health_checker) to configure optional
/// features after construction.
///
/// The registry is wrapped in [`RwLock`] to support runtime provider
/// registration (e.g. when a user submits an API key via the credentials room).
pub struct ProviderRouter {
    registry: Arc<RwLock<ProviderRegistry>>,
    budget: Option<Arc<BudgetEnforcer>>,
    retry_config: RetryConfig,
    health_checker: Option<Arc<dyn HealthChecker>>,
}

impl ProviderRouter {
    /// Create a new router backed by the given provider registry.
    pub fn new(registry: Arc<RwLock<ProviderRegistry>>) -> Self {
        Self {
            registry,
            budget: None,
            retry_config: RetryConfig::default(),
            health_checker: None,
        }
    }

    /// Access the underlying registry for runtime provider management.
    ///
    /// Use this to register new providers or change defaults at runtime
    /// (e.g. when a user submits an API key via the credentials room).
    pub fn registry(&self) -> &Arc<RwLock<ProviderRegistry>> {
        &self.registry
    }

    /// Attach a budget enforcer. Providers that are over-budget will be skipped.
    pub fn with_budget(mut self, budget: Arc<BudgetEnforcer>) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Override the default retry configuration.
    pub fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Attach a health checker. Unhealthy providers will be skipped.
    pub fn with_health_checker(mut self, checker: Arc<dyn HealthChecker>) -> Self {
        self.health_checker = Some(checker);
        self
    }

    /// Route a text completion request with sensitivity awareness.
    ///
    /// The [`CompletionRequest::model_hint`] field is used to re-order
    /// candidates: `CheapFast` prefers haiku/flash/mini models, `MostCapable`
    /// prefers sonnet/opus/gpt-4o. `Default` preserves the normal ordering.
    pub async fn complete(
        &self,
        request: &CompletionRequest,
        sensitivity: Sensitivity,
        source: &str,
    ) -> Result<CompletionResponse, ProviderError> {
        let candidates = self
            .select_candidates(
                ProviderCapability::Completion,
                sensitivity,
                request.model_hint,
            )
            .await?;

        let mut last_err = ProviderError::Unavailable("no completion providers available".into());

        for candidate in &candidates {
            let provider =
                candidate
                    .completion
                    .as_ref()
                    .ok_or(ProviderError::UnsupportedCapability(
                        ProviderCapability::Completion,
                    ))?;

            match self
                .retry(candidate.base.name(), || async {
                    let _ = source; // captured for future metering integration
                    provider.complete(request).await
                })
                .await
            {
                Ok(response) => return Ok(response),
                Err(e) => {
                    last_err = e;
                    continue;
                }
            }
        }

        Err(last_err)
    }

    /// Route an embedding request with sensitivity awareness.
    pub async fn embed(
        &self,
        text: &str,
        sensitivity: Sensitivity,
    ) -> Result<EmbedResult, ProviderError> {
        let candidates = self
            .select_candidates(
                ProviderCapability::Embedding,
                sensitivity,
                ModelHint::Default,
            )
            .await?;

        let mut last_err = ProviderError::Unavailable("no embedding providers available".into());

        for candidate in &candidates {
            let provider =
                candidate
                    .embedding
                    .as_ref()
                    .ok_or(ProviderError::UnsupportedCapability(
                        ProviderCapability::Embedding,
                    ))?;

            match self
                .retry(candidate.base.name(), || async {
                    provider.embed(text).await
                })
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    last_err = e;
                    continue;
                }
            }
        }

        Err(last_err)
    }

    /// Route an image generation request with sensitivity awareness.
    pub async fn generate_image(
        &self,
        request: &ImageRequest,
        sensitivity: Sensitivity,
    ) -> Result<ImageResponse, ProviderError> {
        let candidates = self
            .select_candidates(
                ProviderCapability::ImageGeneration,
                sensitivity,
                ModelHint::Default,
            )
            .await?;

        let mut last_err =
            ProviderError::Unavailable("no image generation providers available".into());

        for candidate in &candidates {
            let provider = candidate
                .image
                .as_ref()
                .ok_or(ProviderError::UnsupportedCapability(
                    ProviderCapability::ImageGeneration,
                ))?;

            match self
                .retry(candidate.base.name(), || async {
                    provider.generate_image(request).await
                })
                .await
            {
                Ok(response) => return Ok(response),
                Err(e) => {
                    last_err = e;
                    continue;
                }
            }
        }

        Err(last_err)
    }

    /// Route a video generation request with sensitivity awareness.
    pub async fn generate_video(
        &self,
        request: &VideoRequest,
        sensitivity: Sensitivity,
    ) -> Result<VideoResponse, ProviderError> {
        let candidates = self
            .select_candidates(
                ProviderCapability::VideoGeneration,
                sensitivity,
                ModelHint::Default,
            )
            .await?;

        let mut last_err =
            ProviderError::Unavailable("no video generation providers available".into());

        for candidate in &candidates {
            let provider = candidate
                .video
                .as_ref()
                .ok_or(ProviderError::UnsupportedCapability(
                    ProviderCapability::VideoGeneration,
                ))?;

            match self
                .retry(candidate.base.name(), || async {
                    provider.generate_video(request).await
                })
                .await
            {
                Ok(response) => return Ok(response),
                Err(e) => {
                    last_err = e;
                    continue;
                }
            }
        }

        Err(last_err)
    }

    /// Submit an agent task to the best available agent provider.
    pub async fn submit_agent_task(
        &self,
        request: &TaskRequest,
        sensitivity: Sensitivity,
    ) -> Result<TaskSession, ProviderError> {
        let candidates = self
            .select_candidates(
                ProviderCapability::AgentExecution,
                sensitivity,
                ModelHint::Default,
            )
            .await?;

        let mut last_err =
            ProviderError::Unavailable("no agent execution providers available".into());

        for candidate in &candidates {
            let provider = candidate
                .agent
                .as_ref()
                .ok_or(ProviderError::UnsupportedCapability(
                    ProviderCapability::AgentExecution,
                ))?;

            match self
                .retry(candidate.base.name(), || async {
                    provider.submit_task(request).await
                })
                .await
            {
                Ok(session) => return Ok(session),
                Err(e) => {
                    last_err = e;
                    continue;
                }
            }
        }

        Err(last_err)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Select eligible provider candidates, ordered by preference.
    ///
    /// Acquires a read lock on the registry, clones the matching providers
    /// (cheap — all fields are `Arc`), and releases the lock before any async
    /// work (health checks). This avoids holding the lock across `.await`.
    ///
    /// After all filtering (capability, sensitivity, budget, health), the
    /// candidates are re-ordered based on:
    /// 1. The `model_hint` — matching models are moved to the front.
    /// 2. The configured default — if no hint match, the default is first.
    async fn select_candidates(
        &self,
        capability: ProviderCapability,
        sensitivity: Sensitivity,
        model_hint: ModelHint,
    ) -> Result<Vec<RegisteredProvider>, ProviderError> {
        // Snapshot providers and default under a short-lived read lock.
        let (all, default_name) = {
            let reg = self
                .registry
                .read()
                .map_err(|e| ProviderError::Unavailable(format!("registry lock poisoned: {e}")))?;
            let providers: Vec<RegisteredProvider> =
                reg.by_capability(capability).into_iter().cloned().collect();
            let default = reg
                .default_for(capability)
                .map(|p| p.base.name().to_string());
            (providers, default)
        };
        // Lock is released here.

        if all.is_empty() {
            return Err(ProviderError::Unavailable(format!(
                "no providers registered for {capability:?}"
            )));
        }

        let request_type = capability_to_request_type(capability);

        // Filter by sensitivity: Restricted/Private → Local only.
        let sensitivity_filtered: Vec<RegisteredProvider> =
            if matches!(sensitivity, Sensitivity::Restricted | Sensitivity::Private) {
                all.into_iter()
                    .filter(|p| p.base.provider_class() == ProviderClass::Local)
                    .collect()
            } else {
                all
            };

        if sensitivity_filtered.is_empty() {
            return Err(ProviderError::SensitivityViolation {
                sensitivity,
                provider_class: ProviderClass::Cloud,
            });
        }

        // Filter by budget.
        let budget_filtered: Vec<RegisteredProvider> = if let Some(ref budget) = self.budget {
            sensitivity_filtered
                .into_iter()
                .filter(|p| budget.check(p.base.name(), request_type).is_ok())
                .collect()
        } else {
            sensitivity_filtered
        };

        if budget_filtered.is_empty() {
            return Err(ProviderError::BudgetExceeded(
                "all eligible providers are over budget".into(),
            ));
        }

        // Filter by health (async — lock already released).
        let health_filtered: Vec<RegisteredProvider> =
            if let Some(ref checker) = self.health_checker {
                let mut healthy = Vec::new();
                for p in budget_filtered {
                    if checker.is_healthy(p.base.name()).await {
                        healthy.push(p);
                    }
                }
                healthy
            } else {
                budget_filtered
            };

        if health_filtered.is_empty() {
            return Err(ProviderError::Unavailable(
                "all eligible providers are unhealthy".into(),
            ));
        }

        // Order candidates by preference:
        // 1. If a ModelHint is active (not Default), matching models go first.
        // 2. The configured default provider comes next.
        // 3. Everything else in original order.
        let mut hint_matches = Vec::new();
        let mut non_matches = Vec::new();

        if model_hint != ModelHint::Default {
            for p in &health_filtered {
                if model_hint.matches_model_name(p.base.model_name()) {
                    hint_matches.push(p.clone());
                } else {
                    non_matches.push(p.clone());
                }
            }
        } else {
            non_matches = health_filtered;
        }

        let mut ordered = Vec::with_capacity(hint_matches.len() + non_matches.len());

        // Push hint-matched models first.
        ordered.extend(hint_matches);

        // Among non-matches, push the default first if present.
        if let Some(ref name) = default_name {
            if let Some(pos) = non_matches
                .iter()
                .position(|p| p.base.name() == name.as_str())
            {
                // Only push if not already in ordered (could be a hint match too).
                let already = ordered.iter().any(|o| o.base.name() == name.as_str());
                if !already {
                    ordered.push(non_matches[pos].clone());
                }
            }
        }

        // Push the rest in their original order.
        for p in &non_matches {
            let already = ordered.iter().any(|o| o.base.name() == p.base.name());
            if !already {
                ordered.push(p.clone());
            }
        }

        Ok(ordered)
    }

    /// Execute an async operation with exponential-backoff retry.
    ///
    /// Retries on [`ProviderError::Unavailable`] and [`ProviderError::RateLimited`].
    /// All other errors are returned immediately.
    async fn retry<F, Fut, T>(
        &self,
        _provider_name: &str,
        mut operation: F,
    ) -> Result<T, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, ProviderError>>,
    {
        let mut backoff_ms = self.retry_config.initial_backoff_ms;
        let mut attempts = 0u32;

        loop {
            match operation().await {
                Ok(result) => return Ok(result),
                Err(e) if is_retryable(&e) && attempts < self.retry_config.max_retries => {
                    // For RateLimited, prefer the server's suggested backoff.
                    let wait = if let ProviderError::RateLimited { retry_after_ms } = &e {
                        *retry_after_ms
                    } else {
                        backoff_ms
                    };

                    tokio::time::sleep(tokio::time::Duration::from_millis(wait)).await;

                    backoff_ms = (backoff_ms as f64 * self.retry_config.backoff_multiplier) as u64;
                    attempts += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Map a capability to the corresponding request type for budget checking.
fn capability_to_request_type(cap: ProviderCapability) -> RequestType {
    match cap {
        ProviderCapability::Completion => RequestType::Completion,
        ProviderCapability::Embedding => RequestType::Embedding,
        ProviderCapability::ImageGeneration => RequestType::ImageGeneration,
        ProviderCapability::VideoGeneration => RequestType::VideoGeneration,
        ProviderCapability::AgentExecution => RequestType::AgentTask,
        // FunctionCall and Vision are sub-capabilities of completion.
        ProviderCapability::FunctionCall | ProviderCapability::Vision => RequestType::Completion,
    }
}

/// Returns `true` for errors that are worth retrying.
fn is_retryable(err: &ProviderError) -> bool {
    matches!(
        err,
        ProviderError::Unavailable(_) | ProviderError::RateLimited { .. }
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::*;
    use crate::types::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, RwLock};

    // -- Mock completion provider ---------------------------------------------

    struct MockCompletionProvider {
        name: String,
        class: ProviderClass,
        model: String,
        capabilities: CapabilitySet,
        response_content: String,
        call_count: AtomicU32,
        should_fail: bool,
    }

    impl MockCompletionProvider {
        fn new(name: &str, class: ProviderClass, response: &str) -> Self {
            Self {
                name: name.to_string(),
                class,
                model: format!("{name}-model"),
                capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
                response_content: response.to_string(),
                call_count: AtomicU32::new(0),
                should_fail: false,
            }
        }

        fn with_model(name: &str, model: &str, class: ProviderClass, response: &str) -> Self {
            Self {
                name: name.to_string(),
                class,
                model: model.to_string(),
                capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
                response_content: response.to_string(),
                call_count: AtomicU32::new(0),
                should_fail: false,
            }
        }

        fn failing(name: &str, class: ProviderClass) -> Self {
            Self {
                name: name.to_string(),
                class,
                model: format!("{name}-model"),
                capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
                response_content: String::new(),
                call_count: AtomicU32::new(0),
                should_fail: true,
            }
        }
    }

    impl ModelProvider for MockCompletionProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn provider_class(&self) -> ProviderClass {
            self.class
        }
        fn model_name(&self) -> &str {
            &self.model
        }
        fn capabilities(&self) -> &CapabilitySet {
            &self.capabilities
        }
        fn pricing(&self) -> Option<&PricingInfo> {
            None
        }
    }

    #[async_trait]
    impl CompletionProvider for MockCompletionProvider {
        async fn complete(
            &self,
            _request: &CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.should_fail {
                return Err(ProviderError::Unavailable(format!("{} is down", self.name)));
            }
            Ok(CompletionResponse {
                content: self.response_content.clone(),
                model: self.model.clone(),
                input_tokens: Some(10),
                output_tokens: Some(20),
                finish_reason: Some("stop".into()),
            })
        }
    }

    // -- Mock embedding provider ----------------------------------------------

    struct MockEmbeddingProvider {
        name: String,
        class: ProviderClass,
        model: String,
        capabilities: CapabilitySet,
    }

    impl MockEmbeddingProvider {
        fn new(name: &str, class: ProviderClass) -> Self {
            Self {
                name: name.to_string(),
                class,
                model: format!("{name}-model"),
                capabilities: CapabilitySet::new(vec![ProviderCapability::Embedding]),
            }
        }
    }

    impl ModelProvider for MockEmbeddingProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn provider_class(&self) -> ProviderClass {
            self.class
        }
        fn model_name(&self) -> &str {
            &self.model
        }
        fn capabilities(&self) -> &CapabilitySet {
            &self.capabilities
        }
        fn pricing(&self) -> Option<&PricingInfo> {
            None
        }
    }

    #[async_trait]
    impl EmbeddingProvider for MockEmbeddingProvider {
        async fn embed(&self, _text: &str) -> Result<EmbedResult, ProviderError> {
            Ok(EmbedResult {
                embedding: vec![0.1, 0.2, 0.3],
                model_name: self.model.clone(),
                dimensions: 3,
            })
        }
    }

    // -- Mock health checker --------------------------------------------------

    struct MockHealthChecker {
        unhealthy: Vec<String>,
    }

    #[async_trait]
    impl HealthChecker for MockHealthChecker {
        async fn is_healthy(&self, provider_name: &str) -> bool {
            !self.unhealthy.contains(&provider_name.to_string())
        }
    }

    // -- Helpers --------------------------------------------------------------

    fn make_request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![ChatMessage {
                role: Role::User,
                content: "Hello".into(),
            }],
            max_tokens: Some(100),
            temperature: None,
            stop: None,
            model_hint: ModelHint::Default,
        }
    }

    fn make_request_with_hint(hint: ModelHint) -> CompletionRequest {
        CompletionRequest {
            messages: vec![ChatMessage {
                role: Role::User,
                content: "Hello".into(),
            }],
            max_tokens: Some(100),
            temperature: None,
            stop: None,
            model_hint: hint,
        }
    }

    fn register_completion(registry: &mut ProviderRegistry, provider: Arc<MockCompletionProvider>) {
        let base: Arc<dyn ModelProvider> = provider.clone();
        let completion: Arc<dyn CompletionProvider> = provider;
        registry.register(crate::registry::RegisteredProvider {
            base,
            completion: Some(completion),
            embedding: None,
            image: None,
            video: None,
            agent: None,
        });
    }

    fn register_embedding(registry: &mut ProviderRegistry, provider: Arc<MockEmbeddingProvider>) {
        let base: Arc<dyn ModelProvider> = provider.clone();
        let embedding: Arc<dyn EmbeddingProvider> = provider;
        registry.register(crate::registry::RegisteredProvider {
            base,
            completion: None,
            embedding: Some(embedding),
            image: None,
            video: None,
            agent: None,
        });
    }

    // -- Tests ----------------------------------------------------------------

    #[tokio::test]
    async fn routes_shareable_content_to_cloud_provider() {
        let mut registry = ProviderRegistry::new();
        let cloud = Arc::new(MockCompletionProvider::new(
            "openai",
            ProviderClass::Cloud,
            "cloud response",
        ));
        register_completion(&mut registry, cloud);

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(&make_request(), Sensitivity::Shareable, "test")
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "cloud response");
    }

    #[tokio::test]
    async fn routes_restricted_content_to_local_only() {
        let mut registry = ProviderRegistry::new();

        let cloud = Arc::new(MockCompletionProvider::new(
            "openai",
            ProviderClass::Cloud,
            "cloud response",
        ));
        let local = Arc::new(MockCompletionProvider::new(
            "ollama",
            ProviderClass::Local,
            "local response",
        ));
        register_completion(&mut registry, cloud);
        register_completion(&mut registry, local);

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(&make_request(), Sensitivity::Restricted, "test")
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "local response");
    }

    #[tokio::test]
    async fn routes_private_content_to_local_only() {
        let mut registry = ProviderRegistry::new();

        let cloud = Arc::new(MockCompletionProvider::new(
            "openai",
            ProviderClass::Cloud,
            "cloud response",
        ));
        let local = Arc::new(MockCompletionProvider::new(
            "ollama",
            ProviderClass::Local,
            "local response",
        ));
        register_completion(&mut registry, cloud);
        register_completion(&mut registry, local);

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(&make_request(), Sensitivity::Private, "test")
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "local response");
    }

    #[tokio::test]
    async fn returns_sensitivity_violation_when_no_local_provider() {
        let mut registry = ProviderRegistry::new();
        let cloud = Arc::new(MockCompletionProvider::new(
            "openai",
            ProviderClass::Cloud,
            "cloud response",
        ));
        register_completion(&mut registry, cloud);

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(&make_request(), Sensitivity::Restricted, "test")
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::SensitivityViolation {
                sensitivity,
                provider_class,
            } => {
                assert_eq!(sensitivity, Sensitivity::Restricted);
                assert_eq!(provider_class, ProviderClass::Cloud);
            }
            other => panic!("expected SensitivityViolation, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn budget_enforcement_skips_over_budget_providers() {
        let mut registry = ProviderRegistry::new();

        let expensive = Arc::new(MockCompletionProvider::new(
            "expensive",
            ProviderClass::Cloud,
            "expensive response",
        ));
        let cheap = Arc::new(MockCompletionProvider::new(
            "cheap",
            ProviderClass::Cloud,
            "cheap response",
        ));
        register_completion(&mut registry, expensive);
        register_completion(&mut registry, cheap);

        // Set "expensive" as default so it would normally be preferred.
        registry
            .set_default(ProviderCapability::Completion, "expensive")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));

        // Create a budget that blocks "expensive" but allows "cheap".
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("usage.ndjson");
        let log = Arc::new(crate::metering::UsageLog::open(&log_path).unwrap());

        // Record enough spend for "expensive" to be over budget.
        log.record(&UsageRecord {
            provider: "expensive".to_string(),
            model: "test".to_string(),
            timestamp: symbiotic_core::now_unix(),
            input_tokens: 100,
            output_tokens: 50,
            media_units: 0,
            cost_usd: Some(5.0),
            request_type: RequestType::Completion,
            source: "test".to_string(),
            session_id: None,
        })
        .unwrap();

        let mut per_provider = std::collections::HashMap::new();
        per_provider.insert(
            "expensive".to_string(),
            crate::config::ProviderBudget {
                daily_limit_usd: Some(1.0), // already spent $5
                monthly_limit_usd: None,
                max_tokens_per_request: None,
                max_media_units_per_day: None,
                max_agent_tasks_per_day: None,
            },
        );

        let budget_config = crate::config::BudgetConfig {
            global_daily_limit_usd: None,
            per_provider,
            alert_threshold_percent: 80.0,
        };

        let budget = Arc::new(BudgetEnforcer::new(budget_config, log));
        let router = ProviderRouter::new(registry).with_budget(budget);

        let result = router
            .complete(&make_request(), Sensitivity::Shareable, "test")
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "cheap response");
    }

    #[tokio::test]
    async fn falls_back_to_next_provider_when_default_fails() {
        let mut registry = ProviderRegistry::new();

        let failing = Arc::new(MockCompletionProvider::failing(
            "primary",
            ProviderClass::Cloud,
        ));
        let fallback = Arc::new(MockCompletionProvider::new(
            "fallback",
            ProviderClass::Cloud,
            "fallback response",
        ));
        register_completion(&mut registry, failing);
        register_completion(&mut registry, fallback);

        registry
            .set_default(ProviderCapability::Completion, "primary")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));
        // Use zero retries so we immediately fall through to the next candidate.
        let router = ProviderRouter::new(registry).with_retry_config(RetryConfig {
            max_retries: 0,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
        });

        let result = router
            .complete(&make_request(), Sensitivity::Shareable, "test")
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "fallback response");
    }

    #[tokio::test]
    async fn health_checker_skips_unhealthy_provider() {
        let mut registry = ProviderRegistry::new();

        let unhealthy = Arc::new(MockCompletionProvider::new(
            "sick",
            ProviderClass::Cloud,
            "sick response",
        ));
        let healthy = Arc::new(MockCompletionProvider::new(
            "healthy",
            ProviderClass::Cloud,
            "healthy response",
        ));
        register_completion(&mut registry, unhealthy);
        register_completion(&mut registry, healthy);

        registry
            .set_default(ProviderCapability::Completion, "sick")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));
        let checker = Arc::new(MockHealthChecker {
            unhealthy: vec!["sick".to_string()],
        });

        let router = ProviderRouter::new(registry).with_health_checker(checker);

        let result = router
            .complete(&make_request(), Sensitivity::Shareable, "test")
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "healthy response");
    }

    #[tokio::test]
    async fn prefers_default_provider_when_eligible() {
        let mut registry = ProviderRegistry::new();

        let a = Arc::new(MockCompletionProvider::new(
            "provider-a",
            ProviderClass::Cloud,
            "response-a",
        ));
        let b = Arc::new(MockCompletionProvider::new(
            "provider-b",
            ProviderClass::Cloud,
            "response-b",
        ));
        register_completion(&mut registry, a);
        register_completion(&mut registry, b);

        registry
            .set_default(ProviderCapability::Completion, "provider-b")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(&make_request(), Sensitivity::Shareable, "test")
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "response-b");
    }

    #[tokio::test]
    async fn embedding_route_works() {
        let mut registry = ProviderRegistry::new();
        let provider = Arc::new(MockEmbeddingProvider::new("ollama", ProviderClass::Local));
        register_embedding(&mut registry, provider);

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router.embed("hello", Sensitivity::Shareable).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().dimensions, 3);
    }

    #[tokio::test]
    async fn embedding_restricted_requires_local() {
        let mut registry = ProviderRegistry::new();
        let cloud = Arc::new(MockEmbeddingProvider::new("openai", ProviderClass::Cloud));
        register_embedding(&mut registry, cloud);

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router.embed("secret", Sensitivity::Restricted).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::SensitivityViolation { .. } => {}
            other => panic!("expected SensitivityViolation, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_providers_returns_unavailable() {
        let registry = Arc::new(RwLock::new(ProviderRegistry::new()));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(&make_request(), Sensitivity::Shareable, "test")
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::Unavailable(msg) => {
                assert!(msg.contains("no providers"), "got: {msg}");
            }
            other => panic!("expected Unavailable, got: {other:?}"),
        }
    }

    #[test]
    fn retry_config_defaults_are_sensible() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.initial_backoff_ms, 1000);
        assert!((config.backoff_multiplier - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn capability_to_request_type_mapping() {
        assert_eq!(
            capability_to_request_type(ProviderCapability::Completion),
            RequestType::Completion
        );
        assert_eq!(
            capability_to_request_type(ProviderCapability::Embedding),
            RequestType::Embedding
        );
        assert_eq!(
            capability_to_request_type(ProviderCapability::ImageGeneration),
            RequestType::ImageGeneration
        );
        assert_eq!(
            capability_to_request_type(ProviderCapability::VideoGeneration),
            RequestType::VideoGeneration
        );
        assert_eq!(
            capability_to_request_type(ProviderCapability::AgentExecution),
            RequestType::AgentTask
        );
        assert_eq!(
            capability_to_request_type(ProviderCapability::FunctionCall),
            RequestType::Completion
        );
        assert_eq!(
            capability_to_request_type(ProviderCapability::Vision),
            RequestType::Completion
        );
    }

    #[test]
    fn is_retryable_identifies_correct_errors() {
        assert!(is_retryable(&ProviderError::Unavailable("test".into())));
        assert!(is_retryable(&ProviderError::RateLimited {
            retry_after_ms: 1000
        }));
        assert!(!is_retryable(&ProviderError::RequestFailed("test".into())));
        assert!(!is_retryable(&ProviderError::AuthFailed("test".into())));
        assert!(!is_retryable(&ProviderError::BudgetExceeded("test".into())));
    }

    #[tokio::test]
    async fn all_providers_over_budget_returns_budget_exceeded() {
        let mut registry = ProviderRegistry::new();
        let provider = Arc::new(MockCompletionProvider::new(
            "openai",
            ProviderClass::Cloud,
            "response",
        ));
        register_completion(&mut registry, provider);

        let registry = Arc::new(RwLock::new(registry));

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("usage.ndjson");
        let log = Arc::new(crate::metering::UsageLog::open(&log_path).unwrap());

        // Exceed global daily limit.
        log.record(&UsageRecord {
            provider: "openai".to_string(),
            model: "test".to_string(),
            timestamp: symbiotic_core::now_unix(),
            input_tokens: 100,
            output_tokens: 50,
            media_units: 0,
            cost_usd: Some(100.0),
            request_type: RequestType::Completion,
            source: "test".to_string(),
            session_id: None,
        })
        .unwrap();

        let budget_config = crate::config::BudgetConfig {
            global_daily_limit_usd: Some(10.0),
            per_provider: std::collections::HashMap::new(),
            alert_threshold_percent: 80.0,
        };

        let budget = Arc::new(BudgetEnforcer::new(budget_config, log));
        let router = ProviderRouter::new(registry).with_budget(budget);

        let result = router
            .complete(&make_request(), Sensitivity::Shareable, "test")
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::BudgetExceeded(msg) => {
                assert!(msg.contains("over budget"), "got: {msg}");
            }
            other => panic!("expected BudgetExceeded, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn all_unhealthy_returns_unavailable() {
        let mut registry = ProviderRegistry::new();
        let provider = Arc::new(MockCompletionProvider::new(
            "openai",
            ProviderClass::Cloud,
            "response",
        ));
        register_completion(&mut registry, provider);

        let registry = Arc::new(RwLock::new(registry));
        let checker = Arc::new(MockHealthChecker {
            unhealthy: vec!["openai".to_string()],
        });

        let router = ProviderRouter::new(registry).with_health_checker(checker);

        let result = router
            .complete(&make_request(), Sensitivity::Shareable, "test")
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ProviderError::Unavailable(msg) => {
                assert!(msg.contains("unhealthy"), "got: {msg}");
            }
            other => panic!("expected Unavailable, got: {other:?}"),
        }
    }

    // -- ModelHint tests -------------------------------------------------------

    #[test]
    fn model_hint_matches_cheap_fast_models() {
        let hint = ModelHint::CheapFast;
        assert!(hint.matches_model_name("claude-3-haiku-20240307"));
        assert!(hint.matches_model_name("claude-3.5-haiku"));
        assert!(hint.matches_model_name("gemini-2.0-flash"));
        assert!(hint.matches_model_name("gemini-3-flash-preview"));
        assert!(hint.matches_model_name("gpt-4o-mini"));
        assert!(hint.matches_model_name("gemini-2.0-flash-lite"));
        assert!(hint.matches_model_name("gemma-nano"));

        // Should NOT match capable models.
        assert!(!hint.matches_model_name("claude-3.5-sonnet"));
        assert!(!hint.matches_model_name("claude-4-opus"));
        assert!(!hint.matches_model_name("gpt-4o"));
        assert!(!hint.matches_model_name("gpt-5"));
    }

    #[test]
    fn model_hint_matches_most_capable_models() {
        let hint = ModelHint::MostCapable;
        assert!(hint.matches_model_name("claude-3.5-sonnet"));
        assert!(hint.matches_model_name("claude-4-opus"));
        assert!(hint.matches_model_name("gpt-4o"));
        assert!(hint.matches_model_name("gpt-5"));

        // Should NOT match cheap models.
        assert!(!hint.matches_model_name("claude-3-haiku-20240307"));
        assert!(!hint.matches_model_name("gemini-2.0-flash"));
        assert!(!hint.matches_model_name("gpt-4o-mini"));

        // gpt-4o-mini contains "gpt-4o" but also contains "mini" — the
        // MostCapable check explicitly excludes "mini".
        assert!(!hint.matches_model_name("gpt-4o-mini"));
    }

    #[test]
    fn model_hint_default_matches_nothing() {
        let hint = ModelHint::Default;
        assert!(!hint.matches_model_name("claude-3-haiku"));
        assert!(!hint.matches_model_name("claude-3.5-sonnet"));
        assert!(!hint.matches_model_name("gpt-4o"));
    }

    #[tokio::test]
    async fn cheap_fast_hint_prefers_haiku_over_sonnet() {
        let mut registry = ProviderRegistry::new();

        let sonnet = Arc::new(MockCompletionProvider::with_model(
            "anthropic-sonnet",
            "claude-3.5-sonnet",
            ProviderClass::Cloud,
            "sonnet response",
        ));
        let haiku = Arc::new(MockCompletionProvider::with_model(
            "anthropic-haiku",
            "claude-3-haiku-20240307",
            ProviderClass::Cloud,
            "haiku response",
        ));
        register_completion(&mut registry, sonnet);
        register_completion(&mut registry, haiku);

        // Set sonnet as default — without the hint it would be preferred.
        registry
            .set_default(ProviderCapability::Completion, "anthropic-sonnet")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(
                &make_request_with_hint(ModelHint::CheapFast),
                Sensitivity::Shareable,
                "test",
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "haiku response");
    }

    #[tokio::test]
    async fn cheap_fast_hint_prefers_flash_over_default() {
        let mut registry = ProviderRegistry::new();

        let gpt4o = Arc::new(MockCompletionProvider::with_model(
            "openai",
            "gpt-4o",
            ProviderClass::Cloud,
            "gpt4o response",
        ));
        let flash = Arc::new(MockCompletionProvider::with_model(
            "google",
            "gemini-2.0-flash",
            ProviderClass::Cloud,
            "flash response",
        ));
        register_completion(&mut registry, gpt4o);
        register_completion(&mut registry, flash);

        registry
            .set_default(ProviderCapability::Completion, "openai")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(
                &make_request_with_hint(ModelHint::CheapFast),
                Sensitivity::Shareable,
                "test",
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "flash response");
    }

    #[tokio::test]
    async fn cheap_fast_hint_falls_back_when_no_cheap_model() {
        let mut registry = ProviderRegistry::new();

        // Only capable models registered — no haiku/flash/mini.
        let sonnet = Arc::new(MockCompletionProvider::with_model(
            "anthropic",
            "claude-3.5-sonnet",
            ProviderClass::Cloud,
            "sonnet response",
        ));
        register_completion(&mut registry, sonnet);

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        // CheapFast hint but no cheap model — should still work (fallback).
        let result = router
            .complete(
                &make_request_with_hint(ModelHint::CheapFast),
                Sensitivity::Shareable,
                "test",
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "sonnet response");
    }

    #[tokio::test]
    async fn default_hint_preserves_existing_behavior() {
        let mut registry = ProviderRegistry::new();

        let haiku = Arc::new(MockCompletionProvider::with_model(
            "anthropic-haiku",
            "claude-3-haiku",
            ProviderClass::Cloud,
            "haiku response",
        ));
        let sonnet = Arc::new(MockCompletionProvider::with_model(
            "anthropic-sonnet",
            "claude-3.5-sonnet",
            ProviderClass::Cloud,
            "sonnet response",
        ));
        register_completion(&mut registry, haiku);
        register_completion(&mut registry, sonnet);

        // Sonnet is default — with Default hint, it should be preferred.
        registry
            .set_default(ProviderCapability::Completion, "anthropic-sonnet")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(
                &make_request_with_hint(ModelHint::Default),
                Sensitivity::Shareable,
                "test",
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "sonnet response");
    }

    #[tokio::test]
    async fn most_capable_hint_prefers_sonnet_over_haiku() {
        let mut registry = ProviderRegistry::new();

        let haiku = Arc::new(MockCompletionProvider::with_model(
            "anthropic-haiku",
            "claude-3-haiku",
            ProviderClass::Cloud,
            "haiku response",
        ));
        let sonnet = Arc::new(MockCompletionProvider::with_model(
            "anthropic-sonnet",
            "claude-3.5-sonnet",
            ProviderClass::Cloud,
            "sonnet response",
        ));
        register_completion(&mut registry, haiku);
        register_completion(&mut registry, sonnet);

        // Haiku is default — but MostCapable should pick sonnet.
        registry
            .set_default(ProviderCapability::Completion, "anthropic-haiku")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(
                &make_request_with_hint(ModelHint::MostCapable),
                Sensitivity::Shareable,
                "test",
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "sonnet response");
    }

    #[tokio::test]
    async fn cheap_fast_hint_prefers_mini_model() {
        let mut registry = ProviderRegistry::new();

        let gpt4o = Arc::new(MockCompletionProvider::with_model(
            "openai",
            "gpt-4o",
            ProviderClass::Cloud,
            "gpt4o response",
        ));
        let mini = Arc::new(MockCompletionProvider::with_model(
            "openai-mini",
            "gpt-4o-mini",
            ProviderClass::Cloud,
            "mini response",
        ));
        register_completion(&mut registry, gpt4o);
        register_completion(&mut registry, mini);

        registry
            .set_default(ProviderCapability::Completion, "openai")
            .unwrap();

        let registry = Arc::new(RwLock::new(registry));
        let router = ProviderRouter::new(registry);

        let result = router
            .complete(
                &make_request_with_hint(ModelHint::CheapFast),
                Sensitivity::Shareable,
                "test",
            )
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "mini response");
    }
}
