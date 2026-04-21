//! Extraction strategies for converting browser snapshots to structured content.

use std::collections::HashMap;

use anyhow::Result;
use symbiotic_core::now_unix;
use thiserror::Error;

use crate::types::{
    BrowserResult, ExtractedContent, ExtractedImage, ExtractedLink, ExtractionType,
};

/// Returned when [`ResultParser::parse_with_firewall_scan`] quarantines an
/// extracted browser snapshot (T132 §05).
#[derive(Debug, Error)]
#[error("browser extract quarantined by firewall ({quarantine_class:?}) at {source_url}")]
pub struct FirewallQuarantineError {
    pub source_url: String,
    pub quarantine_class: Option<symbiotic_firewall::types::QuarantineClass>,
}

/// Strategy trait for site-specific extraction from accessibility snapshots.
pub trait ExtractionStrategy: Send + Sync {
    fn extract(&self, snapshot: &str) -> Result<ExtractedContent>;
}

/// Parser that converts accessibility snapshots into structured results.
pub struct ResultParser {
    strategies: HashMap<ExtractionType, Box<dyn ExtractionStrategy>>,
}

impl ResultParser {
    /// Create a new `ResultParser` with default strategies.
    pub fn new() -> Self {
        let mut strategies: HashMap<ExtractionType, Box<dyn ExtractionStrategy>> = HashMap::new();
        strategies.insert(ExtractionType::Generic, Box::new(GenericStrategy));
        strategies.insert(ExtractionType::Tweet, Box::new(TweetStrategy));
        strategies.insert(ExtractionType::Article, Box::new(ArticleStrategy));
        Self { strategies }
    }

    /// Register a custom extraction strategy.
    pub fn register(
        &mut self,
        extraction_type: ExtractionType,
        strategy: Box<dyn ExtractionStrategy>,
    ) {
        self.strategies.insert(extraction_type, strategy);
    }

    /// Parse an accessibility snapshot into structured content.
    /// Auto-detects the extraction type based on URL and snapshot content.
    pub fn parse(&self, url: &str, snapshot: &str) -> Result<BrowserResult> {
        let extraction_type = self.detect_type(url, snapshot);
        let strategy = self
            .strategies
            .get(&extraction_type)
            .or_else(|| self.strategies.get(&ExtractionType::Generic))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no extraction strategy registered for {extraction_type:?} or Generic"
                )
            })?;
        let content = strategy.extract(snapshot)?;
        Ok(BrowserResult {
            extraction_type,
            content,
            source_url: url.to_string(),
            captured_at: now_unix(),
            parser_version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }

    /// Parse a snapshot **and** run the Content Firewall (T132 §05) on the
    /// extracted body before handing the result to downstream consumers.
    ///
    /// Browser-automation HTML is `VeryLow` trust per design §2.3. Returns
    /// `Ok((result, verdict))` for `Passed`/`Flagged`; returns `Err` carrying
    /// a [`FirewallQuarantineError`] when the firewall quarantines the
    /// extracted body — downstream callers MUST route the error to the
    /// quarantine sink and MUST NOT persist `result`.
    pub fn parse_with_firewall_scan(
        &self,
        url: &str,
        snapshot: &str,
    ) -> Result<(BrowserResult, symbiotic_firewall::types::FirewallVerdict)> {
        let result = self.parse(url, snapshot)?;
        let scan_ctx = symbiotic_firewall::types::ScanContext {
            source: symbiotic_firewall::types::ContentSource {
                kind: "browser.extract".into(),
                url: Some(url.into()),
                fetched_at: time::OffsetDateTime::now_utc(),
                claimed_content_type: Some("text/html".into()),
                headers: Default::default(),
            },
            consuming_agent_scope: symbiotic_firewall::types::ConsumingAgentScope::minimal(
                "browser-extractor",
            ),
            call_site: symbiotic_firewall::types::CallSite::new("browser.extract"),
        };
        let cfg_a = symbiotic_firewall::stages::StageAConfig::default();
        let cfg_b = symbiotic_firewall::stages::StageBConfig::default();
        let mut verdict = symbiotic_firewall::stages::run_stages_a_b(
            &scan_ctx,
            &result.content.body,
            &cfg_a,
            &cfg_b,
        );
        // §07 source-receipt placeholder.
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(snapshot.as_bytes());
        verdict.source_receipt_id = Some(format!("{:x}", h.finalize()));
        if matches!(
            verdict.verdict,
            symbiotic_firewall::types::Verdict::Quarantined
        ) {
            return Err(anyhow::anyhow!(FirewallQuarantineError {
                source_url: url.to_string(),
                quarantine_class: verdict.quarantine_class,
            }));
        }
        Ok((result, verdict))
    }

    /// Detect the type of content at a URL.
    pub fn detect_type(&self, url: &str, snapshot: &str) -> ExtractionType {
        if url.contains("x.com") || url.contains("twitter.com") {
            ExtractionType::Tweet
        } else if snapshot.contains("<article") || snapshot.contains("role=\"article\"") {
            ExtractionType::Article
        } else {
            ExtractionType::Generic
        }
    }
}

impl Default for ResultParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Generic extraction: returns the snapshot text as-is.
pub struct GenericStrategy;

impl ExtractionStrategy for GenericStrategy {
    fn extract(&self, snapshot: &str) -> Result<ExtractedContent> {
        Ok(ExtractedContent::with_body(snapshot.to_string()))
    }
}

/// Tweet extraction: extracts text, author, and links from a tweet snapshot.
pub struct TweetStrategy;

impl ExtractionStrategy for TweetStrategy {
    fn extract(&self, snapshot: &str) -> Result<ExtractedContent> {
        let mut author = None;
        let mut body_lines = Vec::new();
        let mut links = Vec::new();
        let mut images = Vec::new();

        for line in snapshot.lines() {
            let trimmed = line.trim();
            // Heuristic: lines starting with @ are author handles
            if trimmed.starts_with('@') && author.is_none() {
                author = Some(trimmed.to_string());
                continue;
            }
            // Heuristic: lines containing http are links
            if let Some(url_start) = trimmed.find("http") {
                let url_end = trimmed[url_start..]
                    .find(|c: char| c.is_whitespace())
                    .map(|i| url_start + i)
                    .unwrap_or(trimmed.len());
                let url = &trimmed[url_start..url_end];
                links.push(ExtractedLink {
                    url: url.to_string(),
                    text: trimmed.to_string(),
                    context: trimmed.to_string(),
                });
            }
            // Heuristic: lines with img or image
            if trimmed.contains("[image:") || trimmed.contains("[img:") {
                let alt = trimmed
                    .split('[')
                    .nth(1)
                    .and_then(|s| s.split(']').next())
                    .unwrap_or("")
                    .replace("image:", "")
                    .replace("img:", "")
                    .trim()
                    .to_string();
                images.push(ExtractedImage {
                    src: String::new(),
                    alt,
                });
                continue;
            }
            if !trimmed.is_empty() {
                body_lines.push(trimmed.to_string());
            }
        }

        Ok(ExtractedContent {
            body: body_lines.join("\n"),
            title: None,
            author,
            published_at: None,
            links,
            images,
            metadata: serde_json::Value::Null,
        })
    }
}

/// Article extraction: extracts title and body from article-like content.
pub struct ArticleStrategy;

impl ExtractionStrategy for ArticleStrategy {
    fn extract(&self, snapshot: &str) -> Result<ExtractedContent> {
        let mut title = None;
        let mut body_lines = Vec::new();
        let mut links = Vec::new();

        for line in snapshot.lines() {
            let trimmed = line.trim();
            // Heuristic: first heading-like line is the title
            if trimmed.starts_with('#') && title.is_none() {
                title = Some(trimmed.trim_start_matches('#').trim().to_string());
                continue;
            }
            if trimmed.starts_with("heading:") && title.is_none() {
                title = Some(trimmed.trim_start_matches("heading:").trim().to_string());
                continue;
            }
            // Extract links
            if let Some(url_start) = trimmed.find("http") {
                let url_end = trimmed[url_start..]
                    .find(|c: char| c.is_whitespace())
                    .map(|i| url_start + i)
                    .unwrap_or(trimmed.len());
                let url = &trimmed[url_start..url_end];
                links.push(ExtractedLink {
                    url: url.to_string(),
                    text: trimmed.to_string(),
                    context: trimmed.to_string(),
                });
            }
            if !trimmed.is_empty() {
                body_lines.push(trimmed.to_string());
            }
        }

        Ok(ExtractedContent {
            body: body_lines.join("\n"),
            title,
            author: None,
            published_at: None,
            links,
            images: Vec::new(),
            metadata: serde_json::Value::Null,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_type_recognizes_twitter_urls() {
        let parser = ResultParser::new();
        assert_eq!(
            parser.detect_type("https://x.com/user/status/123", ""),
            ExtractionType::Tweet
        );
        assert_eq!(
            parser.detect_type("https://twitter.com/user/status/123", ""),
            ExtractionType::Tweet
        );
    }

    #[test]
    fn detect_type_recognizes_article_snapshots() {
        let parser = ResultParser::new();
        assert_eq!(
            parser.detect_type(
                "https://blog.example.com/post",
                "some <article content here"
            ),
            ExtractionType::Article
        );
        assert_eq!(
            parser.detect_type(
                "https://blog.example.com/post",
                "div role=\"article\" content"
            ),
            ExtractionType::Article
        );
    }

    #[test]
    fn detect_type_falls_back_to_generic() {
        let parser = ResultParser::new();
        assert_eq!(
            parser.detect_type("https://example.com", "just text"),
            ExtractionType::Generic
        );
    }

    #[test]
    fn generic_strategy_returns_snapshot_as_body() {
        let strategy = GenericStrategy;
        let content = strategy.extract("Hello world").expect("extract");
        assert_eq!(content.body, "Hello world");
        assert!(content.title.is_none());
    }

    #[test]
    fn tweet_strategy_extracts_author_and_links() {
        let snapshot = "\
@elonmusk
This is a tweet about https://example.com cool stuff
[image: rocket launch]
More text here";

        let strategy = TweetStrategy;
        let content = strategy.extract(snapshot).expect("extract");
        assert_eq!(content.author.as_deref(), Some("@elonmusk"));
        assert!(content.body.contains("This is a tweet about"));
        assert!(content.body.contains("More text here"));
        assert_eq!(content.links.len(), 1);
        assert_eq!(content.links[0].url, "https://example.com");
        assert_eq!(content.images.len(), 1);
        assert_eq!(content.images[0].alt, "rocket launch");
    }

    #[test]
    fn article_strategy_extracts_title_and_links() {
        let snapshot = "\
# Understanding Rust Lifetimes
This article explains lifetimes.
See more at https://doc.rust-lang.org/book/
Another paragraph.";

        let strategy = ArticleStrategy;
        let content = strategy.extract(snapshot).expect("extract");
        assert_eq!(
            content.title.as_deref(),
            Some("Understanding Rust Lifetimes")
        );
        assert!(content.body.contains("This article explains"));
        assert_eq!(content.links.len(), 1);
        assert!(content.links[0].url.contains("rust-lang.org"));
    }

    #[test]
    fn result_parser_parse_returns_browser_result() {
        let parser = ResultParser::new();
        let result = parser
            .parse(
                "https://x.com/user/status/123",
                "@user\nHello from Twitter\nhttps://t.co/abc",
            )
            .expect("parse");
        assert_eq!(result.extraction_type, ExtractionType::Tweet);
        assert_eq!(result.source_url, "https://x.com/user/status/123");
        assert!(!result.parser_version.is_empty());
        assert!(result.captured_at > 0);
    }

    #[test]
    fn result_parser_custom_strategy() {
        struct Custom;
        impl ExtractionStrategy for Custom {
            fn extract(&self, _snapshot: &str) -> Result<ExtractedContent> {
                Ok(ExtractedContent {
                    body: "custom".to_string(),
                    title: Some("Custom Title".to_string()),
                    author: None,
                    published_at: None,
                    links: Vec::new(),
                    images: Vec::new(),
                    metadata: serde_json::json!({"custom": true}),
                })
            }
        }

        let mut parser = ResultParser::new();
        parser.register(ExtractionType::Structured, Box::new(Custom));

        // Force structured type by registering and calling directly
        let strategy = parser.strategies.get(&ExtractionType::Structured).unwrap();
        let content = strategy.extract("anything").expect("extract");
        assert_eq!(content.body, "custom");
        assert_eq!(content.title.as_deref(), Some("Custom Title"));
    }

    #[test]
    fn result_parser_falls_back_to_generic_for_unknown() {
        let parser = ResultParser::new();
        let result = parser
            .parse("https://example.com/page", "just some text")
            .expect("parse");
        assert_eq!(result.extraction_type, ExtractionType::Generic);
        assert_eq!(result.content.body, "just some text");
    }
}
