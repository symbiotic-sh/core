//! Markdown sanitization for Stage A.
//!
//! We don't parse Markdown fully (heavy dependency for marginal benefit at
//! this stage). Instead we:
//!
//! 1. Scan for raw inline HTML and strip/report dangerous constructs using
//!    the same policy as [`crate::sanitize::html`] — Markdown passes raw
//!    HTML through to the renderer, so `<script>` inside a `.md` file is
//!    just as dangerous as inside an `.html` file.
//! 2. Scan image references (`![alt](url)`) and flag ones whose host is not
//!    on the allowlist. The default allowlist covers common trusted CDNs;
//!    operators can override per [`ImageAllowlist`].
//!
//! Stage A turns the report into findings and a verdict.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

use crate::sanitize::html::{sanitize_html, HtmlSanitizeReport};
use crate::types::{FindingKind, Stage, StageFinding};

/// Default image-host allowlist. Intentionally conservative — operators bump
/// it explicitly for sources they vet.
pub const DEFAULT_IMAGE_ALLOWLIST: &[&str] = &[
    "github.com",
    "raw.githubusercontent.com",
    "user-images.githubusercontent.com",
    "avatars.githubusercontent.com",
    "gitlab.com",
    "wikipedia.org",
    "upload.wikimedia.org",
];

/// An allowlist of image hosts. Exact suffix match on the URL host.
#[derive(Debug, Clone)]
pub struct ImageAllowlist {
    hosts: HashSet<String>,
}

impl ImageAllowlist {
    /// Build an allowlist from an iterator of host strings.
    pub fn new<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            hosts: hosts.into_iter().map(Into::into).collect(),
        }
    }

    /// The default allowlist ([`DEFAULT_IMAGE_ALLOWLIST`]).
    pub fn default_allowlist() -> Self {
        Self::new(DEFAULT_IMAGE_ALLOWLIST.iter().copied())
    }

    /// Is the given URL's host on the allowlist? Missing/empty hosts (e.g.
    /// relative paths) are **allowed** — they can't hit the external network.
    pub fn allows(&self, url: &str) -> bool {
        let host = match extract_host(url) {
            Some(h) => h,
            None => return true,
        };
        self.hosts
            .iter()
            .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
    }
}

impl Default for ImageAllowlist {
    fn default() -> Self {
        Self::default_allowlist()
    }
}

/// Summary of what the Markdown sanitizer observed + rewrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownSanitizeReport {
    /// Cleaned payload (raw HTML stripped/neutralized).
    pub cleaned: String,
    /// Report from the embedded HTML sanitizer.
    pub html_report: HtmlSanitizeReport,
    /// External image URLs that failed the allowlist check.
    pub blocked_image_urls: Vec<String>,
}

impl MarkdownSanitizeReport {
    /// Were any violations found?
    pub fn had_structural_violations(&self) -> bool {
        self.html_report.had_structural_violations() || !self.blocked_image_urls.is_empty()
    }

    /// Convert the report into Stage A findings.
    pub fn to_findings(&self) -> Vec<StageFinding> {
        let mut out = self.html_report.to_findings();
        for url in &self.blocked_image_urls {
            out.push(StageFinding {
                stage: Stage::A,
                kind: FindingKind::StructuralViolation,
                detail: format!("blocked external image URL: {url}"),
                confidence: 1.0,
            });
        }
        out
    }
}

/// Sanitize a Markdown payload.
///
/// - Raw HTML embedded in the Markdown is passed through the HTML sanitizer
///   and the stripped tags are reported.
/// - Image URLs are extracted and checked against `allowlist`.
pub fn sanitize_markdown(input: &str, allowlist: &ImageAllowlist) -> MarkdownSanitizeReport {
    // Delegate raw-HTML handling to the HTML sanitizer; ammonia is happy to
    // accept a Markdown-with-inline-HTML string and return the HTML-safe
    // subset.
    let html_report = sanitize_html(input);

    // Pull image URLs via regex on the *original* input (the HTML sanitizer
    // may have mangled angle brackets, which we need intact for Markdown
    // image syntax recognition).
    let blocked_image_urls = scan_markdown_images(input)
        .into_iter()
        .filter(|url| !allowlist.allows(url))
        .collect();

    MarkdownSanitizeReport {
        cleaned: html_report.cleaned.clone(),
        html_report,
        blocked_image_urls,
    }
}

// `![alt](url)` and `![alt](url "title")`. We intentionally don't try to
// handle every edge case of Markdown (e.g. reference-style images) — the
// allowlist check is defense-in-depth; a missed image goes through the
// HTML sanitizer on the consumer side anyway.
static IMAGE_INLINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"!\[[^\]]*\]\(\s*([^)\s]+)(?:\s+"[^"]*")?\s*\)"#).expect("static regex")
});

fn scan_markdown_images(input: &str) -> Vec<String> {
    IMAGE_INLINE_RE
        .captures_iter(input)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

fn extract_host(url: &str) -> Option<String> {
    // Minimal scheme-aware parse: `scheme://host[:port]/path`.
    // A relative URL has no host.
    let after_scheme = &url[url.find("://")? + 3..];
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let host_port = &after_scheme[..end];
    let host = match host_port.rfind(':') {
        Some(idx) => &host_port[..idx],
        None => host_port,
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_allows_relative_images() {
        let a = ImageAllowlist::default_allowlist();
        assert!(a.allows("./images/foo.png"));
        assert!(a.allows("/foo.png"));
    }

    #[test]
    fn allowlist_allows_github_hosts() {
        let a = ImageAllowlist::default_allowlist();
        assert!(a.allows("https://github.com/foo.png"));
        assert!(a.allows("https://raw.githubusercontent.com/a/b/main/foo.png"));
    }

    #[test]
    fn allowlist_blocks_unknown_hosts() {
        let a = ImageAllowlist::default_allowlist();
        assert!(!a.allows("https://attacker.example.com/tracking.png"));
    }

    #[test]
    fn scans_inline_images() {
        let md = "![alt](https://github.com/foo.png) and ![x](https://evil.example/y.png)";
        let urls = scan_markdown_images(md);
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"https://github.com/foo.png".to_string()));
    }

    #[test]
    fn sanitize_markdown_blocks_evil_images() {
        let md = "![x](https://evil.example/y.png)";
        let report = sanitize_markdown(md, &ImageAllowlist::default_allowlist());
        assert_eq!(report.blocked_image_urls.len(), 1);
        assert!(report.had_structural_violations());
    }

    #[test]
    fn sanitize_markdown_passes_allowed_images() {
        let md = "![x](https://github.com/y.png)";
        let report = sanitize_markdown(md, &ImageAllowlist::default_allowlist());
        assert!(report.blocked_image_urls.is_empty());
    }

    #[test]
    fn sanitize_markdown_strips_inline_html_script() {
        let md = "# Heading\n\n<script>x</script>\n";
        let report = sanitize_markdown(md, &ImageAllowlist::default_allowlist());
        assert!(report.html_report.stripped_tags.contains("script"));
        assert!(report.had_structural_violations());
    }
}
