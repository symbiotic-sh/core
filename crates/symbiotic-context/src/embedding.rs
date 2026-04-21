//! Multi-provider embedding generation with sensitivity-aware routing.
//!
//! Provides an `EmbeddingProvider` trait with implementations for Ollama (local)
//! and OpenAI (cloud), plus an `EmbeddingRouter` that routes requests based on
//! content sensitivity and retries on transient failures.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chunking::RetryConfig;
use crate::Sensitivity;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Classification of where a provider runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderClass {
    /// Runs entirely on local hardware (e.g. Ollama).
    Local,
    /// Calls a remote cloud API (e.g. OpenAI).
    Cloud,
}

/// Successful embedding result.
#[derive(Debug, Clone)]
pub struct EmbedResult {
    /// The embedding vector.
    pub embedding: Vec<f32>,
    /// Model that produced this embedding.
    pub model_name: String,
    /// Dimensionality of the embedding.
    pub dimensions: usize,
}

/// Errors that can occur during embedding generation.
#[derive(Debug, Error)]
pub enum EmbedError {
    /// The provider is not reachable (e.g. Ollama not running).
    #[error("provider unavailable: {0}")]
    Unavailable(String),

    /// The provider returned an error response.
    #[error("request failed: {0}")]
    RequestFailed(String),

    /// The provider returned an empty embedding vector.
    #[error("provider returned empty embedding")]
    EmptyEmbedding,

    /// Attempted to route sensitive content to a cloud provider.
    #[error(
        "sensitivity violation: {sensitivity:?} content cannot use {provider_class:?} provider"
    )]
    SensitivityViolation {
        sensitivity: Sensitivity,
        provider_class: ProviderClass,
    },
}

// ---------------------------------------------------------------------------
// EmbeddingProvider trait
// ---------------------------------------------------------------------------

/// Trait for embedding model backends.
///
/// Designed to be usable anywhere multiple models are needed — the trait is
/// model-agnostic so providers can be composed, swapped, and routed dynamically.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Returns whether this provider runs locally or in the cloud.
    fn provider_class(&self) -> ProviderClass;

    /// Returns the model identifier (e.g. "nomic-embed-text", "text-embedding-3-small").
    fn model_name(&self) -> &str;

    /// Generate an embedding for a single text input.
    async fn embed(&self, text: &str) -> Result<EmbedResult, EmbedError>;

    /// Generate embeddings for a batch of text inputs.
    ///
    /// Default implementation calls `embed()` sequentially. Providers that
    /// support native batching (e.g. OpenAI) should override this.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<EmbedResult>, EmbedError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// OllamaProvider
// ---------------------------------------------------------------------------

/// Default Ollama embedding endpoint.
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434/api/embeddings";

/// Default Ollama embedding model.
const DEFAULT_OLLAMA_MODEL: &str = "nomic-embed-text";

#[derive(Debug, Serialize)]
struct OllamaEmbeddingRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbeddingResponse {
    embedding: Vec<f32>,
}

/// Local embedding provider backed by Ollama.
pub struct OllamaProvider {
    client: reqwest::Client,
    url: String,
    model: String,
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OllamaProvider {
    /// Creates an `OllamaProvider` with default settings.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            url: DEFAULT_OLLAMA_URL.to_string(),
            model: DEFAULT_OLLAMA_MODEL.to_string(),
        }
    }

    /// Creates an `OllamaProvider` with custom endpoint and model.
    pub fn with_config(url: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
            model,
        }
    }

    /// Tries to generate an embedding, returning `None` if Ollama is unavailable.
    pub async fn try_embed(&self, text: &str) -> Option<Vec<f32>> {
        self.embed(text).await.ok().map(|r| r.embedding)
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaProvider {
    fn provider_class(&self) -> ProviderClass {
        ProviderClass::Local
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    async fn embed(&self, text: &str) -> Result<EmbedResult, EmbedError> {
        let request_body = OllamaEmbeddingRequest {
            model: &self.model,
            prompt: text,
        };

        let response = self
            .client
            .post(&self.url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| EmbedError::Unavailable(format!("failed to connect to Ollama: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(EmbedError::RequestFailed(format!(
                "Ollama returned status {status}: {body}"
            )));
        }

        let parsed: OllamaEmbeddingResponse = response
            .json()
            .await
            .map_err(|e| EmbedError::RequestFailed(format!("failed to parse response: {e}")))?;

        if parsed.embedding.is_empty() {
            return Err(EmbedError::EmptyEmbedding);
        }

        let dimensions = parsed.embedding.len();
        Ok(EmbedResult {
            embedding: parsed.embedding,
            model_name: self.model.clone(),
            dimensions,
        })
    }
}

/// Backward-compatible alias.
pub type EmbeddingService = OllamaProvider;

// ---------------------------------------------------------------------------
// OpenAiProvider
// ---------------------------------------------------------------------------

/// Default OpenAI embeddings endpoint.
const DEFAULT_OPENAI_URL: &str = "https://api.openai.com/v1/embeddings";

/// Default OpenAI embedding model.
const DEFAULT_OPENAI_MODEL: &str = "text-embedding-3-small";

#[derive(Debug, Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    model: &'a str,
    input: OpenAiInput<'a>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OpenAiInput<'a> {
    Single(&'a str),
    Batch(Vec<&'a str>),
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
}

/// Cloud embedding provider backed by the OpenAI embeddings API.
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    url: String,
}

impl OpenAiProvider {
    /// Creates an `OpenAiProvider` with the given API key and default model/endpoint.
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model: DEFAULT_OPENAI_MODEL.to_string(),
            url: DEFAULT_OPENAI_URL.to_string(),
        }
    }

    /// Creates an `OpenAiProvider` with custom settings.
    pub fn with_config(api_key: String, model: String, url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            url,
        }
    }

    /// Sends a request to the OpenAI embeddings endpoint.
    async fn request_embeddings(
        &self,
        input: OpenAiInput<'_>,
    ) -> Result<Vec<EmbedResult>, EmbedError> {
        let request_body = OpenAiEmbeddingRequest {
            model: &self.model,
            input,
        };

        let response = self
            .client
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request_body)
            .send()
            .await
            .map_err(|e| EmbedError::Unavailable(format!("failed to connect to OpenAI: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(EmbedError::RequestFailed(format!(
                "OpenAI returned status {status}: {body}"
            )));
        }

        let parsed: OpenAiEmbeddingResponse = response
            .json()
            .await
            .map_err(|e| EmbedError::RequestFailed(format!("failed to parse response: {e}")))?;

        let mut results = Vec::with_capacity(parsed.data.len());
        for item in parsed.data {
            if item.embedding.is_empty() {
                return Err(EmbedError::EmptyEmbedding);
            }
            let dimensions = item.embedding.len();
            results.push(EmbedResult {
                embedding: item.embedding,
                model_name: self.model.clone(),
                dimensions,
            });
        }

        if results.is_empty() {
            return Err(EmbedError::EmptyEmbedding);
        }

        Ok(results)
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiProvider {
    fn provider_class(&self) -> ProviderClass {
        ProviderClass::Cloud
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    async fn embed(&self, text: &str) -> Result<EmbedResult, EmbedError> {
        let mut results = self.request_embeddings(OpenAiInput::Single(text)).await?;
        // Single input always returns exactly one result.
        Ok(results.remove(0))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<EmbedResult>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.request_embeddings(OpenAiInput::Batch(texts.to_vec()))
            .await
    }
}

// ---------------------------------------------------------------------------
// EmbeddingRouter
// ---------------------------------------------------------------------------

/// Sensitivity-aware router that selects between local and cloud providers.
///
/// Routes restricted/private content exclusively to local providers.
/// Shareable content prefers cloud (if available) for cost efficiency,
/// falling back to local.
pub struct EmbeddingRouter {
    local: Arc<dyn EmbeddingProvider>,
    cloud: Option<Arc<dyn EmbeddingProvider>>,
    retry_config: RetryConfig,
}

impl EmbeddingRouter {
    /// Creates a router that only uses a local provider.
    pub fn local_only(provider: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            local: provider,
            cloud: None,
            retry_config: RetryConfig::default(),
        }
    }

    /// Creates a router with both local and cloud providers.
    pub fn with_cloud(
        local: Arc<dyn EmbeddingProvider>,
        cloud: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            local,
            cloud: Some(cloud),
            retry_config: RetryConfig::default(),
        }
    }

    /// Overrides the retry configuration.
    pub fn with_retry_config(mut self, config: RetryConfig) -> Self {
        self.retry_config = config;
        self
    }

    /// Selects the appropriate provider based on content sensitivity.
    fn select_provider(&self, sensitivity: Sensitivity) -> &dyn EmbeddingProvider {
        match sensitivity {
            Sensitivity::Restricted | Sensitivity::Private => self.local.as_ref(),
            Sensitivity::Shareable => {
                if let Some(cloud) = &self.cloud {
                    cloud.as_ref()
                } else {
                    self.local.as_ref()
                }
            }
        }
    }

    /// Embeds text with sensitivity-aware provider selection and retry logic.
    pub async fn embed(
        &self,
        text: &str,
        sensitivity: Sensitivity,
    ) -> Result<EmbedResult, EmbedError> {
        let provider = self.select_provider(sensitivity);

        // Safety check: cloud providers must not receive restricted/private content.
        if provider.provider_class() == ProviderClass::Cloud
            && matches!(sensitivity, Sensitivity::Restricted | Sensitivity::Private)
        {
            return Err(EmbedError::SensitivityViolation {
                sensitivity,
                provider_class: ProviderClass::Cloud,
            });
        }

        self.embed_with_retry(provider, text).await
    }

    /// Attempts embedding with exponential backoff retry.
    async fn embed_with_retry(
        &self,
        provider: &dyn EmbeddingProvider,
        text: &str,
    ) -> Result<EmbedResult, EmbedError> {
        let mut last_error = None;
        let mut backoff_ms = self.retry_config.initial_backoff_ms;

        for attempt in 0..=self.retry_config.max_retries {
            match provider.embed(text).await {
                Ok(result) => return Ok(result),
                Err(EmbedError::Unavailable(msg)) => {
                    last_error = Some(EmbedError::Unavailable(msg));
                    // Unavailable is the only retryable error.
                    if attempt < self.retry_config.max_retries {
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                        backoff_ms =
                            (backoff_ms as f64 * self.retry_config.backoff_multiplier) as u64;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_error.unwrap_or_else(|| EmbedError::Unavailable("exhausted retries".to_string())))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Ollama request/response serialization --

    #[test]
    fn ollama_request_serializes_correctly() {
        let req = OllamaEmbeddingRequest {
            model: "nomic-embed-text",
            prompt: "hello world",
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains("nomic-embed-text"));
        assert!(json.contains("hello world"));
    }

    #[test]
    fn ollama_response_deserializes_correctly() {
        let json = r#"{"embedding":[0.1, 0.2, 0.3]}"#;
        let resp: OllamaEmbeddingResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.embedding.len(), 3);
        assert!((resp.embedding[0] - 0.1).abs() < f32::EPSILON);
    }

    // -- OllamaProvider --

    #[test]
    fn ollama_provider_defaults() {
        let svc = OllamaProvider::new();
        assert_eq!(svc.url, "http://localhost:11434/api/embeddings");
        assert_eq!(svc.model, "nomic-embed-text");
        assert_eq!(svc.provider_class(), ProviderClass::Local);
    }

    #[test]
    fn ollama_provider_custom_config() {
        let svc = OllamaProvider::with_config(
            "http://custom:8080/embed".to_string(),
            "custom-model".to_string(),
        );
        assert_eq!(svc.url, "http://custom:8080/embed");
        assert_eq!(svc.model, "custom-model");
        assert_eq!(svc.model_name(), "custom-model");
    }

    // -- OpenAI request/response serialization --

    #[test]
    fn openai_request_single_serializes() {
        let req = OpenAiEmbeddingRequest {
            model: "text-embedding-3-small",
            input: OpenAiInput::Single("hello"),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains("text-embedding-3-small"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn openai_request_batch_serializes() {
        let req = OpenAiEmbeddingRequest {
            model: "text-embedding-3-small",
            input: OpenAiInput::Batch(vec!["hello", "world"]),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains("[\"hello\",\"world\"]"));
    }

    #[test]
    fn openai_response_deserializes() {
        let json = r#"{"data":[{"embedding":[0.1,0.2]},{"embedding":[0.3,0.4]}]}"#;
        let resp: OpenAiEmbeddingResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].embedding, vec![0.1, 0.2]);
    }

    // -- OpenAiProvider --

    #[test]
    fn openai_provider_defaults() {
        let provider = OpenAiProvider::new("test-key".to_string());
        assert_eq!(provider.provider_class(), ProviderClass::Cloud);
        assert_eq!(provider.model_name(), "text-embedding-3-small");
        assert_eq!(provider.url, "https://api.openai.com/v1/embeddings");
    }

    #[test]
    fn openai_provider_custom_config() {
        let provider = OpenAiProvider::with_config(
            "sk-test".to_string(),
            "custom-model".to_string(),
            "https://custom.api/v1/embeddings".to_string(),
        );
        assert_eq!(provider.model_name(), "custom-model");
        assert_eq!(provider.url, "https://custom.api/v1/embeddings");
    }

    // -- ProviderClass serialization --

    #[test]
    fn provider_class_serialization() {
        let local = ProviderClass::Local;
        let json = serde_json::to_string(&local).expect("serialize");
        assert_eq!(json, "\"local\"");

        let cloud: ProviderClass = serde_json::from_str("\"cloud\"").expect("deserialize");
        assert_eq!(cloud, ProviderClass::Cloud);
    }

    // -- EmbedError display --

    #[test]
    fn embed_error_display() {
        let err = EmbedError::Unavailable("connection refused".to_string());
        assert!(err.to_string().contains("connection refused"));

        let err = EmbedError::SensitivityViolation {
            sensitivity: Sensitivity::Private,
            provider_class: ProviderClass::Cloud,
        };
        assert!(err.to_string().contains("Private"));
        assert!(err.to_string().contains("Cloud"));
    }

    // -- Mock provider for router tests --

    struct MockProvider {
        class: ProviderClass,
        model: String,
        result: Result<Vec<f32>, EmbedError>,
    }

    impl MockProvider {
        fn local_ok(embedding: Vec<f32>) -> Self {
            Self {
                class: ProviderClass::Local,
                model: "mock-local".to_string(),
                result: Ok(embedding),
            }
        }

        fn cloud_ok(embedding: Vec<f32>) -> Self {
            Self {
                class: ProviderClass::Cloud,
                model: "mock-cloud".to_string(),
                result: Ok(embedding),
            }
        }

        fn local_unavailable() -> Self {
            Self {
                class: ProviderClass::Local,
                model: "mock-local".to_string(),
                result: Err(EmbedError::Unavailable("mock unavailable".to_string())),
            }
        }
    }

    #[async_trait]
    impl EmbeddingProvider for MockProvider {
        fn provider_class(&self) -> ProviderClass {
            self.class
        }

        fn model_name(&self) -> &str {
            &self.model
        }

        async fn embed(&self, _text: &str) -> Result<EmbedResult, EmbedError> {
            match &self.result {
                Ok(embedding) => Ok(EmbedResult {
                    embedding: embedding.clone(),
                    model_name: self.model.clone(),
                    dimensions: embedding.len(),
                }),
                Err(EmbedError::Unavailable(msg)) => Err(EmbedError::Unavailable(msg.clone())),
                Err(EmbedError::RequestFailed(msg)) => Err(EmbedError::RequestFailed(msg.clone())),
                Err(EmbedError::EmptyEmbedding) => Err(EmbedError::EmptyEmbedding),
                Err(EmbedError::SensitivityViolation {
                    sensitivity,
                    provider_class,
                }) => Err(EmbedError::SensitivityViolation {
                    sensitivity: *sensitivity,
                    provider_class: *provider_class,
                }),
            }
        }
    }

    // -- EmbeddingRouter tests --

    #[tokio::test]
    async fn router_routes_restricted_to_local() {
        let local = Arc::new(MockProvider::local_ok(vec![1.0, 2.0]));
        let cloud = Arc::new(MockProvider::cloud_ok(vec![3.0, 4.0]));
        let router = EmbeddingRouter::with_cloud(local, cloud);

        let result = router.embed("secret data", Sensitivity::Restricted).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().model_name, "mock-local");
    }

    #[tokio::test]
    async fn router_routes_private_to_local() {
        let local = Arc::new(MockProvider::local_ok(vec![1.0, 2.0]));
        let cloud = Arc::new(MockProvider::cloud_ok(vec![3.0, 4.0]));
        let router = EmbeddingRouter::with_cloud(local, cloud);

        let result = router.embed("private data", Sensitivity::Private).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().model_name, "mock-local");
    }

    #[tokio::test]
    async fn router_routes_shareable_to_cloud() {
        let local = Arc::new(MockProvider::local_ok(vec![1.0, 2.0]));
        let cloud = Arc::new(MockProvider::cloud_ok(vec![3.0, 4.0]));
        let router = EmbeddingRouter::with_cloud(local, cloud);

        let result = router.embed("public data", Sensitivity::Shareable).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().model_name, "mock-cloud");
    }

    #[tokio::test]
    async fn router_falls_back_to_local_when_no_cloud() {
        let local = Arc::new(MockProvider::local_ok(vec![1.0, 2.0]));
        let router = EmbeddingRouter::local_only(local);

        let result = router.embed("public data", Sensitivity::Shareable).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().model_name, "mock-local");
    }

    #[tokio::test]
    async fn router_retry_exhaustion() {
        let local = Arc::new(MockProvider::local_unavailable());
        let router = EmbeddingRouter::local_only(local).with_retry_config(RetryConfig {
            max_retries: 2,
            initial_backoff_ms: 1, // fast for tests
            backoff_multiplier: 1.0,
        });

        let result = router.embed("test", Sensitivity::Shareable).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), EmbedError::Unavailable(_)));
    }

    #[tokio::test]
    async fn router_non_retryable_error_fails_immediately() {
        let provider = Arc::new(MockProvider {
            class: ProviderClass::Local,
            model: "mock".to_string(),
            result: Err(EmbedError::RequestFailed("bad request".to_string())),
        });
        let router = EmbeddingRouter::local_only(provider).with_retry_config(RetryConfig {
            max_retries: 3,
            initial_backoff_ms: 1000, // should not be reached
            backoff_multiplier: 2.0,
        });

        let result = router.embed("test", Sensitivity::Shareable).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), EmbedError::RequestFailed(_)));
    }

    // -- Backward compat --

    #[test]
    fn embedding_service_alias_compiles() {
        let _svc: EmbeddingService = EmbeddingService::new();
        assert_eq!(_svc.provider_class(), ProviderClass::Local);
    }

    // -- Integration tests (feature-gated) --

    #[cfg(feature = "integration")]
    #[tokio::test]
    async fn ollama_try_embed_returns_none_when_unavailable() {
        let svc = OllamaProvider::with_config(
            "http://127.0.0.1:1/api/embeddings".to_string(),
            "nomic-embed-text".to_string(),
        );
        let result = svc.try_embed("hello world").await;
        assert!(result.is_none());
    }

    #[cfg(feature = "integration")]
    #[tokio::test]
    async fn ollama_embed_fails_when_unavailable() {
        let svc = OllamaProvider::with_config(
            "http://127.0.0.1:1/api/embeddings".to_string(),
            "nomic-embed-text".to_string(),
        );
        let result = svc.embed("hello world").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), EmbedError::Unavailable(_)));
    }
}
