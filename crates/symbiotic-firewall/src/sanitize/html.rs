//! HTML sanitization (allowlist-based) for Stage A.
//!
//! We delegate the HTML parsing + allowlist filtering to the `ammonia` crate
//! (built on top of `html5ever`), which is the standard choice in the Rust
//! ecosystem and covers the quirks of real-world HTML (nested tags, broken
//! markup, obfuscated event handlers, data URIs, etc.).
//!
//! On top of `ammonia`, we add a detection pass that reports **which**
//! dangerous patterns were present in the input — Stage A uses those reports
//! to build `StageFinding`s and to decide whether to quarantine (script /
//! iframe detected) or pass-with-rewrite (stray `on*` handler stripped).

use ammonia::Builder;
use std::collections::HashSet;

use crate::types::{FindingKind, Stage, StageFinding};

/// Summary of what the HTML sanitizer observed + rewrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlSanitizeReport {
    /// The sanitized (safe) HTML payload.
    pub cleaned: String,
    /// Which dangerous tags were detected (and stripped). Set because order
    /// doesn't matter and duplicates are uninteresting for reporting.
    pub stripped_tags: HashSet<String>,
    /// Whether any `on*` event handler attributes were removed.
    pub stripped_event_handlers: bool,
    /// Whether a `javascript:` or `data:` URI was seen in an anchor / image.
    pub stripped_dangerous_urls: bool,
}

impl HtmlSanitizeReport {
    /// Were any structural-integrity violations found? (script/iframe/etc.)
    pub fn had_structural_violations(&self) -> bool {
        !self.stripped_tags.is_empty()
            || self.stripped_event_handlers
            || self.stripped_dangerous_urls
    }

    /// Convert the report into Stage A findings. Each distinct violation is
    /// surfaced as one finding so audit tooling can bucket them.
    pub fn to_findings(&self) -> Vec<StageFinding> {
        let mut findings = Vec::new();
        for tag in &self.stripped_tags {
            findings.push(StageFinding {
                stage: Stage::A,
                kind: FindingKind::StructuralViolation,
                detail: format!("stripped <{tag}> tag"),
                confidence: 1.0,
            });
        }
        if self.stripped_event_handlers {
            findings.push(StageFinding {
                stage: Stage::A,
                kind: FindingKind::StructuralViolation,
                detail: "stripped inline event handler attribute (on*)".into(),
                confidence: 1.0,
            });
        }
        if self.stripped_dangerous_urls {
            findings.push(StageFinding {
                stage: Stage::A,
                kind: FindingKind::StructuralViolation,
                detail: "stripped javascript: or data: URL".into(),
                confidence: 1.0,
            });
        }
        findings
    }
}

/// Dangerous tags that, if present, force `QuarantineClass::SourceIntegrity`.
/// Anything else the allowlist strips is merely noted.
pub const HARD_FAIL_TAGS: &[&str] = &[
    "script", "iframe", "object", "embed", "applet", "form", "meta", "link",
];

/// Sanitize an HTML payload, producing a safe rewrite + a report.
///
/// The allowlist mirrors the long-standing Symbiotic intake security policy:
/// safe text / structural tags are preserved; scripts, iframes, event handlers,
/// and `javascript:` URLs are stripped.
pub fn sanitize_html(input: &str) -> HtmlSanitizeReport {
    // Pre-scan the raw input *before* ammonia strips, so we know what was
    // there. We match on tag-open syntax (case-insensitive) rather than
    // parsing — a fuller parse is ammonia's job, and for reporting we just
    // want "did this appear at all?".
    let lower = input.to_ascii_lowercase();

    let mut stripped_tags: HashSet<String> = HashSet::new();
    for tag in HARD_FAIL_TAGS {
        let needle = format!("<{tag}");
        if lower.contains(&needle) {
            stripped_tags.insert((*tag).to_string());
        }
    }

    // Event-handler attributes look like ` on<word>=`; match permissively.
    let stripped_event_handlers = has_inline_event_handler(&lower);

    // javascript: / vbscript: / data:text/html URLs on anchors, images, etc.
    let stripped_dangerous_urls = has_dangerous_url(&lower);

    let cleaned = Builder::default()
        .add_tags(&["pre", "code"])
        .clean(input)
        .to_string();

    HtmlSanitizeReport {
        cleaned,
        stripped_tags,
        stripped_event_handlers,
        stripped_dangerous_urls,
    }
}

/// Cheap check for inline event handlers on any tag: we look for
/// ` on<ident>=` inside tag-open fragments. Runs in O(n) on the lowercased
/// input, no regex engine required.
fn has_inline_event_handler(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find next `<` — cheap short-circuit.
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Scan forward inside the tag for ` on<alpha>`.
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != b'>' {
            // Whitespace followed by "on" followed by an alpha, then `=`.
            if bytes[j].is_ascii_whitespace()
                && j + 3 < bytes.len()
                && bytes[j + 1] == b'o'
                && bytes[j + 2] == b'n'
                && bytes[j + 3].is_ascii_alphabetic()
            {
                // Now find `=` before `>`/whitespace terminator.
                let mut k = j + 3;
                while k < bytes.len() && bytes[k] != b'>' {
                    if bytes[k] == b'=' {
                        return true;
                    }
                    if bytes[k].is_ascii_whitespace() {
                        break;
                    }
                    k += 1;
                }
            }
            j += 1;
        }
        i = j + 1;
    }
    false
}

fn has_dangerous_url(lower: &str) -> bool {
    lower.contains("javascript:")
        || lower.contains("vbscript:")
        // `data:text/html` (with or without whitespace) is a classic
        // HTML-smuggling vector.
        || lower.contains("data:text/html")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_tag() {
        let r = sanitize_html("<p>hi</p><script>alert(1)</script>");
        assert!(r.stripped_tags.contains("script"));
        assert!(!r.cleaned.to_ascii_lowercase().contains("<script"));
        assert!(r.had_structural_violations());
    }

    #[test]
    fn strips_iframe() {
        let r = sanitize_html("<iframe src=\"x\"></iframe>");
        assert!(r.stripped_tags.contains("iframe"));
    }

    #[test]
    fn detects_event_handler() {
        let r = sanitize_html("<a href=\"/\" onclick=\"pwn()\">hi</a>");
        assert!(r.stripped_event_handlers);
    }

    #[test]
    fn detects_javascript_url() {
        let r = sanitize_html("<a href=\"javascript:pwn()\">x</a>");
        assert!(r.stripped_dangerous_urls);
    }

    #[test]
    fn benign_html_passes_with_no_findings() {
        let r = sanitize_html("<p>hello <strong>world</strong></p>");
        assert!(!r.had_structural_violations());
        assert!(r.to_findings().is_empty());
        assert!(r.cleaned.contains("<p>"));
    }

    #[test]
    fn findings_include_stripped_tag_names() {
        let r = sanitize_html("<script>x</script><iframe></iframe>");
        let f = r.to_findings();
        assert!(f.iter().any(|finding| finding.detail.contains("<script>")));
        assert!(f.iter().any(|finding| finding.detail.contains("<iframe>")));
    }
}
