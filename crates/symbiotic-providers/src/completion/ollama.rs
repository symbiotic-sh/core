//! Ollama completion provider for local LLM inference.
//!
//! Connects to a running Ollama instance via its HTTP API (`/api/chat`).
//! No authentication required — Ollama runs on the local machine.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    CapabilitySet, CompletionProvider, CompletionRequest, CompletionResponse, ModelProvider,
    PricingInfo, ProviderCapability, ProviderClass, ProviderError,
};

/// Completion provider backed by a local Ollama instance.
pub struct OllamaCompletionProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    capabilities: CapabilitySet,
}

impl OllamaCompletionProvider {
    /// Create a new Ollama completion provider.
    ///
    /// # Arguments
    /// * `base_url` — Base URL of the Ollama HTTP API (e.g. `"http://localhost:11434"`).
    /// * `model` — Model tag to use (e.g. `"qwen3.5"`).
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            model,
            capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
        }
    }
}

impl ModelProvider for OllamaCompletionProvider {
    fn name(&self) -> &str {
        "ollama"
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
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
    /// Disable reasoning/thinking mode for models that support it (e.g. Qwen 3.5).
    /// When false, the model skips internal chain-of-thought and responds directly.
    think: bool,
    /// Ollama response-format hint. `"json"` forces the sampler to emit only
    /// syntactically valid JSON — the model can't end mid-string or forget a
    /// closing brace. Critical for the ReAct loop, whose entire contract is
    /// that every assistant turn is a parseable `LlmAction` JSON object.
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Serialize)]
struct OllamaOptions {
    /// Maximum number of tokens to generate.
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
    /// Context window in tokens. Ollama defaults to 2048 if unset — far below
    /// what modern models natively support (gemma3/4 is 131072). Without this,
    /// long orchestrator conversations silently truncate, causing the agent
    /// to "forget" earlier dispatches and loop. Read from
    /// `SYMBIOTIC_OLLAMA_NUM_CTX` at request time with a 32K default.
    #[serde(skip_serializing_if = "Option::is_none")]
    num_ctx: Option<u32>,
}

/// Read the Ollama context-window override from the environment. Defaults to
/// 32768 — big enough for multi-agent orchestration with several sub-agent
/// observations in scope, small enough not to OOM modest hardware.
fn resolve_num_ctx() -> u32 {
    std::env::var("SYMBIOTIC_OLLAMA_NUM_CTX")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(32768)
}

/// Read the Ollama max-output-tokens override from the environment. Defaults
/// to 8192 — enough for the orchestrator to emit a full decision memo as a
/// `file_write` tool call (memo content is typically 2-4K tokens) without
/// hitting Ollama's stingy default (128-512 tokens depending on version) that
/// truncates JSON mid-string and breaks the ReAct parser.
fn resolve_num_predict(caller_override: Option<u32>) -> u32 {
    caller_override.unwrap_or_else(|| {
        std::env::var("SYMBIOTIC_OLLAMA_NUM_PREDICT")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(8192)
    })
}

#[derive(Serialize)]
struct OllamaChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaResponseMessage,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

// -- CompletionProvider impl -------------------------------------------------

#[async_trait]
impl CompletionProvider for OllamaCompletionProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let messages: Vec<OllamaChatMessage> = request
            .messages
            .iter()
            .map(|m| OllamaChatMessage {
                role: role_to_string(m.role),
                content: m.content.clone(),
            })
            .collect();

        let options = Some(OllamaOptions {
            num_predict: Some(resolve_num_predict(request.max_tokens)),
            num_ctx: Some(resolve_num_ctx()),
        });

        let body = OllamaChatRequest {
            model: self.model.clone(),
            messages,
            stream: false,
            think: false,
            // Force valid JSON output. The ReAct loop parses every assistant
            // turn as `LlmAction` JSON; without this, long responses can end
            // one closing brace short and the whole turn gets dropped.
            format: Some("json"),
            options,
        };

        let url = format!("{}/api/chat", self.base_url);

        // Optional wire-tap: when SYMBIOTIC_OLLAMA_TRACE_DIR is set, dump the
        // full request body and raw response content to a per-agent folder
        // so every agent's conversation is reviewable on its own with zero
        // post-processing. Layout:
        //
        //   {dir}/
        //     {role}__{agent_id}/
        //       turn-{iteration:04}-request.json
        //       turn-{iteration:04}-response.txt
        //     _untagged/
        //       seq-NNNN-{request,response}.*
        //
        // The TraceTag task-local is set by `run_agent_with_config` in the
        // agents crate. When absent (non-agent callers, tests), we fall back
        // to a flat `_untagged/` bucket.
        let trace_dir = std::env::var("SYMBIOTIC_OLLAMA_TRACE_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let trace_target: Option<(String, String)> = trace_dir.map(|dir| {
            let (folder, stem) = match symbiotic_core::trace::current_tag() {
                Some(tag) => {
                    // agent_id is already `{unix_ts:010}__{origin}__{role}`
                    // (see `run_agent_goal`) so it sorts chronologically
                    // on its own. Use it directly as the folder name —
                    // no synthetic prefix, no redundant timestamp.
                    (
                        format!("{}/{}", dir, tag.agent_id),
                        format!("turn-{:04}", tag.iteration),
                    )
                }
                None => {
                    use std::sync::atomic::{AtomicUsize, Ordering};
                    static SEQ: AtomicUsize = AtomicUsize::new(0);
                    let n = SEQ.fetch_add(1, Ordering::SeqCst);
                    (format!("{}/_untagged", dir), format!("seq-{n:04}"))
                }
            };
            let _ = std::fs::create_dir_all(&folder);
            if let Ok(pretty) = serde_json::to_string_pretty(&body) {
                let _ = std::fs::write(format!("{folder}/{stem}-request.json"), pretty);
            }
            (folder, stem)
        });

        let resp = self
            .client
            .post(&url)
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

        let parsed: OllamaChatResponse = resp
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(format!("json parse failed: {e}")))?;

        if let Some((folder, stem)) = trace_target {
            let _ = std::fs::write(
                format!("{folder}/{stem}-response.txt"),
                &parsed.message.content,
            );
        }

        Ok(CompletionResponse {
            content: parsed.message.content,
            model: parsed.model.unwrap_or_else(|| self.model.clone()),
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

/// Convert our `Role` enum to the lowercase string Ollama expects.
fn role_to_string(role: crate::Role) -> String {
    match role {
        crate::Role::System => "system".to_string(),
        crate::Role::User => "user".to_string(),
        crate::Role::Assistant => "assistant".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_metadata() {
        let provider = OllamaCompletionProvider::new(
            "http://localhost:11434".to_string(),
            "qwen3.5".to_string(),
        );

        assert_eq!(provider.name(), "ollama");
        assert_eq!(provider.provider_class(), ProviderClass::Local);
        assert_eq!(provider.model_name(), "qwen3.5");
        assert!(provider.capabilities().has(ProviderCapability::Completion));
        assert!(!provider.capabilities().has(ProviderCapability::Embedding));
        assert!(provider.pricing().is_none());
    }

    #[test]
    fn test_request_serialization() {
        let body = OllamaChatRequest {
            model: "qwen3.5".to_string(),
            messages: vec![
                OllamaChatMessage {
                    role: "system".to_string(),
                    content: "You are helpful.".to_string(),
                },
                OllamaChatMessage {
                    role: "user".to_string(),
                    content: "Hello".to_string(),
                },
            ],
            stream: false,
            think: false,
            format: None,
            options: None,
        };

        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "qwen3.5");
        assert_eq!(json["stream"], false);
        assert_eq!(json["think"], false);
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        // options omitted when None
        assert!(json.get("options").is_none());
    }

    #[test]
    fn test_request_with_max_tokens() {
        let body = OllamaChatRequest {
            model: "qwen3.5".to_string(),
            messages: vec![],
            stream: false,
            think: false,
            format: None,
            options: Some(OllamaOptions {
                num_predict: Some(4096),
                num_ctx: Some(32768),
            }),
        };

        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["options"]["num_predict"], 4096);
    }

    #[test]
    fn test_response_deserialization() {
        let json = r#"{
            "model": "qwen3.5",
            "message": {
                "role": "assistant",
                "content": "Hello! How can I help you?"
            }
        }"#;

        let resp: OllamaChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "Hello! How can I help you?");
        assert_eq!(resp.model.as_deref(), Some("qwen3.5"));
    }

    #[test]
    fn test_response_without_model() {
        let json = r#"{
            "message": {
                "role": "assistant",
                "content": "test"
            }
        }"#;

        let resp: OllamaChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "test");
        assert!(resp.model.is_none());
    }

    #[test]
    fn test_role_to_string() {
        assert_eq!(role_to_string(crate::Role::System), "system");
        assert_eq!(role_to_string(crate::Role::User), "user");
        assert_eq!(role_to_string(crate::Role::Assistant), "assistant");
    }
}
