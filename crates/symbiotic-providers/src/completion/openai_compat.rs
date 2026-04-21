//! Generic OpenAI-compatible completion provider.
//!
//! Covers any provider that exposes a `/chat/completions` endpoint with the
//! same request/response shape as OpenAI's API: OpenRouter, Venice, Together,
//! Groq, Fireworks, Mistral, and self-hosted vLLM / LiteLLM instances.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    CapabilitySet, CompletionProvider, CompletionRequest, CompletionResponse, ModelProvider,
    PricingInfo, ProviderAuth, ProviderCapability, ProviderClass, ProviderError,
};

/// A completion provider that speaks the OpenAI `/chat/completions` wire format.
///
/// This single struct covers all aggregators and any service that is
/// API-compatible with OpenAI. Differences between providers are handled via
/// the `base_url`, `extra_headers`, and `provider_class` fields.
///
/// # Examples
///
/// ```no_run
/// use symbiotic_providers::{ProviderAuth, completion::GenericOpenAiCompatProvider};
///
/// let provider = GenericOpenAiCompatProvider::openrouter(
///     ProviderAuth::ApiKey("sk-or-...".into()),
///     "meta-llama/llama-3.1-70b".into(),
/// );
/// ```
pub struct GenericOpenAiCompatProvider {
    client: reqwest::Client,
    name: String,
    auth: ProviderAuth,
    base_url: String,
    model: String,
    provider_class: ProviderClass,
    capabilities: CapabilitySet,
    pricing: Option<PricingInfo>,
    extra_headers: Vec<(String, String)>,
}

impl GenericOpenAiCompatProvider {
    /// Create a new generic OpenAI-compatible provider.
    ///
    /// The provider is initialized with [`ProviderCapability::Completion`] by default.
    /// Use the builder methods to add pricing or extra headers.
    pub fn new(
        name: String,
        auth: ProviderAuth,
        base_url: String,
        model: String,
        provider_class: ProviderClass,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            name,
            auth,
            base_url,
            model,
            provider_class,
            capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
            pricing: None,
            extra_headers: Vec::new(),
        }
    }

    /// Attach pricing information to this provider.
    #[must_use]
    pub fn with_pricing(mut self, pricing: PricingInfo) -> Self {
        self.pricing = Some(pricing);
        self
    }

    /// Attach extra HTTP headers sent with every request.
    ///
    /// This is useful for provider-specific requirements such as
    /// OpenRouter's `HTTP-Referer` header.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Add additional capabilities beyond the default [`ProviderCapability::Completion`].
    #[must_use]
    pub fn with_capabilities(mut self, caps: Vec<ProviderCapability>) -> Self {
        for cap in caps {
            self.capabilities.add(cap);
        }
        self
    }

    // -- Factory functions for common aggregators ---------------------------------

    /// Pre-configured for [OpenRouter](https://openrouter.ai).
    ///
    /// Sets the base URL to `https://openrouter.ai/api/v1` and includes the
    /// `HTTP-Referer` header required by OpenRouter's ToS.
    pub fn openrouter(auth: ProviderAuth, model: String) -> Self {
        Self::new(
            "openrouter".to_string(),
            auth,
            "https://openrouter.ai/api/v1".to_string(),
            model,
            ProviderClass::Aggregator,
        )
        .with_extra_headers(vec![(
            "HTTP-Referer".to_string(),
            "https://symbiotic.sh".to_string(),
        )])
    }

    /// Pre-configured for [Venice AI](https://venice.ai).
    ///
    /// Sets the base URL to `https://api.venice.ai/api/v1`.
    pub fn venice(auth: ProviderAuth, model: String) -> Self {
        Self::new(
            "venice".to_string(),
            auth,
            "https://api.venice.ai/api/v1".to_string(),
            model,
            ProviderClass::Aggregator,
        )
    }

    /// Pre-configured for [Together AI](https://together.ai).
    pub fn together(auth: ProviderAuth, model: String) -> Self {
        Self::new(
            "together".to_string(),
            auth,
            "https://api.together.xyz/v1".to_string(),
            model,
            ProviderClass::Aggregator,
        )
    }

    /// Pre-configured for [Groq](https://groq.com).
    pub fn groq(auth: ProviderAuth, model: String) -> Self {
        Self::new(
            "groq".to_string(),
            auth,
            "https://api.groq.com/openai/v1".to_string(),
            model,
            ProviderClass::Cloud,
        )
    }

    /// Pre-configured for [Google Gemini](https://ai.google.dev) via its
    /// OpenAI-compatible endpoint.
    ///
    /// Uses `https://generativelanguage.googleapis.com/v1beta/openai` which
    /// speaks the standard OpenAI chat completions format. Auth is via
    /// Gemini API key (free tier: 1,000 req/day).
    ///
    /// **TOS note:** API key auth is explicitly permitted for programmatic
    /// use in daemons and servers. Do NOT use Google OAuth tokens here.
    pub fn gemini(auth: ProviderAuth, model: String) -> Self {
        Self::new(
            "gemini".to_string(),
            auth,
            "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            model,
            ProviderClass::Cloud,
        )
        .with_pricing(PricingInfo {
            // Gemini 2.5 Flash pricing (as of 2026)
            input_per_1k_tokens: Some(0.00015),
            output_per_1k_tokens: Some(0.0006),
            ..Default::default()
        })
    }

    // -- Private helpers ----------------------------------------------------------

    /// Extract the bearer token from `self.auth`, or return an error.
    fn bearer_token(&self) -> Result<&str, ProviderError> {
        match &self.auth {
            ProviderAuth::ApiKey(key) => Ok(key.as_str()),
            ProviderAuth::OAuthToken(tok) => Ok(tok.as_str()),
            ProviderAuth::None => Err(ProviderError::AuthFailed(
                "no credentials configured".into(),
            )),
            ProviderAuth::SessionToken(_) => Err(ProviderError::AuthFailed(
                "session tokens are not supported for OpenAI-compatible APIs".into(),
            )),
        }
    }

    /// Build the full endpoint URL for chat completions.
    fn completions_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }
}

// -- ModelProvider ----------------------------------------------------------------

impl ModelProvider for GenericOpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn provider_class(&self) -> ProviderClass {
        self.provider_class
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    fn pricing(&self) -> Option<&PricingInfo> {
        self.pricing.as_ref()
    }
}

// -- CompletionProvider -----------------------------------------------------------

#[async_trait]
impl CompletionProvider for GenericOpenAiCompatProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let token = self.bearer_token()?;

        // Build the request body in the OpenAI wire format.
        let messages: Vec<WireMessage> = request
            .messages
            .iter()
            .map(|m| WireMessage {
                role: match m.role {
                    crate::Role::System => "system",
                    crate::Role::User => "user",
                    crate::Role::Assistant => "assistant",
                },
                content: &m.content,
            })
            .collect();

        let body = WireRequest {
            model: &self.model,
            messages: &messages,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop: request.stop.as_deref(),
        };

        let mut req = self
            .client
            .post(self.completions_url())
            .bearer_auth(token)
            .json(&body);

        for (key, value) in &self.extra_headers {
            req = req.header(key, value);
        }

        let http_response = req.send().await.map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                ProviderError::Unavailable(format!("{}: {e}", self.name))
            } else {
                ProviderError::RequestFailed(format!("{}: {e}", self.name))
            }
        })?;

        let status = http_response.status();

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::AuthFailed(format!(
                "{}: HTTP {}",
                self.name, status
            )));
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Try to extract Retry-After, fall back to 1 second.
            let retry_ms = http_response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(1000);
            return Err(ProviderError::RateLimited {
                retry_after_ms: retry_ms,
            });
        }

        if !status.is_success() {
            let body_text = http_response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".into());
            return Err(ProviderError::RequestFailed(format!(
                "{}: HTTP {} — {}",
                self.name, status, body_text
            )));
        }

        // Parse the response.
        let wire: WireResponse = http_response.json().await.map_err(|e| {
            ProviderError::InvalidResponse(format!("{}: failed to parse response: {e}", self.name))
        })?;

        let choice = wire.choices.first().ok_or_else(|| {
            ProviderError::InvalidResponse(format!("{}: response contained no choices", self.name))
        })?;

        Ok(CompletionResponse {
            content: choice.message.content.clone().unwrap_or_default(),
            model: wire.model.unwrap_or_else(|| self.model.clone()),
            input_tokens: wire.usage.as_ref().map(|u| u.prompt_tokens),
            output_tokens: wire.usage.as_ref().map(|u| u.completion_tokens),
            finish_reason: choice.finish_reason.clone(),
        })
    }
}

// -- OpenAI wire format types (private) -------------------------------------------

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: &'a [WireMessage<'a>],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'a [String]>,
}

#[derive(Deserialize)]
struct WireResponse {
    model: Option<String>,
    choices: Vec<WireChoice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

// -- Tests ------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderCapability, ProviderClass};

    // -- Metadata tests -----------------------------------------------------------

    #[test]
    fn metadata_openrouter() {
        let p = GenericOpenAiCompatProvider::openrouter(
            ProviderAuth::ApiKey("test-key".into()),
            "meta-llama/llama-3.1-70b".into(),
        );
        assert_eq!(p.name(), "openrouter");
        assert_eq!(p.provider_class(), ProviderClass::Aggregator);
        assert_eq!(p.model_name(), "meta-llama/llama-3.1-70b");
        assert!(p.capabilities().has(ProviderCapability::Completion));
        assert!(!p.capabilities().has(ProviderCapability::Embedding));
        assert!(p.pricing().is_none());
    }

    #[test]
    fn metadata_venice() {
        let p = GenericOpenAiCompatProvider::venice(
            ProviderAuth::ApiKey("venice-key".into()),
            "llama-3.3-70b".into(),
        );
        assert_eq!(p.name(), "venice");
        assert_eq!(p.provider_class(), ProviderClass::Aggregator);
        assert_eq!(p.model_name(), "llama-3.3-70b");
        assert!(p.capabilities().has(ProviderCapability::Completion));
    }

    #[test]
    fn metadata_together() {
        let p = GenericOpenAiCompatProvider::together(
            ProviderAuth::ApiKey("together-key".into()),
            "mistralai/Mixtral-8x7B-Instruct-v0.1".into(),
        );
        assert_eq!(p.name(), "together");
        assert_eq!(p.provider_class(), ProviderClass::Aggregator);
    }

    #[test]
    fn metadata_groq() {
        let p = GenericOpenAiCompatProvider::groq(
            ProviderAuth::ApiKey("groq-key".into()),
            "llama3-70b-8192".into(),
        );
        assert_eq!(p.name(), "groq");
        assert_eq!(p.provider_class(), ProviderClass::Cloud);
    }

    #[test]
    fn metadata_custom() {
        let p = GenericOpenAiCompatProvider::new(
            "my-vllm".into(),
            ProviderAuth::None,
            "http://localhost:8000/v1".into(),
            "my-model".into(),
            ProviderClass::Local,
        );
        assert_eq!(p.name(), "my-vllm");
        assert_eq!(p.provider_class(), ProviderClass::Local);
        assert_eq!(p.model_name(), "my-model");
    }

    // -- Builder tests ------------------------------------------------------------

    #[test]
    fn builder_with_pricing() {
        let pricing = PricingInfo {
            input_per_1k_tokens: Some(0.001),
            output_per_1k_tokens: Some(0.002),
            ..Default::default()
        };
        let p = GenericOpenAiCompatProvider::openrouter(
            ProviderAuth::ApiKey("k".into()),
            "model".into(),
        )
        .with_pricing(pricing);

        let info = p.pricing().expect("pricing should be set");
        assert_eq!(info.input_per_1k_tokens, Some(0.001));
        assert_eq!(info.output_per_1k_tokens, Some(0.002));
    }

    #[test]
    fn builder_with_extra_headers() {
        let p = GenericOpenAiCompatProvider::new(
            "test".into(),
            ProviderAuth::ApiKey("k".into()),
            "https://example.com/v1".into(),
            "m".into(),
            ProviderClass::Cloud,
        )
        .with_extra_headers(vec![("X-Custom".into(), "value".into())]);
        assert_eq!(p.extra_headers.len(), 1);
        assert_eq!(p.extra_headers[0].0, "X-Custom");
    }

    #[test]
    fn builder_with_capabilities() {
        let p = GenericOpenAiCompatProvider::new(
            "test".into(),
            ProviderAuth::ApiKey("k".into()),
            "https://example.com/v1".into(),
            "m".into(),
            ProviderClass::Cloud,
        )
        .with_capabilities(vec![
            ProviderCapability::FunctionCall,
            ProviderCapability::Vision,
        ]);

        assert!(p.capabilities().has(ProviderCapability::Completion));
        assert!(p.capabilities().has(ProviderCapability::FunctionCall));
        assert!(p.capabilities().has(ProviderCapability::Vision));
    }

    // -- Factory function tests ---------------------------------------------------

    #[test]
    fn openrouter_factory_has_referer_header() {
        let p =
            GenericOpenAiCompatProvider::openrouter(ProviderAuth::ApiKey("k".into()), "m".into());
        assert_eq!(p.extra_headers.len(), 1);
        assert_eq!(p.extra_headers[0].0, "HTTP-Referer");
        assert_eq!(p.extra_headers[0].1, "https://symbiotic.sh");
    }

    #[test]
    fn venice_factory_no_extra_headers() {
        let p = GenericOpenAiCompatProvider::venice(ProviderAuth::ApiKey("k".into()), "m".into());
        assert!(p.extra_headers.is_empty());
    }

    #[test]
    fn factory_base_urls() {
        let or =
            GenericOpenAiCompatProvider::openrouter(ProviderAuth::ApiKey("k".into()), "m".into());
        assert_eq!(or.base_url, "https://openrouter.ai/api/v1");

        let v = GenericOpenAiCompatProvider::venice(ProviderAuth::ApiKey("k".into()), "m".into());
        assert_eq!(v.base_url, "https://api.venice.ai/api/v1");

        let t = GenericOpenAiCompatProvider::together(ProviderAuth::ApiKey("k".into()), "m".into());
        assert_eq!(t.base_url, "https://api.together.xyz/v1");

        let g = GenericOpenAiCompatProvider::groq(ProviderAuth::ApiKey("k".into()), "m".into());
        assert_eq!(g.base_url, "https://api.groq.com/openai/v1");

        let gm = GenericOpenAiCompatProvider::gemini(
            ProviderAuth::ApiKey("k".into()),
            "gemini-2.5-flash".into(),
        );
        assert_eq!(
            gm.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }

    #[test]
    fn metadata_gemini() {
        let p = GenericOpenAiCompatProvider::gemini(
            ProviderAuth::ApiKey("gemini-key".into()),
            "gemini-2.5-flash".into(),
        );
        assert_eq!(p.name(), "gemini");
        assert_eq!(p.provider_class(), ProviderClass::Cloud);
        assert_eq!(p.model_name(), "gemini-2.5-flash");
        assert!(p.capabilities().has(ProviderCapability::Completion));
        let pricing = p.pricing().expect("gemini should have pricing");
        assert_eq!(pricing.input_per_1k_tokens, Some(0.00015));
        assert_eq!(pricing.output_per_1k_tokens, Some(0.0006));
    }

    // -- URL construction ---------------------------------------------------------

    #[test]
    fn completions_url_no_trailing_slash() {
        let p = GenericOpenAiCompatProvider::new(
            "test".into(),
            ProviderAuth::ApiKey("k".into()),
            "https://example.com/v1".into(),
            "m".into(),
            ProviderClass::Cloud,
        );
        assert_eq!(
            p.completions_url(),
            "https://example.com/v1/chat/completions"
        );
    }

    #[test]
    fn completions_url_with_trailing_slash() {
        let p = GenericOpenAiCompatProvider::new(
            "test".into(),
            ProviderAuth::ApiKey("k".into()),
            "https://example.com/v1/".into(),
            "m".into(),
            ProviderClass::Cloud,
        );
        assert_eq!(
            p.completions_url(),
            "https://example.com/v1/chat/completions"
        );
    }

    // -- Auth edge cases ----------------------------------------------------------

    #[test]
    fn bearer_token_from_api_key() {
        let p = GenericOpenAiCompatProvider::new(
            "test".into(),
            ProviderAuth::ApiKey("my-key".into()),
            "https://example.com/v1".into(),
            "m".into(),
            ProviderClass::Cloud,
        );
        assert_eq!(p.bearer_token().unwrap(), "my-key");
    }

    #[test]
    fn bearer_token_from_oauth() {
        let p = GenericOpenAiCompatProvider::new(
            "test".into(),
            ProviderAuth::OAuthToken("oauth-tok".into()),
            "https://example.com/v1".into(),
            "m".into(),
            ProviderClass::Cloud,
        );
        assert_eq!(p.bearer_token().unwrap(), "oauth-tok");
    }

    #[test]
    fn bearer_token_none_auth_fails() {
        let p = GenericOpenAiCompatProvider::new(
            "test".into(),
            ProviderAuth::None,
            "https://example.com/v1".into(),
            "m".into(),
            ProviderClass::Local,
        );
        assert!(p.bearer_token().is_err());
    }

    #[test]
    fn bearer_token_session_token_fails() {
        let p = GenericOpenAiCompatProvider::new(
            "test".into(),
            ProviderAuth::SessionToken("sess".into()),
            "https://example.com/v1".into(),
            "m".into(),
            ProviderClass::Cloud,
        );
        assert!(p.bearer_token().is_err());
    }

    // -- Wire format serialization tests ------------------------------------------

    #[test]
    fn wire_request_serializes_correctly() {
        let messages = vec![
            WireMessage {
                role: "system",
                content: "You are helpful.",
            },
            WireMessage {
                role: "user",
                content: "Hello",
            },
        ];
        let req = WireRequest {
            model: "gpt-4o",
            messages: &messages,
            max_tokens: Some(100),
            temperature: Some(0.7),
            stop: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "gpt-4o");
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["content"], "Hello");
        assert_eq!(json["max_tokens"], 100);
        assert!(!json.as_object().unwrap().contains_key("stop"));
    }

    #[test]
    fn wire_request_omits_none_fields() {
        let messages = vec![WireMessage {
            role: "user",
            content: "Hi",
        }];
        let req = WireRequest {
            model: "m",
            messages: &messages,
            max_tokens: None,
            temperature: None,
            stop: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("max_tokens"));
        assert!(!obj.contains_key("temperature"));
        assert!(!obj.contains_key("stop"));
    }

    #[test]
    fn wire_response_deserializes() {
        let json = serde_json::json!({
            "id": "chatcmpl-abc",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        });
        let wire: WireResponse = serde_json::from_value(json).unwrap();
        assert_eq!(wire.model.as_deref(), Some("gpt-4o"));
        assert_eq!(wire.choices.len(), 1);
        assert_eq!(wire.choices[0].message.content.as_deref(), Some("Hello!"));
        assert_eq!(wire.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = wire.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn wire_response_handles_null_content() {
        let json = serde_json::json!({
            "choices": [{
                "message": { "content": null },
                "finish_reason": "stop"
            }]
        });
        let wire: WireResponse = serde_json::from_value(json).unwrap();
        assert!(wire.choices[0].message.content.is_none());
    }

    #[test]
    fn wire_response_handles_missing_usage() {
        let json = serde_json::json!({
            "model": "m",
            "choices": [{
                "message": { "content": "ok" },
                "finish_reason": "stop"
            }]
        });
        let wire: WireResponse = serde_json::from_value(json).unwrap();
        assert!(wire.usage.is_none());
    }
}
