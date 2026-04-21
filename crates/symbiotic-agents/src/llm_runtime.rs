//! LLM runtime manager for Ollama health checks, model management, and preflight validation.
//!
//! Provides `LlmRuntime` which wraps Ollama's HTTP API to check availability,
//! list/pull models, and run preflight checks before LLM-dependent operations.

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Default health check / availability timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// Default Ollama base URL.
const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Size threshold (bytes) above which we warn before pulling a model.
const LARGE_MODEL_THRESHOLD: u64 = 4_000_000_000; // 4 GB

/// Configuration for the LLM runtime.
#[derive(Debug, Clone)]
pub struct LlmRuntimeConfig {
    /// Ollama base URL (e.g. `http://localhost:11434`).
    pub base_url: String,
    /// Model used for chat/completion tasks.
    pub chat_model: String,
    /// Model used for embedding tasks.
    pub embed_model: String,
    /// Timeout for health/availability checks.
    pub timeout: Duration,
}

impl Default for LlmRuntimeConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            chat_model: "qwen3.5".to_string(),
            embed_model: "nomic-embed-text".to_string(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Information about an available Ollama model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub size: u64,
}

/// Result of a preflight check.
#[derive(Debug, Clone)]
pub struct PreflightResult {
    pub ollama_running: bool,
    pub chat_model_available: bool,
    pub embed_model_available: bool,
    pub errors: Vec<String>,
}

impl PreflightResult {
    /// Returns true if all checks passed.
    pub fn is_ok(&self) -> bool {
        self.ollama_running && self.chat_model_available && self.embed_model_available
    }
}

/// Pull progress information reported during model download.
#[derive(Debug, Clone)]
pub struct PullProgress {
    pub status: String,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}

/// Ollama API response for /api/tags.
#[derive(Debug, Deserialize)]
struct TagsResponse {
    models: Vec<TagsModel>,
}

#[derive(Debug, Deserialize)]
struct TagsModel {
    name: String,
    size: u64,
}

/// Ollama API response line for /api/pull (streamed).
#[derive(Debug, Deserialize)]
struct PullResponseLine {
    status: String,
    #[serde(default)]
    completed: Option<u64>,
    #[serde(default)]
    total: Option<u64>,
}

/// Manages the local Ollama LLM runtime.
pub struct LlmRuntime {
    config: LlmRuntimeConfig,
    http: reqwest::Client,
}

impl LlmRuntime {
    /// Creates a new runtime manager with the given config.
    pub fn new(config: LlmRuntimeConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| anyhow!("failed to build HTTP client: {e}"))?;
        Ok(Self { config, http })
    }

    /// Returns the runtime configuration.
    pub fn config(&self) -> &LlmRuntimeConfig {
        &self.config
    }

    /// Checks if Ollama is running and responsive (GET /api/tags).
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Ollama not reachable at {}: {}", self.config.base_url, e))?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "Ollama health check failed with HTTP {}",
                response.status().as_u16()
            ));
        }

        Ok(())
    }

    /// Quick availability check with the configured timeout.
    /// Returns true if Ollama responds, false otherwise.
    pub async fn is_available(&self) -> bool {
        self.health_check().await.is_ok()
    }

    /// Lists all models currently available in Ollama.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/api/tags", self.config.base_url);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Ollama not reachable at {}: {}", self.config.base_url, e))?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "failed to list models: HTTP {}",
                response.status().as_u16()
            ));
        }

        let tags: TagsResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("failed to parse model list: {e}"))?;

        Ok(tags
            .models
            .into_iter()
            .map(|m| ModelInfo {
                name: m.name,
                size: m.size,
            })
            .collect())
    }

    /// Checks if a specific model is available locally.
    pub async fn has_model(&self, name: &str) -> Result<bool> {
        let models = self.list_models().await?;
        Ok(models.iter().any(|m| model_name_matches(&m.name, name)))
    }

    /// Ensures a model is available, pulling it if necessary.
    ///
    /// Returns `Ok(())` if the model is already available or was pulled successfully.
    /// The `on_progress` callback receives progress updates during the pull.
    pub async fn ensure_model<F>(&self, name: &str, on_progress: F) -> Result<()>
    where
        F: FnMut(PullProgress) + Send,
    {
        if self.has_model(name).await? {
            return Ok(());
        }

        self.pull_model(name, on_progress).await
    }

    /// Pulls a model from the Ollama registry.
    ///
    /// Streams progress and reports via the callback.
    pub async fn pull_model<F>(&self, name: &str, mut on_progress: F) -> Result<()>
    where
        F: FnMut(PullProgress) + Send,
    {
        let url = format!("{}/api/pull", self.config.base_url);

        // Use a longer timeout for pulls (they can take a while).
        let pull_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3600))
            .build()
            .map_err(|e| anyhow!("failed to build pull client: {e}"))?;

        let response = pull_client
            .post(&url)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .map_err(|e| anyhow!("failed to start model pull for {name}: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "model pull failed with HTTP {}: {}",
                status.as_u16(),
                text
            ));
        }

        // Read the streamed response line by line.
        let body = response
            .text()
            .await
            .map_err(|e| anyhow!("failed to read pull response: {e}"))?;

        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(progress) = serde_json::from_str::<PullResponseLine>(line) {
                // Warn if model is large
                if let Some(total) = progress.total {
                    if total > LARGE_MODEL_THRESHOLD {
                        on_progress(PullProgress {
                            status: format!(
                                "WARNING: model {} is {:.1} GB",
                                name,
                                total as f64 / 1_000_000_000.0
                            ),
                            completed: progress.completed,
                            total: Some(total),
                        });
                    }
                }
                on_progress(PullProgress {
                    status: progress.status,
                    completed: progress.completed,
                    total: progress.total,
                });
            }
        }

        Ok(())
    }

    /// Runs preflight checks for all configured models.
    ///
    /// Returns a `PreflightResult` with detailed status. Does NOT fail;
    /// callers should inspect the result and decide how to proceed.
    pub async fn preflight(&self) -> PreflightResult {
        let mut result = PreflightResult {
            ollama_running: false,
            chat_model_available: false,
            embed_model_available: false,
            errors: Vec::new(),
        };

        // Check Ollama is running
        match self.health_check().await {
            Ok(()) => {
                result.ollama_running = true;
            }
            Err(e) => {
                result.errors.push(format!(
                    "Ollama is not running at {}. Install: https://ollama.ai/download — {}",
                    self.config.base_url, e
                ));
                return result;
            }
        }

        // Check chat model
        match self.has_model(&self.config.chat_model).await {
            Ok(true) => {
                result.chat_model_available = true;
            }
            Ok(false) => {
                result.errors.push(format!(
                    "Chat model '{}' not found. Run: ollama pull {}",
                    self.config.chat_model, self.config.chat_model
                ));
            }
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to check chat model: {e}"));
            }
        }

        // Check embed model
        match self.has_model(&self.config.embed_model).await {
            Ok(true) => {
                result.embed_model_available = true;
            }
            Ok(false) => {
                result.errors.push(format!(
                    "Embedding model '{}' not found. Run: ollama pull {}",
                    self.config.embed_model, self.config.embed_model
                ));
            }
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to check embedding model: {e}"));
            }
        }

        result
    }

    /// Formats preflight errors as a user-friendly message with install instructions.
    pub fn format_preflight_errors(result: &PreflightResult) -> String {
        if result.is_ok() {
            return "LLM runtime: all checks passed.".to_string();
        }

        let mut msg = String::from("LLM runtime preflight failed:\n");
        for error in &result.errors {
            msg.push_str(&format!("  - {error}\n"));
        }
        if !result.ollama_running {
            msg.push_str("\nTo install Ollama: https://ollama.ai/download\n");
            msg.push_str("Then start it with: ollama serve\n");
        }
        msg
    }
}

/// Checks whether a model name matches, accounting for tag suffixes.
/// e.g. "qwen3.5:latest" matches "qwen3.5"
fn model_name_matches(installed: &str, requested: &str) -> bool {
    if installed == requested {
        return true;
    }
    // "qwen3.5:latest" matches "qwen3.5"
    if let Some(base) = installed.split(':').next() {
        if base == requested {
            return true;
        }
    }
    // "qwen3.5" matches "qwen3.5:latest"
    if let Some(base) = requested.split(':').next() {
        if base == installed || installed.starts_with(&format!("{base}:")) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- model_name_matches (pure unit tests, no network) --

    #[test]
    fn model_name_exact_match() {
        assert!(model_name_matches("qwen3.5", "qwen3.5"));
    }

    #[test]
    fn model_name_matches_with_tag() {
        assert!(model_name_matches("qwen3.5:latest", "qwen3.5"));
    }

    #[test]
    fn model_name_matches_requested_with_tag() {
        assert!(model_name_matches("qwen3.5", "qwen3.5:latest"));
    }

    #[test]
    fn model_name_does_not_match_different() {
        assert!(!model_name_matches("qwen3.5", "mistral"));
    }

    // -- format_preflight_errors (pure unit tests, no network) --

    #[test]
    fn format_preflight_all_ok() {
        let result = PreflightResult {
            ollama_running: true,
            chat_model_available: true,
            embed_model_available: true,
            errors: vec![],
        };
        let msg = LlmRuntime::format_preflight_errors(&result);
        assert!(msg.contains("all checks passed"));
    }

    #[test]
    fn format_preflight_with_errors_includes_install_instructions() {
        let result = PreflightResult {
            ollama_running: false,
            chat_model_available: false,
            embed_model_available: false,
            errors: vec!["Ollama is not running at http://localhost:11434".to_string()],
        };
        let msg = LlmRuntime::format_preflight_errors(&result);
        assert!(msg.contains("preflight failed"));
        assert!(msg.contains("https://ollama.ai/download"));
        assert!(msg.contains("ollama serve"));
    }

    // -- config defaults (pure unit test, no network) --

    #[test]
    fn config_defaults_are_sensible() {
        let config = LlmRuntimeConfig::default();
        assert_eq!(config.base_url, "http://localhost:11434");
        assert_eq!(config.chat_model, "qwen3.5");
        assert_eq!(config.embed_model, "nomic-embed-text");
        assert_eq!(config.timeout, Duration::from_secs(2));
    }
}

/// Integration tests that require network access (wiremock binds to local ports).
/// Run with: `cargo test -p symbiotic-agents --features integration`
#[cfg(all(test, feature = "integration"))]
mod integration_tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config_for(server: &MockServer) -> LlmRuntimeConfig {
        LlmRuntimeConfig {
            base_url: server.uri(),
            chat_model: "qwen3.5".to_string(),
            embed_model: "nomic-embed-text".to_string(),
            timeout: Duration::from_secs(2),
        }
    }

    // -- health_check --

    #[tokio::test]
    async fn health_check_succeeds_when_ollama_responds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        assert!(runtime.health_check().await.is_ok());
    }

    #[tokio::test]
    async fn health_check_fails_when_ollama_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let err = runtime.health_check().await.unwrap_err();
        assert!(err.to_string().contains("health check failed"));
    }

    #[tokio::test]
    async fn health_check_fails_when_ollama_unreachable() {
        let config = LlmRuntimeConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let runtime = LlmRuntime::new(config).unwrap();
        let err = runtime.health_check().await.unwrap_err();
        assert!(err.to_string().contains("not reachable"));
    }

    // -- is_available --

    #[tokio::test]
    async fn is_available_returns_true_when_healthy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        assert!(runtime.is_available().await);
    }

    #[tokio::test]
    async fn is_available_returns_false_when_unreachable() {
        let config = LlmRuntimeConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let runtime = LlmRuntime::new(config).unwrap();
        assert!(!runtime.is_available().await);
    }

    // -- list_models --

    #[tokio::test]
    async fn list_models_returns_available_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"name": "qwen3.5:latest", "size": 2_000_000_000u64},
                    {"name": "nomic-embed-text:latest", "size": 274_000_000u64}
                ]
            })))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let models = runtime.list_models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "qwen3.5:latest");
        assert_eq!(models[1].name, "nomic-embed-text:latest");
    }

    #[tokio::test]
    async fn list_models_returns_empty_when_no_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let models = runtime.list_models().await.unwrap();
        assert!(models.is_empty());
    }

    // -- has_model --

    #[tokio::test]
    async fn has_model_finds_installed_model() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"name": "qwen3.5:latest", "size": 2_000_000_000u64}
                ]
            })))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        assert!(runtime.has_model("qwen3.5").await.unwrap());
    }

    #[tokio::test]
    async fn has_model_returns_false_for_missing_model() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        assert!(!runtime.has_model("qwen3.5").await.unwrap());
    }

    // -- preflight --

    #[tokio::test]
    async fn preflight_passes_when_all_models_present() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"name": "qwen3.5:latest", "size": 2_000_000_000u64},
                    {"name": "nomic-embed-text:latest", "size": 274_000_000u64}
                ]
            })))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let result = runtime.preflight().await;
        assert!(result.is_ok());
        assert!(result.ollama_running);
        assert!(result.chat_model_available);
        assert!(result.embed_model_available);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn preflight_fails_when_ollama_unreachable() {
        let config = LlmRuntimeConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let runtime = LlmRuntime::new(config).unwrap();
        let result = runtime.preflight().await;
        assert!(!result.is_ok());
        assert!(!result.ollama_running);
        assert!(!result.errors.is_empty());
        assert!(result.errors[0].contains("not running"));
    }

    #[tokio::test]
    async fn preflight_fails_when_chat_model_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"name": "nomic-embed-text:latest", "size": 274_000_000u64}
                ]
            })))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let result = runtime.preflight().await;
        assert!(!result.is_ok());
        assert!(result.ollama_running);
        assert!(!result.chat_model_available);
        assert!(result.embed_model_available);
        assert!(result.errors.iter().any(|e| e.contains("qwen3.5")));
    }

    #[tokio::test]
    async fn preflight_fails_when_embed_model_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"name": "qwen3.5:latest", "size": 2_000_000_000u64}
                ]
            })))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let result = runtime.preflight().await;
        assert!(!result.is_ok());
        assert!(result.ollama_running);
        assert!(result.chat_model_available);
        assert!(!result.embed_model_available);
        assert!(result.errors.iter().any(|e| e.contains("nomic-embed-text")));
    }

    // -- ensure_model --

    #[tokio::test]
    async fn ensure_model_noop_when_already_present() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"name": "qwen3.5:latest", "size": 2_000_000_000u64}
                ]
            })))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        // Should not call /api/pull at all
        runtime.ensure_model("qwen3.5", |_| {}).await.unwrap();
    }

    #[tokio::test]
    async fn ensure_model_pulls_when_missing() {
        let server = MockServer::start().await;

        // First call: model not present
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;

        // Pull endpoint
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"status\":\"success\"}\n"))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let mut progress_received = false;
        runtime
            .ensure_model("qwen3.5", |_p| {
                progress_received = true;
            })
            .await
            .unwrap();
        assert!(progress_received);
    }

    // -- pull_model --

    #[tokio::test]
    async fn pull_model_reports_progress() {
        let server = MockServer::start().await;

        let body = [
            r#"{"status":"pulling manifest"}"#,
            r#"{"status":"downloading","completed":500000000,"total":2000000000}"#,
            r#"{"status":"downloading","completed":2000000000,"total":2000000000}"#,
            r#"{"status":"success"}"#,
        ]
        .join("\n");

        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let mut statuses = Vec::new();
        runtime
            .pull_model("qwen3.5", |p| {
                statuses.push(p.status.clone());
            })
            .await
            .unwrap();

        assert!(statuses.contains(&"pulling manifest".to_string()));
        assert!(statuses.contains(&"success".to_string()));
    }

    #[tokio::test]
    async fn pull_model_warns_on_large_model() {
        let server = MockServer::start().await;

        // 5 GB model
        let body = r#"{"status":"downloading","completed":0,"total":5000000000}"#;

        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let runtime = LlmRuntime::new(config_for(&server)).unwrap();
        let mut got_warning = false;
        runtime
            .pull_model("big-model", |p| {
                if p.status.contains("WARNING") {
                    got_warning = true;
                }
            })
            .await
            .unwrap();

        assert!(got_warning);
    }
}
