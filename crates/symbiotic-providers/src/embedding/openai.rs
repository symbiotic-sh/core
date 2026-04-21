//! OpenAI embedding provider for cloud-hosted vector generation.
//!
//! Supports the standard OpenAI embeddings API (`/v1/embeddings`) with
//! native batching support. Also works with any OpenAI-compatible endpoint.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    CapabilitySet, EmbedResult, EmbeddingProvider, ModelProvider, PricingInfo, ProviderAuth,
    ProviderCapability, ProviderClass, ProviderError,
};

/// Default OpenAI embedding model.
const DEFAULT_OPENAI_MODEL: &str = "text-embedding-3-small";

/// Embedding provider backed by the OpenAI embeddings API.
///
/// Supports native batching — [`EmbeddingProvider::embed_batch`] sends all
/// texts in a single HTTP request, which is more efficient than sequential calls.
pub struct OpenAiEmbeddingProvider {
    client: reqwest::Client,
    auth: ProviderAuth,
    base_url: String,
    model: String,
    capabilities: CapabilitySet,
    pricing: PricingInfo,
}

impl OpenAiEmbeddingProvider {
    /// Create a new OpenAI embedding provider with the default model and endpoint.
    ///
    /// # Arguments
    /// * `auth` — Authentication credential (must be [`ProviderAuth::ApiKey`]).
    pub fn new(auth: ProviderAuth) -> Self {
        Self::with_config(
            auth,
            DEFAULT_OPENAI_MODEL.to_string(),
            "https://api.openai.com/v1".to_string(),
        )
    }

    /// Create an OpenAI embedding provider with custom settings.
    ///
    /// # Arguments
    /// * `auth` — Authentication credential.
    /// * `model` — Model identifier (e.g. `"text-embedding-3-small"`).
    /// * `base_url` — Base URL for the API (e.g. `"https://api.openai.com/v1"`).
    pub fn with_config(auth: ProviderAuth, model: String, base_url: String) -> Self {
        let pricing = default_pricing_for_model(&model);
        Self {
            client: reqwest::Client::new(),
            auth,
            base_url,
            model,
            capabilities: CapabilitySet::new(vec![ProviderCapability::Embedding]),
            pricing,
        }
    }

    /// Extract the API key string from auth, or return an error.
    fn api_key(&self) -> Result<&str, ProviderError> {
        match &self.auth {
            ProviderAuth::ApiKey(key) => Ok(key.as_str()),
            other => Err(ProviderError::AuthFailed(format!(
                "expected ApiKey, got {other:?}"
            ))),
        }
    }

    /// Send a request to the embeddings endpoint.
    async fn request_embeddings(
        &self,
        input: OpenAiInput<'_>,
    ) -> Result<Vec<EmbedResult>, ProviderError> {
        let api_key = self.api_key()?;

        let request_body = OpenAiEmbeddingRequest {
            model: &self.model,
            input,
        };

        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&request_body)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable(format!("connection failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(ProviderError::RequestFailed(format!(
                "HTTP {status}: {body}"
            )));
        }

        let parsed: OpenAiEmbeddingResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(format!("json parse failed: {e}")))?;

        let mut results = Vec::with_capacity(parsed.data.len());
        for item in parsed.data {
            if item.embedding.is_empty() {
                return Err(ProviderError::InvalidResponse(
                    "provider returned empty embedding".to_string(),
                ));
            }
            let dimensions = item.embedding.len();
            results.push(EmbedResult {
                embedding: item.embedding,
                model_name: self.model.clone(),
                dimensions,
            });
        }

        if results.is_empty() {
            return Err(ProviderError::InvalidResponse(
                "provider returned no embeddings".to_string(),
            ));
        }

        Ok(results)
    }
}

/// Return sensible default pricing for known OpenAI embedding models.
fn default_pricing_for_model(model: &str) -> PricingInfo {
    match model {
        "text-embedding-3-small" => PricingInfo {
            embedding_per_1k_tokens: Some(0.00002),
            ..Default::default()
        },
        "text-embedding-3-large" => PricingInfo {
            embedding_per_1k_tokens: Some(0.00013),
            ..Default::default()
        },
        "text-embedding-ada-002" => PricingInfo {
            embedding_per_1k_tokens: Some(0.0001),
            ..Default::default()
        },
        _ => PricingInfo::default(),
    }
}

impl ModelProvider for OpenAiEmbeddingProvider {
    fn name(&self) -> &str {
        "openai-embedding"
    }

    fn provider_class(&self) -> ProviderClass {
        ProviderClass::Cloud
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    fn pricing(&self) -> Option<&PricingInfo> {
        Some(&self.pricing)
    }
}

// -- OpenAI wire types -------------------------------------------------------

#[derive(Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    model: &'a str,
    input: OpenAiInput<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum OpenAiInput<'a> {
    Single(&'a str),
    Batch(Vec<&'a str>),
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
}

// -- EmbeddingProvider impl --------------------------------------------------

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<EmbedResult, ProviderError> {
        let mut results = self.request_embeddings(OpenAiInput::Single(text)).await?;
        Ok(results.remove(0))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<EmbedResult>, ProviderError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.request_embeddings(OpenAiInput::Batch(texts.to_vec()))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_metadata() {
        let provider = OpenAiEmbeddingProvider::new(ProviderAuth::ApiKey("sk-test".into()));
        assert_eq!(provider.name(), "openai-embedding");
        assert_eq!(provider.provider_class(), ProviderClass::Cloud);
        assert_eq!(provider.model_name(), "text-embedding-3-small");
        assert!(provider.capabilities().has(ProviderCapability::Embedding));
        assert!(!provider.capabilities().has(ProviderCapability::Completion));

        let pricing = provider.pricing().expect("should have pricing");
        assert_eq!(pricing.embedding_per_1k_tokens, Some(0.00002));
    }

    #[test]
    fn test_custom_config() {
        let provider = OpenAiEmbeddingProvider::with_config(
            ProviderAuth::ApiKey("sk-test".into()),
            "text-embedding-3-large".to_string(),
            "https://custom.api/v1".to_string(),
        );
        assert_eq!(provider.model_name(), "text-embedding-3-large");
        assert_eq!(provider.base_url, "https://custom.api/v1");

        let pricing = provider.pricing().expect("should have pricing");
        assert_eq!(pricing.embedding_per_1k_tokens, Some(0.00013));
    }

    #[test]
    fn test_ada_pricing() {
        let provider = OpenAiEmbeddingProvider::with_config(
            ProviderAuth::ApiKey("sk-test".into()),
            "text-embedding-ada-002".to_string(),
            "https://api.openai.com/v1".to_string(),
        );
        let pricing = provider.pricing().expect("should have pricing");
        assert_eq!(pricing.embedding_per_1k_tokens, Some(0.0001));
    }

    #[test]
    fn test_unknown_model_default_pricing() {
        let provider = OpenAiEmbeddingProvider::with_config(
            ProviderAuth::ApiKey("sk-test".into()),
            "unknown-model".to_string(),
            "https://api.openai.com/v1".to_string(),
        );
        let pricing = provider.pricing().expect("should have pricing");
        assert!(pricing.embedding_per_1k_tokens.is_none());
    }

    #[test]
    fn test_auth_extraction_success() {
        let provider = OpenAiEmbeddingProvider::new(ProviderAuth::ApiKey("sk-test-key".into()));
        assert_eq!(provider.api_key().unwrap(), "sk-test-key");
    }

    #[test]
    fn test_auth_extraction_failure() {
        let provider = OpenAiEmbeddingProvider::new(ProviderAuth::None);
        assert!(provider.api_key().is_err());
    }

    #[test]
    fn test_single_request_serialization() {
        let req = OpenAiEmbeddingRequest {
            model: "text-embedding-3-small",
            input: OpenAiInput::Single("hello"),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "text-embedding-3-small");
        assert_eq!(json["input"], "hello");
    }

    #[test]
    fn test_batch_request_serialization() {
        let req = OpenAiEmbeddingRequest {
            model: "text-embedding-3-small",
            input: OpenAiInput::Batch(vec!["hello", "world"]),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "text-embedding-3-small");
        let input = json["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0], "hello");
        assert_eq!(input[1], "world");
    }

    #[test]
    fn test_response_deserialization() {
        let json = r#"{"data":[{"embedding":[0.1,0.2]},{"embedding":[0.3,0.4]}]}"#;
        let resp: OpenAiEmbeddingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].embedding, vec![0.1, 0.2]);
        assert_eq!(resp.data[1].embedding, vec![0.3, 0.4]);
    }

    #[test]
    fn test_response_single() {
        let json = r#"{"data":[{"embedding":[0.5,0.6,0.7]}]}"#;
        let resp: OpenAiEmbeddingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].embedding.len(), 3);
    }

    #[tokio::test]
    async fn test_embed_batch_empty() {
        // embed_batch with empty slice should return empty vec without making HTTP calls.
        // We can't test this directly without a mock HTTP server, but we can test
        // the early return branch by using an invalid auth (which would fail on HTTP call).
        let provider = OpenAiEmbeddingProvider::new(ProviderAuth::None);
        let result = provider.embed_batch(&[]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }
}
