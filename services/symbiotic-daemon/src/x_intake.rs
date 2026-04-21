use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use credential_gateway::CredentialVault;
use serde::Deserialize;
use serde_json::Value;
use symbiotic_core::intake::normalize_url;
use symbiotic_intake::twitter::{
    parse_status_url, Tweet, TweetThread, TwitterApiClient, TwitterApiError, TwitterFallbackClient,
    TwitterFetcher,
};
use symbiotic_intake::{ContentFetcher, FetchedContent};
use url::Url;

use crate::FetchMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BookmarksSource {
    Api,
    Browser,
}

impl BookmarksSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Browser => "browser",
        }
    }
}

pub(crate) trait BookmarksSyncClient: Send + Sync {
    fn list_bookmark_urls(&self, source: BookmarksSource, limit: u32) -> Result<Vec<Url>>;
}

pub(crate) trait XApiHttpClient: Send + Sync {
    fn get_json(&self, url: &str, bearer_token: &str) -> Result<String>;
}

pub(crate) struct CurlXApiHttpClient;

impl CurlXApiHttpClient {
    fn is_available() -> bool {
        std::process::Command::new("curl")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}

impl XApiHttpClient for CurlXApiHttpClient {
    fn get_json(&self, url: &str, bearer_token: &str) -> Result<String> {
        if bearer_token.contains('\n') || bearer_token.contains('\r') {
            return Err(anyhow!(
                "x api bearer token contains invalid control characters"
            ));
        }

        // Keep bearer token out of process args (`ps`) by passing curl config
        // through stdin (`--config -`) instead of argv.
        let mut child = std::process::Command::new("curl")
            .arg("-sS")
            .arg("--fail")
            .arg("--max-time")
            .arg("20")
            .arg("--config")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to execute curl for {url}"))?;

        let escaped_url = escape_curl_config_value(url);
        let escaped_token = escape_curl_config_value(bearer_token);
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open curl stdin"))?;
        let curl_config = format!(
            "url = \"{escaped_url}\"\nheader = \"Authorization: Bearer {escaped_token}\"\nheader = \"Accept: application/json\"\n"
        );
        stdin
            .write_all(curl_config.as_bytes())
            .context("failed to write curl config to stdin")?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to collect curl output for {url}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "x api bookmarks request failed (status={}): {}",
                output.status,
                stderr.trim()
            ));
        }

        let body = String::from_utf8(output.stdout)
            .context("x api bookmarks response was not valid UTF-8")?;
        Ok(body)
    }
}

fn escape_curl_config_value(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace('"', r#"\""#)
        .replace(['\n', '\r'], "")
}

#[derive(Debug, Deserialize)]
struct XOAuthTokenFile {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct XBookmarksResponse {
    data: Option<Vec<XBookmarkTweet>>,
    includes: Option<XBookmarksIncludes>,
    meta: Option<XBookmarksMeta>,
    errors: Option<Vec<XBookmarksError>>,
}

#[derive(Debug, Deserialize)]
struct XBookmarkTweet {
    id: String,
    author_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct XBookmarksIncludes {
    users: Option<Vec<XBookmarksUser>>,
}

#[derive(Debug, Deserialize)]
struct XBookmarksUser {
    id: String,
    username: String,
}

#[derive(Debug, Deserialize)]
struct XBookmarksMeta {
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct XBookmarksError {
    title: Option<String>,
    detail: Option<String>,
    message: Option<String>,
}

pub(crate) struct XApiBookmarksClient {
    vault: Arc<dyn CredentialVault>,
    base_url: String,
    http_client: Arc<dyn XApiHttpClient>,
}

impl XApiBookmarksClient {
    fn new(vault: Arc<dyn CredentialVault>, base_url: String) -> Self {
        Self {
            vault,
            base_url,
            http_client: Arc::new(CurlXApiHttpClient),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_http(
        vault: Arc<dyn CredentialVault>,
        base_url: String,
        http_client: Arc<dyn XApiHttpClient>,
    ) -> Self {
        Self {
            vault,
            base_url,
            http_client,
        }
    }

    pub(crate) fn list_bookmark_urls(&self, limit: u32) -> Result<Vec<Url>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let access_token = load_x_access_token_from_vault(self.vault.as_ref())?;
        let endpoint = format!("{}/users/me/bookmarks", self.base_url.trim_end_matches('/'));
        let mut seen_urls = HashSet::new();
        let mut seen_page_tokens = HashSet::new();
        let mut urls = Vec::new();
        let mut next_page: Option<String> = None;

        while urls.len() < limit as usize {
            let remaining = limit as usize - urls.len();
            let page_size = remaining.clamp(1, 100);
            let mut request_url = Url::parse(&endpoint)
                .with_context(|| format!("invalid x api base url {endpoint}"))?;
            {
                let mut query = request_url.query_pairs_mut();
                query.append_pair("max_results", &page_size.to_string());
                query.append_pair("expansions", "author_id");
                query.append_pair("user.fields", "username");
                if let Some(token) = next_page.as_deref() {
                    query.append_pair("pagination_token", token);
                }
            }

            let response_body = self
                .http_client
                .get_json(request_url.as_str(), &access_token)
                .with_context(|| {
                    format!("failed to fetch x bookmarks page {}", request_url.as_str())
                })?;
            let response: XBookmarksResponse = serde_json::from_str(&response_body)
                .context("invalid x bookmarks JSON response")?;

            if let Some(errors) = response.errors {
                if !errors.is_empty() {
                    let detail = errors
                        .into_iter()
                        .find_map(|item| item.detail.or(item.message).or(item.title))
                        .unwrap_or_else(|| "unknown x api error".to_string());
                    return Err(anyhow!("x bookmarks api returned an error: {detail}"));
                }
            }

            let user_map = response
                .includes
                .and_then(|includes| includes.users)
                .unwrap_or_default()
                .into_iter()
                .map(|user| (user.id, user.username))
                .collect::<HashMap<_, _>>();

            for tweet in response.data.unwrap_or_default() {
                if urls.len() >= limit as usize {
                    break;
                }
                let Some(author_id) = tweet.author_id.as_deref() else {
                    continue;
                };
                let Some(username) = user_map.get(author_id) else {
                    continue;
                };
                let candidate = format!("https://x.com/{username}/status/{}", tweet.id);
                let normalized = match normalize_url(&candidate) {
                    Ok(url) => url,
                    Err(_) => continue,
                };
                if seen_urls.insert(normalized.as_str().to_string()) {
                    urls.push(normalized);
                }
            }

            let Some(token) = response.meta.and_then(|meta| meta.next_token) else {
                break;
            };
            if !seen_page_tokens.insert(token.clone()) {
                break;
            }
            next_page = Some(token);
        }

        Ok(urls)
    }
}

/// Vault service key for X OAuth tokens (matches installer constant).
pub(crate) const X_OAUTH_VAULT_SERVICE: &str = "x-oauth";

pub(crate) fn load_x_access_token_from_vault(vault: &dyn CredentialVault) -> Result<String> {
    let record = vault
        .get(X_OAUTH_VAULT_SERVICE)?
        .ok_or_else(|| anyhow!("x oauth token not found in vault"))?;
    let parsed: XOAuthTokenFile =
        serde_json::from_str(&record.secret).context("failed to parse x oauth token from vault")?;
    let access_token = parsed.access_token.trim();
    if access_token.is_empty() {
        return Err(anyhow!("x oauth token in vault has empty access_token"));
    }
    Ok(access_token.to_string())
}

pub(crate) struct FileBookmarksSyncClient {
    api_file: PathBuf,
    browser_file: PathBuf,
}

impl FileBookmarksSyncClient {
    fn new(api_file: PathBuf, browser_file: PathBuf) -> Self {
        Self {
            api_file,
            browser_file,
        }
    }

    fn source_file(&self, source: BookmarksSource) -> &Path {
        match source {
            BookmarksSource::Api => &self.api_file,
            BookmarksSource::Browser => &self.browser_file,
        }
    }

    fn read_urls(&self, source: BookmarksSource, limit: u32) -> Result<Vec<Url>> {
        read_urls_from_file(self.source_file(source), limit)
    }
}

pub(crate) struct HybridBookmarksSyncClient {
    file_client: FileBookmarksSyncClient,
    api_client: Option<XApiBookmarksClient>,
}

impl HybridBookmarksSyncClient {
    pub(crate) fn new(
        api_file: PathBuf,
        browser_file: PathBuf,
        vault: Arc<dyn CredentialVault>,
        x_api_base_url: String,
    ) -> Self {
        let api_client = if CurlXApiHttpClient::is_available() {
            Some(XApiBookmarksClient::new(vault, x_api_base_url))
        } else {
            None
        };
        Self {
            file_client: FileBookmarksSyncClient::new(api_file, browser_file),
            api_client,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_api_client(
        api_file: PathBuf,
        browser_file: PathBuf,
        api_client: Option<XApiBookmarksClient>,
    ) -> Self {
        Self {
            file_client: FileBookmarksSyncClient::new(api_file, browser_file),
            api_client,
        }
    }
}

impl BookmarksSyncClient for HybridBookmarksSyncClient {
    fn list_bookmark_urls(&self, source: BookmarksSource, limit: u32) -> Result<Vec<Url>> {
        match source {
            BookmarksSource::Api => {
                if let Some(api_client) = &self.api_client {
                    match api_client.list_bookmark_urls(limit) {
                        Ok(urls) => return Ok(urls),
                        Err(err) => {
                            let api_urls =
                                self.file_client.read_urls(BookmarksSource::Api, limit)?;
                            if !api_urls.is_empty() {
                                return Ok(api_urls);
                            }
                            let browser_urls = self
                                .file_client
                                .read_urls(BookmarksSource::Browser, limit)?;
                            if !browser_urls.is_empty() {
                                return Ok(browser_urls);
                            }
                            if is_missing_x_oauth_config(&err) {
                                return Ok(Vec::new());
                            }
                            return Err(err);
                        }
                    }
                }
                let api_urls = self.file_client.read_urls(BookmarksSource::Api, limit)?;
                if !api_urls.is_empty() {
                    return Ok(api_urls);
                }
                self.file_client.read_urls(BookmarksSource::Browser, limit)
            }
            BookmarksSource::Browser => {
                let browser_urls = self
                    .file_client
                    .read_urls(BookmarksSource::Browser, limit)?;
                if !browser_urls.is_empty() {
                    return Ok(browser_urls);
                }
                self.file_client.read_urls(BookmarksSource::Api, limit)
            }
        }
    }
}

fn is_missing_x_oauth_config(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}").to_ascii_lowercase();
    message.contains("x oauth token")
        || message.contains("failed to read x oauth token file")
        || message.contains("empty access_token")
}

pub(crate) fn read_urls_from_file(path: &Path, limit: u32) -> Result<Vec<Url>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read bookmarks source {}", path.display()))?;
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    for line in content.lines() {
        if urls.len() >= limit as usize {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let candidate = trimmed.split_whitespace().next().unwrap_or(trimmed);
        let normalized = match normalize_url(candidate) {
            Ok(url) => url,
            Err(_) => continue,
        };
        if seen.insert(normalized.as_str().to_string()) {
            urls.push(normalized);
        }
    }
    Ok(urls)
}

fn looks_like_html(body: &str) -> bool {
    let sample = body.trim_start().to_ascii_lowercase();
    sample.starts_with("<!doctype html") || sample.starts_with("<html") || sample.contains("<body")
}

/// Extract the HTML `<title>` tag content from an HTML document.
///
/// Returns `None` if no title tag is found or the title is empty.
fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?.checked_add(6)?;
    // Skip past any attributes on the title tag (e.g. <title lang="en">)
    let after_open = lower[start..].find('>')? + start + 1;
    let end = lower[after_open..].find("</title")? + after_open;
    let raw = &html[after_open..end];
    let title = raw
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"");
    let trimmed = title.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn strip_html(input: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    let mut pending_break = false;
    for ch in input.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let tag_name = tag
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(
                    tag_name.as_str(),
                    "br" | "p" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                ) {
                    pending_break = true;
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if pending_break {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            pending_break = false;
        }
        out.push(ch);
    }

    out = out.replace("&amp;", "&");
    out = out.replace("&lt;", "<");
    out = out.replace("&gt;", ">");
    out = out.replace("&nbsp;", " ");
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Deserialize)]
struct XTweetLookupResponse {
    data: Option<XTweetData>,
    includes: Option<XTweetIncludes>,
    errors: Option<Vec<XBookmarksError>>,
}

#[derive(Debug, Deserialize)]
struct XTweetData {
    id: String,
    text: String,
    author_id: Option<String>,
    conversation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct XTweetIncludes {
    users: Option<Vec<XBookmarksUser>>,
}

#[derive(Debug, Deserialize)]
struct XThreadSearchResponse {
    data: Option<Vec<XTweetData>>,
    includes: Option<XTweetIncludes>,
    errors: Option<Vec<XBookmarksError>>,
}

pub(crate) struct XApiThreadClient {
    vault: Arc<dyn CredentialVault>,
    base_url: String,
    http_client: Arc<dyn XApiHttpClient>,
}

impl XApiThreadClient {
    pub(crate) fn new(
        vault: Arc<dyn CredentialVault>,
        base_url: String,
        http_client: Arc<dyn XApiHttpClient>,
    ) -> Self {
        Self {
            vault,
            base_url,
            http_client,
        }
    }

    fn fetch_lookup(
        &self,
        tweet_id: &str,
        access_token: &str,
    ) -> std::result::Result<XTweetLookupResponse, TwitterApiError> {
        let mut endpoint = Url::parse(&format!(
            "{}/tweets/{}",
            self.base_url.trim_end_matches('/'),
            tweet_id
        ))
        .map_err(|err| TwitterApiError::Unknown(format!("invalid x api base URL: {err}")))?;
        {
            let mut query = endpoint.query_pairs_mut();
            query.append_pair("expansions", "author_id");
            query.append_pair("tweet.fields", "author_id,conversation_id");
            query.append_pair("user.fields", "username");
        }
        let raw = self
            .http_client
            .get_json(endpoint.as_str(), access_token)
            .map_err(map_x_api_error)?;
        let response: XTweetLookupResponse = serde_json::from_str(&raw)
            .map_err(|err| TwitterApiError::Unknown(format!("invalid x api JSON: {err}")))?;
        if let Some(errors) = response.errors.as_ref().filter(|errors| !errors.is_empty()) {
            let detail = errors
                .iter()
                .find_map(|item| {
                    item.detail
                        .as_ref()
                        .or(item.message.as_ref())
                        .or(item.title.as_ref())
                })
                .cloned()
                .unwrap_or_else(|| "unknown x api error".to_string());
            return Err(map_x_api_detail_error(&detail));
        }
        Ok(response)
    }

    fn fetch_conversation(
        &self,
        conversation_id: &str,
        access_token: &str,
    ) -> std::result::Result<XThreadSearchResponse, TwitterApiError> {
        let mut endpoint = Url::parse(&format!(
            "{}/tweets/search/recent",
            self.base_url.trim_end_matches('/')
        ))
        .map_err(|err| TwitterApiError::Unknown(format!("invalid x api base URL: {err}")))?;
        {
            let mut query = endpoint.query_pairs_mut();
            query.append_pair("query", &format!("conversation_id:{conversation_id}"));
            query.append_pair("max_results", "100");
            query.append_pair("expansions", "author_id");
            query.append_pair("tweet.fields", "author_id,conversation_id");
            query.append_pair("user.fields", "username");
        }
        let raw = self
            .http_client
            .get_json(endpoint.as_str(), access_token)
            .map_err(map_x_api_error)?;
        let response: XThreadSearchResponse = serde_json::from_str(&raw)
            .map_err(|err| TwitterApiError::Unknown(format!("invalid x api JSON: {err}")))?;
        if let Some(errors) = response.errors.as_ref().filter(|errors| !errors.is_empty()) {
            let detail = errors
                .iter()
                .find_map(|item| {
                    item.detail
                        .as_ref()
                        .or(item.message.as_ref())
                        .or(item.title.as_ref())
                })
                .cloned()
                .unwrap_or_else(|| "unknown x api error".to_string());
            return Err(map_x_api_detail_error(&detail));
        }
        Ok(response)
    }
}

impl TwitterApiClient for XApiThreadClient {
    fn fetch_thread(
        &self,
        handle: &str,
        tweet_id: &str,
    ) -> std::result::Result<TweetThread, TwitterApiError> {
        let access_token = load_x_access_token_from_vault(self.vault.as_ref()).map_err(|err| {
            let message = format!("{err:#}").to_ascii_lowercase();
            if message.contains("x oauth token")
                || message.contains("access_token")
                || message.contains("not found in vault")
            {
                TwitterApiError::AuthRequired
            } else {
                TwitterApiError::Unknown(err.to_string())
            }
        })?;

        let lookup = self.fetch_lookup(tweet_id, &access_token)?;
        let root = lookup.data.ok_or(TwitterApiError::NotFound)?;
        let root_author_id = root.author_id.clone();
        let root_text = root.text.clone();
        let conversation_id = root
            .conversation_id
            .clone()
            .unwrap_or_else(|| root.id.clone());
        let mut user_map = lookup
            .includes
            .and_then(|includes| includes.users)
            .unwrap_or_default()
            .into_iter()
            .map(|user| (user.id, user.username))
            .collect::<HashMap<_, _>>();

        let mut tweets = Vec::new();
        tweets.push(Tweet {
            author_handle: root_author_id
                .as_deref()
                .and_then(|author_id| user_map.get(author_id))
                .cloned()
                .unwrap_or_else(|| handle.to_string()),
            text: root_text,
            tweet_id: root.id.clone(),
        });

        if let Ok(conversation) = self.fetch_conversation(&conversation_id, &access_token) {
            if let Some(users) = conversation.includes.and_then(|includes| includes.users) {
                for user in users {
                    user_map.insert(user.id, user.username);
                }
            }
            let mut seen = HashSet::new();
            seen.insert(root.id.clone());
            for tweet in conversation.data.unwrap_or_default() {
                if !seen.insert(tweet.id.clone()) {
                    continue;
                }
                let author_handle = tweet
                    .author_id
                    .as_deref()
                    .and_then(|author_id| user_map.get(author_id))
                    .cloned()
                    .unwrap_or_else(|| handle.to_string());
                tweets.push(Tweet {
                    author_handle,
                    text: tweet.text,
                    tweet_id: tweet.id,
                });
            }
        }

        Ok(TweetThread {
            root_handle: handle.to_string(),
            root_tweet_id: tweet_id.to_string(),
            tweets,
        })
    }
}

fn map_x_api_error(err: anyhow::Error) -> TwitterApiError {
    map_x_api_detail_error(&err.to_string())
}

fn map_x_api_detail_error(detail: &str) -> TwitterApiError {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("401") || lower.contains("unauthorized") || lower.contains("auth") {
        return TwitterApiError::AuthRequired;
    }
    if lower.contains("403") || lower.contains("forbidden") || lower.contains("scope") {
        return TwitterApiError::InsufficientScope;
    }
    if lower.contains("429") || lower.contains("rate") {
        return TwitterApiError::RateLimited;
    }
    if lower.contains("404") || lower.contains("not found") {
        return TwitterApiError::NotFound;
    }
    if lower.contains("5xx")
        || lower.contains("503")
        || lower.contains("upstream")
        || lower.contains("timeout")
    {
        return TwitterApiError::Unavailable;
    }
    TwitterApiError::Unknown(detail.to_string())
}

pub(crate) struct StubTwitterApi;

impl TwitterApiClient for StubTwitterApi {
    fn fetch_thread(
        &self,
        _handle: &str,
        _tweet_id: &str,
    ) -> std::result::Result<TweetThread, TwitterApiError> {
        Err(TwitterApiError::AuthRequired)
    }
}

pub(crate) struct StubTwitterFallback;

impl TwitterFallbackClient for StubTwitterFallback {
    fn fetch_thread(&self, _url: &Url) -> Result<TweetThread> {
        Err(anyhow!("twitter fallback unavailable"))
    }
}

trait FxTwitterHttpClient: Send + Sync {
    fn get_json(&self, url: &str) -> Result<String>;
}

struct CurlFxTwitterHttpClient;

impl FxTwitterHttpClient for CurlFxTwitterHttpClient {
    fn get_json(&self, url: &str) -> Result<String> {
        let output = std::process::Command::new("curl")
            .arg("-sS")
            .arg("--fail")
            .arg("--max-time")
            .arg("20")
            .arg(url)
            .output()
            .with_context(|| format!("failed to execute curl for {url}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "fx twitter request failed (status={}): {}",
                output.status,
                stderr.trim()
            ));
        }

        String::from_utf8(output.stdout).context("fx twitter response was not valid UTF-8")
    }
}

struct FxTwitterFallback {
    base_url: String,
    http_client: Arc<dyn FxTwitterHttpClient>,
    fallback: Box<dyn TwitterFallbackClient>,
}

impl FxTwitterFallback {
    fn new(
        base_url: String,
        http_client: Arc<dyn FxTwitterHttpClient>,
        fallback: Box<dyn TwitterFallbackClient>,
    ) -> Self {
        Self {
            base_url,
            http_client,
            fallback,
        }
    }
}

impl TwitterFallbackClient for FxTwitterFallback {
    fn fetch_thread(&self, url: &Url) -> Result<TweetThread> {
        let (handle, tweet_id) = parse_status_url(url)
            .ok_or_else(|| anyhow!("invalid x/twitter status URL {}", url.as_str()))?;
        let endpoint = format!(
            "{}/{}/status/{}",
            self.base_url.trim_end_matches('/'),
            handle,
            tweet_id
        );

        let result = (|| -> Result<TweetThread> {
            let raw = self.http_client.get_json(&endpoint)?;
            let json: Value =
                serde_json::from_str(&raw).context("invalid fx twitter JSON response")?;
            let text = extract_fxtwitter_text(&json)
                .ok_or_else(|| anyhow!("fx twitter payload missing tweet text"))?;
            let author_handle = extract_fxtwitter_author_handle(&json).unwrap_or(handle.clone());

            Ok(TweetThread {
                root_handle: handle.clone(),
                root_tweet_id: tweet_id.clone(),
                tweets: vec![Tweet {
                    author_handle,
                    text,
                    tweet_id: tweet_id.clone(),
                }],
            })
        })();

        match result {
            Ok(thread) => Ok(thread),
            Err(_) => self.fallback.fetch_thread(url),
        }
    }
}

fn extract_fxtwitter_text(json: &Value) -> Option<String> {
    [
        "/tweet/text",
        "/tweet/content",
        "/tweet/full_text",
        "/text",
        "/content",
    ]
    .iter()
    .find_map(|pointer| {
        json.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn extract_fxtwitter_author_handle(json: &Value) -> Option<String> {
    [
        "/tweet/author/screen_name",
        "/tweet/author/username",
        "/author/screen_name",
        "/author/username",
    ]
    .iter()
    .find_map(|pointer| {
        json.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadFallbackEntry {
    url: String,
    text: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    tweet_id: Option<String>,
}

pub(crate) struct FileTwitterFallback {
    fixture_file: PathBuf,
    fallback: Box<dyn TwitterFallbackClient>,
}

impl FileTwitterFallback {
    pub(crate) fn new(fixture_file: PathBuf, fallback: Box<dyn TwitterFallbackClient>) -> Self {
        Self {
            fixture_file,
            fallback,
        }
    }
}

impl TwitterFallbackClient for FileTwitterFallback {
    fn fetch_thread(&self, url: &Url) -> Result<TweetThread> {
        if self.fixture_file.exists() {
            let content = fs::read_to_string(&self.fixture_file).with_context(|| {
                format!(
                    "failed to read twitter fallback fixture {}",
                    self.fixture_file.display()
                )
            })?;
            let normalized_target = normalize_url(url.as_str())
                .unwrap_or_else(|_| url.clone())
                .to_string();
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if trimmed.starts_with('{') {
                    let Ok(entry) = serde_json::from_str::<ThreadFallbackEntry>(trimmed) else {
                        continue;
                    };
                    let candidate = normalize_url(&entry.url)
                        .map(|value| value.to_string())
                        .unwrap_or(entry.url.clone());
                    if candidate != normalized_target {
                        continue;
                    }
                    let (default_handle, default_tweet_id) = parse_status_url(url)
                        .ok_or_else(|| anyhow!("invalid x/twitter status URL {}", url.as_str()))?;
                    return Ok(TweetThread {
                        root_handle: default_handle.clone(),
                        root_tweet_id: default_tweet_id.clone(),
                        tweets: vec![Tweet {
                            author_handle: entry.author.unwrap_or(default_handle),
                            text: entry.text,
                            tweet_id: entry.tweet_id.unwrap_or(default_tweet_id),
                        }],
                    });
                }

                let mut parts = trimmed.splitn(2, '\t');
                let Some(raw_url) = parts.next() else {
                    continue;
                };
                let Some(text) = parts.next() else {
                    continue;
                };
                let candidate = normalize_url(raw_url)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|_| raw_url.to_string());
                if candidate != normalized_target {
                    continue;
                }
                let (handle, tweet_id) = parse_status_url(url)
                    .ok_or_else(|| anyhow!("invalid x/twitter status URL {}", url.as_str()))?;
                return Ok(TweetThread {
                    root_handle: handle.clone(),
                    root_tweet_id: tweet_id.clone(),
                    tweets: vec![Tweet {
                        author_handle: handle,
                        text: text.to_string(),
                        tweet_id,
                    }],
                });
            }
        }
        self.fallback.fetch_thread(url)
    }
}

pub(crate) struct DaemonFetcher {
    strategy: FetchStrategy,
    twitter_fetcher: TwitterFetcher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchStrategy {
    Curl,
    Stub,
}

impl DaemonFetcher {
    pub(crate) fn new(
        mode: FetchMode,
        vault: Arc<dyn CredentialVault>,
        x_api_base_url: String,
        x_thread_fallback_file: PathBuf,
    ) -> Self {
        let strategy = match mode {
            FetchMode::Stub => FetchStrategy::Stub,
            FetchMode::Curl => FetchStrategy::Curl,
            FetchMode::Auto => {
                if CurlFetcher::is_available() {
                    FetchStrategy::Curl
                } else {
                    FetchStrategy::Stub
                }
            }
        };
        let api_client: Box<dyn TwitterApiClient> = if CurlXApiHttpClient::is_available() {
            Box::new(XApiThreadClient::new(
                vault,
                x_api_base_url,
                Arc::new(CurlXApiHttpClient),
            ))
        } else {
            Box::new(StubTwitterApi)
        };
        let fx_fallback = Box::new(FxTwitterFallback::new(
            "https://api.fxtwitter.com".to_string(),
            Arc::new(CurlFxTwitterHttpClient),
            Box::new(StubTwitterFallback),
        ));
        let fallback_client = Box::new(FileTwitterFallback::new(
            x_thread_fallback_file,
            fx_fallback,
        ));
        Self {
            strategy,
            twitter_fetcher: TwitterFetcher::new(api_client, fallback_client),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_twitter_clients(
        mode: FetchMode,
        api_client: Box<dyn TwitterApiClient>,
        fallback_client: Box<dyn TwitterFallbackClient>,
    ) -> Self {
        let strategy = match mode {
            FetchMode::Stub => FetchStrategy::Stub,
            FetchMode::Curl => FetchStrategy::Curl,
            FetchMode::Auto => {
                if CurlFetcher::is_available() {
                    FetchStrategy::Curl
                } else {
                    FetchStrategy::Stub
                }
            }
        };
        Self {
            strategy,
            twitter_fetcher: TwitterFetcher::new(api_client, fallback_client),
        }
    }
}

impl ContentFetcher for DaemonFetcher {
    fn fetch(&self, url: &Url) -> Result<FetchedContent> {
        if TwitterFetcher::supports(url) {
            return self.twitter_fetcher.fetch_content(url);
        }

        match self.strategy {
            FetchStrategy::Stub => Ok(FetchedContent {
                markdown: format!("# fetched {}\n", url.as_str()),
                html_title: None,
                title: None,
            }),
            FetchStrategy::Curl => CurlFetcher::fetch(url),
        }
    }
}

pub(crate) struct CurlFetcher;

impl CurlFetcher {
    pub(crate) fn is_available() -> bool {
        std::process::Command::new("curl")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    pub(crate) fn fetch(url: &Url) -> Result<FetchedContent> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(anyhow!("unsupported URL scheme {}", url.scheme()));
        }
        let output = std::process::Command::new("curl")
            .arg("-sSL")
            .arg("--max-time")
            .arg("20")
            .arg("--fail")
            .arg("-A")
            .arg("Symbiotic/1.0 (https://symbiotic.sh)")
            .arg(url.as_str())
            .stderr(Stdio::piped())
            .output()
            .context("failed to run curl")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "curl failed with status {} for {}: {}",
                output.status.code().unwrap_or(-1),
                url.as_str(),
                stderr.trim()
            ));
        }
        let mut body = String::from_utf8_lossy(&output.stdout).to_string();
        let max_bytes = 2_000_000usize;
        if body.len() > max_bytes {
            body.truncate(max_bytes);
        }
        let html_title = if looks_like_html(&body) {
            extract_html_title(&body)
        } else {
            None
        };
        let markdown = if looks_like_html(&body) {
            strip_html(&body)
        } else {
            body
        };
        Ok(FetchedContent {
            markdown,
            html_title,
            title: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_with_newline_is_rejected() {
        let client = CurlXApiHttpClient;
        let err = client
            .get_json("http://localhost:0/test", "token\ninjection")
            .expect_err("should reject token with newline");
        assert!(
            err.to_string().contains("invalid control characters"),
            "expected control characters error, got: {err}"
        );
    }

    #[test]
    fn bearer_token_with_carriage_return_is_rejected() {
        let client = CurlXApiHttpClient;
        let err = client
            .get_json("http://localhost:0/test", "token\rinjection")
            .expect_err("should reject token with carriage return");
        assert!(
            err.to_string().contains("invalid control characters"),
            "expected control characters error, got: {err}"
        );
    }
}
