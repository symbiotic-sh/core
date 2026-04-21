//! Core browser automation types.

use serde::{Deserialize, Serialize};

/// Typed extraction result from a browser snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserResult {
    pub extraction_type: ExtractionType,
    pub content: ExtractedContent,
    pub source_url: String,
    pub captured_at: u64,
    pub parser_version: String,
}

/// The kind of content extracted from a page.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionType {
    /// Tweet or thread from X/Twitter.
    Tweet,
    /// Article or blog post.
    Article,
    /// Search results page.
    SearchResults,
    /// Generic page content.
    Generic,
    /// Structured data (table, list, etc.).
    Structured,
}

/// Content extracted from a browser snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedContent {
    /// Main text content (markdown).
    pub body: String,
    /// Page title.
    pub title: Option<String>,
    /// Author or account name.
    pub author: Option<String>,
    /// Publication timestamp (ISO 8601).
    pub published_at: Option<String>,
    /// URLs found in the content.
    pub links: Vec<ExtractedLink>,
    /// Images with alt text.
    pub images: Vec<ExtractedImage>,
    /// Structured data fields (for tables, metadata, etc.).
    pub metadata: serde_json::Value,
}

impl ExtractedContent {
    /// Create an empty `ExtractedContent` with only a body.
    pub fn with_body(body: String) -> Self {
        Self {
            body,
            title: None,
            author: None,
            published_at: None,
            links: Vec::new(),
            images: Vec::new(),
            metadata: serde_json::Value::Null,
        }
    }
}

/// A link extracted from page content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedLink {
    pub url: String,
    pub text: String,
    /// Surrounding text for relevance scoring.
    pub context: String,
}

/// An image extracted from page content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedImage {
    pub src: String,
    pub alt: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_type_serde_roundtrip() {
        let variants = [
            ExtractionType::Tweet,
            ExtractionType::Article,
            ExtractionType::SearchResults,
            ExtractionType::Generic,
            ExtractionType::Structured,
        ];
        for variant in &variants {
            let json = serde_json::to_string(variant).expect("serialize");
            let parsed: ExtractionType = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(&parsed, variant);
        }
    }

    #[test]
    fn extraction_type_uses_snake_case() {
        let json = serde_json::to_string(&ExtractionType::SearchResults).expect("serialize");
        assert_eq!(json, "\"search_results\"");
    }

    #[test]
    fn browser_result_serde_roundtrip() {
        let result = BrowserResult {
            extraction_type: ExtractionType::Tweet,
            content: ExtractedContent::with_body("Hello world".to_string()),
            source_url: "https://x.com/user/status/123".to_string(),
            captured_at: 1700000000,
            parser_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: BrowserResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.extraction_type, ExtractionType::Tweet);
        assert_eq!(parsed.source_url, "https://x.com/user/status/123");
        assert_eq!(parsed.content.body, "Hello world");
    }

    #[test]
    fn extracted_content_with_body_has_empty_collections() {
        let content = ExtractedContent::with_body("test".to_string());
        assert_eq!(content.body, "test");
        assert!(content.title.is_none());
        assert!(content.author.is_none());
        assert!(content.published_at.is_none());
        assert!(content.links.is_empty());
        assert!(content.images.is_empty());
        assert_eq!(content.metadata, serde_json::Value::Null);
    }

    #[test]
    fn extracted_content_with_full_fields() {
        let content = ExtractedContent {
            body: "Article body".to_string(),
            title: Some("My Article".to_string()),
            author: Some("author".to_string()),
            published_at: Some("2024-01-15T10:00:00Z".to_string()),
            links: vec![ExtractedLink {
                url: "https://example.com".to_string(),
                text: "Example".to_string(),
                context: "See Example for more".to_string(),
            }],
            images: vec![ExtractedImage {
                src: "https://example.com/img.png".to_string(),
                alt: "A diagram".to_string(),
            }],
            metadata: serde_json::json!({"tags": ["rust"]}),
        };
        let json = serde_json::to_string(&content).expect("serialize");
        let parsed: ExtractedContent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.title.as_deref(), Some("My Article"));
        assert_eq!(parsed.links.len(), 1);
        assert_eq!(parsed.images.len(), 1);
    }
}
