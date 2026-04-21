//! OpenAI DALL-E image generation provider.
//!
//! Implements [`ImageProvider`] against the OpenAI images/generations API.
//! Uses DALL-E 3 by default.

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    CapabilitySet, GeneratedImage, ImageProvider, ImageRequest, ImageResponse, ModelProvider,
    PricingInfo, ProviderAuth, ProviderCapability, ProviderClass, ProviderError,
};

/// OpenAI DALL-E image generation provider.
///
/// Makes real HTTP calls to `https://api.openai.com/v1/images/generations`.
pub struct OpenAiImageProvider {
    client: reqwest::Client,
    auth: ProviderAuth,
    model: String,
    capabilities: CapabilitySet,
    pricing: PricingInfo,
}

/// Response shape from the OpenAI images API.
#[derive(Debug, Deserialize)]
struct DalleApiResponse {
    data: Vec<DalleImageData>,
}

/// A single image entry in the OpenAI response.
#[derive(Debug, Deserialize)]
struct DalleImageData {
    url: String,
}

impl OpenAiImageProvider {
    /// Create a new OpenAI image provider with the given auth.
    ///
    /// Defaults to DALL-E 3 with pricing of $0.04 per image (1024x1024 standard).
    pub fn new(auth: ProviderAuth) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth,
            model: "dall-e-3".to_string(),
            capabilities: CapabilitySet::new(vec![ProviderCapability::ImageGeneration]),
            pricing: PricingInfo {
                per_image: Some(0.04),
                ..Default::default()
            },
        }
    }
}

impl ModelProvider for OpenAiImageProvider {
    fn name(&self) -> &str {
        "openai-image"
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

#[async_trait]
impl ImageProvider for OpenAiImageProvider {
    async fn generate_image(&self, request: &ImageRequest) -> Result<ImageResponse, ProviderError> {
        let api_key = match &self.auth {
            ProviderAuth::ApiKey(key) => key.clone(),
            _ => {
                return Err(ProviderError::AuthFailed(
                    "OpenAI image provider requires ProviderAuth::ApiKey".to_string(),
                ))
            }
        };

        let width = request.width.unwrap_or(1024);
        let height = request.height.unwrap_or(1024);
        let size = format!("{width}x{height}");
        let n = request.count.unwrap_or(1);

        let body = serde_json::json!({
            "model": self.model,
            "prompt": request.prompt,
            "n": n,
            "size": size,
        });

        let response = self
            .client
            .post("https://api.openai.com/v1/images/generations")
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable(format!("HTTP request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .await
                .unwrap_or_else(|_| "unable to read body".to_string());
            return Err(ProviderError::RequestFailed(format!(
                "OpenAI image API returned {status}: {text}"
            )));
        }

        let api_response: DalleApiResponse = response.json().await.map_err(|e| {
            ProviderError::InvalidResponse(format!("failed to parse response: {e}"))
        })?;

        let images = api_response
            .data
            .into_iter()
            .map(|d| GeneratedImage::Url(d.url))
            .collect();

        Ok(ImageResponse {
            images,
            model: self.model.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata() {
        let provider = OpenAiImageProvider::new(ProviderAuth::ApiKey("test-key".into()));
        assert_eq!(provider.name(), "openai-image");
        assert_eq!(provider.provider_class(), ProviderClass::Cloud);
        assert_eq!(provider.model_name(), "dall-e-3");
        assert!(provider
            .capabilities()
            .has(ProviderCapability::ImageGeneration));
        assert!(provider.pricing().is_some());
        assert_eq!(provider.pricing().unwrap().per_image, Some(0.04));
    }

    #[tokio::test]
    async fn auth_failure_on_wrong_auth_type() {
        let provider = OpenAiImageProvider::new(ProviderAuth::None);
        let request = ImageRequest {
            prompt: "a test image".into(),
            width: None,
            height: None,
            count: None,
        };
        let err = provider.generate_image(&request).await.unwrap_err();
        match err {
            ProviderError::AuthFailed(msg) => {
                assert!(msg.contains("ApiKey"), "expected ApiKey mention: {msg}");
            }
            other => panic!("expected AuthFailed, got: {other}"),
        }
    }
}
