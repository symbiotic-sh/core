//! OpenAI completion provider for cloud-hosted chat models.
//!
//! Supports the standard OpenAI chat completions API (`/v1/chat/completions`).
//! Also works with any OpenAI-compatible endpoint via [`OpenAiCompletionProvider::with_base_url`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    CapabilitySet, CompletionProvider, CompletionRequest, CompletionResponse, ModelProvider,
    PricingInfo, ProviderAuth, ProviderCapability, ProviderClass, ProviderError,
};

/// Completion provider for OpenAI and OpenAI-compatible APIs.
pub struct OpenAiCompletionProvider {
    client: reqwest::Client,
    auth: ProviderAuth,
    base_url: String,
    model: String,
    capabilities: CapabilitySet,
    pricing: PricingInfo,
}

impl OpenAiCompletionProvider {
    /// Create a new OpenAI provider with the default API base URL.
    ///
    /// # Arguments
    /// * `auth` — Authentication credential (must be [`ProviderAuth::ApiKey`]).
    /// * `model` — Model identifier (e.g. `"gpt-4o-mini"`).
    pub fn new(auth: ProviderAuth, model: String) -> Self {
        Self::with_base_url(auth, model, "https://api.openai.com/v1".to_string())
    }

    /// Create an OpenAI-compatible provider with a custom base URL.
    ///
    /// Useful for proxies, local deployments, or third-party compatible APIs.
    pub fn with_base_url(auth: ProviderAuth, model: String, base_url: String) -> Self {
        let pricing = default_pricing_for_model(&model);
        Self {
            client: reqwest::Client::new(),
            auth,
            base_url,
            model,
            capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
            pricing,
        }
    }
}

/// Return sensible default pricing for known OpenAI models.
fn default_pricing_for_model(model: &str) -> PricingInfo {
    match model {
        "gpt-4o-mini" => PricingInfo {
            input_per_1k_tokens: Some(0.000_15),
            output_per_1k_tokens: Some(0.000_6),
            ..Default::default()
        },
        "gpt-4o" => PricingInfo {
            input_per_1k_tokens: Some(0.005),
            output_per_1k_tokens: Some(0.015),
            ..Default::default()
        },
        _ => PricingInfo::default(),
    }
}

impl ModelProvider for OpenAiCompletionProvider {
    fn name(&self) -> &str {
        "openai"
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
struct OpenAiChatRequest {
    model: String,
    messages: Vec<OpenAiChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
}

#[derive(Serialize)]
struct OpenAiChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAiChatResponse {
    choices: Vec<OpenAiChoice>,
    model: Option<String>,
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

// -- Helpers -----------------------------------------------------------------

/// Extract the API key string from a [`ProviderAuth`], or return an error.
fn extract_api_key(auth: &ProviderAuth) -> Result<&str, ProviderError> {
    match auth {
        ProviderAuth::ApiKey(key) => Ok(key.as_str()),
        other => Err(ProviderError::AuthFailed(format!(
            "expected ApiKey, got {other:?}"
        ))),
    }
}

/// Convert our `Role` enum to the lowercase string OpenAI expects.
fn role_to_string(role: crate::Role) -> String {
    match role {
        crate::Role::System => "system".to_string(),
        crate::Role::User => "user".to_string(),
        crate::Role::Assistant => "assistant".to_string(),
    }
}

// -- CompletionProvider impl -------------------------------------------------

#[async_trait]
impl CompletionProvider for OpenAiCompletionProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let api_key = extract_api_key(&self.auth)?;

        let messages: Vec<OpenAiChatMessage> = request
            .messages
            .iter()
            .map(|m| OpenAiChatMessage {
                role: role_to_string(m.role),
                content: m.content.clone(),
            })
            .collect();

        let body = OpenAiChatRequest {
            model: self.model.clone(),
            messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop: request.stop.clone(),
        };

        let url = format!("{}/chat/completions", self.base_url);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Unavailable(format!("connection failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::RequestFailed(format!(
                "HTTP {status}: {text}"
            )));
        }

        let parsed: OpenAiChatResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(format!("json parse failed: {e}")))?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::InvalidResponse("empty choices array".to_string()))?;

        let content = choice.message.content.unwrap_or_default();

        let (input_tokens, output_tokens) = match parsed.usage {
            Some(u) => (u.prompt_tokens, u.completion_tokens),
            None => (None, None),
        };

        Ok(CompletionResponse {
            content,
            model: parsed.model.unwrap_or_else(|| self.model.clone()),
            input_tokens,
            output_tokens,
            finish_reason: choice.finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_metadata() {
        let provider = OpenAiCompletionProvider::new(
            ProviderAuth::ApiKey("sk-test".into()),
            "gpt-4o-mini".into(),
        );

        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.provider_class(), ProviderClass::Cloud);
        assert_eq!(provider.model_name(), "gpt-4o-mini");
        assert!(provider.capabilities().has(ProviderCapability::Completion));
        assert!(!provider.capabilities().has(ProviderCapability::Embedding));

        let pricing = provider.pricing().expect("should have pricing");
        assert_eq!(pricing.input_per_1k_tokens, Some(0.000_15));
        assert_eq!(pricing.output_per_1k_tokens, Some(0.000_6));
    }

    #[test]
    fn test_custom_base_url() {
        let provider = OpenAiCompletionProvider::with_base_url(
            ProviderAuth::ApiKey("sk-test".into()),
            "custom-model".into(),
            "https://custom-api.example.com/v1".into(),
        );

        assert_eq!(provider.base_url, "https://custom-api.example.com/v1");
        assert_eq!(provider.model_name(), "custom-model");
    }

    #[test]
    fn test_unknown_model_default_pricing() {
        let provider = OpenAiCompletionProvider::new(
            ProviderAuth::ApiKey("sk-test".into()),
            "unknown-model".into(),
        );

        let pricing = provider.pricing().expect("should have pricing");
        assert!(pricing.input_per_1k_tokens.is_none());
        assert!(pricing.output_per_1k_tokens.is_none());
    }

    #[test]
    fn test_auth_extraction_success() {
        let auth = ProviderAuth::ApiKey("sk-test-key".into());
        let key = extract_api_key(&auth).unwrap();
        assert_eq!(key, "sk-test-key");
    }

    #[test]
    fn test_auth_extraction_failure() {
        let auth = ProviderAuth::None;
        let result = extract_api_key(&auth);
        assert!(result.is_err());
    }

    #[test]
    fn test_request_serialization() {
        let body = OpenAiChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![
                OpenAiChatMessage {
                    role: "system".to_string(),
                    content: "You are helpful.".to_string(),
                },
                OpenAiChatMessage {
                    role: "user".to_string(),
                    content: "Hello".to_string(),
                },
            ],
            max_tokens: Some(1024),
            temperature: Some(0.7),
            stop: None,
        };

        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "gpt-4o-mini");
        assert_eq!(json["max_tokens"], 1024);
        // f32 precision: 0.7f32 serializes as 0.699999988079071 in JSON
        let temp = json["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.001);
        assert!(json.get("stop").is_none()); // skip_serializing_if = None
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_request_serialization_omits_none_fields() {
        let body = OpenAiChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            stop: None,
        };

        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("stop").is_none());
    }

    #[test]
    fn test_response_deserialization() {
        let json = r#"{
            "id": "chatcmpl-abc123",
            "object": "chat.completion",
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 7
            }
        }"#;

        let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("Hello! How can I help?")
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.as_ref().unwrap().prompt_tokens, Some(10));
        assert_eq!(resp.usage.as_ref().unwrap().completion_tokens, Some(7));
        assert_eq!(resp.model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn test_response_without_usage() {
        let json = r#"{
            "choices": [{
                "message": { "content": "test" },
                "finish_reason": null
            }]
        }"#;

        let resp: OpenAiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("test"));
        assert!(resp.usage.is_none());
        assert!(resp.model.is_none());
    }
}
