//! LLM client for Ollama chat API.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
pub use symbiotic_core::protocol::{ChatMessage, LlmClient};

/// Configuration for the LLM client.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    /// Maximum tokens to generate per request (Ollama `num_predict`).
    /// Default: 40000 — generous headroom; the model self-terminates well
    /// before this on most tasks, and performance is unaffected.
    pub max_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434".to_string(),
            model: "qwen3.5".to_string(),
            max_tokens: 40_000,
        }
    }
}

/// Response from the Ollama chat API.
#[derive(Debug, Clone, Deserialize)]
pub struct OllamaChatResponse {
    pub message: ChatMessage,
    pub done: bool,
}

/// Request body for the Ollama chat API.
#[derive(Debug, Clone, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    /// Disable reasoning/thinking mode (e.g. Qwen 3.5 thinking tokens).
    think: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Debug, Clone, Serialize)]
struct OllamaOptions {
    num_predict: u32,
}

/// Production client that talks to Ollama's HTTP API.
pub struct OllamaClient {
    config: LlmConfig,
    http: reqwest::Client,
}

impl OllamaClient {
    pub fn new(config: LlmConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl LlmClient for OllamaClient {
    async fn chat(&self, messages: &[ChatMessage], json_mode: bool) -> Result<String> {
        let url = format!("{}/api/chat", self.config.base_url);
        let body = OllamaChatRequest {
            model: self.config.model.clone(),
            messages: messages.to_vec(),
            stream: false,
            think: false,
            format: if json_mode {
                Some("json".to_string())
            } else {
                None
            },
            options: Some(OllamaOptions {
                num_predict: self.config.max_tokens,
            }),
        };

        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("Ollama unavailable at {}: {}", self.config.base_url, e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Ollama returned HTTP {}: {}",
                status.as_u16(),
                text
            ));
        }

        let chat_response: OllamaChatResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("failed to parse Ollama response: {e}"))?;

        Ok(chat_response.message.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_config_default_values() {
        let config = LlmConfig::default();
        assert_eq!(config.base_url, "http://localhost:11434");
        assert_eq!(config.model, "qwen3.5");
        assert_eq!(config.max_tokens, 40_000);
    }

    #[test]
    fn chat_message_serializes() {
        let msg = ChatMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));
    }

    #[test]
    fn ollama_response_deserializes() {
        let json = r#"{"message":{"role":"assistant","content":"hi"},"done":true}"#;
        let resp: OllamaChatResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.message.role, "assistant");
        assert_eq!(resp.message.content, "hi");
        assert!(resp.done);
    }
}
