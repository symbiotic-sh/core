use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::llm_audit::{ModelVerification, ModelVerificationStatus};

const DEFAULT_OLLAMA_CHAT_MODEL: &str = "qwen3.5";
const DEFAULT_OLLAMA_EMBED_MODEL: &str = "nomic-embed-text";

#[derive(Debug, Deserialize)]
struct ModelManifestFile {
    #[serde(default)]
    models: Vec<ModelManifestEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelManifestEntry {
    name: String,
    digest: String,
    #[allow(dead_code)]
    version: Option<String>,
    #[allow(dead_code)]
    verified_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagModel>,
}

#[derive(Debug, Clone, Deserialize)]
struct OllamaTagModel {
    name: String,
    #[serde(default)]
    digest: Option<String>,
}

pub fn requested_ollama_models(chat_model: Option<&str>) -> Vec<String> {
    let mut names = BTreeSet::new();
    names.insert(
        chat_model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(DEFAULT_OLLAMA_CHAT_MODEL)
            .to_string(),
    );
    names.insert(DEFAULT_OLLAMA_EMBED_MODEL.to_string());
    names.into_iter().collect()
}

pub async fn verify_ollama_models(
    base_url: &str,
    models: &[String],
    manifest_path: &Path,
) -> Vec<ModelVerification> {
    if models.is_empty() {
        return Vec::new();
    }

    let manifest = load_manifest(manifest_path).map_err(|error| error.to_string());
    let tags = match fetch_ollama_tags(base_url).await {
        Ok(tags) => tags,
        Err(error) => {
            return models
                .iter()
                .map(|model_name| ModelVerification {
                    provider: "ollama".to_string(),
                    model_name: model_name.clone(),
                    expected_digest: None,
                    actual_digest: None,
                    matched: false,
                    status: ModelVerificationStatus::ProviderUnavailable,
                    detail: Some(error.to_string()),
                })
                .collect();
        }
    };

    models
        .iter()
        .map(|model_name| {
            let installed = tags
                .iter()
                .find(|installed| model_name_matches(&installed.name, model_name));
            let Some(installed) = installed else {
                return ModelVerification {
                    provider: "ollama".to_string(),
                    model_name: model_name.clone(),
                    expected_digest: manifest
                        .as_ref()
                        .ok()
                        .and_then(|manifest| manifest.get(model_name).cloned()),
                    actual_digest: None,
                    matched: false,
                    status: ModelVerificationStatus::ModelMissing,
                    detail: Some(format!("model {model_name} not reported by /api/tags")),
                };
            };

            let Some(actual_digest) = installed.digest.clone().filter(|value| !value.is_empty())
            else {
                return ModelVerification {
                    provider: "ollama".to_string(),
                    model_name: model_name.clone(),
                    expected_digest: manifest
                        .as_ref()
                        .ok()
                        .and_then(|manifest| manifest.get(model_name).cloned()),
                    actual_digest: None,
                    matched: false,
                    status: ModelVerificationStatus::DigestMissing,
                    detail: Some(format!("model {model_name} has no digest in /api/tags")),
                };
            };

            match &manifest {
                Ok(manifest) => match manifest_digest_for(manifest, model_name) {
                    Some(expected_digest) if expected_digest == &actual_digest => {
                        ModelVerification {
                            provider: "ollama".to_string(),
                            model_name: model_name.clone(),
                            expected_digest: Some(expected_digest.clone()),
                            actual_digest: Some(actual_digest),
                            matched: true,
                            status: ModelVerificationStatus::Verified,
                            detail: None,
                        }
                    }
                    Some(expected_digest) => ModelVerification {
                        provider: "ollama".to_string(),
                        model_name: model_name.clone(),
                        expected_digest: Some(expected_digest.clone()),
                        actual_digest: Some(actual_digest.clone()),
                        matched: false,
                        status: ModelVerificationStatus::Mismatch,
                        detail: Some(format!(
                            "expected digest {expected_digest} but Ollama reported {actual_digest}"
                        )),
                    },
                    None => ModelVerification {
                        provider: "ollama".to_string(),
                        model_name: model_name.clone(),
                        expected_digest: None,
                        actual_digest: Some(actual_digest),
                        matched: false,
                        status: ModelVerificationStatus::ManifestMissing,
                        detail: Some(format!(
                            "no manifest entry for model {model_name} in {}",
                            manifest_path.display()
                        )),
                    },
                },
                Err(error) => ModelVerification {
                    provider: "ollama".to_string(),
                    model_name: model_name.clone(),
                    expected_digest: None,
                    actual_digest: Some(actual_digest),
                    matched: false,
                    status: ModelVerificationStatus::ManifestMissing,
                    detail: Some(error.clone()),
                },
            }
        })
        .collect()
}

fn load_manifest(path: &Path) -> Result<HashMap<String, String>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading model manifest {}", path.display()))?;
    let parsed: ModelManifestFile = toml::from_str(&raw)
        .with_context(|| format!("parsing model manifest {}", path.display()))?;
    Ok(parsed
        .models
        .into_iter()
        .map(|entry| (entry.name, entry.digest))
        .collect())
}

async fn fetch_ollama_tags(base_url: &str) -> Result<Vec<OllamaTagModel>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("requesting Ollama tags from {url}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Ollama tags request failed with HTTP {status}: {body}");
    }
    let parsed: OllamaTagsResponse = response
        .json()
        .await
        .with_context(|| format!("parsing Ollama tags response from {url}"))?;
    Ok(parsed.models)
}

fn manifest_digest_for<'a>(
    manifest: &'a HashMap<String, String>,
    model_name: &str,
) -> Option<&'a String> {
    manifest
        .iter()
        .find(|(name, _)| model_name_matches(name, model_name))
        .map(|(_, digest)| digest)
}

fn model_name_matches(installed: &str, requested: &str) -> bool {
    normalize_model_name(installed) == normalize_model_name(requested)
}

fn normalize_model_name(name: &str) -> &str {
    name.split(':').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn requested_models_include_chat_and_embedding_once() {
        let models = requested_ollama_models(Some("nomic-embed-text"));
        assert_eq!(models, vec!["nomic-embed-text".to_string()]);

        let models = requested_ollama_models(Some("qwen3.5"));
        assert_eq!(
            models,
            vec!["nomic-embed-text".to_string(), "qwen3.5".to_string()]
        );
    }

    #[tokio::test]
    async fn verify_ollama_models_reports_verified_and_missing_manifest_entries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    { "name": "qwen3.5:latest", "digest": "sha256:qwen" },
                    { "name": "nomic-embed-text:latest", "digest": "sha256:embed" }
                ]
            })))
            .mount(&server)
            .await;

        let dir = tempdir().expect("tempdir");
        let manifest_path = dir.path().join("model-manifest.toml");
        std::fs::write(
            &manifest_path,
            r#"
[[models]]
name = "qwen3.5"
digest = "sha256:qwen"
"#,
        )
        .expect("write manifest");

        let results = verify_ollama_models(
            &server.uri(),
            &["qwen3.5".to_string(), "nomic-embed-text".to_string()],
            &manifest_path,
        )
        .await;

        assert_eq!(results.len(), 2);
        let embed = results
            .iter()
            .find(|verification| verification.model_name == "nomic-embed-text")
            .expect("embedding verification");
        let chat = results
            .iter()
            .find(|verification| verification.model_name == "qwen3.5")
            .expect("chat verification");
        assert_eq!(embed.status, ModelVerificationStatus::ManifestMissing);
        assert_eq!(chat.status, ModelVerificationStatus::Verified);
    }

    #[tokio::test]
    async fn verify_ollama_models_reports_provider_unavailable() {
        let dir = tempdir().expect("tempdir");
        let manifest_path = dir.path().join("model-manifest.toml");
        std::fs::write(
            &manifest_path,
            r#"
[[models]]
name = "qwen3.5"
digest = "sha256:qwen"
"#,
        )
        .expect("write manifest");

        let results = verify_ollama_models(
            "http://127.0.0.1:1",
            &["qwen3.5".to_string()],
            &manifest_path,
        )
        .await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].status,
            ModelVerificationStatus::ProviderUnavailable
        );
    }
}
