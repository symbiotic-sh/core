use anyhow::Result;
use thiserror::Error;
use url::Url;

use crate::FetchedContent;

// ---------------------------------------------------------------------------
// T54: Twitter URL Deduplication
// ---------------------------------------------------------------------------

/// Check whether a URL is a Twitter/X URL (any path, not just status).
pub fn is_twitter_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    host == "x.com"
        || host.ends_with(".x.com")
        || host == "twitter.com"
        || host.ends_with(".twitter.com")
}

/// Extract the tweet ID from a Twitter/X status URL.
///
/// Handles all known URL variants:
/// - `https://x.com/user/status/123`
/// - `https://twitter.com/user/status/123`
/// - `https://x.com/i/status/123` (anonymous/intent links)
/// - `https://x.com/user/status/123?s=20` (with query params)
/// - `https://x.com/user/status/123/photo/1` (media suffixes)
///
/// Returns `None` if the URL is not a recognized Twitter status URL.
pub fn extract_tweet_id(url: &Url) -> Option<String> {
    if !is_twitter_url(url) {
        return None;
    }

    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();

    // Look for the pattern: .../status/{tweet_id}...
    for (i, segment) in segments.iter().enumerate() {
        if *segment == "status" {
            if let Some(id_segment) = segments.get(i + 1) {
                let id = id_segment.trim();
                // Validate: tweet IDs are numeric
                if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
                    return Some(id.to_string());
                }
            }
        }
    }

    None
}

/// Canonicalize a Twitter/X URL to a normalized form based on tweet ID.
///
/// All Twitter status URL variants are normalized to:
/// `https://x.com/i/status/{tweet_id}`
///
/// Non-Twitter URLs or non-status Twitter URLs are returned unchanged.
pub fn canonicalize_twitter_url(url: &Url) -> Url {
    if let Some(tweet_id) = extract_tweet_id(url) {
        // Safe to unwrap: this is a well-formed URL constructed from known parts
        Url::parse(&format!("https://x.com/i/status/{tweet_id}")).unwrap_or_else(|_| url.clone())
    } else {
        url.clone()
    }
}

/// Generate a content-based deduplication key for a URL.
///
/// For Twitter/X status URLs, returns `twitter:{tweet_id}` so that all URL
/// variants for the same tweet map to the same key.
///
/// For all other URLs, returns the URL string as-is (callers should use
/// their existing normalization before calling this).
pub fn content_dedup_key(url: &Url) -> String {
    if let Some(tweet_id) = extract_tweet_id(url) {
        format!("twitter:{tweet_id}")
    } else {
        url.as_str().to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tweet {
    pub author_handle: String,
    pub text: String,
    pub tweet_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TweetThread {
    pub root_handle: String,
    pub root_tweet_id: String,
    pub tweets: Vec<Tweet>,
}

#[derive(Debug, Error)]
pub enum TwitterApiError {
    #[error("auth required")]
    AuthRequired,
    #[error("insufficient scope")]
    InsufficientScope,
    #[error("rate limited")]
    RateLimited,
    #[error("upstream unavailable")]
    Unavailable,
    #[error("not found")]
    NotFound,
    #[error("unknown error: {0}")]
    Unknown(String),
}

pub trait TwitterApiClient: Send + Sync {
    fn fetch_thread(
        &self,
        handle: &str,
        tweet_id: &str,
    ) -> std::result::Result<TweetThread, TwitterApiError>;
}

pub trait TwitterFallbackClient: Send + Sync {
    fn fetch_thread(&self, url: &Url) -> Result<TweetThread>;
}

pub struct TwitterFetcher {
    api: Box<dyn TwitterApiClient>,
    fallback: Box<dyn TwitterFallbackClient>,
}

impl TwitterFetcher {
    pub fn new(api: Box<dyn TwitterApiClient>, fallback: Box<dyn TwitterFallbackClient>) -> Self {
        Self { api, fallback }
    }

    pub fn supports(url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        (host == "x.com"
            || host.ends_with(".x.com")
            || host == "twitter.com"
            || host.ends_with(".twitter.com"))
            && url.path().contains("/status/")
    }

    pub fn fetch_content(&self, url: &Url) -> Result<FetchedContent> {
        let (handle, tweet_id) = parse_status_url(url)
            .ok_or_else(|| anyhow::anyhow!("unsupported twitter/x status URL: {}", url.as_str()))?;

        let thread = match self.api.fetch_thread(&handle, &tweet_id) {
            Ok(thread) => thread,
            Err(
                TwitterApiError::AuthRequired
                | TwitterApiError::InsufficientScope
                | TwitterApiError::RateLimited
                | TwitterApiError::Unavailable,
            ) => self.fallback.fetch_thread(url)?,
            Err(err) => return Err(anyhow::anyhow!(err)),
        };

        Ok(FetchedContent {
            markdown: thread_to_markdown(&thread),
            html_title: None,
            title: None,
        })
    }
}

pub fn parse_status_url(url: &Url) -> Option<(String, String)> {
    let mut segments = url.path_segments()?;
    let handle = segments.next()?.trim().to_string();
    let status = segments.next()?;
    if status != "status" {
        return None;
    }
    let tweet_id = segments.next()?.trim().to_string();
    if handle.is_empty() || tweet_id.is_empty() {
        return None;
    }
    Some((handle, tweet_id))
}

pub fn thread_to_markdown(thread: &TweetThread) -> String {
    let mut out = format!(
        "# X Thread by @{}\n\nSource: https://x.com/{}/status/{}\n\n",
        thread.root_handle, thread.root_handle, thread.root_tweet_id
    );
    for (idx, tweet) in thread.tweets.iter().enumerate() {
        let number = idx + 1;
        out.push_str(&format!(
            "{number}. @{}: {}\n",
            tweet.author_handle, tweet.text
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SuccessApi;

    impl TwitterApiClient for SuccessApi {
        fn fetch_thread(
            &self,
            handle: &str,
            tweet_id: &str,
        ) -> std::result::Result<TweetThread, TwitterApiError> {
            Ok(TweetThread {
                root_handle: handle.to_string(),
                root_tweet_id: tweet_id.to_string(),
                tweets: vec![Tweet {
                    author_handle: handle.to_string(),
                    text: "API primary".to_string(),
                    tweet_id: tweet_id.to_string(),
                }],
            })
        }
    }

    struct FallbackApi;

    impl TwitterApiClient for FallbackApi {
        fn fetch_thread(
            &self,
            _handle: &str,
            _tweet_id: &str,
        ) -> std::result::Result<TweetThread, TwitterApiError> {
            Err(TwitterApiError::RateLimited)
        }
    }

    struct SuccessFallback;

    impl TwitterFallbackClient for SuccessFallback {
        fn fetch_thread(&self, _url: &Url) -> Result<TweetThread> {
            Ok(TweetThread {
                root_handle: "fallback".to_string(),
                root_tweet_id: "1".to_string(),
                tweets: vec![Tweet {
                    author_handle: "fallback".to_string(),
                    text: "Browser fallback".to_string(),
                    tweet_id: "1".to_string(),
                }],
            })
        }
    }

    #[test]
    fn supports_detects_x_status_urls() {
        let url = Url::parse("https://x.com/user/status/123").expect("valid url");
        assert!(TwitterFetcher::supports(&url));
    }

    #[test]
    fn parse_status_url_extracts_handle_and_tweet_id() {
        let url = Url::parse("https://twitter.com/user/status/1234567890").expect("valid url");
        let parsed = parse_status_url(&url).expect("must parse");
        assert_eq!(parsed.0, "user");
        assert_eq!(parsed.1, "1234567890");
    }

    #[test]
    fn fetch_content_uses_primary_api_when_available() {
        let fetcher = TwitterFetcher::new(Box::new(SuccessApi), Box::new(SuccessFallback));
        let url = Url::parse("https://x.com/user/status/123").expect("valid url");
        let content = fetcher.fetch_content(&url).expect("must fetch");
        assert!(content.markdown.contains("API primary"));
    }

    #[test]
    fn fetch_content_uses_fallback_for_rate_limit() {
        let fetcher = TwitterFetcher::new(Box::new(FallbackApi), Box::new(SuccessFallback));
        let url = Url::parse("https://x.com/user/status/123").expect("valid url");
        let content = fetcher.fetch_content(&url).expect("must fetch");
        assert!(content.markdown.contains("Browser fallback"));
    }

    // -------------------------------------------------------------------
    // T54: Twitter URL deduplication tests
    // -------------------------------------------------------------------

    #[test]
    fn is_twitter_url_recognizes_x_com() {
        let url = Url::parse("https://x.com/user/status/123").expect("valid url");
        assert!(is_twitter_url(&url));
    }

    #[test]
    fn is_twitter_url_recognizes_twitter_com() {
        let url = Url::parse("https://twitter.com/user/status/123").expect("valid url");
        assert!(is_twitter_url(&url));
    }

    #[test]
    fn is_twitter_url_rejects_non_twitter() {
        let url = Url::parse("https://example.com/status/123").expect("valid url");
        assert!(!is_twitter_url(&url));
    }

    #[test]
    fn extract_tweet_id_from_x_com_user_status() {
        let url = Url::parse("https://x.com/alex_prompter/status/2006304107196539292")
            .expect("valid url");
        assert_eq!(
            extract_tweet_id(&url),
            Some("2006304107196539292".to_string())
        );
    }

    #[test]
    fn extract_tweet_id_from_x_com_i_status() {
        let url = Url::parse("https://x.com/i/status/2006304107196539292").expect("valid url");
        assert_eq!(
            extract_tweet_id(&url),
            Some("2006304107196539292".to_string())
        );
    }

    #[test]
    fn extract_tweet_id_from_twitter_com() {
        let url =
            Url::parse("https://twitter.com/user/status/2006304107196539292").expect("valid url");
        assert_eq!(
            extract_tweet_id(&url),
            Some("2006304107196539292".to_string())
        );
    }

    #[test]
    fn extract_tweet_id_with_query_params() {
        let url = Url::parse("https://x.com/user/status/123456?s=20&t=abc").expect("valid url");
        assert_eq!(extract_tweet_id(&url), Some("123456".to_string()));
    }

    #[test]
    fn extract_tweet_id_with_media_suffix() {
        let url = Url::parse("https://x.com/user/status/123456/photo/1").expect("valid url");
        assert_eq!(extract_tweet_id(&url), Some("123456".to_string()));
    }

    #[test]
    fn extract_tweet_id_returns_none_for_non_twitter() {
        let url = Url::parse("https://example.com/status/123").expect("valid url");
        assert_eq!(extract_tweet_id(&url), None);
    }

    #[test]
    fn extract_tweet_id_returns_none_for_profile_url() {
        let url = Url::parse("https://x.com/user").expect("valid url");
        assert_eq!(extract_tweet_id(&url), None);
    }

    #[test]
    fn extract_tweet_id_returns_none_for_non_numeric_id() {
        let url = Url::parse("https://x.com/user/status/not_a_number").expect("valid url");
        assert_eq!(extract_tweet_id(&url), None);
    }

    #[test]
    fn canonicalize_twitter_url_normalizes_variants() {
        let variants = [
            "https://x.com/alex_prompter/status/123",
            "https://x.com/i/status/123",
            "https://twitter.com/alex_prompter/status/123",
            "https://twitter.com/i/status/123",
            "https://x.com/alex_prompter/status/123?s=20",
            "https://x.com/alex_prompter/status/123/photo/1",
        ];

        let expected = Url::parse("https://x.com/i/status/123").expect("valid url");

        for variant in &variants {
            let url = Url::parse(variant).expect("valid url");
            let canonical = canonicalize_twitter_url(&url);
            assert_eq!(
                canonical, expected,
                "variant {variant} should canonicalize to {expected}"
            );
        }
    }

    #[test]
    fn canonicalize_twitter_url_preserves_non_twitter() {
        let url = Url::parse("https://example.com/article").expect("valid url");
        let canonical = canonicalize_twitter_url(&url);
        assert_eq!(canonical, url);
    }

    #[test]
    fn content_dedup_key_uses_tweet_id_for_twitter() {
        let variants = [
            "https://x.com/alex_prompter/status/2006304107196539292",
            "https://x.com/i/status/2006304107196539292",
            "https://twitter.com/alex_prompter/status/2006304107196539292",
        ];

        for variant in &variants {
            let url = Url::parse(variant).expect("valid url");
            assert_eq!(
                content_dedup_key(&url),
                "twitter:2006304107196539292",
                "variant {variant} should produce same dedup key"
            );
        }
    }

    #[test]
    fn content_dedup_key_uses_url_for_non_twitter() {
        let url = Url::parse("https://example.com/article").expect("valid url");
        assert_eq!(content_dedup_key(&url), "https://example.com/article");
    }
}
