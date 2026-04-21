//! TOML configuration parsing for AI providers.
//!
//! The configuration file (`config/providers.toml`) declares which providers
//! are available, their class, optional URLs, auth method, and budget limits.
//! API keys are **not** stored in config — they live in the credential vault.

use std::collections::HashMap;

use serde::Deserialize;

use crate::error::ProviderError;
use crate::types::{ProviderCapability, ProviderClass};

/// Top-level providers configuration, parsed from TOML.
#[derive(Debug, Deserialize)]
pub struct ProvidersConfig {
    /// Map of provider name to its configuration.
    pub providers: HashMap<String, ProviderConfig>,
    /// Default provider for each capability (capability name -> provider name).
    #[serde(default)]
    pub defaults: HashMap<String, String>,
    /// Optional budget configuration.
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
}

/// Configuration for a single provider.
#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    /// Whether this provider runs locally, in the cloud, or is an aggregator.
    pub class: ProviderClass,
    /// Base URL for the provider's API (if not a well-known endpoint).
    pub url: Option<String>,
    /// How credentials are resolved for this provider.
    #[serde(default = "default_auth_method")]
    pub auth: AuthMethod,
    /// List of model identifiers supported by this provider.
    pub models: Option<Vec<String>>,
    /// Capabilities advertised by this provider (overrides auto-detection).
    pub capabilities: Option<Vec<ProviderCapability>>,
    /// Provider type hint (e.g. "anthropic", "openai-compat", "agent").
    #[serde(rename = "type")]
    pub provider_type: Option<String>,
    /// Extra HTTP headers to include in requests.
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
}

/// How a provider resolves credentials.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// Look up credentials from the credential vault by provider name.
    Vault,
    /// No authentication required (e.g. local Ollama).
    None,
    /// Look up credentials from environment variables.
    Env,
}

fn default_auth_method() -> AuthMethod {
    AuthMethod::Vault
}

/// Budget limits to prevent runaway API spend.
#[derive(Debug, Clone, Deserialize)]
pub struct BudgetConfig {
    /// Maximum total daily spend across all providers, in USD.
    pub global_daily_limit_usd: Option<f64>,
    /// Per-provider budget overrides.
    #[serde(default)]
    pub per_provider: HashMap<String, ProviderBudget>,
    /// Percentage of budget at which to fire an alert (default 80%).
    #[serde(default = "default_alert_threshold")]
    pub alert_threshold_percent: f64,
}

fn default_alert_threshold() -> f64 {
    80.0
}

/// Budget limits for a single provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderBudget {
    /// Maximum daily spend in USD.
    pub daily_limit_usd: Option<f64>,
    /// Maximum monthly spend in USD.
    pub monthly_limit_usd: Option<f64>,
    /// Maximum tokens allowed per single request.
    pub max_tokens_per_request: Option<u64>,
    /// Maximum media units (images/video seconds) per day.
    pub max_media_units_per_day: Option<u64>,
    /// Maximum agent tasks per day.
    pub max_agent_tasks_per_day: Option<u64>,
}

/// Load and parse providers config from a TOML file.
pub fn load_providers_config(path: &std::path::Path) -> Result<ProvidersConfig, ProviderError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        ProviderError::ConfigError(format!("failed to read {}: {e}", path.display()))
    })?;
    toml::from_str(&content)
        .map_err(|e| ProviderError::ConfigError(format!("failed to parse TOML: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
[providers.ollama]
class = "local"
url = "http://localhost:11434"
auth = "none"
models = ["qwen3.5", "nomic-embed-text"]

[providers.openai]
class = "cloud"
models = ["gpt-4o-mini", "text-embedding-3-small", "dall-e-3"]

[providers.openrouter]
class = "aggregator"
type = "openai-compat"
url = "https://openrouter.ai/api/v1"
models = ["meta-llama/llama-3.1-70b"]

[defaults]
completion = "ollama"
embedding = "ollama"

[budget]
global_daily_limit_usd = 10.0
alert_threshold_percent = 90.0

[budget.per_provider.openai]
daily_limit_usd = 5.0
monthly_limit_usd = 100.0
max_tokens_per_request = 4096
"#;

        let config: ProvidersConfig = toml::from_str(toml).unwrap();

        // Providers
        assert_eq!(config.providers.len(), 3);

        let ollama = &config.providers["ollama"];
        assert_eq!(ollama.class, ProviderClass::Local);
        assert_eq!(ollama.auth, AuthMethod::None);
        assert_eq!(ollama.url.as_deref(), Some("http://localhost:11434"));
        assert_eq!(
            ollama.models.as_ref().unwrap(),
            &["qwen3.5", "nomic-embed-text"]
        );

        let openai = &config.providers["openai"];
        assert_eq!(openai.class, ProviderClass::Cloud);
        // Default auth is vault.
        assert_eq!(openai.auth, AuthMethod::Vault);
        assert!(openai.url.is_none());

        let openrouter = &config.providers["openrouter"];
        assert_eq!(openrouter.class, ProviderClass::Aggregator);
        assert_eq!(openrouter.provider_type.as_deref(), Some("openai-compat"));

        // Defaults
        assert_eq!(config.defaults["completion"], "ollama");
        assert_eq!(config.defaults["embedding"], "ollama");

        // Budget
        let budget = config.budget.unwrap();
        assert_eq!(budget.global_daily_limit_usd, Some(10.0));
        assert!((budget.alert_threshold_percent - 90.0).abs() < f64::EPSILON);

        let openai_budget = &budget.per_provider["openai"];
        assert_eq!(openai_budget.daily_limit_usd, Some(5.0));
        assert_eq!(openai_budget.monthly_limit_usd, Some(100.0));
        assert_eq!(openai_budget.max_tokens_per_request, Some(4096));
        assert!(openai_budget.max_media_units_per_day.is_none());
        assert!(openai_budget.max_agent_tasks_per_day.is_none());
    }

    #[test]
    fn default_auth_is_vault() {
        let toml = r#"
[providers.anthropic]
class = "cloud"
"#;
        let config: ProvidersConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.providers["anthropic"].auth, AuthMethod::Vault);
    }

    #[test]
    fn budget_config_defaults() {
        let toml = r#"
[providers.test]
class = "local"
auth = "none"

[budget]
"#;
        let config: ProvidersConfig = toml::from_str(toml).unwrap();
        let budget = config.budget.unwrap();
        assert!(budget.global_daily_limit_usd.is_none());
        assert!(budget.per_provider.is_empty());
        assert!((budget.alert_threshold_percent - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn capabilities_in_config() {
        let toml = r#"
[providers.higgsfield]
class = "cloud"
capabilities = ["video_generation", "image_generation"]
"#;
        let config: ProvidersConfig = toml::from_str(toml).unwrap();
        let caps = config.providers["higgsfield"]
            .capabilities
            .as_ref()
            .unwrap();
        assert_eq!(caps.len(), 2);
        assert!(caps.contains(&ProviderCapability::VideoGeneration));
        assert!(caps.contains(&ProviderCapability::ImageGeneration));
    }

    #[test]
    fn extra_headers_parsing() {
        let toml = r#"
[providers.custom]
class = "cloud"

[providers.custom.extra_headers]
X-Custom = "value"
Authorization = "custom-scheme token"
"#;
        let config: ProvidersConfig = toml::from_str(toml).unwrap();
        let headers = &config.providers["custom"].extra_headers;
        assert_eq!(headers["X-Custom"], "value");
        assert_eq!(headers["Authorization"], "custom-scheme token");
    }

    #[test]
    fn no_defaults_or_budget_is_ok() {
        let toml = r#"
[providers.test]
class = "local"
auth = "none"
"#;
        let config: ProvidersConfig = toml::from_str(toml).unwrap();
        assert!(config.defaults.is_empty());
        assert!(config.budget.is_none());
    }

    #[test]
    fn load_providers_config_from_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.toml");
        std::fs::write(
            &path,
            r#"
[providers.ollama]
class = "local"
auth = "none"
models = ["qwen3.5"]

[defaults]
completion = "ollama"
"#,
        )
        .unwrap();

        let config = load_providers_config(&path).unwrap();
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers["ollama"].class, ProviderClass::Local);
        assert_eq!(config.defaults["completion"], "ollama");
    }

    #[test]
    fn load_providers_config_missing_file() {
        let result = load_providers_config(std::path::Path::new("/nonexistent/path.toml"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("failed to read"));
    }

    #[test]
    fn load_providers_config_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not valid { toml }}}").unwrap();

        let result = load_providers_config(&path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("failed to parse TOML"));
    }
}
