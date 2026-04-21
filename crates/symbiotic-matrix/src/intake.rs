use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use symbiotic_core::intake::{
    normalize_tags, normalize_url, IntakeBatchResult, IntakeItemResult, IntakeKind, IntakeRequest,
    IntakeRoute, IntakeSource, IntakeStatus, IntakeSummary,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIntakeMessage {
    pub urls: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntakeReply {
    pub body: String,
    pub result: IntakeBatchResult,
}

pub trait IntakeExecutor: Send + Sync {
    fn execute(&self, request: IntakeRequest) -> Result<IntakeBatchResult>;
}

pub struct IntakeMessageHandler {
    executor: Arc<dyn IntakeExecutor>,
}

impl IntakeMessageHandler {
    pub fn new(executor: Arc<dyn IntakeExecutor>) -> Self {
        Self { executor }
    }

    pub fn handle_text(&self, message: &str, source: IntakeSource) -> Result<IntakeReply> {
        let parsed = parse_intake_message(message);
        let tags = normalize_tags(&parsed.tags);

        let mut normalized_urls = Vec::new();
        let mut invalid_items = Vec::new();
        for raw_url in parsed.urls {
            match normalize_url(&raw_url) {
                Ok(url) => normalized_urls.push(url),
                Err(_) => invalid_items.push(IntakeItemResult {
                    input: raw_url,
                    normalized_url: None,
                    status: IntakeStatus::Invalid,
                    route: IntakeRoute::Archive,
                    review_queued: false,
                    review_job_id: None,
                    idempotency_key: None,
                    record_id: None,
                    error: Some("invalid URL format".to_string()),
                }),
            }
        }

        if normalized_urls.is_empty() && invalid_items.is_empty() {
            return Ok(IntakeReply {
                body: "No URLs detected. Paste one or more http(s) links into #intake.".to_string(),
                result: IntakeBatchResult {
                    run_id: "run_empty_intake".to_string(),
                    items: Vec::new(),
                    summary: IntakeSummary::default(),
                },
            });
        }

        let mut result = if normalized_urls.is_empty() {
            IntakeBatchResult {
                run_id: "run_empty_intake".to_string(),
                items: Vec::new(),
                summary: IntakeSummary::default(),
            }
        } else {
            self.executor.execute(IntakeRequest {
                source,
                kind: IntakeKind::Url,
                urls: normalized_urls,
                note: None,
                tags,
                file_path: None,
                title: None,
            })?
        };

        if !invalid_items.is_empty() {
            result.items.extend(invalid_items);
            result.summary = summarize(&result.items);
        }

        let accepted_count =
            result.summary.ingested + result.summary.duplicates + result.summary.secure_routed;
        let body = if accepted_count > 0 {
            format!(
                "Intake accepted: total={} accepted={} invalid={} failed={}",
                result.summary.total, accepted_count, result.summary.invalid, result.summary.failed
            )
        } else {
            format!(
                "Intake rejected: total={} invalid={} failed={}",
                result.summary.total, result.summary.invalid, result.summary.failed
            )
        };

        Ok(IntakeReply { body, result })
    }
}

pub fn parse_intake_message(message: &str) -> ParsedIntakeMessage {
    let explicit_tags = parse_explicit_tags(message);

    let mut urls = Vec::new();
    let mut tags = Vec::new();

    for token in message.split_whitespace() {
        let cleaned = clean_token(token);
        if cleaned.is_empty() {
            continue;
        }

        if is_url_candidate(cleaned) {
            urls.push(cleaned.to_string());
            continue;
        }

        if let Some(tag) = parse_hashtag(cleaned) {
            tags.push(tag);
        }
    }

    tags.extend(explicit_tags);
    ParsedIntakeMessage {
        urls,
        tags: normalize_tags(&tags),
    }
}

fn parse_explicit_tags(message: &str) -> Vec<String> {
    let lower = message.to_ascii_lowercase();
    let Some(start_idx) = lower.find("[tags:") else {
        return Vec::new();
    };
    let slice = &message[start_idx + "[tags:".len()..];
    let Some(end_idx) = slice.find(']') else {
        return Vec::new();
    };
    let inside = &slice[..end_idx];
    inside
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_hashtag(token: &str) -> Option<String> {
    if !token.starts_with('#') {
        return None;
    }

    let stripped = token.trim_start_matches('#');
    if stripped.is_empty() {
        return None;
    }

    if stripped
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/'))
    {
        Some(stripped.to_string())
    } else {
        None
    }
}

fn clean_token(token: &str) -> &str {
    token
        .trim()
        .trim_matches(|ch: char| matches!(ch, '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';'))
}

fn is_url_candidate(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://") || value.contains("://")
}

fn summarize(items: &[IntakeItemResult]) -> IntakeSummary {
    let mut summary = IntakeSummary {
        total: items.len(),
        ..IntakeSummary::default()
    };

    for item in items {
        match item.status {
            IntakeStatus::Ingested => summary.ingested += 1,
            IntakeStatus::Duplicate => summary.duplicates += 1,
            IntakeStatus::Blocked => summary.blocked += 1,
            IntakeStatus::Invalid => summary.invalid += 1,
            IntakeStatus::SecureRouted => summary.secure_routed += 1,
            IntakeStatus::FetchFailed
            | IntakeStatus::ParseFailed
            | IntakeStatus::StoreFailed
            | IntakeStatus::QueueFailed
            | IntakeStatus::SensitivePendingApproval => summary.failed += 1,
        }

        if item.review_queued {
            summary.review_queued += 1;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use symbiotic_core::intake::{IntakeRoute, IntakeStatus};

    struct MockExecutor {
        should_fail: bool,
    }

    impl IntakeExecutor for MockExecutor {
        fn execute(&self, request: IntakeRequest) -> Result<IntakeBatchResult> {
            if self.should_fail {
                return Err(anyhow!("simulated executor error"));
            }

            let items = request
                .urls
                .iter()
                .map(|url| IntakeItemResult {
                    input: url.as_str().to_string(),
                    normalized_url: Some(url.clone()),
                    status: IntakeStatus::Ingested,
                    route: IntakeRoute::Archive,
                    review_queued: true,
                    review_job_id: Some("job_1".to_string()),
                    idempotency_key: Some("key_1".to_string()),
                    record_id: Some("record_1".to_string()),
                    error: None,
                })
                .collect::<Vec<_>>();

            Ok(IntakeBatchResult {
                run_id: "run_test".to_string(),
                summary: summarize(&items),
                items,
            })
        }
    }

    #[test]
    fn parser_extracts_urls_hashtags_and_explicit_tags() {
        let parsed = parse_intake_message(
            "Check this https://example.com/a and http://x.com/u/status/1 #symbiotic [tags: ai, security]",
        );
        assert_eq!(parsed.urls.len(), 2);
        assert_eq!(parsed.tags, vec!["ai", "security", "symbiotic"]);
    }

    #[test]
    fn parser_ignores_non_url_tokens() {
        let parsed = parse_intake_message("hello world #tag not-a-url");
        assert!(parsed.urls.is_empty());
        assert_eq!(parsed.tags, vec!["tag".to_string()]);
    }

    #[test]
    fn handler_appends_invalid_url_items_to_result() {
        let handler = IntakeMessageHandler::new(Arc::new(MockExecutor { should_fail: false }));
        let reply = handler
            .handle_text("https://example.com ok://invalid", IntakeSource::Matrix)
            .expect("handler should succeed");

        assert_eq!(reply.result.summary.total, 2);
        assert_eq!(reply.result.summary.ingested, 1);
        assert_eq!(reply.result.summary.invalid, 1);
    }

    #[test]
    fn handler_rejects_messages_without_urls() {
        let handler = IntakeMessageHandler::new(Arc::new(MockExecutor { should_fail: false }));
        let reply = handler
            .handle_text("hello world #tag", IntakeSource::Matrix)
            .expect("handler should succeed");
        assert_eq!(reply.result.summary.total, 0);
        assert!(reply.body.contains("No URLs detected"));
    }

    #[test]
    fn handler_returns_error_when_executor_fails() {
        let handler = IntakeMessageHandler::new(Arc::new(MockExecutor { should_fail: true }));
        let err = handler
            .handle_text("https://example.com", IntakeSource::Matrix)
            .expect_err("handler should fail");
        assert!(err.to_string().contains("simulated executor error"));
    }
}
