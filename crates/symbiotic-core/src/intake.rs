use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IntakeSource {
    Cli,
    Matrix,
    Share,
    Bookmarks,
    Notes,
    Api,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IntakeKind {
    Url,
    Note,
    LocalFile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IntakeRoute {
    Archive,
    Vault,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IntakeStatus {
    Ingested,
    Duplicate,
    Blocked,
    Invalid,
    FetchFailed,
    ParseFailed,
    StoreFailed,
    QueueFailed,
    SensitivePendingApproval,
    SecureRouted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeRequest {
    pub source: IntakeSource,
    pub kind: IntakeKind,
    pub urls: Vec<Url>,
    pub note: Option<String>,
    pub tags: Vec<String>,
    pub file_path: Option<String>,
    /// Optional operator-supplied title. When set, skips LLM title generation
    /// and is persisted directly onto the stored `ArchiveDocument`.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntakeItemResult {
    pub input: String,
    pub normalized_url: Option<Url>,
    pub status: IntakeStatus,
    pub route: IntakeRoute,
    pub review_queued: bool,
    pub review_job_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub record_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeResult {
    pub run_id: String,
    pub status: IntakeStatus,
    pub route: IntakeRoute,
    pub review_queued: bool,
    pub review_job_id: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct IntakeSummary {
    pub total: usize,
    pub ingested: usize,
    pub duplicates: usize,
    pub blocked: usize,
    pub invalid: usize,
    pub failed: usize,
    pub secure_routed: usize,
    pub review_queued: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeBatchResult {
    pub run_id: String,
    pub items: Vec<IntakeItemResult>,
    pub summary: IntakeSummary,
}

#[derive(Debug, Error)]
pub enum IntakeError {
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

const STRIP_QUERY_PREFIXES: &[&str] = &["utm_", "fbclid", "gclid"];

pub fn normalize_url(input: &str) -> Result<Url, IntakeError> {
    let mut parsed = Url::parse(input).map_err(|e| IntakeError::InvalidUrl(e.to_string()))?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(IntakeError::UnsupportedScheme(other.to_string())),
    }

    parsed.set_fragment(None);

    if let Some(host) = parsed.host_str() {
        let lowered = host.to_lowercase();
        parsed
            .set_host(Some(&lowered))
            .map_err(|e| IntakeError::InvalidUrl(e.to_string()))?;
    }

    if let Some(query) = parsed.query() {
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .filter(|(k, _)| !should_strip_param(k))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        parsed.set_query(None);
        if !pairs.is_empty() {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in pairs {
                serializer.append_pair(&k, &v);
            }
            parsed.set_query(Some(&serializer.finish()));
        }
    }

    Ok(parsed)
}

pub fn idempotency_key(normalized_url: &Url, tags: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized_url.as_str());
    hasher.update("|");

    let sorted_tags = normalize_tags(tags);
    for tag in sorted_tags {
        hasher.update(tag.as_bytes());
        hasher.update(",");
    }

    format!("{:x}", hasher.finalize())
}

pub fn idempotency_key_for_note(note: &str, tags: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(note.as_bytes());
    hasher.update("|");

    let sorted_tags = normalize_tags(tags);
    for tag in sorted_tags {
        hasher.update(tag.as_bytes());
        hasher.update(",");
    }

    format!("{:x}", hasher.finalize())
}

pub fn idempotency_key_for_file(file_path: &str, tags: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"file:");
    hasher.update(file_path.as_bytes());
    hasher.update("|");

    let sorted_tags = normalize_tags(tags);
    for tag in sorted_tags {
        hasher.update(tag.as_bytes());
        hasher.update(",");
    }

    format!("{:x}", hasher.finalize())
}

pub fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = tags
        .iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn should_strip_param(key: &str) -> bool {
    STRIP_QUERY_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_tracking_params_and_fragment() {
        let normalized =
            normalize_url("https://Example.com/a/b?utm_source=x&keep=1&fbclid=abc#section")
                .expect("normalize should succeed");

        assert_eq!(normalized.as_str(), "https://example.com/a/b?keep=1");
    }

    #[test]
    fn normalize_rejects_non_http_scheme() {
        let err = normalize_url("file:///tmp/data.txt").expect_err("must fail");
        assert!(matches!(err, IntakeError::UnsupportedScheme(_)));
    }

    #[test]
    fn idempotency_key_is_stable_for_tag_order() {
        let url = normalize_url("https://example.com/x?keep=1").expect("valid URL");
        let a = idempotency_key(&url, &["b".into(), "a".into()]);
        let b = idempotency_key(&url, &["a".into(), "b".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn idempotency_key_for_note_is_stable_for_tag_order() {
        let a = idempotency_key_for_note("my secret", &["b".into(), "a".into()]);
        let b = idempotency_key_for_note("my secret", &["a".into(), "b".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_tags_trims_dedupes_and_sorts() {
        let tags = normalize_tags(&[
            "  Security ".to_string(),
            "security".to_string(),
            "AI".to_string(),
            "".to_string(),
        ]);
        assert_eq!(tags, vec!["ai".to_string(), "security".to_string()]);
    }
}
