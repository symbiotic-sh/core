//! Anthropic completion provider for Claude models.
//!
//! Uses the Anthropic Messages API (`/v1/messages`). System messages are
//! extracted from the conversation and sent as the top-level `system` field,
//! as required by the Anthropic API format.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    CapabilitySet, CompletionProvider, CompletionRequest, CompletionResponse, ModelProvider,
    PricingInfo, ProviderAuth, ProviderCapability, ProviderClass, ProviderError, Role,
};

/// Base URL for the Anthropic Messages API.
const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

/// Required API version header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Completion provider for Anthropic Claude models.
pub struct AnthropicProvider {
    client: reqwest::Client,
    auth: ProviderAuth,
    model: String,
    capabilities: CapabilitySet,
    pricing: PricingInfo,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    ///
    /// # Arguments
    /// * `auth` — Authentication credential (must be [`ProviderAuth::ApiKey`]).
    /// * `model` — Model identifier (e.g. `"claude-sonnet-4-20250514"`).
    pub fn new(auth: ProviderAuth, model: String) -> Self {
        let pricing = default_pricing_for_model(&model);
        Self {
            client: reqwest::Client::new(),
            auth,
            model,
            capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
            pricing,
        }
    }
}

/// Return sensible default pricing for known Anthropic models.
fn default_pricing_for_model(model: &str) -> PricingInfo {
    if model.starts_with("claude-sonnet-4") {
        PricingInfo {
            input_per_1k_tokens: Some(0.003),
            output_per_1k_tokens: Some(0.015),
            ..Default::default()
        }
    } else if model.starts_with("claude-opus-4") {
        PricingInfo {
            input_per_1k_tokens: Some(0.015),
            output_per_1k_tokens: Some(0.075),
            ..Default::default()
        }
    } else if model.starts_with("claude-haiku-3") || model.starts_with("claude-3-5-haiku") {
        PricingInfo {
            input_per_1k_tokens: Some(0.001),
            output_per_1k_tokens: Some(0.005),
            ..Default::default()
        }
    } else {
        PricingInfo::default()
    }
}

impl ModelProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
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

// -- Anthropic wire types ----------------------------------------------------

#[derive(Serialize)]
struct AnthropicMessagesRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicMessagesResponse {
    content: Vec<AnthropicContentBlock>,
    model: Option<String>,
    stop_reason: Option<String>,
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    text: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
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

/// Separate system messages from user/assistant messages.
///
/// The Anthropic API requires the system prompt as a top-level field, not
/// as a message in the conversation array. This function extracts all system
/// messages (concatenated with newlines) and returns the remaining messages.
fn separate_system_messages(
    messages: &[crate::ChatMessage],
) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut conversation = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                system_parts.push(&msg.content);
            }
            Role::User => {
                conversation.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: msg.content.clone(),
                });
            }
            Role::Assistant => {
                conversation.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: msg.content.clone(),
                });
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n"))
    };

    (system, conversation)
}

// -- CompletionProvider impl -------------------------------------------------

#[async_trait]
impl CompletionProvider for AnthropicProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let api_key = extract_api_key(&self.auth)?;

        let (system, messages) = separate_system_messages(&request.messages);

        let max_tokens = request.max_tokens.unwrap_or(4096);

        let body = AnthropicMessagesRequest {
            model: self.model.clone(),
            max_tokens,
            system,
            messages,
        };

        let resp = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
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

        let parsed: AnthropicMessagesResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(format!("json parse failed: {e}")))?;

        let content = parsed
            .content
            .into_iter()
            .filter_map(|block| block.text)
            .collect::<Vec<_>>()
            .join("");

        let (input_tokens, output_tokens) = match parsed.usage {
            Some(u) => (u.input_tokens, u.output_tokens),
            None => (None, None),
        };

        Ok(CompletionResponse {
            content,
            model: parsed.model.unwrap_or_else(|| self.model.clone()),
            input_tokens,
            output_tokens,
            finish_reason: parsed.stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMessage;

    #[test]
    fn test_provider_metadata() {
        let provider = AnthropicProvider::new(
            ProviderAuth::ApiKey("sk-ant-test".into()),
            "claude-sonnet-4-20250514".into(),
        );

        assert_eq!(provider.name(), "anthropic");
        assert_eq!(provider.provider_class(), ProviderClass::Cloud);
        assert_eq!(provider.model_name(), "claude-sonnet-4-20250514");
        assert!(provider.capabilities().has(ProviderCapability::Completion));
        assert!(!provider.capabilities().has(ProviderCapability::Embedding));

        let pricing = provider.pricing().expect("should have pricing");
        assert_eq!(pricing.input_per_1k_tokens, Some(0.003));
        assert_eq!(pricing.output_per_1k_tokens, Some(0.015));
    }

    #[test]
    fn test_opus_pricing() {
        let provider = AnthropicProvider::new(
            ProviderAuth::ApiKey("sk-ant-test".into()),
            "claude-opus-4-20250514".into(),
        );

        let pricing = provider.pricing().expect("should have pricing");
        assert_eq!(pricing.input_per_1k_tokens, Some(0.015));
        assert_eq!(pricing.output_per_1k_tokens, Some(0.075));
    }

    #[test]
    fn test_unknown_model_default_pricing() {
        let provider = AnthropicProvider::new(
            ProviderAuth::ApiKey("sk-ant-test".into()),
            "claude-unknown-999".into(),
        );

        let pricing = provider.pricing().expect("should have pricing");
        assert!(pricing.input_per_1k_tokens.is_none());
        assert!(pricing.output_per_1k_tokens.is_none());
    }

    #[test]
    fn test_auth_extraction_success() {
        let auth = ProviderAuth::ApiKey("sk-ant-test".into());
        let key = extract_api_key(&auth).unwrap();
        assert_eq!(key, "sk-ant-test");
    }

    #[test]
    fn test_auth_extraction_failure_none() {
        let result = extract_api_key(&ProviderAuth::None);
        assert!(result.is_err());
    }

    #[test]
    fn test_auth_extraction_failure_oauth() {
        let auth = ProviderAuth::OAuthToken("token".into());
        let result = extract_api_key(&auth);
        assert!(result.is_err());
    }

    #[test]
    fn test_separate_system_messages() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "You are helpful.".to_string(),
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".to_string(),
            },
            ChatMessage {
                role: Role::Assistant,
                content: "Hi there!".to_string(),
            },
        ];

        let (system, conversation) = separate_system_messages(&messages);
        assert_eq!(system.as_deref(), Some("You are helpful."));
        assert_eq!(conversation.len(), 2);
        assert_eq!(conversation[0].role, "user");
        assert_eq!(conversation[0].content, "Hello");
        assert_eq!(conversation[1].role, "assistant");
        assert_eq!(conversation[1].content, "Hi there!");
    }

    #[test]
    fn test_separate_system_messages_multiple_system() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "Rule 1".to_string(),
            },
            ChatMessage {
                role: Role::System,
                content: "Rule 2".to_string(),
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".to_string(),
            },
        ];

        let (system, conversation) = separate_system_messages(&messages);
        assert_eq!(system.as_deref(), Some("Rule 1\nRule 2"));
        assert_eq!(conversation.len(), 1);
    }

    #[test]
    fn test_separate_system_messages_no_system() {
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "Hello".to_string(),
        }];

        let (system, conversation) = separate_system_messages(&messages);
        assert!(system.is_none());
        assert_eq!(conversation.len(), 1);
    }

    #[test]
    fn test_request_serialization() {
        let body = AnthropicMessagesRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
            system: Some("You are helpful.".to_string()),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
            }],
        };

        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-20250514");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["system"], "You are helpful.");
        assert_eq!(json["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_request_serialization_no_system() {
        let body = AnthropicMessagesRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 1024,
            system: None,
            messages: vec![],
        };

        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("system").is_none()); // skip_serializing_if = None
    }

    #[test]
    fn test_response_deserialization() {
        let json = r#"{
            "id": "msg_abc123",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-20250514",
            "content": [
                { "type": "text", "text": "Hello! How can I help?" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 15,
                "output_tokens": 8
            }
        }"#;

        let resp: AnthropicMessagesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert_eq!(
            resp.content[0].text.as_deref(),
            Some("Hello! How can I help?")
        );
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.usage.as_ref().unwrap().input_tokens, Some(15));
        assert_eq!(resp.usage.as_ref().unwrap().output_tokens, Some(8));
        assert_eq!(resp.model.as_deref(), Some("claude-sonnet-4-20250514"));
    }

    #[test]
    fn test_response_without_usage() {
        let json = r#"{
            "content": [
                { "type": "text", "text": "test" }
            ]
        }"#;

        let resp: AnthropicMessagesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content[0].text.as_deref(), Some("test"));
        assert!(resp.usage.is_none());
        assert!(resp.model.is_none());
    }

    #[test]
    fn test_response_multiple_content_blocks() {
        let json = r#"{
            "content": [
                { "type": "text", "text": "Hello " },
                { "type": "text", "text": "world!" }
            ]
        }"#;

        let resp: AnthropicMessagesResponse = serde_json::from_str(json).unwrap();
        let combined: String = resp
            .content
            .into_iter()
            .filter_map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(combined, "Hello world!");
    }
}
