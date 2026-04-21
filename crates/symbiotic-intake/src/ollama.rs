//! Ollama-powered intake helpers.
//!
//! Provides two LLM-backed features:
//! - **Title extraction** (T12): Generate clean 5-10 word titles from page content
//!   instead of relying on HTML `<title>` tags.
//! - **Link relevance filtering** (T22): Check if a discovered link is worth
//!   following during recursive intake.
//!
//! Both features use the [`LlmClient`] trait from `symbiotic-agents`, making
//! them easy to test with mock responses.

use symbiotic_agents::llm::{ChatMessage, LlmClient};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during Ollama-backed intake helpers.
#[derive(Debug, Error)]
pub enum OllamaIntakeError {
    #[error("LLM call failed: {0}")]
    LlmFailed(String),

    #[error("LLM returned empty response")]
    EmptyResponse,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for Ollama title extraction (T12).
#[derive(Debug, Clone)]
pub struct TitleExtractionConfig {
    /// Enable LLM-based title extraction. When false, the caller should
    /// fall back to HTML `<title>` or other heuristics.
    pub enabled: bool,

    /// Maximum number of characters of page content to send to the LLM.
    /// Keeps the prompt small for fast inference.
    pub max_content_chars: usize,
}

impl Default for TitleExtractionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_content_chars: 2000,
        }
    }
}

/// Configuration for LLM link relevance filtering (T22).
#[derive(Debug, Clone)]
pub struct LinkRelevanceConfig {
    /// Enable LLM-based link relevance checks. When false, the
    /// `fallback_policy` determines whether to follow or skip.
    pub enabled: bool,

    /// What to do when Ollama is unavailable or disabled.
    pub fallback_policy: LinkFallbackPolicy,
}

impl Default for LinkRelevanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fallback_policy: LinkFallbackPolicy::FollowAll,
        }
    }
}

/// Fallback behavior when the LLM relevance check is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFallbackPolicy {
    /// Follow all discovered links (permissive).
    FollowAll,
    /// Skip all discovered links (conservative).
    SkipAll,
}

// ---------------------------------------------------------------------------
// T12: Title Extraction
// ---------------------------------------------------------------------------

/// Maximum length for generated titles (characters). Titles longer than this
/// are truncated with an ellipsis.
const MAX_TITLE_LEN: usize = 120;

/// Generate a clean, descriptive title for a page using an LLM.
///
/// The prompt instructs the model to:
/// - Extract the core topic
/// - Remove site names, author info, SEO cruft
/// - Keep the title concise (5-10 words)
/// - Make it searchable
///
/// Returns the generated title, or an error if the LLM call fails.
/// Callers should fall back to `html_title` on error.
pub async fn generate_title(
    url: &str,
    page_content: &str,
    config: &TitleExtractionConfig,
    llm: &dyn LlmClient,
) -> Result<String, OllamaIntakeError> {
    let truncated_content = truncate_content(page_content, config.max_content_chars);

    let system_prompt = "\
You are a title generator. Given a URL and page content, produce a single clean, \
descriptive title of 5-10 words. Rules:\n\
- Extract the core topic of the content\n\
- Remove site names, author names, SEO cruft\n\
- Do NOT include prefixes like \"Title:\" or quotes\n\
- For tweets/threads, describe the topic rather than the author\n\
- Make it searchable and descriptive\n\
- Respond with ONLY the title text, nothing else";

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!("URL: {url}\n\nContent:\n{truncated_content}"),
        },
    ];

    let response = llm
        .chat(&messages, false)
        .await
        .map_err(|e| OllamaIntakeError::LlmFailed(e.to_string()))?;

    let title = clean_title_response(&response);
    if title.is_empty() {
        return Err(OllamaIntakeError::EmptyResponse);
    }

    Ok(title)
}

/// Generate a title with automatic fallback to the provided HTML title.
///
/// If the LLM is disabled or fails, returns `html_title` instead.
/// If `html_title` is also empty, returns a generic placeholder.
pub async fn generate_title_with_fallback(
    url: &str,
    page_content: &str,
    html_title: Option<&str>,
    config: &TitleExtractionConfig,
    llm: Option<&dyn LlmClient>,
) -> String {
    if config.enabled {
        if let Some(llm) = llm {
            match generate_title(url, page_content, config, llm).await {
                Ok(title) => return title,
                Err(_) => { /* fall through to HTML title */ }
            }
        }
    }

    // Fallback chain: HTML title -> generic
    match html_title {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => "Untitled".to_string(),
    }
}

/// Clean up an LLM title response: strip quotes, "Title:" prefixes, and whitespace.
fn clean_title_response(raw: &str) -> String {
    let mut title = raw.trim().to_string();

    // Strip surrounding quotes
    if (title.starts_with('"') && title.ends_with('"'))
        || (title.starts_with('\'') && title.ends_with('\''))
    {
        title = title[1..title.len() - 1].to_string();
    }

    // Strip common LLM prefixes
    for prefix in &["Title:", "title:", "TITLE:"] {
        if let Some(rest) = title.strip_prefix(prefix) {
            title = rest.trim().to_string();
        }
    }

    // Truncate overly long titles
    if title.len() > MAX_TITLE_LEN {
        // Find last space before the limit to avoid cutting words
        let truncated = &title[..MAX_TITLE_LEN];
        title = match truncated.rfind(' ') {
            Some(pos) => format!("{}...", &truncated[..pos]),
            None => format!("{truncated}..."),
        };
    }

    title
}

// ---------------------------------------------------------------------------
// T22: Link Relevance Filter
// ---------------------------------------------------------------------------

/// Context about a discovered link for relevance checking.
#[derive(Debug, Clone)]
pub struct LinkContext {
    /// Title or topic of the parent page.
    pub parent_title: String,
    /// URL of the parent page.
    pub parent_url: String,
    /// The discovered link URL.
    pub link_url: String,
    /// Text surrounding the link on the parent page (anchor text, nearby sentences).
    pub surrounding_text: String,
}

/// Check whether a discovered link is relevant enough to follow during
/// recursive intake.
///
/// The LLM is asked a binary question: is this link likely to contain
/// content related to the parent page's topic?
///
/// Returns `true` if the link should be followed, `false` otherwise.
pub async fn check_link_relevance(
    context: &LinkContext,
    config: &LinkRelevanceConfig,
    llm: Option<&dyn LlmClient>,
) -> bool {
    if !config.enabled {
        return match config.fallback_policy {
            LinkFallbackPolicy::FollowAll => true,
            LinkFallbackPolicy::SkipAll => false,
        };
    }

    let Some(llm) = llm else {
        return match config.fallback_policy {
            LinkFallbackPolicy::FollowAll => true,
            LinkFallbackPolicy::SkipAll => false,
        };
    };

    match check_link_relevance_llm(context, llm).await {
        Ok(relevant) => relevant,
        Err(_) => match config.fallback_policy {
            LinkFallbackPolicy::FollowAll => true,
            LinkFallbackPolicy::SkipAll => false,
        },
    }
}

/// Internal LLM call for link relevance checking.
async fn check_link_relevance_llm(
    context: &LinkContext,
    llm: &dyn LlmClient,
) -> Result<bool, OllamaIntakeError> {
    let system_prompt = "\
You are a link relevance classifier for a knowledge intake system. \
Given a parent page's topic and a discovered link, decide if the link is \
worth following for deeper knowledge extraction.\n\
\n\
Answer ONLY with \"yes\" or \"no\".\n\
\n\
Follow links that:\n\
- Contain technical content related to the parent topic\n\
- Are primary sources, documentation, or research papers\n\
- Provide deeper context or evidence for claims on the parent page\n\
\n\
Skip links that:\n\
- Are ads, social media profiles, or generic navigation\n\
- Are login/signup pages\n\
- Are unrelated to the parent page's topic\n\
- Are image/video/media files";

    let user_content = format!(
        "Parent page: {} ({})\nDiscovered link: {}\nSurrounding text: {}",
        context.parent_title, context.parent_url, context.link_url, context.surrounding_text
    );

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: user_content,
        },
    ];

    let response = llm
        .chat(&messages, false)
        .await
        .map_err(|e| OllamaIntakeError::LlmFailed(e.to_string()))?;

    let answer = response.trim().to_lowercase();
    Ok(answer.starts_with("yes"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncate content to approximately `max_chars` characters, breaking at a
/// word boundary when possible.
fn truncate_content(content: &str, max_chars: usize) -> &str {
    if content.len() <= max_chars {
        return content;
    }

    // Find the last space before the limit
    match content[..max_chars].rfind(' ') {
        Some(pos) => &content[..pos],
        None => &content[..max_chars],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock LLM client for testing that returns pre-configured responses.
    struct MockLlm {
        response: String,
    }

    impl MockLlm {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
            }
        }
    }

    /// A mock LLM that always fails.
    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _json_mode: bool,
        ) -> anyhow::Result<String> {
            Ok(self.response.clone())
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for FailingLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _json_mode: bool,
        ) -> anyhow::Result<String> {
            Err(anyhow::anyhow!("Ollama connection refused"))
        }
    }

    // -----------------------------------------------------------------------
    // T12: Title extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn clean_title_response_strips_quotes() {
        assert_eq!(
            clean_title_response("\"Building RAG Systems with Rust\""),
            "Building RAG Systems with Rust"
        );
        assert_eq!(
            clean_title_response("'Single Quoted Title'"),
            "Single Quoted Title"
        );
    }

    #[test]
    fn clean_title_response_strips_prefix() {
        assert_eq!(
            clean_title_response("Title: Building RAG Systems"),
            "Building RAG Systems"
        );
    }

    #[test]
    fn clean_title_response_strips_whitespace() {
        assert_eq!(clean_title_response("  Some Title  "), "Some Title");
    }

    #[test]
    fn clean_title_response_truncates_long_titles() {
        let long_title = "A ".repeat(200);
        let result = clean_title_response(&long_title);
        assert!(result.len() <= MAX_TITLE_LEN + 3); // +3 for "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_content_no_op_for_short() {
        let content = "short content";
        assert_eq!(truncate_content(content, 100), "short content");
    }

    #[test]
    fn truncate_content_breaks_at_word() {
        let content = "hello world this is a test";
        let result = truncate_content(content, 15);
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn generate_title_returns_llm_response() {
        let llm = MockLlm::new("Building RAG Systems in Rust");
        let config = TitleExtractionConfig::default();

        let title = generate_title(
            "https://example.com/article",
            "This article is about building RAG systems using the Rust programming language...",
            &config,
            &llm,
        )
        .await
        .expect("should succeed");

        assert_eq!(title, "Building RAG Systems in Rust");
    }

    #[tokio::test]
    async fn generate_title_cleans_quoted_response() {
        let llm = MockLlm::new("\"RAG Systems in Rust\"");
        let config = TitleExtractionConfig::default();

        let title = generate_title(
            "https://example.com/article",
            "Content about RAG",
            &config,
            &llm,
        )
        .await
        .expect("should succeed");

        assert_eq!(title, "RAG Systems in Rust");
    }

    #[tokio::test]
    async fn generate_title_fails_on_llm_error() {
        let llm = FailingLlm;
        let config = TitleExtractionConfig::default();

        let result = generate_title("https://example.com/article", "Content", &config, &llm).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OllamaIntakeError::LlmFailed(_)
        ));
    }

    #[tokio::test]
    async fn generate_title_fails_on_empty_response() {
        let llm = MockLlm::new("   ");
        let config = TitleExtractionConfig::default();

        let result = generate_title("https://example.com/article", "Content", &config, &llm).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OllamaIntakeError::EmptyResponse
        ));
    }

    #[tokio::test]
    async fn generate_title_with_fallback_uses_llm_when_available() {
        let llm = MockLlm::new("LLM Generated Title");
        let config = TitleExtractionConfig::default();

        let title = generate_title_with_fallback(
            "https://example.com",
            "Content",
            Some("HTML Title"),
            &config,
            Some(&llm),
        )
        .await;

        assert_eq!(title, "LLM Generated Title");
    }

    #[tokio::test]
    async fn generate_title_with_fallback_uses_html_on_llm_failure() {
        let llm = FailingLlm;
        let config = TitleExtractionConfig::default();

        let title = generate_title_with_fallback(
            "https://example.com",
            "Content",
            Some("HTML Fallback Title"),
            &config,
            Some(&llm),
        )
        .await;

        assert_eq!(title, "HTML Fallback Title");
    }

    #[tokio::test]
    async fn generate_title_with_fallback_uses_html_when_disabled() {
        let config = TitleExtractionConfig {
            enabled: false,
            ..Default::default()
        };

        let title = generate_title_with_fallback(
            "https://example.com",
            "Content",
            Some("HTML Title"),
            &config,
            None,
        )
        .await;

        assert_eq!(title, "HTML Title");
    }

    #[tokio::test]
    async fn generate_title_with_fallback_returns_untitled_when_no_fallback() {
        let config = TitleExtractionConfig {
            enabled: false,
            ..Default::default()
        };

        let title =
            generate_title_with_fallback("https://example.com", "Content", None, &config, None)
                .await;

        assert_eq!(title, "Untitled");
    }

    #[tokio::test]
    async fn generate_title_with_fallback_skips_empty_html_title() {
        let config = TitleExtractionConfig {
            enabled: false,
            ..Default::default()
        };

        let title = generate_title_with_fallback(
            "https://example.com",
            "Content",
            Some("  "),
            &config,
            None,
        )
        .await;

        assert_eq!(title, "Untitled");
    }

    // -----------------------------------------------------------------------
    // T22: Link relevance tests
    // -----------------------------------------------------------------------

    fn test_link_context() -> LinkContext {
        LinkContext {
            parent_title: "Building RAG Systems".to_string(),
            parent_url: "https://example.com/rag-article".to_string(),
            link_url: "https://docs.example.com/vector-db-guide".to_string(),
            surrounding_text: "For more on vector databases, see this guide".to_string(),
        }
    }

    #[tokio::test]
    async fn check_link_relevance_returns_true_for_yes() {
        let llm = MockLlm::new("yes");
        let config = LinkRelevanceConfig::default();

        let relevant = check_link_relevance(&test_link_context(), &config, Some(&llm)).await;

        assert!(relevant);
    }

    #[tokio::test]
    async fn check_link_relevance_returns_true_for_yes_with_explanation() {
        // Some models might add explanation after "yes"
        let llm =
            MockLlm::new("Yes, this link appears to contain relevant technical documentation.");
        let config = LinkRelevanceConfig::default();

        let relevant = check_link_relevance(&test_link_context(), &config, Some(&llm)).await;

        assert!(relevant);
    }

    #[tokio::test]
    async fn check_link_relevance_returns_false_for_no() {
        let llm = MockLlm::new("no");
        let config = LinkRelevanceConfig::default();

        let relevant = check_link_relevance(&test_link_context(), &config, Some(&llm)).await;

        assert!(!relevant);
    }

    #[tokio::test]
    async fn check_link_relevance_fallback_follow_all_on_error() {
        let llm = FailingLlm;
        let config = LinkRelevanceConfig {
            enabled: true,
            fallback_policy: LinkFallbackPolicy::FollowAll,
        };

        let relevant = check_link_relevance(&test_link_context(), &config, Some(&llm)).await;

        assert!(relevant);
    }

    #[tokio::test]
    async fn check_link_relevance_fallback_skip_all_on_error() {
        let llm = FailingLlm;
        let config = LinkRelevanceConfig {
            enabled: true,
            fallback_policy: LinkFallbackPolicy::SkipAll,
        };

        let relevant = check_link_relevance(&test_link_context(), &config, Some(&llm)).await;

        assert!(!relevant);
    }

    #[tokio::test]
    async fn check_link_relevance_disabled_follow_all() {
        let config = LinkRelevanceConfig {
            enabled: false,
            fallback_policy: LinkFallbackPolicy::FollowAll,
        };

        let relevant = check_link_relevance(&test_link_context(), &config, None).await;

        assert!(relevant);
    }

    #[tokio::test]
    async fn check_link_relevance_disabled_skip_all() {
        let config = LinkRelevanceConfig {
            enabled: false,
            fallback_policy: LinkFallbackPolicy::SkipAll,
        };

        let relevant = check_link_relevance(&test_link_context(), &config, None).await;

        assert!(!relevant);
    }

    #[tokio::test]
    async fn check_link_relevance_no_llm_uses_fallback() {
        let config = LinkRelevanceConfig {
            enabled: true,
            fallback_policy: LinkFallbackPolicy::FollowAll,
        };

        let relevant = check_link_relevance(
            &test_link_context(),
            &config,
            None, // no LLM available
        )
        .await;

        assert!(relevant);
    }
}
