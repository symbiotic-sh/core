//! Ollama embedding provider for local vector generation.
//!
//! Connects to a running Ollama instance via its HTTP API (`/api/embeddings`).
//! No authentication required — Ollama runs on the local machine.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    CapabilitySet, EmbedResult, EmbeddingProvider, ModelProvider, PricingInfo, ProviderCapability,
    ProviderClass, ProviderError,
};

/// Default Ollama embedding endpoint.
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Default Ollama embedding model.
const DEFAULT_OLLAMA_MODEL: &str = "nomic-embed-text";

/// Embedding provider backed by a local Ollama instance.
pub struct OllamaEmbeddingProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    capabilities: CapabilitySet,
}

impl OllamaEmbeddingProvider {
    /// Create a new Ollama embedding provider with default settings.
    pub fn new() -> Self {
        Self::with_config(
            DEFAULT_OLLAMA_URL.to_string(),
            DEFAULT_OLLAMA_MODEL.to_string(),
        )
    }

    /// Create a new Ollama embedding provider with custom endpoint and model.
    ///
    /// # Arguments
    /// * `base_url` — Base URL of the Ollama HTTP API (e.g. `"http://localhost:11434"`).
    /// * `model` — Model tag to use (e.g. `"nomic-embed-text"`).
    pub fn with_config(base_url: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            model,
            capabilities: CapabilitySet::new(vec![ProviderCapability::Embedding]),
        }
    }

    /// Tries to generate an embedding, returning `None` if Ollama is unavailable.
    pub async fn try_embed(&self, text: &str) -> Option<Vec<f32>> {
        self.embed(text).await.ok().map(|r| r.embedding)
    }
}

impl Default for OllamaEmbeddingProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelProvider for OllamaEmbeddingProvider {
    fn name(&self) -> &str {
        "ollama-embedding"
    }

    fn provider_class(&self) -> ProviderClass {
        ProviderClass::Local
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

// -- Ollama wire types -------------------------------------------------------

#[derive(Serialize)]
struct OllamaEmbeddingRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Deserialize)]
struct OllamaEmbeddingResponse {
    embedding: Vec<f32>,
}

// -- EmbeddingProvider impl --------------------------------------------------

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<EmbedResult, ProviderError> {
        let request_body = OllamaEmbeddingRequest {
            model: &self.model,
            prompt: text,
        };

        let url = format!("{}/api/embeddings", self.base_url.trim_end_matches('/'));

        let response = self
            .client
            .post(&url)
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

        let parsed: OllamaEmbeddingResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(format!("json parse failed: {e}")))?;

        if parsed.embedding.is_empty() {
            return Err(ProviderError::InvalidResponse(
                "provider returned empty embedding".to_string(),
            ));
        }

        let dimensions = parsed.embedding.len();
        Ok(EmbedResult {
            embedding: parsed.embedding,
            model_name: self.model.clone(),
            dimensions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_metadata() {
        let provider = OllamaEmbeddingProvider::new();
        assert_eq!(provider.name(), "ollama-embedding");
        assert_eq!(provider.provider_class(), ProviderClass::Local);
        assert_eq!(provider.model_name(), "nomic-embed-text");
        assert!(provider.capabilities().has(ProviderCapability::Embedding));
        assert!(!provider.capabilities().has(ProviderCapability::Completion));
        assert!(provider.pricing().is_none());
    }

    #[test]
    fn test_custom_config() {
        let provider = OllamaEmbeddingProvider::with_config(
            "http://custom:8080".to_string(),
            "custom-model".to_string(),
        );
        assert_eq!(provider.model_name(), "custom-model");
        assert_eq!(provider.base_url, "http://custom:8080");
    }

    #[test]
    fn test_default_impl() {
        let provider = OllamaEmbeddingProvider::default();
        assert_eq!(provider.model_name(), "nomic-embed-text");
    }

    #[test]
    fn test_request_serialization() {
        let req = OllamaEmbeddingRequest {
            model: "nomic-embed-text",
            prompt: "hello world",
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "nomic-embed-text");
        assert_eq!(json["prompt"], "hello world");
    }

    #[test]
    fn test_response_deserialization() {
        let json = r#"{"embedding":[0.1, 0.2, 0.3]}"#;
        let resp: OllamaEmbeddingResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.embedding.len(), 3);
        assert!((resp.embedding[0] - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn test_response_empty_embedding() {
        let json = r#"{"embedding":[]}"#;
        let resp: OllamaEmbeddingResponse = serde_json::from_str(json).unwrap();
        assert!(resp.embedding.is_empty());
    }
}
